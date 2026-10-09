//! PRD #76 M2.2 — `dot-agent-deck remote add --type=ssh ...`.
//!
//! Registers an ssh-reachable host as a deck environment: verifies
//! reachability, installs the matching `dot-agent-deck` binary on the remote
//! (downloaded from GitHub releases), runs `dot-agent-deck hooks install` on
//! the remote, and writes a registry entry to
//! `~/.config/dot-agent-deck/remotes.toml`.
//!
//! All side-effecting ssh work goes through the `SshExecutor` trait so tests
//! can drive the flow with a `FakeSshExecutor` that records commands and
//! returns canned outputs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::untrusted_text::strip_control_and_bidi;
use crate::version::parse_version_output;

/// GitHub releases base URL used to download `dot-agent-deck` binaries onto
/// remote hosts. Kept as a re-export under this crate-local name because three
/// call sites already read it; the repo slug it is built from lives in
/// [`crate::repo_identity`] (issue #945), which `version.rs` derives the
/// release feed from too. Not read from `[package.repository]`, which Cargo
/// doesn't export to the build and our `Cargo.toml` doesn't set.
pub const RELEASE_BASE: &str = crate::repo_identity::RELEASE_DOWNLOAD_BASE;

/// Default ssh port.
pub const DEFAULT_SSH_PORT: u16 = 22;

// ---------------------------------------------------------------------------
// SshExecutor abstraction — the seam that lets us test the add flow without
// shelling out to a real `ssh` binary.
// ---------------------------------------------------------------------------

/// Where to ssh to, parsed from the CLI args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub user: Option<String>,
    pub port: u16,
    pub key: Option<PathBuf>,
    /// PRD #1487 (Risk 4): a `Host` name from `~/.ssh/config` to reach the deck
    /// through, as `ssh -J <alias>` — the deck list's `jump_host`, so `remote
    /// upgrade` takes the route the desktop tunnel uses. Emitted by
    /// [`SystemSshExecutor::build_command`] only when it validates as a
    /// [`crate::remote_tunnel::HostAlias`]; `connect`'s
    /// [`crate::connect::build_connect_command`] still ignores it.
    pub jump: Option<String>,
}

impl SshTarget {
    /// Parse `[user@]host` and combine with `--port` / `--key` flags.
    pub fn parse(target: &str, port: u16, key: Option<PathBuf>) -> Self {
        // The last `@`, as ssh splits a destination (issue #1350's review).
        let (user, host) = crate::deck_list::split_login(target);
        let (user, host) = (user.map(str::to_string), host.to_string());
        Self {
            host,
            user,
            port,
            key,
            jump: None,
        }
    }

    /// `user@host` if a user was given, else just `host`. The form ssh wants
    /// as its destination argument.
    pub fn user_host(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// The command a user should run in a terminal to evaluate this host's key
    /// themselves — the remedy
    /// [`SshError::HostKeyVerificationFailed`] renders.
    ///
    /// **It carries the port, and that is a fix rather than a decoration**
    /// (PRD #741 M5 audit, A5). The remedy used to be built from
    /// [`Self::user_host`], which drops the port, so a failure on port 2222
    /// sent the user to port 22 — a *different endpoint*. `known_hosts` keys a
    /// non-default port as `[host]:port`, so a key accepted at the wrong port
    /// does not satisfy the connection that failed, and the user has been
    /// induced to trust a host key for a host nothing asked them to evaluate.
    ///
    /// `-i` is deliberately absent: host-key verification happens before
    /// authentication, so naming a key buys nothing here, and leaving it out
    /// keeps a `--key` path out of a rendered message.
    ///
    /// The jump host is named when the session takes one (Qodo 4202060288):
    /// without `-J` the remedy reaches the deck by a different route than the
    /// one that failed, or not at all.
    pub fn host_key_remedy(&self) -> String {
        let mut line = String::from("ssh");
        if let Some(jump) = self.valid_jump() {
            line.push_str(&format!(" -J {}", shell_word(jump.as_str())));
        }
        if self.port != DEFAULT_SSH_PORT {
            line.push_str(&format!(" -p {}", self.port));
        }
        line.push(' ');
        line.push_str(&self.user_host());
        line
    }

    /// [`Self::jump`] when it validates as a
    /// [`crate::remote_tunnel::HostAlias`], the form every ssh session and
    /// every printed command uses; an invalid value (a hand-edited registry)
    /// is dropped, as [`SystemSshExecutor::build_command`] drops it.
    pub fn valid_jump(&self) -> Option<crate::remote_tunnel::HostAlias> {
        crate::remote_tunnel::HostAlias::parse(self.jump.as_deref()?).ok()
    }

    /// A command line the user can paste to run `remote_command` on this
    /// remote themselves: the registered port and identity file included, so
    /// it reaches the endpoint the deck talks to rather than port 22 (PR
    /// #1373 review — the same wrong-endpoint defect `host_key_remedy`
    /// records). Every word is quoted for a POSIX shell whenever it holds
    /// anything outside a plain set. For the destination and key path, which
    /// came from the user's `remote add`, that makes pasting the line run only
    /// `ssh` locally. For `remote_command`, it keeps a `~` for the REMOTE shell
    /// to expand, not the laptop's, whose home can be a different path (PR
    /// #1373 review). A destination
    /// that starts with `-` is preceded by `--` so ssh cannot read it as an
    /// option (PR #1373 review). The jump host rides along as `-J`, in the
    /// position [`SystemSshExecutor::build_command`] passes it, so the line
    /// takes the deck's route (Qodo 4202060288).
    pub fn command_line(&self, remote_command: &str) -> String {
        let mut line = String::from("ssh");
        if self.port != DEFAULT_SSH_PORT {
            line.push_str(&format!(" -p {}", self.port));
        }
        if let Some(key) = &self.key {
            line.push_str(&format!(" -i {}", shell_word(&key.to_string_lossy())));
        }
        if let Some(jump) = self.valid_jump() {
            line.push_str(&format!(" -J {}", shell_word(jump.as_str())));
        }
        let destination = self.user_host();
        if destination.starts_with('-') {
            line.push_str(" --");
        }
        line.push_str(&format!(
            " {} {}",
            shell_word(&destination),
            shell_word(remote_command)
        ));
        line
    }
}

/// `word` as one POSIX shell word: unchanged when it is made only of
/// characters no shell treats specially, single-quoted otherwise.
pub(crate) fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./_-".contains(c));
    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// Captured output of one ssh invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Failure modes the executor distinguishes. Mapped from ssh's stderr/exit
/// status by `SystemSshExecutor`; the production matcher is conservative —
/// anything we can't classify ends up as `Other` so the caller can surface
/// stderr.
///
/// Every `detail` field here is remote-controlled text and is scrubbed by
/// [`scrub_remote_text`] at construction, so this type's `Display` — printed
/// raw to the operator's terminal by `main.rs` — is safe by construction. See
/// that function for why the seam is here and not at the `eprintln!`.
#[derive(Debug, Error)]
pub enum SshError {
    #[error(
        "Could not reach {host}:{port}. Check the host is up and ssh is exposed on this port.\nDetails: {detail}"
    )]
    ConnectionRefused {
        host: String,
        port: u16,
        detail: String,
    },
    #[error(
        "ssh authentication to {target} failed. Check your key (`--key`) or `~/.ssh/config`.\nDetails: {detail}"
    )]
    AuthFailed { target: String, detail: String },
    #[error("ssh I/O error contacting {target}: {source}")]
    Io {
        target: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "ssh failed: host key not yet trusted for {target}. If this is a first-time connection, run `{remedy}` once to accept the host key, then retry. If the key has changed unexpectedly, investigate before connecting."
    )]
    HostKeyVerificationFailed { target: String, remedy: String },
    #[error("ssh to {target} failed: {detail}")]
    Other { target: String, detail: String },
}

/// Scrub text the *remote* wrote — ssh's own stderr, or the stdout/stderr of a
/// command we ran over ssh — before it becomes a field of an error whose
/// `Display` is printed straight to the operator's terminal.
///
/// `remote add` is the sharpest case this exists for: it is the **first**
/// contact with an endpoint the operator has not yet decided to trust, it runs
/// before any registry entry exists, and its failure path is
/// `eprintln!("{e}")`. A host that merely answers the connection could
/// therefore write a CSI/OSC sequence into the terminal — clear the screen,
/// move the cursor over earlier output, retitle the window — or plant a bidi
/// override that visually reorders the line the operator is reading to decide
/// what went wrong (issue #723).
///
/// Sealed here, at the boundary where a remote-controlled byte enters an error
/// type, rather than at `main.rs`'s four `eprintln!("{e}")` sites, for the
/// reason [`crate::project_config::ProjectConfigError`]'s `Display` records:
/// one seam covers every sink by construction and cannot be forgotten by a
/// fifth caller added later. Sanitizing at the print sites would pass the same
/// tests today and rot the first time someone adds a `remote` subcommand.
///
/// **Stripping, not escaping**, and newlines survive. `SshError`'s messages
/// deliberately put `Details:` on its own line and multi-line ssh stderr is
/// genuinely useful diagnostic output, so this passes `keep_newlines: true`;
/// the leading/trailing trim that the call sites used to do themselves happens
/// *after* the strip, so a `"\x1b[2J\n"` tail cannot leave a stray blank
/// line behind. `remote doctor` reaches for
/// [`crate::untrusted_text::escape_control_and_bidi`] instead because a
/// *report* exists to show what the peer actually sent; an error message is
/// read for its advice, so the noise is better dropped than displayed. The one
/// place those two policies meet is [`crate::connect::ssh_error_detail`],
/// which reads `SshError`'s `detail` for the doctor and escapes it: it now
/// receives a stripped value, so its escape became a second line of defence
/// and the doctor quotes the residue rather than the escaped original. See its
/// doc comment — that trade is recorded there.
pub(crate) fn scrub_remote_text(s: &str) -> String {
    strip_control_and_bidi(s, true).trim().to_string()
}

/// Map ssh's stderr output (when the process exited with 255) onto a typed
/// `SshError` variant. Extracted from `SystemSshExecutor::run` so it can be
/// unit-tested without spawning a process.
///
/// Public since PRD #741 M5, which reuses it rather than growing a third
/// classifier: the `ssh -N -L` tunnel's child dies with the same stderr and the
/// same exit 255, and `HostKeyVerificationFailed` already carries the shape of
/// remedy a GUI has to show — run the connection once in a terminal. What it
/// does **not** carry is the remedy's text, which is why
/// [`classify_ssh_error_with_remedy`] exists: this function fills it from
/// [`SshTarget::host_key_remedy`], and the tunnel fills it from its endpoint so
/// the bastion and the port survive (M5 audit A5).
///
/// Classification matches against the **raw** stderr while `detail` carries the
/// [`scrub_remote_text`] form: the matcher looks for ssh's own fixed phrases,
/// and scrubbing first could only ever change what it sees. PRD #741 M5's
/// tunnel did not honour that and now does — see
/// [`crate::remote_tunnel::RemoteTunnel::stderr_raw_text`].
pub fn classify_ssh_error(target: &SshTarget, stderr: &str) -> SshError {
    classify_ssh_error_with_remedy(target, stderr, &target.host_key_remedy())
}

/// [`classify_ssh_error`] with the host-key remedy supplied by the caller.
///
/// Exists because an [`SshTarget`] cannot name every endpoint this crate
/// reaches: PRD #741's tunnel may go through `ssh -J <bastion>`, and a jump
/// host has no field in this type. The remedy a user is told to run has to
/// describe the endpoint that actually failed — port and bastion included —
/// or it sends them to a different host (audit A5). `remote add` and
/// `remote doctor` keep [`SshTarget::host_key_remedy`], which is the same
/// string plus the port they were dropping.
pub fn classify_ssh_error_with_remedy(
    target: &SshTarget,
    stderr: &str,
    host_key_remedy: &str,
) -> SshError {
    let lower = stderr.to_ascii_lowercase();
    let detail = scrub_remote_text(stderr);
    if lower.contains("connection refused")
        || lower.contains("network is unreachable")
        || lower.contains("no route to host")
        || lower.contains("could not resolve hostname")
        || lower.contains("connection timed out")
    {
        return SshError::ConnectionRefused {
            host: target.host.clone(),
            port: target.port,
            detail,
        };
    }
    // Host-key issues are checked BEFORE the generic auth match because under
    // BatchMode=yes the canonical message is "Host key verification failed."
    // and the user's recourse (run `ssh <target>` once) differs from a key
    // mismatch. We treat both first-trust and key-changed scenarios as the
    // same variant — the Display message tells the user to investigate.
    if lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
        || lower.contains("are you sure you want to continue connecting")
    {
        return SshError::HostKeyVerificationFailed {
            target: target.user_host(),
            remedy: host_key_remedy.to_string(),
        };
    }
    if lower.contains("permission denied") || lower.contains("publickey") {
        return SshError::AuthFailed {
            target: target.user_host(),
            detail,
        };
    }
    SshError::Other {
        target: target.user_host(),
        detail,
    }
}

/// What one *capped* ssh invocation produced: the captured output, plus
/// whether the cap cut it short.
///
/// The flag is the entire reason this type exists. [`SshExecutor::run_capped`]
/// used to hand back a bare [`SshOutput`], so no caller could tell a complete
/// answer from the first `max_capture_bytes` of a flood — and a prefix that
/// happens to parse is the most dangerous shape a bounded read has (PRD #345
/// audit). Two concrete cases were reachable: a status-0 remote printing
/// exactly `PROBE_VERSION_CAP` bytes that merely *begin* with a valid
/// `dot-agent-deck <version>` pair, and a valid protocol JSON reply padded
/// with whitespace to exactly `PROBE_PROTOCOL_STDOUT_CAP` — both parsed clean
/// and were trusted. [`run_local_bounded`] has always distinguished the two
/// outcomes; the signal was simply dropped on the way back to these callers.
///
/// Every caller must decide what a truncated stream means *before* parsing it,
/// and none of them may parse it as authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CappedOutput {
    pub output: SshOutput,
    /// Whether either stream reached `max_capture_bytes`, so what is here is a
    /// prefix of what the remote wanted to say. Mirrors
    /// [`LocalCapture::truncated`] exactly, including that a stream landing
    /// *on* the cap counts as truncated: a drainer that stops at the cap
    /// cannot tell "that was all" from "there was more", and erring toward
    /// "I could not read all of it" is the safe direction for every caller.
    pub truncated: bool,
}

/// Abstraction over running shell commands on a remote ssh host. The
/// production impl shells out to the `ssh` binary; tests use a fake.
pub trait SshExecutor {
    fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError>;

    /// Variant of [`run`](Self::run) that caps the captured output at
    /// `max_capture_bytes` per stream. The default implementation calls `run`
    /// and truncates the resulting stdout `String` afterward — adequate for
    /// tests and any executor whose underlying transport already bounds
    /// memory. The production [`SystemSshExecutor`] overrides this to bound
    /// the in-memory capture for BOTH stdout and stderr at the pipe-drain
    /// layer (each pipe is a separate DoS vector), so a hostile remote that
    /// streams unbounded output on either stream can't push the laptop into
    /// memory pressure before the cap is observed.
    ///
    /// The default impl only caps stdout because the test-grade fallback
    /// doesn't have a pipe-drain layer to extend; production callers that
    /// care about the symmetric cap go through `SystemSshExecutor`.
    ///
    /// Both impls report [`CappedOutput::truncated`] the same way — either
    /// stream *reaching* the cap — so a caller's cap handling does not change
    /// under a fake.
    fn run_capped(
        &self,
        target: &SshTarget,
        command: &str,
        max_capture_bytes: usize,
    ) -> Result<CappedOutput, SshError> {
        let mut output = self.run(target, command)?;
        let truncated =
            output.stdout.len() >= max_capture_bytes || output.stderr.len() >= max_capture_bytes;
        if output.stdout.len() > max_capture_bytes {
            // Truncate on a UTF-8 char boundary at or before the cap so the
            // returned `String` stays a valid `String` (callers JSON-parse
            // it). `floor_char_boundary` is unstable, so do the search by
            // hand: walk backward at most 3 bytes to land on a char boundary
            // (UTF-8 chars are at most 4 bytes, so the boundary is within 3
            // bytes of any cap position).
            let mut cap = max_capture_bytes;
            while cap > 0 && !output.stdout.is_char_boundary(cap) {
                cap -= 1;
            }
            output.stdout.truncate(cap);
        }
        Ok(CappedOutput { output, truncated })
    }

    /// [`run_capped`](Self::run_capped) under a laptop-side wall-clock
    /// `deadline` of the call's own, whatever the executor was built for.
    ///
    /// PRD #1487 audit A1: the remote-upgrade executor is deliberately built
    /// with no wall-clock kill, because a release download may legitimately
    /// take minutes. The short plumbing commands that reach the remote daemon
    /// run on the same ssh target and jump route, but a reply to one of them is
    /// a few KiB and arrives in seconds, so they must be bounded per command:
    /// both streams capped while they drain, and the session killed at
    /// `deadline`. The production [`SystemSshExecutor`] does exactly that; this
    /// default, for fakes whose transport is already bounded, ignores
    /// `deadline` and falls back to [`run_capped`](Self::run_capped).
    fn run_capped_within(
        &self,
        target: &SshTarget,
        command: &str,
        max_capture_bytes: usize,
        deadline: std::time::Duration,
    ) -> Result<CappedOutput, SshError> {
        let _ = deadline;
        self.run_capped(target, command, max_capture_bytes)
    }

    /// The shortest `deadline` [`run_capped_within`](Self::run_capped_within)
    /// honours: a caller with less time left should not start a command. Zero
    /// for this default, which ignores the deadline; the production
    /// [`SystemSshExecutor`] counts whole seconds and returns
    /// [`MIN_BOUNDED_REMOTE_RUN`].
    fn min_bounded_run(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

/// The shortest deadline [`SystemSshExecutor`]'s
/// [`SshExecutor::run_capped_within`] honours. Its kill timer counts whole
/// seconds, so a deadline is rounded down to one, and one under a second is
/// refused without starting a session — a caller with less than this left
/// should not start a remote command at all.
pub const MIN_BOUNDED_REMOTE_RUN: std::time::Duration = std::time::Duration::from_secs(1);

/// Issue #858: seconds of headroom the laptop-side wallclock kill gets on top
/// of the worst case the probe's own ssh options permit.
///
/// The kill's job is the case those options cannot see — a remote that
/// completes the handshake, answers keepalives, and still never returns. It
/// only ever does that job if it fires *after* ssh has given up on its own, so
/// the margin exists to keep the two from racing on a link whose real latency
/// lands near the bound. Five seconds is small next to the budgets involved
/// and large next to the scheduling jitter of a 50ms poll loop.
const WALLCLOCK_KILL_MARGIN_SECS: u64 = 5;

/// The laptop-side wallclock deadline, in seconds, for an ssh invocation whose
/// options [`SystemSshExecutor::build_command`] built from `ssh_budget_secs`.
///
/// Those options are `ConnectTimeout=secs`, `ServerAliveInterval=secs` and
/// `ServerAliveCountMax=1`, which bound ssh's own wallclock at roughly
/// `2 * secs`: up to `secs` in the pre-handshake phase, then up to one missed
/// keepalive interval of post-handshake silence.
///
/// **Arming the kill at `secs` made it the binding constraint rather than the
/// backstop it is documented to be** (issue #858): a connection ssh was still
/// allowed ~20s to complete was killed at 10s and reported to the user as
/// `probe exceeded 10s wallclock deadline`, on a link that worked when they
/// immediately ran the command again. Deriving the deadline from the same
/// number the options are built from keeps the kill strictly outside them
/// whatever `secs` is — which is why the fix is this derivation and not a
/// larger `PROBE_TIMEOUT_SECS_DEFAULT`, a change that would have scaled the
/// same inconsistency instead of removing it.
///
/// Saturating throughout: the probe timeout is clamped to an hour at its own
/// call sites, but this is reachable with any `u64` and must not panic.
pub const fn wallclock_kill_secs(ssh_budget_secs: u64) -> u64 {
    // ConnectTimeout=secs, then ServerAliveInterval=secs x ServerAliveCountMax=1.
    let ssh_worst_case = ssh_budget_secs.saturating_mul(2);
    ssh_worst_case.saturating_add(WALLCLOCK_KILL_MARGIN_SECS)
}

/// Production implementation: shells out to the `ssh` binary on the user's
/// machine. No new dependency on a Rust ssh client — we deliberately reuse
/// the user's existing ssh config (`~/.ssh/config`, agent, known_hosts).
///
/// `wallclock_timeout` is an optional fail-fast cap used by short-lived probe
/// commands (e.g. M2.9's `connect` version probe). It applies on TWO layers:
///
/// 1. The ssh `-o ConnectTimeout=N -o ServerAliveInterval=N -o
///    ServerAliveCountMax=1` triple, which catches transport-level stalls
///    (DNS, TCP, ssh handshake, dead-TCP keepalives).
/// 2. A laptop-side wallclock kill enforced by `run_with_wallclock_kill`
///    inside `run()`, which catches the reachable-but-stalled-remote-command
///    case the ssh options miss: server completes the handshake and answers
///    keepalives, but the remote command hangs before producing output. Its
///    deadline is NOT `wallclock_timeout` itself but [`wallclock_kill_secs`]
///    of it, so the kill outlasts what layer 1 permits instead of pre-empting
///    it (issue #858). Read the deadline back with
///    [`SystemSshExecutor::kill_deadline_secs`].
///
/// `None` keeps the original behavior on both layers — relevant for
/// long-running invocations like `remote add`'s install pipeline where a 10s
/// ceiling would be too tight.
pub struct SystemSshExecutor {
    wallclock_timeout: Option<u64>,
    /// PRD #345 audit: this executor's sessions are *observation* sessions —
    /// they must not create the state they are inspecting. See
    /// [`apply_observation_options`] for the flags and the reasoning.
    observation: bool,
    /// PRD #161 FIX 3: ssh keepalive + ConnectTimeout for the remote-UPGRADE
    /// path. Unlike `wallclock_timeout` (which imposes a hard laptop-side
    /// wallclock kill — fine for short probes), upgrade must tolerate a
    /// legitimately long release download. Keepalives DETECT a dead/stalled
    /// connection (the server stops answering `ServerAlive` probes) without
    /// capping a slow-but-alive transfer. `None` keeps the original behavior.
    /// Mutually exclusive with `wallclock_timeout` by construction.
    keepalive: Option<SshKeepalive>,
    /// Issue #1490: force `StrictHostKeyChecking=yes`, so a session refuses a
    /// host whose key is not already trusted instead of following a permissive
    /// user `Host` block. See [`Self::requiring_known_host_key`].
    require_known_host_key: bool,
    /// The ssh client to run: `ssh`, found on `PATH`. Only a unit test points
    /// it elsewhere, at a stand-in that misbehaves the way a hostile remote
    /// would, so the production executor's own bounds are what get exercised.
    program: std::ffi::OsString,
}

/// PRD #161 FIX 3: ssh keepalive parameters for the remote-upgrade executor.
#[derive(Clone, Copy)]
struct SshKeepalive {
    /// `-o ConnectTimeout=` — caps the pre-handshake phase (DNS, TCP, ssh).
    connect_timeout: u64,
    /// `-o ServerAliveInterval=` — seconds of silence before a keepalive probe.
    interval: u64,
    /// `-o ServerAliveCountMax=` — unanswered probes tolerated before ssh
    /// declares the connection dead and disconnects.
    count_max: u32,
}

impl SystemSshExecutor {
    pub fn new() -> Self {
        Self {
            wallclock_timeout: None,
            observation: false,
            keepalive: None,
            require_known_host_key: false,
            program: "ssh".into(),
        }
    }

    /// Construct an executor with a fail-fast wallclock cap (in seconds) on
    /// each ssh invocation. Used by the M2.9 connect-probe path; without it,
    /// `executor.run(...)` could hang indefinitely on a reachable-but-stalled
    /// remote shell because `cmd.output()` provides no timeout of its own.
    pub fn with_wallclock_timeout(secs: u64) -> Self {
        Self {
            wallclock_timeout: Some(secs),
            observation: false,
            keepalive: None,
            require_known_host_key: false,
            program: "ssh".into(),
        }
    }

    /// PRD #345: an executor whose every session is an **observation** session
    /// — same fail-fast wallclock cap as [`Self::with_wallclock_timeout`],
    /// plus the [`apply_observation_options`] flags that stop the session from
    /// creating, persisting or writing any of the state it is there to read.
    ///
    /// Built for `remote doctor`, whose whole contract is that it probes and
    /// reports without mutating anything. Use it for any future read-only
    /// inspection over ssh; do NOT use it for `connect`, `remote add` or
    /// `remote upgrade`, whose sessions are supposed to honour the user's
    /// `Host` block in full.
    pub fn for_observation(secs: u64) -> Self {
        Self {
            wallclock_timeout: Some(secs),
            observation: true,
            keepalive: None,
            require_known_host_key: false,
            program: "ssh".into(),
        }
    }

    /// PRD #161 FIX 3: construct an executor for the remote-UPGRADE path with
    /// ssh keepalives (and a ConnectTimeout) but NO laptop-side wallclock kill.
    /// A dropped / stalled connection is DETECTED — ssh disconnects after the
    /// server fails to answer `count_max` consecutive keepalive probes (~
    /// `interval * count_max` seconds of a truly dead link) — while a
    /// legitimately long-but-alive release download keeps answering probes at
    /// the transport layer and is NOT killed (which a hard wallclock cap would
    /// wrongly do).
    pub fn with_keepalive(connect_timeout: u64, interval: u64, count_max: u32) -> Self {
        Self {
            wallclock_timeout: None,
            observation: false,
            keepalive: Some(SshKeepalive {
                connect_timeout,
                interval,
                count_max,
            }),
            require_known_host_key: false,
            program: "ssh".into(),
        }
    }

    /// Make every session of this executor an **observation** session — the
    /// [`apply_observation_options`] flags, the one list of them — whatever
    /// bounds it was built with. [`Self::for_observation`] is this over the
    /// wallclock cap; issue #1490's desktop checks and start use it over the
    /// keepalive bounds, because they run unattended against a deck the app
    /// is not connected to and need to authenticate to the host, never to
    /// delegate a credential to it.
    pub fn observing(mut self) -> Self {
        self.observation = true;
        self
    }

    /// Force `StrictHostKeyChecking=yes` (issue #1490), the host-key policy the
    /// desktop's tunnel forces ([`crate::remote_tunnel`]'s `forced_options`).
    /// A session to a host whose key is not already trusted then fails with
    /// [`SshError::HostKeyVerificationFailed`] instead of following a user
    /// `Host` block's `accept-new` or `no` — so a desktop check or start
    /// cannot silently accept a key the tunnel would then refuse. Not part of
    /// [`apply_observation_options`], whose docs say why `remote doctor` must
    /// still work against a host it has never connected to.
    pub fn requiring_known_host_key(mut self) -> Self {
        self.require_known_host_key = true;
        self
    }

    /// The laptop-side wallclock deadline this executor arms on each ssh
    /// invocation, in seconds — `None` when it imposes no kill at all.
    ///
    /// Exposed so a test can assert the deadline against the ssh options
    /// [`Self::build_command`] emits alongside it, rather than against a
    /// hard-coded number.
    pub fn kill_deadline_secs(&self) -> Option<u64> {
        self.wallclock_timeout.map(wallclock_kill_secs)
    }

    /// Run `program` instead of `ssh` — a test's stand-in for a misbehaving
    /// remote. Everything else about the executor stays as built.
    #[cfg(test)]
    pub(crate) fn with_program(mut self, program: impl Into<std::ffi::OsString>) -> Self {
        self.program = program.into();
        self
    }

    /// Build the `ssh` command without spawning it. Exposed for tests so we
    /// can verify argument quoting without forking a subprocess.
    pub fn build_command(&self, target: &SshTarget, remote_command: &str) -> Command {
        let mut cmd = Command::new(&self.program);
        // BatchMode=yes makes ssh fail fast on missing keys/known_hosts
        // instead of hanging on a TTY prompt. Users who haven't trusted the
        // host yet will see an actionable error rather than the deck CLI
        // wedging.
        cmd.arg("-o").arg("BatchMode=yes");
        if self.require_known_host_key {
            cmd.arg("-o").arg("StrictHostKeyChecking=yes");
        }
        if self.observation {
            apply_observation_options(&mut cmd);
        }
        if let Some(secs) = self.wallclock_timeout {
            // ConnectTimeout caps the pre-handshake phase (DNS, TCP, ssh
            // handshake). ServerAliveInterval + ServerAliveCountMax=1 forces
            // a disconnect after `secs` of post-handshake silence, so a
            // remote shell that accepts the connection but never produces
            // output can't pin the probe forever. Together they bound the
            // wallclock at roughly 2*secs in the worst case.
            cmd.arg("-o").arg(format!("ConnectTimeout={secs}"));
            cmd.arg("-o").arg(format!("ServerAliveInterval={secs}"));
            cmd.arg("-o").arg("ServerAliveCountMax=1");
        }
        if let Some(ka) = self.keepalive {
            // PRD #161 FIX 3 (remote upgrade): detect a dead/stalled connection
            // without a hard wallclock cap. ServerAliveInterval +
            // ServerAliveCountMax disconnect only after the server fails to
            // answer `count_max` consecutive keepalive probes (~
            // interval*count_max seconds of a truly dead link) — a
            // slow-but-alive download keeps answering at the transport layer
            // and is NOT killed. ConnectTimeout still caps the pre-handshake
            // phase. Mutually exclusive with `wallclock_timeout` above.
            cmd.arg("-o")
                .arg(format!("ConnectTimeout={}", ka.connect_timeout));
            cmd.arg("-o")
                .arg(format!("ServerAliveInterval={}", ka.interval));
            cmd.arg("-o")
                .arg(format!("ServerAliveCountMax={}", ka.count_max));
        }
        cmd.arg("-p").arg(target.port.to_string());
        if let Some(key) = &target.key {
            cmd.arg("-i").arg(key);
        }
        // PRD #1487: the deck list's jump host, validated as the tunnel
        // validates it — a name from the user's ssh config, nothing an option
        // or a `ProxyCommand` could reinterpret. A value that fails the check
        // (a hand-edited registry) is dropped with a warning rather than passed.
        if let Some(jump) = &target.jump {
            match crate::remote_tunnel::HostAlias::parse(jump) {
                Ok(alias) => {
                    cmd.arg("-J").arg(alias.as_str());
                }
                Err(e) => tracing::warn!(
                    target: "remote",
                    error = %e,
                    "ignoring an invalid jump host for this ssh session"
                ),
            }
        }
        cmd.arg("--");
        cmd.arg(target.user_host());
        // Pass the remote command as a single argv entry. ssh joins remaining
        // args with spaces and runs them through the remote shell, so passing
        // one arg keeps quoting predictable. NOTE: `remote_command` is a
        // string we (the deck) construct entirely from internal templates and
        // the resolved version/platform — no user input is interpolated into
        // shell here. The parsed user@host arg also goes through `arg(...)`,
        // not through a shell, so there's no local shell-injection surface.
        cmd.arg(remote_command);
        cmd
    }
}

/// Turn an ssh invocation into an **observation** session: one that reads the
/// remote's state without becoming part of it (PRD #345 audit).
///
/// Ordinary deck sessions deliberately honour the user's `Host` block, which
/// is exactly what a diagnostic must not do. Without these flags a probe
/// session applies the block's `LocalForward` / `DynamicForward` /
/// `RemoteForward`, so the doctor **creates the very forward it then checks**
/// — a bindable reverse forward passes because the doctor bound it, not
/// because a user session had. It also breaks the PRD's "never mutates the
/// remote" criterion in four concrete ways: a reverse-*dynamic* forward
/// briefly exposes the laptop's reachable network through a SOCKS listener on
/// the remote; `ControlMaster`/`ControlPersist` can leave a master connection
/// *and its forwards* alive after the command exits; `UpdateHostKeys` writes
/// `known_hosts`; and `PermitLocalCommand` runs whatever `LocalCommand` the
/// config names. `BatchMode=yes` disables none of that.
///
/// So, in order:
///
/// - `ClearAllForwardings=yes` — the session creates no forward, so the
///   liveness probe observes **pre-existing** remote state and answers the
///   honest question ("is something already listening there?") instead of a
///   self-fulfilling one.
/// - `ControlMaster=no` / `ControlPath=none` — never join or spawn a shared
///   master, so nothing (and no forward) outlives the probe, and the probe
///   never rides a *pre-existing* master that already carries the user's
///   forwards, which would silently defeat `ClearAllForwardings`.
/// - `PermitLocalCommand=no` — a diagnostic runs no side effects on the laptop.
/// - `UpdateHostKeys=no` — reading a host's configuration must not rewrite
///   `known_hosts`.
///
/// Then the **delegation** half, which `ClearAllForwardings` does NOT cover
/// (PRD #345 second audit, verified against OpenSSH 10.2: `ssh -G -o
/// ClearAllForwardings=yes -o ForwardAgent=yes -o ForwardX11=yes` still
/// resolves `forwardagent yes` and `forwardx11 yes` — that option clears
/// local, remote, dynamic and tunnel forwards and nothing else):
///
/// - `ForwardAgent=no` — the sharp one. A `Host` block carrying `ForwardAgent
///   yes` otherwise exposes the laptop's ssh-agent to the endpoint on *every*
///   probe — version, protocol, `sshd -T`, liveness — and does so before the
///   report's own `ForwardAgent` advisory has been rendered. A compromised
///   endpoint cannot extract private key material through an agent socket,
///   but it can *use* the key to authenticate or sign as the user for the
///   life of the probe, and that is the damaging capability. `remote doctor`
///   is precisely the command you run against an endpoint you already
///   suspect, so inheriting credential delegation is an unsafe default here
///   in a way it is not for `connect`.
/// - `ForwardX11=no` / `ForwardX11Trusted=no` — X11 forwarding hands the
///   endpoint a channel to the laptop's display; trusted forwarding removes
///   even the X security-extension restrictions on it.
/// - `GSSAPIDelegateCredentials=no` — the Kerberos-flavoured spelling of the
///   same mistake: a delegated TGT lets the endpoint act as the user against
///   every service in the realm.
/// - `AddKeysToAgent=no` — a diagnostic must not leave a new identity loaded
///   in the user's agent as a side effect of having run.
///
/// None of the five has a legitimate use in an observation session: nothing
/// the doctor runs on the remote needs the user's credentials, a display, or a
/// realm ticket. Note the check-vs-session split this relies on — the report's
/// `ForwardAgent` advisory reads the user's *configured* value out of `ssh
/// -G`, which is built by [`ssh_config_dump_command`](crate::remote_doctor)
/// and deliberately carries none of these flags, so forcing them here does not
/// change what the report says about the user's config.
///
/// What is deliberately NOT here: `StrictHostKeyChecking`. Host-key
/// *verification* is a security control, not a mutation, and weakening it to
/// make a diagnostic quieter would be strictly worse than the problem —
/// but neither is it *tightened*, and the consequence is worth naming rather
/// than papering over. `UpdateHostKeys=no` stops the rotation-driven rewrite;
/// it does not stop **first-use** persistence. Under a user config that sets
/// `StrictHostKeyChecking accept-new` (or `no`/`off`), a host key the deck has
/// never seen is still appended to `known_hosts` by this session. Forcing
/// `yes` would break the legitimate first-run case — a diagnostic is exactly
/// what you reach for on a remote you have not connected to yet, and failing
/// with "host key not known" would make the command useless precisely when it
/// is most wanted. So: the doctor issues no *deck-authored* mutation and
/// suppresses every delegation and persistence option it can without weakening
/// verification, and ssh still honours what the user's own config tells it to
/// do on any connection. `docs/remote-recipes.md` states that scope to users
/// in the same terms.
///
/// Do not "helpfully" drop these flags; each one is load-bearing, and removing
/// `ClearAllForwardings` in particular restores a check that reports PASS
/// because of its own side effect.
fn apply_observation_options(cmd: &mut Command) {
    for option in [
        "ClearAllForwardings=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "PermitLocalCommand=no",
        "UpdateHostKeys=no",
        // Delegation and agent persistence — NOT covered by
        // `ClearAllForwardings`, which clears forwards only.
        "ForwardAgent=no",
        "ForwardX11=no",
        "ForwardX11Trusted=no",
        "GSSAPIDelegateCredentials=no",
        "AddKeysToAgent=no",
    ] {
        cmd.arg("-o").arg(option);
    }
}

impl Default for SystemSshExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl SshExecutor for SystemSshExecutor {
    fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
        let mut cmd = self.build_command(target, command);
        let output = match self.kill_deadline_secs() {
            // Uncapped, so the truncation flag is always false here.
            Some(secs) => run_with_wallclock_kill(&mut cmd, target, secs, None)?.0,
            None => cmd.output().map_err(|source| SshError::Io {
                target: target.user_host(),
                source,
            })?,
        };
        let status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        // ssh uses exit 255 to signal its own (i.e. transport/auth) errors;
        // anything else is the remote command's exit code. Only translate to
        // typed transport errors on 255 — otherwise return the SshOutput so
        // callers can decide based on the remote command's status.
        if status == 255 {
            return Err(classify_ssh_error(target, &stderr));
        }

        Ok(SshOutput {
            status,
            stdout,
            stderr,
        })
    }

    /// Override the default trait impl so the byte cap is enforced at the
    /// pipe-drain layer, not after the full payload lands in memory. A
    /// hostile remote binary that streams unbounded output on *either*
    /// stdout or stderr would otherwise push the laptop into memory pressure
    /// long before the trait default's post-hoc `truncate` ran. The
    /// wallclock-kill path threads `max_capture_bytes` through to the
    /// per-stream drainer and applies it symmetrically to stdout and stderr
    /// (each pipe is a separate attack vector); the no-timeout path is only
    /// used by long-running install commands (not probes), so it falls back
    /// to the default capture-then-truncate behavior.
    ///
    /// Either way the drainer's own [`CappedOutput::truncated`] verdict is
    /// carried out to the caller rather than dropped — see that type for the
    /// two probes that were parsing cap-limited prefixes as authoritative.
    fn run_capped(
        &self,
        target: &SshTarget,
        command: &str,
        max_capture_bytes: usize,
    ) -> Result<CappedOutput, SshError> {
        let Some(secs) = self.kill_deadline_secs() else {
            // No wallclock: use the post-hoc truncation from the default impl.
            // This branch is only reached by callers that have opted out of
            // the timeout (e.g. `remote add`'s install pipeline), which today
            // do not need a byte cap.
            let mut output = SshExecutor::run(self, target, command)?;
            let truncated = output.stdout.len() >= max_capture_bytes
                || output.stderr.len() >= max_capture_bytes;
            if output.stdout.len() > max_capture_bytes {
                let mut cap = max_capture_bytes;
                while cap > 0 && !output.stdout.is_char_boundary(cap) {
                    cap -= 1;
                }
                output.stdout.truncate(cap);
            }
            return Ok(CappedOutput { output, truncated });
        };

        let mut cmd = self.build_command(target, command);
        let (output, truncated) =
            run_with_wallclock_kill(&mut cmd, target, secs, Some(max_capture_bytes))?;
        let status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        if status == 255 {
            return Err(classify_ssh_error(target, &stderr));
        }

        Ok(CappedOutput {
            output: SshOutput {
                status,
                stdout,
                stderr,
            },
            truncated,
        })
    }

    /// Always bounded, whatever this executor was built for: both streams are
    /// capped at `max_capture_bytes` while they drain, and the session is
    /// killed at `deadline` rounded down to whole seconds (or at this
    /// executor's own kill deadline, when it has a shorter one). A deadline
    /// under [`MIN_BOUNDED_REMOTE_RUN`] starts nothing and is an error. The ssh options the executor was built with — the
    /// upgrade path's keepalives, a jump host — are kept, so the command takes
    /// the same route as every other session to that deck (PRD #1487 audit A1).
    fn run_capped_within(
        &self,
        target: &SshTarget,
        command: &str,
        max_capture_bytes: usize,
        deadline: std::time::Duration,
    ) -> Result<CappedOutput, SshError> {
        // Whole seconds, rounded DOWN: `run_local_bounded`'s granularity. A
        // deadline under one second cannot be honoured, so nothing is started
        // rather than granting the session a whole second the caller does not
        // have (PRD #1487, Qodo review item 13).
        let mut secs = deadline.as_secs();
        if let Some(own) = self.kill_deadline_secs() {
            secs = secs.min(own);
        }
        if secs == 0 {
            return Err(SshError::Other {
                target: target.user_host(),
                detail: format!(
                    "{:.1}s left is less than the {}s a remote command needs, so it was not started",
                    deadline.as_secs_f64(),
                    MIN_BOUNDED_REMOTE_RUN.as_secs()
                ),
            });
        }
        let mut cmd = self.build_command(target, command);
        // Its own process group: a `ProxyCommand` or jump-route helper that
        // outlives the session is killed with it (PRD #1487 re-check R1).
        let capture = run_local_bounded_owning_group(&mut cmd, secs, max_capture_bytes).map_err(
            |source| SshError::Io {
                target: target.user_host(),
                source,
            },
        )?;
        let Some(status) = capture.status else {
            return Err(SshError::Other {
                target: target.user_host(),
                detail: format!(
                    "the remote command did not finish within {secs}s, so it was stopped"
                ),
            });
        };
        let status = status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&capture.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&capture.stderr).into_owned();
        if status == 255 {
            return Err(classify_ssh_error(target, &stderr));
        }
        Ok(CappedOutput {
            output: SshOutput {
                status,
                stdout,
                stderr,
            },
            truncated: capture.truncated,
        })
    }

    fn min_bounded_run(&self) -> std::time::Duration {
        MIN_BOUNDED_REMOTE_RUN
    }
}

/// What one bounded local subprocess run produced.
///
/// Distinguishes the three outcomes a caller has to treat differently — the
/// process finished (`status` is `Some`), the deadline fired first
/// (`timed_out`), or a stream hit its byte cap (`truncated`) — instead of
/// collapsing them into a plain [`std::process::Output`] whose empty stdout
/// reads like an authoritative "nothing configured".
pub struct LocalCapture {
    /// The child's exit status, or `None` when the deadline killed it.
    pub status: Option<std::process::ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether either stream reached `max_capture_bytes`, so what is here is a
    /// prefix and not the whole output. A truncated dump must never be parsed
    /// as if it were complete.
    pub truncated: bool,
    /// Whether the wallclock deadline fired and the child was killed.
    pub timed_out: bool,
}

/// How long a stream may stay open after the child [`run_local_bounded`]
/// spawned has exited. A process that exited has said everything it will say —
/// what it wrote is already in the pipe — so a stream still open past this is
/// held by a descendant that inherited it (a `ProxyCommand`, a jump-route
/// helper, a wrapper, a `ControlPersist` master started with `-v`), and waiting
/// on it would put that descendant's lifetime in charge of the call's.
const POST_EXIT_STREAM_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How long a cancelled stream reader gets to drain what is already buffered
/// and close its pipe. On Unix the reader is non-blocking and stops within one
/// poll tick; where it cannot be made non-blocking it is abandoned after this.
const READER_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// After a cancel, a reader keeps reading only what the kernel already holds
/// — at most a pipe buffer, which Linux caps at 1 MiB — so a descendant that
/// keeps writing cannot keep a cancelled reader alive.
const FINAL_DRAIN_BYTES: usize = 1024 * 1024;

/// Spawn `cmd`, enforce a laptop-side wallclock kill at `secs` seconds, and
/// bound the in-memory capture of each stream at `max_capture_bytes`.
///
/// `cmd.output()` has no timeout and no byte bound: a child that hangs before
/// producing output waits forever, and one that streams at line rate grows the
/// capture until the machine notices. This helper is the single place both are
/// enforced, for ssh probes ([`run_with_wallclock_kill`], which wraps it) and
/// for the purely local `ssh -G` dump `remote_doctor` runs.
///
/// Behavior:
/// - Nulls stdin so the child can't read from the user's terminal — mirrors
///   `Command::output()`'s implicit stdin nulling. Important because probes are
///   meant to be non-interactive (`BatchMode=yes`); without this, a tampered
///   remote binary or wrapping shell could observe local input for up to the
///   deadline.
/// - Pipes stdout/stderr and drains them concurrently in two helper threads
///   while the main loop polls the child for exit. This is what
///   `Command::output()` does internally, and it's required for any child
///   producing more output than a single pipe buffer (~64 KiB on Linux):
///   without concurrent draining, the child blocks in `write(2)` before it
///   can exit, the poll keeps reporting it running, and the wallclock fires
///   even though the child wasn't actually stalled.
/// - Applies `max_capture_bytes` to *each* stream independently — stdout and
///   stderr are separate attack vectors, and a hostile peer that floods stderr
///   drives memory growth just as easily as one that floods stdout. A drainer
///   stops reading altogether at its cap and closes its pipe, so a child that
///   keeps writing dies of `SIGPIPE`/`EPIPE` or is reaped at the deadline.
/// - Polls the child every 50ms until the deadline — on Unix with
///   `waitid(…, WNOWAIT)`, which observes the exit without reaping, so the
///   child is reaped exactly once, after the last signal (see [`Leader`]).
///   Polling cadence
///   is a wallclock-vs-CPU tradeoff; 50ms keeps the worst-case overshoot
///   under a tick while costing ~20 syscalls/sec.
/// - **The deadline bounds the streams, not only the child** (PRD #1487
///   re-check R1). A descendant that inherited a stream keeps its write end
///   open after the child exits or is killed, so "wait for EOF" can outlive any
///   deadline. The call therefore never joins a reader unconditionally: once
///   the child has exited, a stream still open after
///   [`POST_EXIT_STREAM_GRACE`] is cancelled, and at the deadline both are.
///   A cancelled reader drains what is already buffered and closes its pipe —
///   on Unix it reads non-blocking under `poll(2)`, so it notices the cancel
///   within a tick; elsewhere a reader still blocked after
///   [`READER_STOP_GRACE`] is abandoned rather than joined, and exits when the
///   last writer closes.
/// - On deadline: SIGKILL the child, then reap it, and
///   return `timed_out: true`. **Here the kill reaches the child only.**
///   `ssh -G` evaluates `Match exec`, so a config with `Match exec "sleep 30"`
///   has already forked a descendant that this does not signal; such a
///   descendant is orphaned and reaped by init when it exits on its own. The
///   bound this helper offers is on *our* wait and *our* memory, not on what
///   the user's own configuration chose to spawn.
///   [`run_local_bounded_owning_group`] is the variant that also kills them.
/// - Computes the deadline with `Instant::checked_add` so an absurd `secs`
///   (e.g. `u64::MAX`) can never panic between `spawn` and the polling
///   loop and leak the child — probe callers already clamp to a sane upper
///   bound, this is belt-and-suspenders.
pub fn run_local_bounded(
    cmd: &mut Command,
    secs: u64,
    max_capture_bytes: usize,
) -> std::io::Result<LocalCapture> {
    run_local_bounded_in(cmd, secs, max_capture_bytes, false)
}

/// [`run_local_bounded`], with the child started in a process group of its own
/// on Unix, so that whatever it spawns into that group is killed with it.
///
/// The group is sent `SIGKILL` whenever the call ends with any of it possibly
/// still running: at the deadline, and when the child has exited but a stream
/// is still held open past [`POST_EXIT_STREAM_GRACE`] — the shape of a
/// `ProxyCommand` or jump-route helper outliving its `ssh`. The child itself is
/// reaped here, and only after that signal: until then it is kept an unreaped
/// zombie, so the group id it names cannot have been reused by the time the
/// signal is sent ([`Leader`]). A killed descendant is not our child and is
/// reaped by whoever inherited it. A descendant that left the group (`setsid`, as a
/// `ControlPersist` master does) is not signalled, and the call still returns
/// on time because its readers are cancelled rather than joined.
///
/// Not the default, because a child in its own group no longer receives the
/// terminal's `Ctrl+C`: this is for short plumbing commands whose lifetime the
/// caller owns outright (PRD #1487's remote upgrade), not for the
/// user-interruptible install pipeline. On non-Unix hosts it is
/// [`run_local_bounded`].
pub fn run_local_bounded_owning_group(
    cmd: &mut Command,
    secs: u64,
    max_capture_bytes: usize,
) -> std::io::Result<LocalCapture> {
    run_local_bounded_in(cmd, secs, max_capture_bytes, true)
}

fn run_local_bounded_in(
    cmd: &mut Command,
    secs: u64,
    max_capture_bytes: usize,
    own_group: bool,
) -> std::io::Result<LocalCapture> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    if own_group {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn()?;
    let mut leader = Leader::new(child, own_group);

    // `usize::MAX` is the wrapper's spelling of "no cap"; map it back to
    // `None` so the uncapped install path reads without a byte limit.
    let cap = (max_capture_bytes != usize::MAX).then_some(max_capture_bytes);
    let stdout = leader
        .child
        .stdout
        .take()
        .map(|s| PipeReader::spawn(s, cap));
    let stderr = leader
        .child
        .stderr
        .take()
        .map(|s| PipeReader::spawn(s, cap));
    let readers: Vec<&PipeReader> = stdout.iter().chain(stderr.iter()).collect();

    let deadline = Instant::now()
        .checked_add(Duration::from_secs(secs))
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
    let poll_interval = Duration::from_millis(50);

    // The child is observed here but never reaped: it stays a zombie, holding
    // its pid — and so its group id — until `leader.reap()` below, which comes
    // after the last signal this call sends (PRD #1487 final audit F1).
    let mut exited_at: Option<Instant> = None;
    let mut timed_out = false;
    loop {
        if exited_at.is_none() {
            match leader.has_exited() {
                Ok(true) => exited_at = Some(Instant::now()),
                Ok(false) => {}
                Err(source) => {
                    leader.abandon(&source);
                    PipeReader::stop_all(&readers);
                    return Err(source);
                }
            }
        }
        let streams_closed = readers.iter().all(|r| r.is_done());
        if exited_at.is_some() && streams_closed {
            break;
        }
        let now = Instant::now();
        let held_after_exit = exited_at.is_some_and(|at| {
            at.checked_add(POST_EXIT_STREAM_GRACE)
                .is_none_or(|limit| now >= limit)
        });
        if now >= deadline || held_after_exit {
            // Something this call started may still be running: the child
            // itself, or a descendant holding one of its streams open. Best
            // effort, secondary errors ignored — SIGKILL is unblockable, so
            // the reap below returns, and the readers are cancelled, never
            // joined.
            leader.kill();
            timed_out = exited_at.is_none();
            PipeReader::stop_all(&readers);
            break;
        }
        std::thread::sleep(poll_interval);
    }
    // The only reap, after every signal: from here on the pid and the group id
    // may belong to someone else, and `reap` consumes the leader so nothing
    // can signal them.
    let reaped = leader.reap();
    let status = if timed_out { None } else { Some(reaped?) };

    let stdout = stdout.map(|r| r.take()).unwrap_or_default();
    let stderr = stderr.map(|r| r.take()).unwrap_or_default();
    // A drainer stops exactly AT its cap, so a stream that reached it is a
    // prefix of what the child wanted to say. An output that happens to be
    // exactly `max_capture_bytes` long is reported truncated too; that errs
    // toward "I could not read all of it", which is the safe direction for
    // every caller here.
    let truncated = stdout.len() >= max_capture_bytes || stderr.len() >= max_capture_bytes;
    Ok(LocalCapture {
        status,
        stdout,
        stderr,
        truncated,
        timed_out,
    })
}

/// Spawn `cmd` and enforce a laptop-side wallclock kill at `secs` seconds,
/// mapping the outcome onto the ssh-flavoured error type.
///
/// A thin wrapper over [`run_local_bounded`] — see that function for the
/// mechanics and the threat model. The only decisions here are ssh-specific:
/// a deadline becomes [`SshError::Other`] so the connect-layer mapper folds it
/// into `HostUnreachable` (the right user-visible classification — the user's
/// recourse is the same as transport-unreachable), and `max_capture_bytes` of
/// `None` means "no cap", which the long-running install pipeline relies on.
///
/// Returns the capture's `truncated` verdict alongside the output. Converting
/// a [`LocalCapture`] into a plain [`std::process::Output`] silently discarded
/// it, which is how a cap-limited prefix reached `probe_remote_version` and
/// `probe_remote_protocol` looking like a complete answer (PRD #345 audit).
fn run_with_wallclock_kill(
    cmd: &mut Command,
    target: &SshTarget,
    secs: u64,
    max_capture_bytes: Option<usize>,
) -> Result<(std::process::Output, bool), SshError> {
    let capture = run_local_bounded(cmd, secs, max_capture_bytes.unwrap_or(usize::MAX)).map_err(
        |source| SshError::Io {
            target: target.user_host(),
            source,
        },
    )?;
    match capture.status {
        Some(status) => Ok((
            std::process::Output {
                status,
                stdout: capture.stdout,
                stderr: capture.stderr,
            },
            capture.truncated,
        )),
        None => Err(SshError::Other {
            target: target.user_host(),
            detail: format!(
                "probe exceeded {secs}s wallclock deadline (laptop-side kill after ssh did not return)"
            ),
        }),
    }
}

/// The child [`run_local_bounded_in`] spawned, and the process group it owns
/// when the call is [`run_local_bounded_owning_group`].
///
/// The group id is the child's pid, so it names this call's group only while
/// the kernel keeps that pid: while the child runs, and after it exits for as
/// long as it stays an unreaped zombie. A group signal sent after the reap
/// could reach a stranger's group once the number is reused, and a descendant
/// that left the group with `setsid` while holding a stream keeps the call
/// waiting through exactly that window, with no member left to hold the id
/// (PRD #1487 final audit F1). So the child's exit is observed without reaping
/// it (`waitid(…, WNOWAIT)`), every signal is sent before [`Leader::reap`], and
/// `reap` consumes the value, so no signal can follow it.
struct Leader {
    child: std::process::Child,
    #[cfg(unix)]
    pgid: Option<libc::pid_t>,
}

impl Leader {
    fn new(child: std::process::Child, own_group: bool) -> Self {
        #[cfg(unix)]
        {
            let pgid = own_group
                .then(|| libc::pid_t::try_from(child.id()).ok())
                .flatten()
                .filter(|&pgid| group_is_signalable(pgid));
            Self { child, pgid }
        }
        #[cfg(not(unix))]
        {
            let _ = own_group;
            Self { child }
        }
    }

    /// Whether the child has exited. On Unix it is left unreaped, so its pid
    /// and group id stay reserved until [`Leader::reap`].
    fn has_exited(&mut self) -> std::io::Result<bool> {
        #[cfg(unix)]
        {
            exited_unreaped(self.child.id())
        }
        // No process groups here; std keeps the status for `reap` to return.
        #[cfg(not(unix))]
        {
            self.child.try_wait().map(|status| status.is_some())
        }
    }

    /// SIGKILL the child and, when this call owns one, its whole group. Safe
    /// to aim at the group because the child has not been reaped: whether it
    /// is running or a zombie, its pid — the group id — is still its own.
    fn kill(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.pgid.filter(|&pgid| group_is_signalable(pgid)) {
            #[cfg(all(test, unix))]
            leader_seam::record(leader_seam::Event::GroupSignal {
                leader_unreaped: leader_seam::unreaped(self.child.id()),
            });
            // SAFETY: killpg takes plain integers and has no memory effects.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }

    /// Give up after the child's exit could not be observed. No group signal:
    /// the failure may mean something else reaped the child (a process that
    /// ignores `SIGCHLD` has its children reaped automatically), and then
    /// neither its pid nor its group id is ours any more. `ECHILD` says exactly
    /// that, so nothing is signalled; any other failure kills the child alone
    /// and reaps it, as before.
    fn abandon(mut self, err: &std::io::Error) {
        #[cfg(unix)]
        if err.raw_os_error() == Some(libc::ECHILD) {
            return;
        }
        let _ = err;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Reap the child — the last thing done with it. Consumes the leader, so
    /// no signal can be sent after the pid and group id are released.
    fn reap(mut self) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(all(test, unix))]
        leader_seam::record(leader_seam::Event::Reap);
        self.child.wait()
    }
}

/// Whether child `pid` has exited, observed without reaping it.
#[cfg(unix)]
fn exited_unreaped(pid: u32) -> std::io::Result<bool> {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid out-parameter, and
        // `waitid` writes no more than one. `WNOWAIT` leaves the child
        // waitable, so this neither reaps it nor changes what a later wait
        // sees; `WNOHANG` returns at once with `si_pid` still 0 when the child
        // has not exited.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                libc::id_t::from(pid),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `waitid` filled `info` (or left it zeroed).
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Defence in depth for a group signal: `killpg(0)` would signal the caller's
/// own group, `1` is init's, and a non-positive id is not a group at all.
/// Whatever produced `pgid`, these are never this call's to kill.
#[cfg(unix)]
fn group_is_signalable(pgid: libc::pid_t) -> bool {
    // SAFETY: getpgrp takes no arguments and cannot fail.
    pgid > 1 && pgid != unsafe { libc::getpgrp() }
}

/// What [`Leader`] did, in order, so a test can assert that no group signal
/// follows the reap, and that the kernel still held the child at the moment
/// of each signal. Recording is per thread and off until a test turns it on.
#[cfg(all(test, unix))]
mod leader_seam {
    use std::cell::RefCell;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Event {
        /// The group was signalled; `leader_unreaped` is the kernel's answer,
        /// at that moment, to whether the child was still waitable.
        GroupSignal {
            leader_unreaped: bool,
        },
        Reap,
    }

    thread_local! {
        static EVENTS: RefCell<Option<Vec<Event>>> = const { RefCell::new(None) };
    }

    pub(super) fn record(event: Event) {
        EVENTS.with(|events| {
            if let Some(events) = events.borrow_mut().as_mut() {
                events.push(event);
            }
        });
    }

    /// Run `f` with recording on, and return what it recorded.
    pub(super) fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<Event>) {
        EVENTS.with(|events| *events.borrow_mut() = Some(Vec::new()));
        let out = f();
        let recorded = EVENTS.with(|events| events.borrow_mut().take().unwrap_or_default());
        (out, recorded)
    }

    /// Whether child `pid` is still waitable — running, or an unreaped zombie.
    pub(super) fn unreaped(pid: u32) -> bool {
        super::exited_unreaped(pid).is_ok()
    }
}

/// One stream's drainer: a helper thread that reads the pipe into a shared
/// buffer, so the caller can stop waiting for it — and still keep what it
/// read — without joining the thread.
struct PipeReader {
    shared: std::sync::Arc<PipeShared>,
}

#[derive(Default)]
struct PipeShared {
    buf: std::sync::Mutex<Vec<u8>>,
    /// Set by the reader once it has stopped and closed its pipe.
    done: std::sync::atomic::AtomicBool,
    /// Set by the caller to ask the reader to drain what is buffered and stop.
    cancel: std::sync::atomic::AtomicBool,
}

impl PipeReader {
    fn spawn<R>(pipe: R, cap: Option<usize>) -> Self
    where
        R: std::io::Read + PipeFd + Send + 'static,
    {
        let shared = std::sync::Arc::new(PipeShared::default());
        let thread_shared = std::sync::Arc::clone(&shared);
        std::thread::spawn(move || {
            drain_pipe(pipe, cap, &thread_shared);
            thread_shared
                .done
                .store(true, std::sync::atomic::Ordering::Release);
        });
        Self { shared }
    }

    fn is_done(&self) -> bool {
        self.shared.done.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Cancel every reader, then give them [`READER_STOP_GRACE`] together to
    /// drain and close. A reader still running after that is abandoned: it
    /// owns nothing but its pipe and its share of the buffer.
    fn stop_all(readers: &[&PipeReader]) {
        for reader in readers {
            reader
                .shared
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let until = std::time::Instant::now() + READER_STOP_GRACE;
        while !readers.iter().all(|r| r.is_done()) && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// What this stream delivered so far.
    fn take(self) -> Vec<u8> {
        let mut buf = self
            .shared
            .buf
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *buf)
    }
}

/// Drain a child-process pipe into `shared.buf`, optionally capping how much
/// is retained. When `cap` is `Some(n)`, at most `n` bytes are buffered and
/// the helper stops — closing the pipe — once that bound is hit. Errors end
/// the drain: a half-read pipe still keeps the bytes that did land, matching
/// the behavior `Command::output()` exhibits when the kernel closes the writer.
///
/// On Unix the pipe is read non-blocking under `poll(2)`, so a cancel is
/// noticed within a tick even while a quiet descendant holds the write end;
/// after a cancel the reader takes what the kernel already holds (at most
/// [`FINAL_DRAIN_BYTES`]) and stops. Elsewhere the read blocks, and a cancel
/// is only noticed between reads.
fn drain_pipe<R: std::io::Read + PipeFd>(mut pipe: R, cap: Option<usize>, shared: &PipeShared) {
    use std::sync::atomic::Ordering;

    let nonblocking = pipe.set_nonblocking();
    let mut chunk = [0u8; 8192];
    let mut after_cancel = 0usize;
    let mut len = 0usize;
    loop {
        let cancelled = shared.cancel.load(Ordering::Acquire);
        if cancelled && (!nonblocking || after_cancel >= FINAL_DRAIN_BYTES) {
            break;
        }
        let room = cap.map_or(chunk.len(), |cap| cap.saturating_sub(len));
        if room == 0 {
            break;
        }
        let take = chunk.len().min(room);
        match pipe.read(&mut chunk[..take]) {
            Ok(0) => break,
            Ok(n) => {
                shared
                    .buf
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend_from_slice(&chunk[..n]);
                len += n;
                if cancelled {
                    after_cancel += n;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if cancelled {
                    // Everything the kernel held has been read.
                    break;
                }
                pipe.wait_readable(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    drop(pipe);
}

/// The two things [`drain_pipe`] needs from a pipe beyond `Read`, which only
/// Unix can provide; elsewhere both are no-ops and the read blocks.
trait PipeFd {
    /// Switch the read end to non-blocking. `false` when that failed or is
    /// not supported, in which case reads block.
    fn set_nonblocking(&self) -> bool;
    /// Wait up to `timeout` for the pipe to become readable (or closed).
    fn wait_readable(&self, timeout: std::time::Duration);
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd> PipeFd for T {
    fn set_nonblocking(&self) -> bool {
        let fd = self.as_raw_fd();
        // SAFETY: fcntl on a descriptor this value owns; F_GETFL/F_SETFL have
        // no memory effects. O_NONBLOCK is per open file description, and the
        // child's write end is a different one, so the child is unaffected.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
        }
    }

    fn wait_readable(&self, timeout: std::time::Duration) {
        let mut pfd = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one valid pollfd, count 1; an EINTR return is just an early
        // wake-up, and the caller reads again either way.
        unsafe {
            libc::poll(&mut pfd, 1, millis);
        }
    }
}

#[cfg(not(unix))]
impl<T> PipeFd for T {
    fn set_nonblocking(&self) -> bool {
        false
    }

    fn wait_readable(&self, _timeout: std::time::Duration) {}
}

// ---------------------------------------------------------------------------
// Registry: ~/.config/dot-agent-deck/remotes.toml
// ---------------------------------------------------------------------------

/// One row in `remotes.toml`. `host` carries the full `[user@]host` string
/// the user passed on the CLI — we keep it intact so `connect` (M2.4) can
/// re-parse it the same way the user typed it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub version: String,
    pub added_at: String,
    /// Timestamp of the most recent `remote upgrade`. `None` until the entry
    /// has been upgraded at least once. `added_at` stays at the original
    /// registration timestamp so users can see both moments separately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgraded_at: Option<String>,
    /// Timestamp of the most recent successful `connect`. `None` until the
    /// entry has been connected to at least once. Updated by `connect`'s
    /// post-spawn bookkeeping when the remote ssh session exits cleanly
    /// (status 0); a failed spawn or a non-zero remote exit leaves the field
    /// untouched so the registry only records sessions that actually ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<String>,
    /// How the deck is installed on the remote, as `remote add` / `remote
    /// upgrade` last detected it (issue #1372): [`INSTALL_LOCAL_BIN`] or
    /// [`INSTALL_HOMEBREW`]. `None` on entries written before the field
    /// existed, which try [`REMOTE_INSTALL_PATH`] first; `connect` can discover
    /// and record a Homebrew install if that path is gone. Kept as a string
    /// rather than an enum so a value written by a newer build does not make
    /// this build refuse the whole registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
    /// Absolute path of the deck binary on the remote, recorded when it is not
    /// [`REMOTE_INSTALL_PATH`] — today, a Homebrew install's
    /// `<prefix>/bin/dot-agent-deck`. `connect`, `remote doctor` and the hook
    /// install run this path; `None` means [`REMOTE_INSTALL_PATH`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<RemoteBinaryPath>,
    /// Issue #1350: the fields the desktop app's deck rows carry, optional so
    /// every row `remote add` wrote before them still loads. See
    /// [`crate::deck_list`], which is how both clients edit this file.
    ///
    /// The desktop's stable id for the row. A row without one has an id
    /// derived from its `name` ([`crate::deck_list::deck_id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The login name, when it cannot be folded into `host` as `user@host`
    /// (a login containing `@` itself). Wins over a user in `host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// A `Host` name from `~/.ssh/config` to reach the deck through (`ssh -J`).
    /// Used by the desktop; `connect` does not route through it yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump_host: Option<String>,
    /// The daemon's attach socket path on the remote, as the desktop's `Test
    /// connection` discovered it. `connect` does not need it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
}

impl RemoteEntry {
    /// Reconstruct an [`SshTarget`] from this entry's stored host/port/key. The
    /// `host` field carries the original `[user@]host` string the user typed
    /// at `remote add` time; we re-parse it the same way `add` does. An
    /// explicit `user` field (issue #1350) wins over the one in `host`.
    pub fn ssh_target(&self) -> SshTarget {
        let mut target =
            SshTarget::parse(&self.host, self.port, self.key.as_ref().map(PathBuf::from));
        if let Some(user) = &self.user {
            target.user = Some(user.clone());
        }
        target.jump = self.jump_host.clone();
        target
    }

    /// Whether `other` reaches the same daemon this row does: the same ssh
    /// route (host, login, port, key and jump host) and the same daemon
    /// socket. What an upgrade captured at its start is compared with the row
    /// it is about to record into, so a row moved to another machine in the
    /// meantime is refused rather than written over (PRD #1487, Greptile
    /// 4208066970).
    pub fn same_route(&self, other: &RemoteEntry) -> bool {
        self.ssh_target() == other.ssh_target() && self.socket == other.socket
    }

    /// The deck binary to invoke on the remote, spelled for the remote shell:
    /// the recorded [`binary`](Self::binary) when there is one, otherwise
    /// [`REMOTE_INSTALL_PATH`]. Safe to interpolate into a remote command
    /// unquoted, because [`RemoteBinaryPath`] admits no shell metacharacter
    /// and [`REMOTE_INSTALL_PATH`] is a constant.
    pub fn remote_binary(&self) -> &str {
        self.binary
            .as_ref()
            .map_or(REMOTE_INSTALL_PATH, RemoteBinaryPath::as_str)
    }

    /// [`Self::remote_binary`] as the validated type a remote command takes.
    pub fn deck_binary(&self) -> RemoteDeckBinary {
        RemoteDeckBinary::recorded_or_default(self.binary.as_ref())
    }
}

/// Where `remote add` installs the deck on a remote that has no Homebrew
/// install of it, and the binary every command runs for an entry that records
/// no other. Invoked by absolute path because a non-interactive ssh shell
/// typically does not have `~/.local/bin` on `PATH`.
pub use crate::connect::REMOTE_INSTALL_PATH;

/// [`RemoteEntry::install`] for a deck downloaded to [`REMOTE_INSTALL_PATH`].
pub const INSTALL_LOCAL_BIN: &str = "local-bin";
/// [`RemoteEntry::install`] for a deck installed by a Homebrew formula.
pub const INSTALL_HOMEBREW: &str = "homebrew";

/// Homebrew's default prefixes — Apple silicon, Intel macOS, Linux — probed
/// by absolute path because a non-interactive ssh shell usually does not have
/// `brew` on `PATH` (Homebrew adds itself in a login profile). A custom prefix
/// is found only through `command -v brew`.
pub const HOMEBREW_PREFIXES: &[&str] =
    &["/opt/homebrew", "/usr/local", "/home/linuxbrew/.linuxbrew"];

/// An absolute path on the remote, restricted to characters that need no
/// quoting in a POSIX shell, so it can be interpolated into a remote command
/// as-is. The restriction is enforced on both ways in — parsing the remote's
/// own answer, and deserializing `remotes.toml` — so a hand-edited or hostile
/// value is refused rather than handed to the remote shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RemoteBinaryPath(String);

impl RemoteBinaryPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for RemoteBinaryPath {
    type Error = String;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        let safe = path.len() > 1
            && path.starts_with('/')
            && path
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'));
        if safe {
            Ok(Self(path))
        } else {
            Err(format!(
                "`{}` is not an absolute path made only of letters, digits and `/._-+`",
                scrub_remote_text(&path)
            ))
        }
    }
}

impl From<RemoteBinaryPath> for String {
    fn from(path: RemoteBinaryPath) -> Self {
        path.0
    }
}

/// The deck binary a remote command runs, as a type that can hold only a value
/// safe to put **unquoted** at the start of a remote shell command (issue
/// #1490): the default install or a validated [`RemoteBinaryPath`]. A raw
/// string enters only through [`TryFrom<&str>`], which refuses anything else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDeckBinary {
    /// [`REMOTE_INSTALL_PATH`], the `~/.local/bin` install. Its leading `~` is
    /// deliberately left unquoted so the remote shell expands it to the remote
    /// user's home; quoting it would name a directory literally called `~`.
    DefaultInstall,
    /// A recorded absolute path, such as a Homebrew install.
    Path(RemoteBinaryPath),
}

impl RemoteDeckBinary {
    /// The deck-list row's recorded binary, or the default install when it
    /// records none.
    pub fn recorded_or_default(recorded: Option<&RemoteBinaryPath>) -> Self {
        recorded.map_or(Self::DefaultInstall, |path| Self::Path(path.clone()))
    }

    /// The binary as spelled for the remote shell, ready to interpolate
    /// unquoted.
    pub fn as_shell_word(&self) -> &str {
        match self {
            Self::DefaultInstall => REMOTE_INSTALL_PATH,
            Self::Path(path) => path.as_str(),
        }
    }
}

impl From<RemoteBinaryPath> for RemoteDeckBinary {
    fn from(path: RemoteBinaryPath) -> Self {
        Self::Path(path)
    }
}

impl TryFrom<&str> for RemoteDeckBinary {
    type Error = String;

    /// [`REMOTE_INSTALL_PATH`] exactly, or an absolute path
    /// [`RemoteBinaryPath`] accepts. Anything else — whitespace, a shell
    /// metacharacter, a relative path, any other `~` spelling — is refused.
    fn try_from(binary: &str) -> Result<Self, Self::Error> {
        if binary == REMOTE_INSTALL_PATH {
            Ok(Self::DefaultInstall)
        } else {
            RemoteBinaryPath::try_from(binary.to_string()).map(Self::Path)
        }
    }
}

impl std::fmt::Display for RemoteDeckBinary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_shell_word())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemotesFile {
    #[serde(default)]
    pub remotes: Vec<RemoteEntry>,
}

#[derive(Debug, Error)]
pub enum RemoteConfigError {
    #[error("Failed to read remotes file at {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to parse remotes file at {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("Failed to serialize remotes file: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("Cannot edit remotes file at {path}: {reason}")]
    Unwritable { path: String, reason: String },
    /// Issue #1350: an edit would have written an ssh address field that
    /// [`crate::deck_list::validate_deck_address`] refuses, so nothing was
    /// written.
    #[error("Refusing to write remotes file at {path}: {source}")]
    InvalidAddress {
        path: String,
        #[source]
        source: crate::remote_tunnel::SshArgumentError,
    },
    /// Issue #1350: the cross-process edit lock
    /// ([`crate::deck_list::edit`]) could not be taken, so nothing was read or
    /// written. `reason` names no path.
    #[error("Cannot take the edit lock for remotes file at {path}: {reason}")]
    Locked { path: String, reason: String },
}

impl RemotesFile {
    /// Load the registry from `path`. Missing file → empty registry. Bounded:
    /// a path that is not a regular file, or is over
    /// [`crate::deck_list::MAX_REGISTRY_BYTES`], is a read error (issue #1350).
    pub fn load(path: &Path) -> Result<Self, RemoteConfigError> {
        match crate::deck_list::read_registry(path)? {
            Some(contents) => {
                toml::from_str(&contents).map_err(|source| RemoteConfigError::Parse {
                    path: path.display().to_string(),
                    source,
                })
            }
            None => Ok(Self::default()),
        }
    }

    /// Atomically replace the file at `path` with the serialized form of
    /// `self` — see [`crate::deck_list::write_atomic`].
    ///
    /// Rewrites the **whole** file, so it drops any key [`RemoteEntry`] does
    /// not know. Every production writer edits one row through
    /// [`crate::deck_list`] instead (issue #1350); this remains for callers
    /// that build a registry from scratch, such as test fixtures.
    pub fn save(&self, path: &Path) -> Result<(), RemoteConfigError> {
        let contents = toml::to_string_pretty(self)?;
        crate::deck_list::write_atomic(path, &contents)
    }
}

/// Default location for the registry: `$DOT_AGENT_DECK_REMOTES` if set,
/// else `~/.config/dot-agent-deck/remotes.toml`. The env var override is
/// new; tests use it (or pass an explicit path to `add`).
///
/// The one resolver for the deck list: the desktop app calls this too
/// (issue #1350), so the two clients cannot disagree about which file it is.
pub fn default_remotes_path() -> PathBuf {
    if let Ok(p) = std::env::var("DOT_AGENT_DECK_REMOTES") {
        return PathBuf::from(p);
    }
    crate::config::config_dir().join("remotes.toml")
}

// ---------------------------------------------------------------------------
// `remote add` flow
// ---------------------------------------------------------------------------

/// CLI options accepted by `remote add`. `release_base` is overridable so the
/// happy-path test can inject a stub URL without changing global state.
#[derive(Debug, Clone)]
pub struct AddOptions {
    pub name: String,
    pub remote_type: String,
    pub target: String,
    pub port: u16,
    pub key: Option<PathBuf>,
    pub version: String,
    pub no_install: bool,
    pub release_base: String,
}

/// Failure modes of `remote add` (and, via `Inner`, of `remote upgrade`).
///
/// Several variants quote bytes the *remote* wrote — the arch probe's stderr
/// and its stdout, the version the remote binary reports, the download step's
/// stderr, the hook install's stderr. Every one of them is passed through
/// [`scrub_remote_text`] at construction, because this type's `Display` is
/// printed straight to the operator's terminal by `main.rs`. See that function
/// for why the seam is there rather than at the `eprintln!`.
#[derive(Debug, Error)]
pub enum RemoteAddError {
    #[error(
        "A remote named '{name}' already exists. Use `dot-agent-deck remote remove {name}` first or pick a different name."
    )]
    DuplicateName { name: String },
    #[error("Invalid remote name: {0}.")]
    InvalidName(crate::deck_list::DeckNameError),
    #[error("Invalid remote address: {0}.")]
    InvalidAddress(crate::remote_tunnel::SshArgumentError),
    #[error("Remote type 'kubernetes' is not yet implemented; planned in PRD #81.")]
    KubernetesNotYetImplemented,
    #[error("Unsupported remote type '{kind}'. Supported: ssh.")]
    UnsupportedType { kind: String },
    #[error("Invalid --version '{input}': must look like '0.24.5' or 'v0.24.5'.")]
    InvalidVersion { input: String },
    #[error(transparent)]
    Ssh(#[from] SshError),
    #[error("Remote arch is `{arch}`; supported: linux-{{amd64,arm64}}, darwin-{{amd64,arm64}}.")]
    UnsupportedArch { arch: String },
    #[error("Failed to detect remote arch (`uname -s -m` exited {status}): {stderr}")]
    UnameFailed { status: i32, stderr: String },
    #[error(
        "Failed to download dot-agent-deck v{version} for {platform} from {url}.\nCheck the remote has internet egress and the version exists.\nDetails: {detail}"
    )]
    DownloadFailed {
        version: String,
        platform: String,
        url: String,
        detail: String,
    },
    #[error("Installed binary reports `{actual}` but expected `{expected}`.")]
    VersionMismatch { actual: String, expected: String },
    /// The install had already put a build at `binary` — the download moved
    /// into place, or `brew upgrade` succeeded — when the version check that
    /// follows failed (`source`). Nothing is rolled back, so the binary is no
    /// longer the one that was there before (PRD #1487 review). `on_disk` is
    /// the version the check read, when it read one, and otherwise
    /// [`UNVERIFIED_BUILD`].
    #[error(
        "{binary} on the remote was replaced, but the new binary did not pass its version check: {source} What is installed there now is {on_disk}. Run the install again; if the check keeps failing, run `{binary} --version` on the remote to see what it reports."
    )]
    ReplacedButUnverified {
        binary: String,
        on_disk: String,
        #[source]
        source: Box<RemoteAddError>,
    },
    #[error("`dot-agent-deck hooks install` on remote failed (exit {status}): {stderr}")]
    HooksInstallFailed { status: i32, stderr: String },
    #[error(
        "Could not tell how dot-agent-deck is installed on the remote (exit {status}): {detail}"
    )]
    InstallProbeFailed { status: i32, detail: String },
    #[error("The remote's Homebrew prefix cannot be used: {detail}")]
    UnsafeHomebrewPrefix { detail: String },
    #[error(
        "`brew upgrade dot-agent-deck` on remote failed (exit {status}): {stderr}\nThe remote's deck is installed by Homebrew, so it is upgraded through Homebrew and never by downloading a second copy."
    )]
    BrewUpgradeFailed { status: i32, stderr: String },
    #[error(transparent)]
    Registry(#[from] RemoteConfigError),
}

/// SemVer-ish pattern accepted by `--version`: optional `v` prefix, three
/// numeric components, optional pre-release suffix. Rejects anything that
/// could carry shell metacharacters into the remote install command.
const VERSION_PATTERN: &str = r"^v?\d+(\.\d+){2}(-[A-Za-z0-9.\-]+)?$";

fn version_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(VERSION_PATTERN).expect("static version regex compiles"))
}

/// Validate a user-supplied version string and return its canonical
/// unprefixed form. Anything that doesn't look like a SemVer version is
/// rejected — this is the primary defense against shell injection via
/// `--version` (e.g. `0.24.5; rm -rf ~`).
///
/// Convention: internally we always carry the *unprefixed* form (`0.24.5`).
/// The URL builder and any other consumer that needs the `v`-prefixed form
/// (GitHub release tag) prepends `v` themselves. The user may type either
/// `0.24.5` or `v0.24.5`; both normalize to `0.24.5`.
pub fn validate_version_string(version: &str) -> Result<String, RemoteAddError> {
    if version_regex().is_match(version) {
        Ok(version.strip_prefix('v').unwrap_or(version).to_string())
    } else {
        Err(RemoteAddError::InvalidVersion {
            input: version.to_string(),
        })
    }
}

/// Build the install URL and the remote shell command. Validates `version`
/// internally as a defense-in-depth so that even an internal misuse (a code
/// path that bypassed the entry-point check in `add()`) cannot slip a shell
/// metacharacter into the remote command. Accepts both `0.24.5` and `v0.24.5`
/// — see `validate_version_string` for the normalization convention.
fn build_install_command(
    release_base: &str,
    version: &str,
    platform: &str,
) -> Result<(String, String), RemoteAddError> {
    let version = validate_version_string(version)?;
    let url = format!("{release_base}/v{version}/dot-agent-deck-{platform}");
    // Atomic install: download to a PID-suffixed sibling temp file, chmod the
    // temp, then `mv` it into place. Same-filesystem `mv` is POSIX-atomic, so
    // an interrupted curl leaves the existing binary untouched instead of
    // overwriting it with a truncated download. A per-process temp leak on a
    // failed install is acceptable — a wildcard sweep would race with a
    // concurrent upgrade's in-flight download.
    let install_cmd = format!(
        "mkdir -p ~/.local/bin \
         && tmp=~/.local/bin/.dot-agent-deck.$$ \
         && curl -fsSL {url} -o \"$tmp\" \
         && chmod 0755 \"$tmp\" \
         && mv \"$tmp\" ~/.local/bin/dot-agent-deck"
    );
    Ok((url, install_cmd))
}

/// Convert a `uname -s -m` line to one of our four supported platform tags.
fn detect_platform(uname_stdout: &str) -> Option<&'static str> {
    let trimmed = uname_stdout.trim();
    let mut parts = trimmed.split_whitespace();
    let os = parts.next()?;
    let arch = parts.next()?;
    match (os, arch) {
        ("Linux", "x86_64") => Some("linux-amd64"),
        ("Linux", "aarch64") | ("Linux", "arm64") => Some("linux-arm64"),
        ("Darwin", "x86_64") => Some("darwin-amd64"),
        ("Darwin", "arm64") => Some("darwin-arm64"),
        _ => None,
    }
}

/// Install (or version-check, with `no_install`) the remote binary and assert
/// it reports the expected version. Shared between `add` and `upgrade` — the
/// only piece that's identical between the two flows. Caller is responsible
/// for arch detect (passes `platform`), pre-validating `version` (caller has
/// already run `validate_version_string`), and any post-install hooks work.
fn install_and_verify(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    platform: &str,
    version: &str,
    release_base: &str,
    no_install: bool,
) -> Result<(), RemoteAddError> {
    if no_install {
        // Use the absolute path the install flow targets (line 548 below).
        // A non-interactive ssh shell typically doesn't have `~/.local/bin`
        // on PATH, so a bare `dot-agent-deck` lookup fails for binaries
        // placed at the standard install location.
        let v = executor.run(target, "~/.local/bin/dot-agent-deck --version")?;
        if v.status != 0 {
            return Err(RemoteAddError::VersionMismatch {
                actual: format!("(exit {}) {}", v.status, scrub_remote_text(&v.stderr)),
                expected: version.to_string(),
            });
        }
        let actual = parse_version_output(&v.stdout).unwrap_or_else(|| v.stdout.trim().to_string());
        if actual != version {
            return Err(RemoteAddError::VersionMismatch {
                actual: scrub_remote_text(&actual),
                expected: version.to_string(),
            });
        }
        return Ok(());
    }

    // Single shell command: any step's failure aborts the rest, and we get
    // one stderr+exit pair instead of three round-trips' worth to
    // disentangle. `build_install_command` re-validates the version (defense
    // in depth) so even an internal misuse can't shell-inject.
    let (url, install_cmd) = build_install_command(release_base, version, platform)?;
    let install = executor.run(target, &install_cmd)?;
    if install.status != 0 {
        return Err(RemoteAddError::DownloadFailed {
            version: version.to_string(),
            platform: platform.to_string(),
            url,
            detail: format!(
                "exit {}: {}",
                install.status,
                scrub_remote_text(&install.stderr)
            ),
        });
    }
    // The download is in place from here on: a failed check no longer means
    // "nothing changed", so it says what replaced the binary.
    remote_binary_version(executor, target, REMOTE_INSTALL_PATH, version)
        .and_then(|actual| {
            if actual == version {
                Ok(())
            } else {
                Err(RemoteAddError::VersionMismatch {
                    actual,
                    expected: version.to_string(),
                })
            }
        })
        .map_err(|e| replaced_but_unverified(REMOTE_INSTALL_PATH, e))
}

/// What a remote's binary is called once a build replaced it and its version
/// check failed without reading a version.
pub const UNVERIFIED_BUILD: &str = "an unverified build";

/// Wrap a version-check failure that came after the install put a new binary at
/// `binary`, keeping the version the check read when it read a valid one.
fn replaced_but_unverified(binary: &str, check: RemoteAddError) -> RemoteAddError {
    let on_disk = match &check {
        RemoteAddError::VersionMismatch { actual, .. } => validate_version_string(actual).ok(),
        _ => None,
    };
    RemoteAddError::ReplacedButUnverified {
        binary: binary.to_string(),
        on_disk: on_disk.unwrap_or_else(|| UNVERIFIED_BUILD.to_string()),
        source: Box::new(check),
    }
}

/// How the deck is installed on a remote, as [`detect_install`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteInstall {
    /// An executable exists at [`REMOTE_INSTALL_PATH`].
    local_bin: bool,
    /// The prefix of a Homebrew that has the `dot-agent-deck` formula
    /// installed and linked, when there is one.
    homebrew_prefix: Option<String>,
    /// Formula that owns the linked binary, validated against the two names
    /// the probe can emit before it is interpolated into an upgrade command.
    homebrew_formula: Option<&'static str>,
}

impl RemoteInstall {
    /// The Homebrew install's `<prefix>/bin/dot-agent-deck` — the symlink
    /// Homebrew keeps pointing at the current keg, so it survives upgrades
    /// where a `Cellar/<version>` path would not.
    fn homebrew_binary(&self) -> Option<RemoteBinaryPath> {
        let prefix = self.homebrew_prefix.as_deref()?;
        RemoteBinaryPath::try_from(format!("{prefix}/bin/dot-agent-deck")).ok()
    }
}

/// The remote command behind [`detect_install`]. It prints `local-bin=`,
/// `homebrew=` and `formula=` lines, and exits 0.
///
/// A Homebrew install is recognised by asking Homebrew, not by the shape of a
/// path: `brew list --formula` must succeed for stable or beta, and the
/// linked executable must be the same file as the formula's own binary.
/// A formula can remain installed after another one takes over the link, so
/// checking only that both exist can upgrade the wrong channel.
/// `brew` is looked for on `PATH` first and then
/// at each of [`HOMEBREW_PREFIXES`], because the `PATH` of a non-interactive
/// ssh command usually lacks it.
fn install_probe_command() -> String {
    let brews: Vec<String> = HOMEBREW_PREFIXES
        .iter()
        .map(|prefix| format!("{prefix}/bin/brew"))
        .collect();
    format!(
        concat!(
            "if [ -x {local_bin} ]; then echo local-bin=present; else echo local-bin=; fi; ",
            "for dad_brew in \"$(command -v brew 2>/dev/null)\" {brews}; do ",
            "[ -n \"$dad_brew\" ] && [ -x \"$dad_brew\" ] || continue; ",
            "dad_prefix=$(\"$dad_brew\" --prefix 2>/dev/null) || continue; ",
            "[ -x \"$dad_prefix/bin/dot-agent-deck\" ] || continue; ",
            "for dad_formula in dot-agent-deck dot-agent-deck-beta; do ",
            "\"$dad_brew\" list --formula \"$dad_formula\" >/dev/null 2>&1 || continue; ",
            "dad_keg=$(\"$dad_brew\" --prefix \"$dad_formula\" 2>/dev/null) || continue; ",
            "[ \"$dad_prefix/bin/dot-agent-deck\" -ef \"$dad_keg/bin/dot-agent-deck\" ] || continue; ",
            "echo \"homebrew=$dad_prefix\"; echo \"formula=$dad_formula\"; exit 0; ",
            "done; ",
            "done; ",
            "echo homebrew=; echo formula="
        ),
        local_bin = REMOTE_INSTALL_PATH,
        brews = brews.join(" "),
    )
}

/// Discover a Homebrew binary without changing the remote install. Used when
/// `connect` finds that a legacy entry's default ~/.local/bin path is gone.
/// The connect executor supplies its short SSH deadline; cap the answer as
/// well, since this is a probe rather than an installation operation.
pub(crate) fn discover_homebrew_binary(
    executor: &dyn SshExecutor,
    target: &SshTarget,
) -> Result<Option<RemoteBinaryPath>, SshError> {
    let probe = executor.run_capped(target, &install_probe_command(), 8 * 1024)?;
    if probe.truncated || probe.output.status != 0 {
        return Ok(None);
    }
    let prefix = probe.output.stdout.lines().find_map(|line| {
        line.strip_prefix("homebrew=")
            .filter(|prefix| !prefix.is_empty())
    });
    Ok(prefix
        .and_then(|prefix| RemoteBinaryPath::try_from(format!("{prefix}/bin/dot-agent-deck")).ok()))
}

/// Find out how the deck is installed on the remote (issue #1372).
fn detect_install(
    executor: &dyn SshExecutor,
    target: &SshTarget,
) -> Result<RemoteInstall, RemoteAddError> {
    let probe = executor.run(target, &install_probe_command())?;
    let probe_failed = |detail: &str| RemoteAddError::InstallProbeFailed {
        status: probe.status,
        detail: scrub_remote_text(detail),
    };
    if probe.status != 0 {
        return Err(probe_failed(&probe.stderr));
    }
    let mut local_bin = None;
    let mut homebrew = None;
    let mut formula = None;
    for line in probe.stdout.lines() {
        if let Some(value) = line.strip_prefix("local-bin=") {
            local_bin = Some(!value.is_empty());
        } else if let Some(value) = line.strip_prefix("homebrew=") {
            homebrew = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("formula=") {
            formula = Some(value);
        }
    }
    let (Some(local_bin), Some(homebrew), Some(formula)) = (local_bin, homebrew, formula) else {
        return Err(probe_failed(&probe.stdout));
    };
    let homebrew_formula = match (homebrew.is_empty(), formula) {
        (true, "") => None,
        (false, "dot-agent-deck") => Some("dot-agent-deck"),
        (false, "dot-agent-deck-beta") => Some("dot-agent-deck-beta"),
        _ => return Err(probe_failed(&probe.stdout)),
    };
    let install = RemoteInstall {
        local_bin,
        homebrew_prefix: (!homebrew.is_empty()).then_some(homebrew),
        homebrew_formula,
    };
    if install.homebrew_prefix.is_some() && install.homebrew_binary().is_none() {
        let prefix = install.homebrew_prefix.unwrap_or_default();
        return Err(RemoteAddError::UnsafeHomebrewPrefix {
            detail: RemoteBinaryPath::try_from(prefix).err().unwrap_or_default(),
        });
    }
    Ok(install)
}

/// Run `<binary> --version` on the remote and return the version it reports.
fn remote_binary_version(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    binary: &str,
    expected: &str,
) -> Result<String, RemoteAddError> {
    let v = executor.run(target, &format!("{binary} --version"))?;
    if v.status != 0 {
        return Err(RemoteAddError::VersionMismatch {
            actual: format!("(exit {}) {}", v.status, scrub_remote_text(&v.stderr)),
            expected: expected.to_string(),
        });
    }
    let actual = parse_version_output(&v.stdout).unwrap_or_else(|| v.stdout.trim().to_string());
    validate_version_string(&actual).map_err(|_| RemoteAddError::VersionMismatch {
        actual: scrub_remote_text(&actual),
        expected: expected.to_string(),
    })
}

/// A command that runs `program` with `<prefix>/bin` first on `PATH`.
///
/// For `brew`, which expects its own `bin` on `PATH`. For `hooks install`,
/// because the hook installer pins an *installed* deck, and it recognises the
/// binary it is running as one only when that binary's directory is on `PATH`
/// (`platform::paths::durable_binary_path`). Without this, a non-interactive
/// ssh `PATH` that lacks Homebrew's `bin` sends it looking elsewhere, which is
/// how hooks ended up pinned to a `~/.local/bin` copy.
fn with_homebrew_path(prefix: &str, program: &str) -> String {
    format!("PATH={prefix}/bin:\"$PATH\" {program}")
}

/// What [`install_or_upgrade`] left on the remote.
struct Installed {
    method: &'static str,
    /// `None` for [`REMOTE_INSTALL_PATH`].
    binary: Option<RemoteBinaryPath>,
    /// The Homebrew prefix, for a Homebrew install.
    homebrew_prefix: Option<String>,
    /// The version the installed binary reports.
    version: String,
}

impl Installed {
    fn remote_binary(&self) -> &str {
        self.binary
            .as_ref()
            .map_or(REMOTE_INSTALL_PATH, RemoteBinaryPath::as_str)
    }
}

/// Put the requested deck on the remote through whatever owns it there, and
/// report what landed. Shared by `add` and `upgrade`.
///
/// - **Homebrew** owns the install when [`detect_install`] finds the formula.
///   Nothing is downloaded and nothing is written to [`REMOTE_INSTALL_PATH`].
///   `brew upgrade dot-agent-deck` runs only when `run_brew_upgrade` is set
///   (`remote upgrade` without `--no-install`); `remote add` registers the
///   install as it is. Homebrew cannot install a chosen version, so the
///   version that landed is what is recorded, and a difference from
///   `version` is reported rather than refused — except under `no_install`,
///   which requires the match exactly as it does for `~/.local/bin`.
/// - Otherwise the release binary goes to [`REMOTE_INSTALL_PATH`], exactly as
///   before issue #1372.
///
/// When both exist — the state issue #1372's bug leaves behind — both are
/// reported, along with which one the deck uses.
#[allow(clippy::too_many_arguments)]
fn install_or_upgrade(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    name: &str,
    platform: &str,
    version: &str,
    release_base: &str,
    no_install: bool,
    run_brew_upgrade: bool,
    out: &mut dyn std::io::Write,
) -> Result<Installed, RemoteAddError> {
    let found = detect_install(executor, target)?;
    let Some(binary) = found.homebrew_binary() else {
        install_and_verify(
            executor,
            target,
            platform,
            version,
            release_base,
            no_install,
        )?;
        return Ok(Installed {
            method: INSTALL_LOCAL_BIN,
            binary: None,
            homebrew_prefix: None,
            version: version.to_string(),
        });
    };
    let prefix = found.homebrew_prefix.as_deref().unwrap_or_default();

    if found.local_bin {
        let _ = writeln!(
            out,
            "Remote '{name}' has two dot-agent-deck installs: Homebrew's at {binary} and a copy at {REMOTE_INSTALL_PATH}. The deck uses the Homebrew one and leaves the other untouched. Remove it (`{cleanup}`) — an older dot-agent-deck client still runs it on `connect`.",
            binary = binary.as_str(),
            cleanup = target.command_line(&format!("rm {REMOTE_INSTALL_PATH}")),
        );
    }

    if run_brew_upgrade {
        let formula = found
            .homebrew_formula
            .expect("a detected Homebrew install has a formula");
        let brew = with_homebrew_path(prefix, &format!("{prefix}/bin/brew upgrade {formula}"));
        let upgraded = executor.run(target, &brew)?;
        if upgraded.status != 0 {
            return Err(RemoteAddError::BrewUpgradeFailed {
                status: upgraded.status,
                stderr: scrub_remote_text(&upgraded.stderr),
            });
        }
    }

    // After a `brew upgrade` that succeeded, Homebrew may have replaced the
    // binary, so a failed check says so; without one nothing changed.
    let landed =
        remote_binary_version(executor, target, binary.as_str(), version).map_err(|e| {
            if run_brew_upgrade {
                replaced_but_unverified(binary.as_str(), e)
            } else {
                e
            }
        })?;
    // `--no-install` is a pre-flight that the remote already runs the
    // requested version, on this path as on the `~/.local/bin` one; it is
    // only when this command installs through Homebrew that a different
    // version is accepted, because Homebrew cannot install a chosen one.
    if no_install && landed != version {
        return Err(RemoteAddError::VersionMismatch {
            actual: landed,
            expected: version.to_string(),
        });
    }
    if landed != version {
        let next = if run_brew_upgrade {
            "Homebrew installs its tap's latest release and cannot install a chosen one"
        } else {
            "Run `dot-agent-deck remote upgrade` to upgrade it through Homebrew"
        };
        let _ = writeln!(
            out,
            "Remote '{name}' runs dot-agent-deck {landed} from Homebrew ({binary}), not the requested {version}. {next}; the registry records {landed}.",
            binary = binary.as_str(),
        );
    }
    Ok(Installed {
        method: INSTALL_HOMEBREW,
        binary: Some(binary),
        homebrew_prefix: Some(prefix.to_string()),
        version: landed,
    })
}

/// Run `hooks install` on the remote with the binary that was just installed,
/// printing whatever it reports. Shared by `add` and `upgrade`.
fn install_remote_hooks(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    installed: &Installed,
    out: &mut dyn std::io::Write,
) -> Result<(), RemoteAddError> {
    let command = format!("{} hooks install", installed.remote_binary());
    let command = match installed.homebrew_prefix.as_deref() {
        Some(prefix) => with_homebrew_path(prefix, &command),
        None => command,
    };
    let hooks = executor.run(target, &command)?;
    if hooks.status != 0 {
        return Err(RemoteAddError::HooksInstallFailed {
            status: hooks.status,
            stderr: scrub_remote_text(&hooks.stderr),
        });
    }
    if !hooks.stdout.is_empty() {
        let _ = write!(out, "{}", hooks.stdout);
        if !hooks.stdout.ends_with('\n') {
            let _ = writeln!(out);
        }
    }
    Ok(())
}

/// Run the `add` flow. Returns the registry entry that was written, so
/// callers (and tests) can assert on it. Progress goes to stdout.
pub fn add(
    opts: &AddOptions,
    executor: &dyn SshExecutor,
    remotes_path: &Path,
) -> Result<RemoteEntry, RemoteAddError> {
    add_reporting_to(opts, executor, remotes_path, &mut std::io::stdout().lock())
}

/// [`add`], writing what it reports to `out` instead of stdout.
pub fn add_reporting_to(
    opts: &AddOptions,
    executor: &dyn SshExecutor,
    remotes_path: &Path,
    out: &mut dyn std::io::Write,
) -> Result<RemoteEntry, RemoteAddError> {
    // 1. Type validation.
    match opts.remote_type.as_str() {
        "ssh" => {}
        "kubernetes" => return Err(RemoteAddError::KubernetesNotYetImplemented),
        other => {
            return Err(RemoteAddError::UnsupportedType {
                kind: other.to_string(),
            });
        }
    }

    // 2. Version validation — runs BEFORE any ssh call or URL construction so
    //    a malicious `--version` (e.g. `0.24.5; rm -rf ~`) can't reach the
    //    remote shell. Asserted by the `version_string_with_shell_metacharacters_rejected`
    //    test, which checks zero ssh calls were attempted. We use the
    //    normalized (unprefixed) form everywhere downstream so that whether
    //    the user typed `0.24.5` or `v0.24.5`, the URL gets exactly one `v`
    //    and the post-install version comparison matches the binary's
    //    unprefixed `--version` output.
    let version = validate_version_string(&opts.version)?;
    // Issue #1350: the name is a slug, checked before any ssh call too.
    crate::deck_list::validate_deck_name(&opts.name).map_err(RemoteAddError::InvalidName)?;
    // …and so is the address: the rules `deck_list::add` enforces on the row,
    // applied here so an unsafe target never reaches ssh.
    let key = opts.key.as_ref().map(|p| p.to_string_lossy());
    crate::deck_list::validate_ssh_target(&opts.target, opts.port, key.as_deref())
        .map_err(RemoteAddError::InvalidAddress)?;

    // 3. Uniqueness check — done *before* any ssh call so a duplicate name
    //    short-circuits without bothering the remote (and lets the
    //    `duplicate_name_rejected` test assert the fake recorded zero
    //    commands).
    let registry = RemotesFile::load(remotes_path)?;
    if registry.remotes.iter().any(|r| r.name == opts.name) {
        return Err(RemoteAddError::DuplicateName {
            name: opts.name.clone(),
        });
    }

    let target = SshTarget::parse(&opts.target, opts.port, opts.key.clone());

    // 3. Pre-flight reachability + arch detection.
    let uname = executor.run(&target, "uname -s -m")?;
    if uname.status != 0 {
        return Err(RemoteAddError::UnameFailed {
            status: uname.status,
            stderr: scrub_remote_text(&uname.stderr),
        });
    }
    let platform =
        detect_platform(&uname.stdout).ok_or_else(|| RemoteAddError::UnsupportedArch {
            arch: scrub_remote_text(&uname.stdout),
        })?;

    // 4. Install or version-check through whatever owns the install on the
    //    remote (shared between `add` and `upgrade`, issue #1372). `add`
    //    registers an existing Homebrew install as it is rather than
    //    upgrading it.
    let installed = install_or_upgrade(
        executor,
        &target,
        &opts.name,
        platform,
        &version,
        &opts.release_base,
        opts.no_install,
        false,
        out,
    )?;

    // 5. Hook install on the remote, by the binary that was just verified.
    install_remote_hooks(executor, &target, &installed, out)?;

    // 6. Append to registry.
    let entry = RemoteEntry {
        name: opts.name.clone(),
        kind: "ssh".to_string(),
        host: opts.target.clone(),
        port: opts.port,
        key: opts
            .key
            .as_ref()
            .and_then(|p| p.as_os_str().to_str())
            .map(|s| s.to_string())
            .or_else(|| opts.key.as_ref().map(|p| p.to_string_lossy().into_owned())),
        version: installed.version.clone(),
        added_at: chrono::Utc::now().to_rfc3339(),
        upgraded_at: None,
        last_connected: None,
        install: Some(installed.method.to_string()),
        binary: installed.binary.clone(),
        id: None,
        user: None,
        jump_host: None,
        socket: None,
    };
    // Against a fresh read (issue #1350): the install above took seconds, and
    // the desktop may have saved a deck meanwhile.
    let entry = crate::deck_list::add(remotes_path, entry)?;

    // 7. Final success line.
    let _ = writeln!(
        out,
        "Added remote '{}' (ssh: {}, version {}). Run `dot-agent-deck connect {}` to attach.",
        opts.name, opts.target, entry.version, opts.name,
    );

    Ok(entry)
}

// ---------------------------------------------------------------------------
// `remote list` — purely offline metadata read (no ssh, no probing). Live
// status comes in M2.6.
// ---------------------------------------------------------------------------

/// Render a registry entry's `added_at` (RFC3339) as a human-friendly relative
/// form ("2d ago"). Falls back to the raw string if parsing fails — better to
/// surface the registry's exact contents than to silently swallow malformed
/// data. Goes up to days; `format_elapsed` in `ui.rs` is for short-lived
/// pane sessions and tops out at hours, so it's not the right shape here.
fn format_relative_time_rfc3339(rfc3339: &str) -> String {
    let parsed = match chrono::DateTime::parse_from_rfc3339(rfc3339) {
        Ok(t) => t.with_timezone(&chrono::Utc),
        Err(_) => return rfc3339.to_string(),
    };
    let secs = chrono::Utc::now()
        .signed_duration_since(parsed)
        .num_seconds()
        .max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

/// Format a registry entry's host column: `[user@]host[:port]`. The default
/// ssh port (22) is omitted to keep the table tidy; any non-default port is
/// suffixed.
fn format_host_column(entry: &RemoteEntry) -> String {
    if entry.port == DEFAULT_SSH_PORT {
        entry.host.clone()
    } else {
        format!("{}:{}", entry.host, entry.port)
    }
}

/// Render the registry as a column-aligned table. Empty registry produces
/// the "No remotes configured" hint instead of a header-only table.
pub fn list(remotes_path: &Path, out: &mut dyn std::io::Write) -> Result<(), RemoteConfigError> {
    let registry = RemotesFile::load(remotes_path)?;
    if registry.remotes.is_empty() {
        writeln!(
            out,
            "No remotes configured. Use `dot-agent-deck remote add <name> <host>` to add one."
        )
        .map_err(|source| RemoteConfigError::Io {
            path: remotes_path.display().to_string(),
            source,
        })?;
        return Ok(());
    }

    let headers = ["NAME", "TYPE", "HOST", "VERSION", "ADDED_AT"];
    let rows: Vec<[String; 5]> = registry
        .remotes
        .iter()
        .map(|e| {
            [
                e.name.clone(),
                e.kind.clone(),
                format_host_column(e),
                e.version.clone(),
                format_relative_time_rfc3339(&e.added_at),
            ]
        })
        .collect();

    let mut widths = headers.map(|h| h.len());
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }

    let write_io = |result: std::io::Result<()>| -> Result<(), RemoteConfigError> {
        result.map_err(|source| RemoteConfigError::Io {
            path: remotes_path.display().to_string(),
            source,
        })
    };

    write_io(write_table_row(out, &headers, &widths))?;
    for row in &rows {
        let cells: [&str; 5] = [&row[0], &row[1], &row[2], &row[3], &row[4]];
        write_io(write_table_row(out, &cells, &widths))?;
    }
    Ok(())
}

/// Write one table row, two-space gap between columns, last column is not
/// padded (avoids trailing whitespace).
fn write_table_row(
    out: &mut dyn std::io::Write,
    cells: &[&str; 5],
    widths: &[usize; 5],
) -> std::io::Result<()> {
    for i in 0..4 {
        write!(out, "{:<width$}  ", cells[i], width = widths[i])?;
    }
    writeln!(out, "{}", cells[4])
}

// ---------------------------------------------------------------------------
// `remote remove` — registry-only. Does not touch the remote host.
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum RemoteRemoveError {
    #[error(
        "No remote named '{name}'. Run `dot-agent-deck remote list` to see configured remotes."
    )]
    UnknownName { name: String },
    #[error(transparent)]
    Registry(#[from] RemoteConfigError),
}

/// Remove the registry entry for `name`. Returns the removed entry so callers
/// can confirm what was deleted. **Does not** touch the remote host — no
/// hooks uninstall, no binary cleanup, no ssh call.
pub fn remove(name: &str, remotes_path: &Path) -> Result<RemoteEntry, RemoteRemoveError> {
    crate::deck_list::remove(remotes_path, crate::deck_list::DeckRef::Name(name))?.ok_or_else(
        || RemoteRemoveError::UnknownName {
            name: name.to_string(),
        },
    )
}

// ---------------------------------------------------------------------------
// `remote upgrade` — re-run the install pipeline against an existing entry.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UpgradeOptions {
    pub name: String,
    pub version: String,
    pub no_install: bool,
    pub release_base: String,
}

#[derive(Debug, Error)]
pub enum RemoteUpgradeError {
    #[error(
        "No remote named '{name}'. Run `dot-agent-deck remote list` to see configured remotes."
    )]
    UnknownName { name: String },
    /// The install + version-check pipeline shared with `add` already covers
    /// version validation, ssh transport errors, arch detection, install
    /// failure, version mismatch, and registry I/O — wrap that error type
    /// rather than duplicating the variants. Two manual `From` impls below
    /// keep the `?` ergonomics for `SshError` and `RemoteConfigError`.
    #[error(transparent)]
    Inner(#[from] RemoteAddError),
    /// The deck list's row for this remote no longer reaches the machine the
    /// upgrade installed on: it was removed and re-added, or edited, for
    /// another address while the upgrade ran (PRD #1487, Greptile
    /// 4208066970). Nothing is recorded over the row.
    #[error(
        "the deck list's row for '{name}' was changed to reach a different machine while the upgrade ran, so the result was not recorded over it. Check the row with `dot-agent-deck remote list` and run the upgrade again."
    )]
    RouteChanged { name: String },
    /// The new build is already in place on the remote, and a step after it —
    /// reinstalling the hooks, or recording it in the deck list — failed. The
    /// binary is not rolled back; running the upgrade again finishes the rest.
    #[error("{installed_version} was installed, but {step} failed: {source}")]
    AfterInstall {
        installed_version: String,
        step: &'static str,
        #[source]
        source: Box<RemoteUpgradeError>,
    },
}

impl RemoteUpgradeError {
    /// What is in place when this error happened, if the install had already
    /// replaced the binary: the version it finished with, or — when the check
    /// after the replacement failed — the version that check read, or
    /// [`UNVERIFIED_BUILD`].
    pub fn installed_version(&self) -> Option<&str> {
        match self {
            Self::AfterInstall {
                installed_version, ..
            } => Some(installed_version),
            Self::Inner(RemoteAddError::ReplacedButUnverified { on_disk, .. }) => Some(on_disk),
            _ => None,
        }
    }
}

impl From<SshError> for RemoteUpgradeError {
    fn from(e: SshError) -> Self {
        RemoteUpgradeError::Inner(e.into())
    }
}

impl From<RemoteConfigError> for RemoteUpgradeError {
    fn from(e: RemoteConfigError) -> Self {
        RemoteUpgradeError::Inner(e.into())
    }
}

/// Run the `upgrade` flow. Validates `version`, looks up the existing entry,
/// re-runs install + version-check, then updates the registry's `version`
/// and `upgraded_at` fields. `added_at` stays at the original registration
/// timestamp.
pub fn upgrade(
    opts: &UpgradeOptions,
    executor: &dyn SshExecutor,
    remotes_path: &Path,
) -> Result<RemoteEntry, RemoteUpgradeError> {
    upgrade_reporting_to(opts, executor, remotes_path, &mut std::io::stdout().lock())
}

/// [`upgrade`], writing what it reports to `out` instead of stdout.
pub fn upgrade_reporting_to(
    opts: &UpgradeOptions,
    executor: &dyn SshExecutor,
    remotes_path: &Path,
    out: &mut dyn std::io::Write,
) -> Result<RemoteEntry, RemoteUpgradeError> {
    // 1. Version validation BEFORE any ssh call (mirrors `add`).
    validate_version_string(&opts.version)?;

    // 2. Lookup. Unknown name short-circuits before any ssh work.
    let registry = RemotesFile::load(remotes_path)?;
    let existing = registry
        .remotes
        .into_iter()
        .find(|r| r.name == opts.name)
        .ok_or_else(|| RemoteUpgradeError::UnknownName {
            name: opts.name.clone(),
        })?;
    upgrade_entry_reporting_to(opts, &existing, executor, remotes_path, out)
}

/// [`upgrade_reporting_to`] against `existing`, the row the caller already
/// read, instead of reading it again by name — so the install goes to the
/// machine the caller's other commands (its probe and restart) go to. The
/// result is recorded only while the row named `opts.name` still reaches that
/// machine ([`RemoteEntry::same_route`]); a row moved elsewhere in the
/// meantime is refused with [`RemoteUpgradeError::RouteChanged`] (PRD #1487,
/// Greptile 4208066970).
pub fn upgrade_entry_reporting_to(
    opts: &UpgradeOptions,
    existing: &RemoteEntry,
    executor: &dyn SshExecutor,
    remotes_path: &Path,
    out: &mut dyn std::io::Write,
) -> Result<RemoteEntry, RemoteUpgradeError> {
    // 1. Version validation BEFORE any ssh call (mirrors `add`).
    let version = validate_version_string(&opts.version)?;

    let target = existing.ssh_target();
    let was_homebrew = existing.install.as_deref() == Some(INSTALL_HOMEBREW);

    // 3. Reachability + arch detect.
    let uname = executor.run(&target, "uname -s -m")?;
    if uname.status != 0 {
        return Err(RemoteAddError::UnameFailed {
            status: uname.status,
            stderr: scrub_remote_text(&uname.stderr),
        }
        .into());
    }
    let platform =
        detect_platform(&uname.stdout).ok_or_else(|| RemoteAddError::UnsupportedArch {
            arch: scrub_remote_text(&uname.stdout),
        })?;

    // 4. Install + version-check through whatever owns the install on the
    //    remote (shared with `add`, issue #1372): `brew upgrade` for a
    //    Homebrew install, the release download to `~/.local/bin` otherwise.
    let installed = install_or_upgrade(
        executor,
        &target,
        &opts.name,
        platform,
        &version,
        &opts.release_base,
        opts.no_install,
        !opts.no_install,
        out,
    )?;
    if was_homebrew && installed.method != INSTALL_HOMEBREW {
        let _ = writeln!(
            out,
            "Remote '{}' no longer has a Homebrew install of dot-agent-deck; it now uses {REMOTE_INSTALL_PATH}.",
            opts.name
        );
    }

    // 5. Reinstall hooks on the remote. A release may change hook behavior,
    //    so the upgraded binary must be paired with refreshed hook scripts —
    //    otherwise `remote upgrade` reports success while leaving the remote
    //    on stale hooks. Mirrors the same step in `add()` so both paths stay
    //    in lockstep. Runs BEFORE the registry update so a hook-install failure
    //    fails loud rather than persisting half-finished state. The binary is
    //    already in place by now, so a failure from here on says so.
    let after_install = |step: &'static str| {
        let installed_version = installed.version.clone();
        move |source: RemoteUpgradeError| RemoteUpgradeError::AfterInstall {
            installed_version,
            step,
            source: Box::new(source),
        }
    };
    install_remote_hooks(executor, &target, &installed, out)
        .map_err(|e| after_install("reinstalling the hooks")(e.into()))?;

    // 6. Update registry. `added_at` stays at the original registration
    //    timestamp; `upgraded_at` records the most recent upgrade so users
    //    can see both moments without losing the registration history. The
    //    install method and binary are re-recorded on every upgrade, which is
    //    what moves an entry written before issue #1372 onto the method that
    //    actually owns the remote's install.
    //    Only while the row still reaches the machine this run installed on:
    //    one moved to another address since `existing` was read is refused
    //    rather than stamped with a version that machine does not run.
    let now = chrono::Utc::now().to_rfc3339();
    let updated = crate::deck_list::update_if(
        remotes_path,
        crate::deck_list::DeckRef::Name(&opts.name),
        |entry| {
            if entry.same_route(existing) {
                Ok(())
            } else {
                Err(RemoteUpgradeError::RouteChanged {
                    name: opts.name.clone(),
                })
            }
        },
        |entry| {
            entry.version = installed.version.clone();
            entry.upgraded_at = Some(now);
            entry.install = Some(installed.method.to_string());
            entry.binary = installed.binary.clone();
        },
    )
    .map_err(after_install("recording it in the deck list"))?
    .ok_or_else(|| {
        after_install("recording it in the deck list")(RemoteUpgradeError::UnknownName {
            name: opts.name.clone(),
        })
    })?;

    let _ = writeln!(
        out,
        "Upgraded remote '{}' to version {}.",
        opts.name, updated.version,
    );

    Ok(updated)
}

// ---------------------------------------------------------------------------
// Tests for the production SshExecutor's argument construction. Crucially
// these do NOT spawn ssh — they inspect the `Command`'s args, which is
// enough to catch quoting regressions and shell-injection mistakes.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(OsStr::to_string_lossy)
            .map(|s| s.into_owned())
            .collect()
    }

    #[test]
    fn ssh_target_parse_with_user() {
        let t = SshTarget::parse("viktor@hetzner-1.example.com", 2222, None);
        assert_eq!(t.user.as_deref(), Some("viktor"));
        assert_eq!(t.host, "hetzner-1.example.com");
        assert_eq!(t.port, 2222);
        assert_eq!(t.user_host(), "viktor@hetzner-1.example.com");
    }

    #[test]
    fn command_line_carries_the_registered_port_and_key() {
        assert_eq!(
            SshTarget::parse("u@h", 22, None).command_line("rm x"),
            "ssh u@h 'rm x'"
        );
        assert_eq!(
            SshTarget::parse("u@h", 2222, Some(PathBuf::from("/k/it's key"))).command_line("rm x"),
            "ssh -p 2222 -i '/k/it'\\''s key' u@h 'rm x'"
        );
        assert_eq!(
            SshTarget::parse("u@h", 22, Some(PathBuf::from("/home/u/.ssh/id_ed25519")))
                .command_line("rm x"),
            "ssh -i /home/u/.ssh/id_ed25519 u@h 'rm x'"
        );
        // The destination is the user's own `remote add` input; pasting the
        // line must still run nothing but `ssh` locally.
        let hostile = SshTarget::parse("u@h;touch pwned", 22, None).command_line("rm x");
        assert_eq!(hostile, "ssh 'u@h;touch pwned' 'rm x'");
        let option = SshTarget::parse("-oProxyCommand=id", 22, None).command_line("rm x");
        assert_eq!(option, "ssh -- -oProxyCommand=id 'rm x'");
    }

    /// Scenario: a deck reached through a jump host prints its remedy and its
    /// cleanup commands with `-J <jump host>`, in the place the real session
    /// passes it, so a pasted command takes the deck's route; a jump host that
    /// does not validate is left out of both, as the session leaves it out
    /// (PRD #1487, Qodo 4202060288).
    #[test]
    fn printed_commands_take_the_jump_host_the_session_takes() {
        let mut routed = SshTarget::parse("u@h", 2222, Some(PathBuf::from("/k/id")));
        routed.jump = Some("bastion".to_string());
        assert_eq!(
            routed.command_line("rm x"),
            "ssh -p 2222 -i /k/id -J bastion u@h 'rm x'"
        );
        assert_eq!(routed.host_key_remedy(), "ssh -J bastion -p 2222 u@h");
        let session = SystemSshExecutor::default().build_command(&routed, "rm x");
        let args: Vec<String> = session
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let jump_at = args.iter().position(|a| a == "-J").expect("-J is passed");
        assert_eq!(args[jump_at + 1], "bastion");

        let mut hostile = SshTarget::parse("u@h", 22, None);
        hostile.jump = Some("-oProxyCommand=id".to_string());
        assert_eq!(hostile.command_line("rm x"), "ssh u@h 'rm x'");
        assert_eq!(hostile.host_key_remedy(), "ssh u@h");
    }

    #[test]
    fn ssh_target_parse_without_user() {
        let t = SshTarget::parse("hetzner-1.example.com", 22, None);
        assert!(t.user.is_none());
        assert_eq!(t.user_host(), "hetzner-1.example.com");
    }

    #[test]
    fn system_ssh_executor_quotes_arguments_safely() {
        // Hostile-looking host string + a remote command that would be
        // catastrophic if naively interpolated into a shell. Our impl uses
        // `Command::arg` (no shell), so each lands as its own argv entry —
        // the local ssh process never sees a meta-character it can interpret.
        let target = SshTarget {
            host: "host`whoami`".to_string(),
            user: Some("user;rm -rf /".to_string()),
            port: 2222,
            key: Some(PathBuf::from("/tmp/key id_rsa")),
            jump: None,
        };
        let cmd = SystemSshExecutor::new().build_command(&target, "uname -s -m; echo $(id)");
        let args = args_of(&cmd);

        // Order matters: -o BatchMode -p PORT [-i KEY] -- user@host CMD.
        assert_eq!(args[0], "-o");
        assert_eq!(args[1], "BatchMode=yes");
        assert_eq!(args[2], "-p");
        assert_eq!(args[3], "2222");
        assert_eq!(args[4], "-i");
        assert_eq!(args[5], "/tmp/key id_rsa"); // single arg, space preserved
        assert_eq!(args[6], "--");
        assert_eq!(args[7], "user;rm -rf /@host`whoami`");
        // Remote command is one arg — ssh ships it to the remote shell as a
        // single string, but locally it's a single argv entry that the *local*
        // shell never parses.
        assert_eq!(args[8], "uname -s -m; echo $(id)");
        assert_eq!(args.len(), 9);
    }

    #[test]
    fn system_ssh_executor_omits_key_flag_when_none() {
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
            jump: None,
        };
        let cmd = SystemSshExecutor::new().build_command(&target, "echo hi");
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-i"));
        // -- precedes the destination
        let dash_dash_pos = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_dash_pos + 1], "h");
        assert_eq!(args[dash_dash_pos + 2], "echo hi");
    }

    #[test]
    fn system_ssh_executor_emits_no_timeout_options_by_default() {
        // The default constructor must not silently impose a ConnectTimeout
        // — long-running flows like `remote add` install the binary across
        // the network, and a 10s ceiling there would be wrong. The opt-in
        // `with_wallclock_timeout` constructor is the only path that adds
        // timeout flags to the argv.
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
            jump: None,
        };
        let cmd = SystemSshExecutor::new().build_command(&target, "echo hi");
        let args = args_of(&cmd);
        assert!(
            !args.iter().any(|a| a.starts_with("ConnectTimeout=")),
            "default executor must not impose ConnectTimeout: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("ServerAliveInterval=")),
            "default executor must not impose ServerAliveInterval: {args:?}"
        );
    }

    #[test]
    fn system_ssh_executor_with_wallclock_timeout_sets_ssh_options() {
        // The probe path uses `with_wallclock_timeout(N)` to bound a
        // reachable-but-stalled remote. The fix maps to ssh's own
        // ConnectTimeout (pre-handshake cap) AND ServerAliveInterval +
        // ServerAliveCountMax=1 (post-handshake cap) so a remote that
        // accepts the TCP connection but never speaks can't pin us forever.
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
            jump: None,
        };
        let cmd = SystemSshExecutor::with_wallclock_timeout(7).build_command(&target, "echo hi");
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "ConnectTimeout=7"),
            "with_wallclock_timeout must set ConnectTimeout: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "ServerAliveInterval=7"),
            "with_wallclock_timeout must set ServerAliveInterval: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "ServerAliveCountMax=1"),
            "with_wallclock_timeout must force a single missed-keepalive abort: {args:?}"
        );

        // Issue #858: the laptop-side kill is documented as a BACKSTOP against
        // a remote that accepts the connection and never returns — the case
        // the ssh options above cannot see. So its deadline must exceed the
        // worst case those very options permit: up to `ConnectTimeout` in the
        // pre-handshake phase, then up to `ServerAliveInterval ×
        // ServerAliveCountMax` of post-handshake silence before ssh itself
        // disconnects. Armed at `secs` it was the BINDING constraint instead,
        // and a connection ssh was still allowed ~20s to finish was killed at
        // 10s and reported as an unreachable host.
        //
        // Asserted as a RELATIONSHIP between the argv and the deadline rather
        // than as a number, so it keeps holding if either constant moves.
        let option = |key: &str| -> u64 {
            args.iter()
                .find_map(|a| a.strip_prefix(key))
                .unwrap_or_else(|| panic!("missing ssh option {key}: {args:?}"))
                .parse()
                .unwrap_or_else(|e| panic!("unparseable ssh option {key}: {e}"))
        };
        let ssh_worst_case = option("ConnectTimeout=")
            + option("ServerAliveInterval=") * option("ServerAliveCountMax=");
        let deadline = SystemSshExecutor::with_wallclock_timeout(7)
            .kill_deadline_secs()
            .expect("the probe executor arms a laptop-side kill");
        assert!(
            deadline > ssh_worst_case,
            "the laptop-side kill ({deadline}s) must outlast what its own ssh options \
             permit ({ssh_worst_case}s), or it pre-empts ssh instead of backstopping it"
        );
    }

    /// PRD #345 audit: an observation session must not create, persist or
    /// write any of the state it is inspecting — and must not weaken host-key
    /// verification while doing so.
    #[test]
    fn system_ssh_executor_for_observation_creates_no_forwards_and_no_master() {
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
            jump: None,
        };
        let cmd = SystemSshExecutor::for_observation(9).build_command(&target, "echo hi");
        let args = args_of(&cmd);

        for expected in [
            // The load-bearing one: without it the doctor's own session
            // establishes the forward it then reports on.
            "ClearAllForwardings=yes",
            "ControlMaster=no",
            "ControlPath=none",
            "PermitLocalCommand=no",
            "UpdateHostKeys=no",
            // Delegation: `ClearAllForwardings` clears local/remote/dynamic
            // /tunnel forwards and NOTHING else, so a `Host` block carrying
            // `ForwardAgent yes` would otherwise hand the laptop's ssh-agent
            // to every probe — including the ones run against an endpoint the
            // user already suspects (PRD #345 second audit).
            "ForwardAgent=no",
            "ForwardX11=no",
            "ForwardX11Trusted=no",
            "GSSAPIDelegateCredentials=no",
            "AddKeysToAgent=no",
        ] {
            assert!(
                args.iter().any(|a| a == expected),
                "observation session must set {expected}: {args:?}"
            );
        }
        // It is still a probe: the fail-fast wallclock options come too.
        assert!(args.iter().any(|a| a == "ConnectTimeout=9"), "{args:?}");
        assert!(args.iter().any(|a| a == "BatchMode=yes"), "{args:?}");
        // Host-key VERIFICATION is a security control, not a mutation. A
        // diagnostic has no business relaxing it to be quieter.
        assert!(
            !args.iter().any(|a| a.contains("StrictHostKeyChecking")),
            "observation must not touch host-key verification: {args:?}"
        );

        // Ordinary sessions are untouched: `connect` / `remote add` /
        // `remote upgrade` are supposed to honour the user's Host block.
        let ordinary =
            SystemSshExecutor::with_wallclock_timeout(9).build_command(&target, "echo hi");
        assert!(
            !args_of(&ordinary)
                .iter()
                .any(|a| a == "ClearAllForwardings=yes"),
            "only observation sessions clear forwardings"
        );
    }

    /// Issue #1490 audit A1: `observing()` over the keepalive bounds applies
    /// the one observation list, and `requiring_known_host_key()` adds the
    /// tunnel's host-key policy — neither is applied unless asked for.
    #[test]
    fn system_ssh_executor_observing_with_keepalive_requires_a_known_host_key() {
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
            jump: None,
        };
        let observing = SystemSshExecutor::with_keepalive(10, 15, 8)
            .observing()
            .requiring_known_host_key();
        let args = args_of(&observing.build_command(&target, "echo hi"));
        let mut observation_only = Command::new("ssh");
        apply_observation_options(&mut observation_only);
        for expected in args_of(&observation_only)
            .iter()
            .filter(|arg| *arg != "-o")
            .map(String::as_str)
            .chain([
                "StrictHostKeyChecking=yes",
                "BatchMode=yes",
                "ConnectTimeout=10",
                "ServerAliveInterval=15",
                "ServerAliveCountMax=8",
            ])
        {
            assert!(args.iter().any(|a| a == expected), "{expected}: {args:?}");
        }
        // Keepalive-bounded, not wallclock-killed.
        assert_eq!(observing.kill_deadline_secs(), None);

        let plain =
            args_of(&SystemSshExecutor::with_keepalive(10, 15, 8).build_command(&target, "x"));
        assert!(
            !plain
                .iter()
                .any(|a| a.starts_with("StrictHostKeyChecking") || a == "ForwardAgent=no"),
            "an ordinary keepalive session honours the user's Host block: {plain:?}"
        );
    }

    /// Issue #1490 audit A2: a raw string becomes a remote command's binary
    /// only through `RemoteDeckBinary::try_from`, which accepts the default
    /// install and a safe absolute path and refuses whitespace, every shell
    /// metacharacter, relative paths and any other `~` spelling.
    #[test]
    fn remote_deck_binary_refuses_anything_a_shell_would_reinterpret() {
        assert_eq!(
            RemoteDeckBinary::try_from(REMOTE_INSTALL_PATH),
            Ok(RemoteDeckBinary::DefaultInstall)
        );
        assert_eq!(
            RemoteDeckBinary::DefaultInstall.as_shell_word(),
            "~/.local/bin/dot-agent-deck",
            "the default keeps its `~` for the remote shell to expand"
        );
        let homebrew = RemoteDeckBinary::try_from("/opt/homebrew/bin/dot-agent-deck").unwrap();
        assert_eq!(homebrew.as_shell_word(), "/opt/homebrew/bin/dot-agent-deck");
        for refused in [
            "",
            "/",
            "dot-agent-deck",
            "bin/dot-agent-deck",
            "~/bin/dot-agent-deck",
            "~other/.local/bin/dot-agent-deck",
            "/opt/my bin/dot-agent-deck",
            "/opt/bin/dot-agent-deck\t",
            "/opt/bin/dot-agent-deck\n",
            "/opt/bin/dot-agent-deck;rm -rf ~",
            "/opt/bin/dot-agent-deck&&id",
            "/opt/bin/dot-agent-deck|id",
            "/opt/bin/$(id)",
            "/opt/bin/`id`",
            "/opt/bin/$HOME",
            "/opt/bin/dot-agent-deck>x",
            "/opt/bin/dot-agent-deck<x",
            "/opt/bin/'dot-agent-deck'",
            "/opt/bin/\"dot-agent-deck\"",
            "/opt/bin/dot*",
            "/opt/bin/dot?",
            "/opt/bin/\\dot",
            "/opt/bin/(dot)",
            "/opt/bin/{dot}",
            "/opt/bin/dot#x",
            "/opt/bin/dot!x",
        ] {
            assert!(
                RemoteDeckBinary::try_from(refused).is_err(),
                "{refused:?} must be refused"
            );
        }
        let row = RemoteEntry {
            binary: Some(
                RemoteBinaryPath::try_from("/opt/homebrew/bin/dot-agent-deck".to_string()).unwrap(),
            ),
            ..entry_with_binary_none()
        };
        assert_eq!(row.deck_binary(), homebrew);
        assert_eq!(
            entry_with_binary_none().deck_binary(),
            RemoteDeckBinary::DefaultInstall
        );
    }

    fn entry_with_binary_none() -> RemoteEntry {
        RemoteEntry {
            name: "mac".to_string(),
            kind: "ssh".to_string(),
            host: "user@mac".to_string(),
            port: 22,
            key: None,
            version: "0.1.0".to_string(),
            added_at: "2026-01-01T00:00:00Z".to_string(),
            upgraded_at: None,
            last_connected: None,
            install: None,
            binary: None,
            id: None,
            user: None,
            jump_host: None,
            socket: None,
        }
    }

    #[test]
    fn system_ssh_executor_with_keepalive_sets_alive_options_without_count_max_1() {
        // PRD #161 FIX 3: the remote-UPGRADE executor sets ConnectTimeout +
        // ServerAliveInterval + ServerAliveCountMax so a dead connection is
        // DETECTED — but with a count-max > 1 (and no laptop-side wallclock
        // kill), so a slow-but-alive download isn't aborted on the first
        // missed keepalive the way the probe path's CountMax=1 would.
        let target = SshTarget {
            host: "h".to_string(),
            user: None,
            port: 22,
            key: None,
            jump: None,
        };
        let cmd = SystemSshExecutor::with_keepalive(30, 15, 8).build_command(&target, "echo hi");
        let args = args_of(&cmd);
        assert!(
            args.iter().any(|a| a == "ConnectTimeout=30"),
            "with_keepalive must set ConnectTimeout: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "ServerAliveInterval=15"),
            "with_keepalive must set ServerAliveInterval: {args:?}"
        );
        assert!(
            args.iter().any(|a| a == "ServerAliveCountMax=8"),
            "with_keepalive must set a count-max > 1 so a slow download survives \
             a transiently missed keepalive: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "ServerAliveCountMax=1"),
            "with_keepalive must NOT use the probe path's single-miss abort: {args:?}"
        );
    }

    #[test]
    fn detect_platform_known() {
        assert_eq!(detect_platform("Linux x86_64\n"), Some("linux-amd64"));
        assert_eq!(detect_platform("Linux aarch64"), Some("linux-arm64"));
        assert_eq!(detect_platform("Linux arm64"), Some("linux-arm64"));
        assert_eq!(detect_platform("Darwin x86_64"), Some("darwin-amd64"));
        assert_eq!(detect_platform("Darwin arm64"), Some("darwin-arm64"));
    }

    #[test]
    fn detect_platform_unknown() {
        assert_eq!(detect_platform("Linux riscv64"), None);
        assert_eq!(detect_platform("FreeBSD amd64"), None);
        assert_eq!(detect_platform(""), None);
    }

    #[test]
    fn parse_version_output_typical() {
        assert_eq!(
            parse_version_output("dot-agent-deck 0.24.5\n"),
            Some("0.24.5".to_string())
        );
        assert_eq!(
            parse_version_output("dot-agent-deck 1.2.3"),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn validate_version_string_accepts_semver_shapes() {
        for v in [
            "0.24.5",
            "v0.24.5",
            "1.2.3",
            "0.0.1",
            "10.20.30",
            "1.0.0-rc.1",
            "0.24.5-pre.2",
        ] {
            assert!(
                validate_version_string(v).is_ok(),
                "expected `{v}` to validate"
            );
        }
    }

    #[test]
    fn validate_version_string_strips_optional_v_prefix() {
        // The canonical internal form is unprefixed — both inputs normalize
        // to `0.24.5`. Without this, `--version v0.24.5` would build a URL
        // with `vv` in the release-tag segment and 404 on GitHub.
        assert_eq!(
            validate_version_string("v0.24.5").unwrap(),
            "0.24.5".to_string()
        );
        assert_eq!(
            validate_version_string("0.24.5").unwrap(),
            "0.24.5".to_string()
        );
        // Pre-release suffixes are preserved; only the leading `v` is stripped.
        assert_eq!(
            validate_version_string("v1.0.0-rc.1").unwrap(),
            "1.0.0-rc.1".to_string()
        );
    }

    #[test]
    fn validate_version_string_rejects_malformed() {
        for v in [
            "",
            "not-a-version",
            "1.2",
            "1.2.3.4",
            "v1.2",
            "1.2.3 ", // trailing whitespace
            " 1.2.3", // leading whitespace
            "1.2.3;", // metacharacter
            "1.2.3$x",
        ] {
            let err = validate_version_string(v).expect_err(&format!("expected `{v}` to fail"));
            match err {
                RemoteAddError::InvalidVersion { input } => assert_eq!(input, v),
                other => panic!("unexpected error for `{v}`: {other:?}"),
            }
        }
    }

    #[test]
    fn build_install_command_rejects_invalid_version() {
        let err = build_install_command("https://example.test", "0.24.5; rm -rf ~", "linux-amd64")
            .expect_err("malicious version must be rejected by the builder too");
        assert!(matches!(err, RemoteAddError::InvalidVersion { .. }));
    }

    #[test]
    fn build_install_command_url_unprefixed_version() {
        let (url, install_cmd) =
            build_install_command("https://example.test/releases", "0.24.5", "linux-amd64")
                .expect("valid version must build an install command");
        assert_eq!(
            url,
            "https://example.test/releases/v0.24.5/dot-agent-deck-linux-amd64"
        );
        assert!(install_cmd.contains(&url));
    }

    #[test]
    fn build_install_command_is_atomic() {
        // Regression: previously curl wrote directly to the final path, so an
        // interrupted download left a truncated binary at the install target
        // with no rollback. The command must download to a sibling temp file
        // and `mv` it into place — same-filesystem mv is POSIX-atomic.
        let (_, install_cmd) =
            build_install_command("https://example.test/releases", "0.24.5", "linux-amd64")
                .expect("valid version must build an install command");
        assert!(
            install_cmd.contains("mv "),
            "install command must `mv` the temp file into place: {install_cmd}"
        );
        assert!(
            install_cmd.contains(".dot-agent-deck."),
            "install command must download to a sibling temp file: {install_cmd}"
        );
        assert!(
            install_cmd.contains("$tmp"),
            "install command must reference the temp file by variable so chmod and mv target the temp: {install_cmd}"
        );
        // The chmod must happen on the temp file, not on the final path —
        // otherwise an interrupted download still leaves a chmod-ed final
        // path.
        assert!(
            install_cmd.contains("chmod 0755 \"$tmp\""),
            "chmod must target the temp file, not the final path: {install_cmd}"
        );
    }

    #[test]
    fn build_install_command_url_normalizes_v_prefixed_version() {
        // Regression: prior to normalization, `v0.24.5` produced a URL with
        // `vv0.24.5/` and 404'd on GitHub. The builder must strip the leading
        // `v` so exactly one `v` precedes the version segment.
        let (url_v, _) =
            build_install_command("https://example.test/releases", "v0.24.5", "linux-amd64")
                .expect("v-prefixed version must be accepted");
        let (url_plain, _) =
            build_install_command("https://example.test/releases", "0.24.5", "linux-amd64")
                .expect("plain version must be accepted");
        assert_eq!(
            url_v, url_plain,
            "URL must be the same regardless of leading-`v` input"
        );
        assert_eq!(
            url_v,
            "https://example.test/releases/v0.24.5/dot-agent-deck-linux-amd64"
        );
        assert!(
            !url_v.contains("vv"),
            "URL must contain exactly one `v` before the version: {url_v}"
        );
    }

    #[test]
    fn classify_ssh_error_host_key_verification_failed() {
        let target = SshTarget::parse("user@host", 22, None);
        let err = classify_ssh_error(&target, "Host key verification failed.\r\n");
        assert!(
            matches!(err, SshError::HostKeyVerificationFailed { .. }),
            "got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("user@host"),
            "Display should name target: {msg}"
        );
        assert!(
            msg.contains("first-time connection"),
            "Display should advise the user: {msg}"
        );
    }

    #[test]
    fn classify_ssh_error_host_key_changed_routes_to_same_variant() {
        let target = SshTarget::parse("h", 22, None);
        let stderr = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n@    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n";
        let err = classify_ssh_error(&target, stderr);
        assert!(matches!(err, SshError::HostKeyVerificationFailed { .. }));
    }

    #[test]
    fn classify_ssh_error_connection_refused_still_works() {
        let target = SshTarget::parse("h", 22, None);
        let err = classify_ssh_error(
            &target,
            "ssh: connect to host h port 22: Connection refused",
        );
        assert!(matches!(err, SshError::ConnectionRefused { .. }));
    }

    #[test]
    fn classify_ssh_error_auth_failed_still_works() {
        let target = SshTarget::parse("u@h", 22, None);
        let err = classify_ssh_error(&target, "u@h: Permission denied (publickey).");
        assert!(matches!(err, SshError::AuthFailed { .. }));
    }

    // -----------------------------------------------------------------
    // Issue #723 — a host that merely answers the ssh connection must not
    // be able to drive the terminal that reports the failure.
    // -----------------------------------------------------------------

    /// What a hostile remote plants on stderr: an ANSI erase-display, an OSC
    /// title-set terminated by BEL, and a RIGHT-TO-LEFT OVERRIDE. One fixture
    /// shared by every case, so each `detail`-carrying variant is pinned
    /// against exactly the same payload.
    const HOSTILE: &str = "\x1b[2J\x1b]0;pwned\x07\u{202e}";

    /// `HOSTILE` after [`scrub_remote_text`]: the bytes a terminal *acts on*
    /// are gone and their printable residue stays, because the policy at this
    /// seam is stripping rather than escaping.
    const HOSTILE_SCRUBBED: &str = "[2J]0;pwned";

    /// Fail on any character a terminal would interpret. `\n` is the one
    /// exemption: the messages deliberately put `Details:` on its own line,
    /// and multi-line ssh stderr is diagnostic output worth keeping.
    fn assert_terminal_safe(msg: &str) {
        for c in msg.chars() {
            assert!(
                c == '\n' || !(c.is_control() || crate::untrusted_text::is_bidi_format_char(c)),
                "U+{:04X} survived into a message printed to the terminal: {msg:?}",
                c as u32
            );
        }
    }

    #[test]
    fn scrub_remote_text_keeps_interior_newlines_and_trims_the_edges() {
        assert_eq!(
            scrub_remote_text("\x1b[2Jfirst\nsecond\x1b[0m\n"),
            "[2Jfirst\nsecond[0m",
            "the ESC bytes go; their printable residue and the interior newline stay"
        );
        // The trim runs AFTER the strip, so a trailing line made of nothing
        // but an escape sequence cannot leave a blank line behind — which is
        // what `stderr.trim()` alone used to do here.
        assert_eq!(scrub_remote_text("body\n\x1b\x07\u{202e}\n"), "body");
        // Ordinary multi-line stderr is untouched.
        assert_eq!(
            scrub_remote_text("line one\nline two"),
            "line one\nline two"
        );
    }

    /// One row of the variant table below: the ssh stderr phrase that routes
    /// to a variant, the matcher that recognises it, and whether that
    /// variant's message puts the detail behind its own `Details:` line.
    type DetailCase = (&'static str, fn(&SshError) -> bool, bool);

    #[test]
    fn classify_ssh_error_scrubs_control_bytes_from_every_detail_variant() {
        let target = SshTarget::parse("user@host", 22, None);
        // Every `SshError` variant that carries a `detail`: the ssh stderr
        // phrase that routes to it, the matcher, and whether its message puts
        // the detail behind a `Details:` line (`Other` inlines it instead).
        // The two variants that carry no `detail` — `HostKeyVerificationFailed`
        // and `Io` — give a remote nothing to steer, so there is nothing to
        // pin for them.
        let cases: [DetailCase; 3] = [
            (
                "ssh: connect to host h port 22: Connection refused",
                |e| matches!(e, SshError::ConnectionRefused { .. }),
                true,
            ),
            (
                "user@host: Permission denied (publickey).",
                |e| matches!(e, SshError::AuthFailed { .. }),
                true,
            ),
            (
                "kex_exchange_identification: read: Broken pipe",
                |e| matches!(e, SshError::Other { .. }),
                false,
            ),
        ];

        for (phrase, is_variant, uses_details_line) in cases {
            let stderr = format!("{HOSTILE}{phrase}\nsecond line{HOSTILE}\n");
            let err = classify_ssh_error(&target, &stderr);
            // Scrubbing must not disturb routing — the matcher reads the raw
            // stderr, the `detail` carries the scrubbed form.
            assert!(is_variant(&err), "routing changed for {phrase:?}: {err:?}");

            let msg = err.to_string();
            assert_terminal_safe(&msg);
            // Still a useful diagnostic: the remote's real words survive...
            assert!(msg.contains(phrase), "detail lost the real stderr: {msg:?}");
            // ...including the newline it wrote between its own two lines.
            assert!(
                msg.contains(&format!("{phrase}\nsecond line")),
                "an interior newline was dropped: {msg:?}"
            );
            if uses_details_line {
                assert!(
                    msg.contains("\nDetails: "),
                    "`Details:` lost its own line: {msg:?}"
                );
            }
            // Stripped, not escaped: the printable residue is there, and no
            // `\u{1b}`-style escape appears — that is `remote doctor`'s policy,
            // not this seam's.
            assert!(
                msg.contains(HOSTILE_SCRUBBED),
                "expected the stripped residue in {msg:?}"
            );
            assert!(
                !msg.contains("\\u{1b}"),
                "this seam strips rather than escapes: {msg:?}"
            );
        }
    }

    /// [`SshExecutor`] double that hands back canned outputs in call order.
    /// Every error path under test is reached by the Nth ssh call returning a
    /// particular status, so a queue is the whole fixture — no commands are
    /// recorded because none of these assertions look at them.
    struct ScriptedSsh(std::cell::RefCell<std::collections::VecDeque<SshOutput>>);

    impl ScriptedSsh {
        fn new(outputs: impl IntoIterator<Item = SshOutput>) -> Self {
            Self(std::cell::RefCell::new(outputs.into_iter().collect()))
        }
    }

    impl SshExecutor for ScriptedSsh {
        fn run(&self, _target: &SshTarget, _command: &str) -> Result<SshOutput, SshError> {
            Ok(self
                .0
                .borrow_mut()
                .pop_front()
                .expect("the script ran out of canned ssh outputs"))
        }
    }

    /// A successful ssh call with the given stdout.
    fn ssh_ok(stdout: &str) -> SshOutput {
        SshOutput {
            status: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    /// A failing ssh call whose stderr wraps `body` in [`HOSTILE`] on both
    /// sides and spans two lines, so one fixture exercises stripping and
    /// newline preservation together.
    fn ssh_hostile_failure(status: i32, body: &str) -> SshOutput {
        SshOutput {
            status,
            stdout: String::new(),
            stderr: format!("{HOSTILE}{body}\nsecond line{HOSTILE}\n"),
        }
    }

    #[test]
    fn download_failure_scrubs_control_bytes_from_the_remote_stderr() {
        // The second site that reads a remote's stderr into an error field,
        // and the one `remote upgrade` reaches too — both go through
        // `install_and_verify`.
        let executor = ScriptedSsh::new([ssh_hostile_failure(7, "curl: (22) 404 Not Found")]);
        let target = SshTarget::parse("user@host", 22, None);
        let err = install_and_verify(
            &executor,
            &target,
            "linux-amd64",
            "0.24.5",
            "https://example.test/releases/download",
            false,
        )
        .expect_err("a non-zero install exit must fail");
        assert!(
            matches!(err, RemoteAddError::DownloadFailed { .. }),
            "got {err:?}"
        );

        let msg = err.to_string();
        assert_terminal_safe(&msg);
        assert!(msg.contains("exit 7: "), "exit status lost: {msg:?}");
        assert!(
            msg.contains("curl: (22) 404 Not Found\nsecond line"),
            "the remote's real stderr and its interior newline must survive: {msg:?}"
        );
        assert!(
            msg.contains(HOSTILE_SCRUBBED),
            "expected the stripped residue in {msg:?}"
        );
    }

    /// A check that read no version after the binary moved — it exited
    /// non-zero — reports an unverified build rather than a version, on both
    /// the download path and after a `brew upgrade`. Under `--no-install`, and
    /// on `remote add`'s Homebrew path, nothing was replaced, so the plain
    /// mismatch stands and no version is claimed.
    #[test]
    fn a_replaced_binary_that_reads_no_version_is_an_unverified_build() {
        let target = SshTarget::parse("user@host", 22, None);
        let version_fails = || SshOutput {
            status: 126,
            stdout: String::new(),
            stderr: "cannot execute binary file".to_string(),
        };

        let executor = ScriptedSsh::new([ssh_ok(""), version_fails()]);
        let err = install_and_verify(
            &executor,
            &target,
            "linux-amd64",
            "0.24.5",
            "https://example.test/releases/download",
            false,
        )
        .expect_err("a failed version check must fail");
        assert!(
            matches!(
                &err,
                RemoteAddError::ReplacedButUnverified { binary, on_disk, .. }
                    if binary == REMOTE_INSTALL_PATH && on_disk == UNVERIFIED_BUILD
            ),
            "{err:?}"
        );
        let upgrade_err = RemoteUpgradeError::Inner(err);
        assert_eq!(upgrade_err.installed_version(), Some(UNVERIFIED_BUILD));
        let msg = upgrade_err.to_string();
        assert!(msg.contains("cannot execute binary file"), "{msg}");
        assert!(msg.contains("an unverified build"), "{msg}");

        let brew_probe = || ssh_ok("local-bin=\nhomebrew=/opt/homebrew\nformula=dot-agent-deck\n");
        let executor = ScriptedSsh::new([brew_probe(), ssh_ok(""), version_fails()]);
        let err = install_or_upgrade(
            &executor,
            &target,
            "mac",
            "darwin-arm64",
            "0.24.5",
            "https://example.test/releases/download",
            false,
            true,
            &mut Vec::new(),
        )
        .err()
        .expect("a failed version check after brew upgrade must fail");
        assert!(
            matches!(
                &err,
                RemoteAddError::ReplacedButUnverified { binary, on_disk, .. }
                    if binary == "/opt/homebrew/bin/dot-agent-deck" && on_disk == UNVERIFIED_BUILD
            ),
            "{err:?}"
        );

        let executor = ScriptedSsh::new([brew_probe(), version_fails()]);
        let err = install_or_upgrade(
            &executor,
            &target,
            "mac",
            "darwin-arm64",
            "0.24.5",
            "https://example.test/releases/download",
            false,
            false,
            &mut Vec::new(),
        )
        .err()
        .expect("a failed version check must fail");
        assert!(
            matches!(err, RemoteAddError::VersionMismatch { .. }),
            "{err:?}"
        );

        let executor = ScriptedSsh::new([version_fails()]);
        let err = install_and_verify(
            &executor,
            &target,
            "linux-amd64",
            "0.24.5",
            "https://example.test/releases/download",
            true,
        )
        .expect_err("a failed --no-install check must fail");
        assert!(
            matches!(err, RemoteAddError::VersionMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(RemoteUpgradeError::Inner(err).installed_version(), None);
    }

    /// Issue #1350: `remote add` refuses an unsafe address with the rules the
    /// shared deck list enforces, before any ssh call — an empty script panics
    /// on the first one — and writes nothing.
    #[test]
    fn remote_add_refuses_an_unsafe_address_before_any_ssh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("remotes.toml");
        let base = AddOptions {
            name: "prod".to_string(),
            remote_type: "ssh".to_string(),
            target: "user@host".to_string(),
            port: 22,
            key: None,
            version: "0.24.5".to_string(),
            no_install: true,
            release_base: "https://example.test/releases/download".to_string(),
        };
        let cases = [
            AddOptions {
                target: "-oProxyCommand=touch /tmp/x".to_string(),
                ..base.clone()
            },
            AddOptions {
                target: "user@host;id".to_string(),
                ..base.clone()
            },
            AddOptions {
                target: "-l@host".to_string(),
                ..base.clone()
            },
            AddOptions {
                port: 0,
                ..base.clone()
            },
            AddOptions {
                key: Some(PathBuf::from("-oProxyCommand=x")),
                ..base.clone()
            },
        ];
        for opts in cases {
            let executor = ScriptedSsh::new([]);
            match add(&opts, &executor, &path) {
                Err(RemoteAddError::InvalidAddress(_)) => {}
                other => panic!("{:?}: expected InvalidAddress, got {other:?}", opts.target),
            }
            assert!(!path.exists(), "a refused add wrote the registry");
        }
    }

    #[test]
    fn remote_add_scrubs_control_bytes_from_every_remote_derived_error() {
        // `remote add` reads the remote's own bytes at four points beyond the
        // ssh-transport classifier — the arch probe's stderr, the arch probe's
        // *stdout* when it does not parse, the version the remote binary
        // reports, and the hook install's stderr. All four land in a
        // `RemoteAddError` printed by the same `eprintln!("{e}")`, and a host
        // that merely answers the connection reaches the first of them
        // *before* any ssh error is classified, so leaving them raw would
        // leave the sharpest case open.
        let dir = tempfile::tempdir().expect("tempdir");
        // Deliberately a path that does not exist: `RemotesFile::load` treats
        // NotFound as an empty registry, and every case below fails before
        // anything is saved, so no file is ever written.
        let path = dir.path().join("remotes.toml");
        let opts = AddOptions {
            name: "prod".to_string(),
            remote_type: "ssh".to_string(),
            target: "user@host".to_string(),
            port: 22,
            key: None,
            version: "0.24.5".to_string(),
            // Skips the download step so the version check is reached with a
            // two-call script; the download path has its own test above.
            no_install: true,
            release_base: "https://example.test/releases/download".to_string(),
        };

        // (label, ssh script, variant matcher, a substring of the remote's
        // real words that must survive with its newline)
        #[allow(clippy::type_complexity)]
        let cases: [(&str, Vec<SshOutput>, fn(&RemoteAddError) -> bool, &str); 4] = [
            (
                "uname failed",
                vec![ssh_hostile_failure(127, "uname: command not found")],
                |e| matches!(e, RemoteAddError::UnameFailed { .. }),
                "uname: command not found\nsecond line",
            ),
            (
                "uname output does not parse",
                // `detect_platform` still reads the RAW stdout — only the
                // error field is scrubbed — so this must fail to parse on the
                // strength of `Plan9 pdp11`, not because of the payload.
                vec![ssh_ok(&format!("{HOSTILE}Plan9 pdp11\n"))],
                |e| matches!(e, RemoteAddError::UnsupportedArch { .. }),
                "Plan9 pdp11",
            ),
            (
                "remote reports a different version",
                vec![
                    ssh_ok("Linux x86_64\n"),
                    ssh_ok("local-bin=present\nhomebrew=\nformula=\n"),
                    ssh_ok(&format!("dot-agent-deck {HOSTILE}9.9.9\n")),
                ],
                |e| matches!(e, RemoteAddError::VersionMismatch { .. }),
                "9.9.9",
            ),
            (
                "hook install failed",
                vec![
                    ssh_ok("Linux x86_64\n"),
                    ssh_ok("local-bin=present\nhomebrew=\nformula=\n"),
                    ssh_ok("dot-agent-deck 0.24.5\n"),
                    ssh_hostile_failure(3, "hooks: settings.json is not writable"),
                ],
                |e| matches!(e, RemoteAddError::HooksInstallFailed { .. }),
                "hooks: settings.json is not writable\nsecond line",
            ),
        ];

        for (label, script, is_variant, must_survive) in cases {
            let executor = ScriptedSsh::new(script);
            let err = match add(&opts, &executor, &path) {
                Err(e) => e,
                Ok(_) => panic!("{label}: this script must fail"),
            };
            assert!(is_variant(&err), "{label}: wrong variant {err:?}");

            let msg = err.to_string();
            assert_terminal_safe(&msg);
            assert!(
                msg.contains(must_survive),
                "{label}: the remote's real words were lost: {msg:?}"
            );
            assert!(
                msg.contains(HOSTILE_SCRUBBED),
                "{label}: expected the stripped residue in {msg:?}"
            );
        }
    }

    /// Scenario: a child that starts a quiet descendant inheriting its
    /// streams, prints one line and exits. The plain runner does not own the
    /// descendant, so it does not kill it — but it stops waiting for the
    /// streams shortly after the child exits instead of until the descendant
    /// does, and keeps the line (PRD #1487 re-check R1).
    #[cfg(unix)]
    #[test]
    fn run_local_bounded_does_not_wait_on_a_descendant_after_the_child_exits() {
        let started = std::time::Instant::now();
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "sleep 600 & echo \"pid=$!\""]);
        let capture = run_local_bounded(&mut cmd, 60, 4096).unwrap();
        let elapsed = started.elapsed();
        let stdout = String::from_utf8(capture.stdout).unwrap();
        if let Some(pid) = stdout
            .trim()
            .strip_prefix("pid=")
            .and_then(|p| p.parse::<libc::pid_t>().ok())
        {
            // SAFETY: plain signal to the pid this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "the call waited on the descendant: {elapsed:?}"
        );
        assert!(capture.status.is_some_and(|s| s.success()));
        assert!(!capture.timed_out && !capture.truncated);
        assert!(stdout.starts_with("pid="), "the line was lost: {stdout:?}");
    }

    /// Run `script` under `/bin/sh` through the group-owning runner with the
    /// leader seam recording, and return the capture, what the runner did to
    /// the leader, and the pid the script printed as `pid=<n>`, if any.
    #[cfg(unix)]
    fn run_owning_group_recorded(
        script: &str,
        secs: u64,
    ) -> (LocalCapture, Vec<leader_seam::Event>, Option<libc::pid_t>) {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        let (capture, events) =
            leader_seam::capture(|| run_local_bounded_owning_group(&mut cmd, secs, 4096));
        let capture = capture.unwrap();
        let pid = String::from_utf8_lossy(&capture.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("pid=")?.trim().parse().ok());
        (capture, events, pid)
    }

    /// The order every group-owning run that signals must keep: one group
    /// signal, sent while the kernel still held the child, then the one reap.
    #[cfg(unix)]
    fn assert_signalled_before_reaping(events: &[leader_seam::Event], label: &str) {
        use leader_seam::Event;
        assert_eq!(
            events,
            [
                Event::GroupSignal {
                    leader_unreaped: true
                },
                Event::Reap
            ],
            "{label}: the group must be signalled while the leader is unreaped, then reaped once"
        );
    }

    /// Scenario: the escaped-writer interleaving from PRD #1487's final audit
    /// (F1) — the child starts a descendant that leaves its process group with
    /// `setsid` while keeping stdout open, prints a line and exits. The call
    /// waits out the stream grace and signals the group; that signal must go
    /// out while the exited child is still an unreaped zombie holding the
    /// group id, and the child is reaped only afterwards.
    #[cfg(target_os = "linux")]
    #[test]
    fn owning_group_signals_an_escapees_old_group_only_before_reaping_its_leader() {
        if Command::new("setsid")
            .arg("true")
            .status()
            .map_or(true, |s| !s.success())
        {
            eprintln!("SKIP: no `setsid` on this host");
            return;
        }
        let (capture, events, pid) =
            run_owning_group_recorded("setsid sleep 600 &\necho \"pid=$!\"\nexit 0", 60);
        if let Some(pid) = pid {
            // Out of reach of the group kill, so this test cleans it up.
            // SAFETY: plain signal to the pid this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(pid.is_some(), "the line was lost: {:?}", capture.stdout);
        assert!(capture.status.is_some_and(|s| s.success()));
        assert!(!capture.timed_out);
        assert_signalled_before_reaping(&events, "escapee");
    }

    /// Scenario: a descendant that stays in the group holds the streams after
    /// the child exits, and, separately, a child that never exits. Both end
    /// in a group signal; in both the signal precedes the only reap and finds
    /// the child still unreaped.
    #[cfg(unix)]
    #[test]
    fn owning_group_never_signals_after_reaping_its_leader() {
        let (capture, events, pid) =
            run_owning_group_recorded("sleep 600 &\necho \"pid=$!\"\nexit 0", 60);
        if let Some(pid) = pid {
            // SAFETY: plain signal to the pid this test started; it is
            // already dead if the group kill reached it.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(capture.status.is_some_and(|s| s.success()));
        assert_signalled_before_reaping(&events, "held after exit");

        let (capture, events, _) = run_owning_group_recorded("exec sleep 600", 1);
        assert!(capture.timed_out && capture.status.is_none());
        assert_signalled_before_reaping(&events, "deadline");
    }

    /// Scenario: a child that prints a line and exits with nothing left
    /// holding its streams. Nothing could still be running, so the group is
    /// not signalled at all; the child is reaped once and its status kept.
    #[cfg(unix)]
    #[test]
    fn owning_group_reaps_a_clean_exit_without_signalling() {
        let (capture, events, _) = run_owning_group_recorded("echo pid=0; exit 3", 60);
        assert_eq!(capture.status.and_then(|s| s.code()), Some(3));
        assert_eq!(events, [leader_seam::Event::Reap]);
    }

    /// Scenario: the defence-in-depth check on a group id. Zero (the caller's
    /// own group to `killpg`), one (init's), a negative number, and the
    /// caller's own group are refused; an ordinary other id is allowed.
    #[cfg(unix)]
    #[test]
    fn group_signal_refuses_ids_that_are_never_this_calls() {
        // SAFETY: getpgrp takes no arguments and cannot fail.
        let own = unsafe { libc::getpgrp() };
        for refused in [0, 1, -1, -own, own] {
            assert!(!group_is_signalable(refused), "{refused} must be refused");
        }
        let other = if own == i32::MAX { own - 1 } else { own + 1 };
        assert!(group_is_signalable(other.max(2)));
    }
}

/// Issue #1372: `remote add` / `remote upgrade` against a remote whose deck is
/// installed by Homebrew.
///
/// The "remote" is this machine's own `/bin/sh`, run with a sandbox `HOME` and
/// `PATH`, so every command the flows send is **executed** rather than
/// string-matched. Stand-ins, named for what they stand in for:
///
/// - `brew` is a script that answers `--prefix`, `list --formula
///   dot-agent-deck` and `upgrade dot-agent-deck` the way Homebrew does,
///   swapping a `Cellar/dot-agent-deck/<version>` keg in behind
///   `<prefix>/bin/dot-agent-deck`, which is a symlink into the Cellar as a
///   real brew install is.
/// - `curl` is a script that writes a deck stand-in to its `-o` path, so the
///   `~/.local/bin` installer "succeeds" exactly as it does against a real
///   release — which is what made the second copy invisible.
/// - each deck stand-in answers `--version` and records `hooks install` along
///   with the path it was invoked as.
///
/// None of this reaches a real remote, a real `ssh` or a real Homebrew; what
/// it pins is the remote-side behaviour of the commands themselves.
#[cfg(all(test, unix))]
mod homebrew_remote_tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// Where the remote's `brew` can be found.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum BrewAt {
        /// On the `PATH` of the non-interactive shell.
        OnPath,
        /// Only at its prefix — the usual macOS case, where Homebrew puts
        /// itself on `PATH` from a login profile a non-interactive ssh command
        /// never reads.
        PrefixOnly,
    }

    /// Runs each remote command under `/bin/sh -c` in the sandbox, and records
    /// it. Nothing inherits from the test's own environment.
    ///
    /// `rewrites` points each of the production [`HOMEBREW_PREFIXES`] at a
    /// sandbox path, so neither a Homebrew on the machine running the tests
    /// nor its absence can change an answer.
    struct SandboxShell {
        home: PathBuf,
        path: String,
        rewrites: Vec<(String, String)>,
        commands: std::cell::RefCell<Vec<String>>,
    }

    impl SshExecutor for SandboxShell {
        fn run(&self, _target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
            let mut command = command.to_string();
            if command.contains("list --formula") {
                for (from, to) in &self.rewrites {
                    assert!(
                        command.contains(from.as_str()),
                        "the install probe no longer names {from}: {command}"
                    );
                    command = command.replace(from.as_str(), to);
                }
            }
            self.commands.borrow_mut().push(command.clone());
            let out = Command::new("/bin/sh")
                .arg("-c")
                .arg(&command)
                .env_clear()
                .env("HOME", &self.home)
                .env("PATH", &self.path)
                .current_dir(&self.home)
                .output()
                .expect("/bin/sh must run");
            Ok(SshOutput {
                status: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    }

    fn write_script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::test_isolation::write_script(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A deck stand-in that reports `version` and logs `hooks install` as
    /// `hooks <the path it was invoked as> PATH=<its PATH>`.
    fn deck_script(version: &str, log: &Path) -> String {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo \"dot-agent-deck {version}\" ;;\nhooks) echo \"hooks $0 PATH=$PATH\" >> '{log}' ;;\nesac\n",
            log = log.display()
        )
    }

    /// What the remote has installed before the flow under test runs.
    struct Fixture {
        /// Version Homebrew has installed, if any.
        brew: Option<&'static str>,
        /// Version of a copy at `~/.local/bin`, if any.
        local_bin: Option<&'static str>,
        /// What `brew upgrade` lands, and what the release download writes.
        tap: &'static str,
        /// `brew upgrade` exits non-zero without changing anything.
        brew_upgrade_fails: bool,
    }

    struct Remote {
        _dir: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        brew_prefix: PathBuf,
        log: PathBuf,
        registry: PathBuf,
    }

    impl Remote {
        fn new(fixture: Fixture) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().canonicalize().unwrap();
            let home = root.join("home");
            let brew_prefix = root.join("brew");
            let log = root.join("remote.log");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(&log, "").unwrap();

            write_script(
                &root.join("stubs/curl"),
                &format!(
                    "#!/bin/sh\necho \"curl $*\" >> '{log}'\nwhile [ $# -gt 0 ]; do\nif [ \"$1\" = -o ]; then out=\"$2\"; fi\nshift\ndone\ncat > \"$out\" <<'EOF'\n{deck}EOF\n",
                    log = log.display(),
                    deck = deck_script(fixture.tap, &log),
                ),
            );

            if let Some(v) = fixture.brew {
                let keg = |v: &str| {
                    brew_prefix.join(format!("Cellar/dot-agent-deck/{v}/bin/dot-agent-deck"))
                };
                write_script(&keg(v), &deck_script(v, &log));
                std::fs::create_dir_all(brew_prefix.join("bin")).unwrap();
                symlink(keg(v), brew_prefix.join("bin/dot-agent-deck")).unwrap();
                let new_keg = keg(fixture.tap);
                let upgrade = if fixture.brew_upgrade_fails {
                    "echo 'Error: dot-agent-deck is pinned' >&2\nexit 1".to_string()
                } else {
                    format!(
                        "mkdir -p '{new_dir}'\ncat > '{new_keg}' <<'EOF'\n{deck}EOF\nchmod 0755 '{new_keg}'\nln -sfn '{new_keg}' \"$prefix/bin/dot-agent-deck\"",
                        new_dir = new_keg.parent().unwrap().display(),
                        new_keg = new_keg.display(),
                        deck = deck_script(fixture.tap, &log),
                    )
                };
                write_script(
                    &brew_prefix.join("bin/brew"),
                    &format!(
                        "#!/bin/sh\nprefix='{prefix}'\ncase \"$1\" in\n--prefix) if [ \"$2\" = dot-agent-deck ]; then printf '%s\\n' \"$prefix/Cellar/dot-agent-deck/{version}\"; else printf '%s\\n' \"$prefix\"; fi ;;\nlist) [ \"$2\" = --formula ] && [ \"$3\" = dot-agent-deck ] && [ -d \"$prefix/Cellar/dot-agent-deck\" ] ;;\nupgrade)\necho \"brew $*\" >> '{log}'\n{upgrade} ;;\n*) exit 1 ;;\nesac\n",
                        prefix = brew_prefix.display(),
                        version = v,
                        log = log.display(),
                    ),
                );
            }
            if let Some(v) = fixture.local_bin {
                write_script(
                    &home.join(".local/bin/dot-agent-deck"),
                    &deck_script(v, &log),
                );
            }

            Self {
                _dir: dir,
                registry: root.join("remotes.toml"),
                root,
                home,
                brew_prefix,
                log,
            }
        }

        fn shell(&self, brew_at: BrewAt) -> SandboxShell {
            let mut path = format!("{}:/usr/bin:/bin", self.root.join("stubs").display());
            if brew_at == BrewAt::OnPath {
                path = format!("{}:{path}", self.brew_prefix.join("bin").display());
            }
            let rewrites = HOMEBREW_PREFIXES
                .iter()
                .enumerate()
                .map(|(i, prefix)| {
                    let to = if brew_at == BrewAt::PrefixOnly && i == 0 {
                        self.brew_prefix.join("bin/brew")
                    } else {
                        self.root.join(format!("absent-{i}/bin/brew"))
                    };
                    (format!("{prefix}/bin/brew"), to.display().to_string())
                })
                .collect();
            SandboxShell {
                home: self.home.clone(),
                path,
                rewrites,
                commands: Default::default(),
            }
        }

        fn log(&self) -> String {
            std::fs::read_to_string(&self.log).unwrap()
        }

        fn local_bin_copy(&self) -> PathBuf {
            self.home.join(".local/bin/dot-agent-deck")
        }

        fn brew_binary(&self) -> PathBuf {
            self.brew_prefix.join("bin/dot-agent-deck")
        }

        fn version_of(&self, binary: &Path) -> String {
            let out = Command::new(binary).arg("--version").output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        /// Register the remote as an entry written before #1372: no install
        /// method recorded.
        fn register_legacy_entry(&self, version: &str) {
            RemotesFile {
                remotes: vec![RemoteEntry {
                    name: "mac".to_string(),
                    kind: "ssh".to_string(),
                    host: "user@mac".to_string(),
                    port: 22,
                    key: None,
                    version: version.to_string(),
                    added_at: "2026-01-01T00:00:00Z".to_string(),
                    upgraded_at: None,
                    last_connected: None,
                    install: None,
                    binary: None,
                    id: None,
                    user: None,
                    jump_host: None,
                    socket: None,
                }],
            }
            .save(&self.registry)
            .unwrap();
        }

        fn entry(&self) -> RemoteEntry {
            RemotesFile::load(&self.registry).unwrap().remotes[0].clone()
        }

        fn upgrade(
            &self,
            brew_at: BrewAt,
            version: &str,
            no_install: bool,
        ) -> (Result<RemoteEntry, RemoteUpgradeError>, String) {
            let opts = UpgradeOptions {
                name: "mac".to_string(),
                version: version.to_string(),
                no_install,
                release_base: "https://example.test/releases/download".to_string(),
            };
            let mut out = Vec::new();
            let result =
                upgrade_reporting_to(&opts, &self.shell(brew_at), &self.registry, &mut out);
            (result, String::from_utf8(out).unwrap())
        }
    }

    #[test]
    fn beta_link_wins_over_installed_but_unlinked_stable_formula() {
        let remote = Remote::new(Fixture {
            brew: None,
            local_bin: None,
            tap: "0.44.0-rc.1",
            brew_upgrade_fails: false,
        });
        let stable_keg = remote
            .brew_prefix
            .join("Cellar/dot-agent-deck/0.44.0/bin/dot-agent-deck");
        let beta_keg = remote
            .brew_prefix
            .join("Cellar/dot-agent-deck-beta/0.44.0-rc.1/bin/dot-agent-deck");
        write_script(&stable_keg, &deck_script("0.44.0", &remote.log));
        write_script(&beta_keg, &deck_script("0.44.0-rc.1", &remote.log));
        std::fs::create_dir_all(remote.brew_prefix.join("bin")).unwrap();
        symlink(&beta_keg, remote.brew_binary()).unwrap();
        write_script(
            &remote.brew_prefix.join("bin/brew"),
            &format!(
                "#!/bin/sh\ncase \"$1\" in\n--prefix) case \"$2\" in\ndot-agent-deck) echo '{stable}' ;;\ndot-agent-deck-beta) echo '{beta}' ;;\n*) echo '{prefix}' ;;\nesac ;;\nlist) [ \"$2\" = --formula ] && case \"$3\" in dot-agent-deck|dot-agent-deck-beta) true ;; *) false ;; esac ;;\nupgrade) echo \"brew $*\" >> '{log}' ;;\n*) exit 1 ;;\nesac\n",
                prefix = remote.brew_prefix.display(),
                stable = stable_keg.parent().unwrap().parent().unwrap().display(),
                beta = beta_keg.parent().unwrap().parent().unwrap().display(),
                log = remote.log.display(),
            ),
        );
        let shell = remote.shell(BrewAt::OnPath);
        let target = SshTarget::parse("user@mac", 22, None);
        assert_eq!(
            discover_homebrew_binary(&shell, &target)
                .unwrap()
                .unwrap()
                .as_str(),
            remote.brew_binary().to_str().unwrap()
        );
        assert_eq!(
            detect_install(&shell, &target).unwrap().homebrew_formula,
            Some("dot-agent-deck-beta")
        );
        remote.register_legacy_entry("0.44.0-rc.1");
        let (upgraded, _) = remote.upgrade(BrewAt::OnPath, "0.44.0-rc.1", false);
        upgraded.expect("upgrade keeps the beta formula");
        assert!(
            remote.log().contains("brew upgrade dot-agent-deck-beta"),
            "the beta formula, not stable, owns this install"
        );
        assert!(!remote.local_bin_copy().exists());
        assert_eq!(
            remote.entry().remote_binary(),
            remote.brew_binary().to_str().unwrap()
        );
    }

    /// The issue's report: a brew-installed remote, upgraded with `remote
    /// upgrade`, must be upgraded by brew and must not gain a second copy in
    /// `~/.local/bin` — whether or not `brew` is on the non-interactive
    /// `PATH`. The entry, written before #1372, then records the Homebrew
    /// binary, which is what `connect` and `remote doctor` run.
    #[test]
    fn upgrade_on_a_homebrew_remote_goes_through_brew_and_writes_no_second_copy() {
        for brew_at in [BrewAt::OnPath, BrewAt::PrefixOnly] {
            let remote = Remote::new(Fixture {
                brew: Some("0.40.0"),
                local_bin: None,
                tap: "0.43.0",
                brew_upgrade_fails: false,
            });
            remote.register_legacy_entry("0.40.0");

            let (result, out) = remote.upgrade(brew_at, "0.43.0", false);
            let log = remote.log();

            assert!(
                !remote.local_bin_copy().exists(),
                "{brew_at:?}: `remote upgrade` wrote a second copy to ~/.local/bin on a brew \
                 remote (result: {result:?}, remote log:\n{log})"
            );
            assert!(
                !log.contains("curl "),
                "{brew_at:?}: nothing may be downloaded: {log}"
            );
            assert!(
                log.contains("brew upgrade dot-agent-deck"),
                "{brew_at:?}: the upgrade must go through brew: {log}"
            );
            assert_eq!(
                remote.version_of(&remote.brew_binary()),
                "dot-agent-deck 0.43.0",
                "{brew_at:?}: the brew copy must be the upgraded one"
            );
            assert!(
                log.contains(&format!("hooks {}", remote.brew_binary().display())),
                "{brew_at:?}: hooks must be installed by the brew binary: {log}"
            );
            // The hook installer pins the binary it runs only when that
            // binary's directory is on `PATH` (`durable_binary_path`), and a
            // non-interactive ssh `PATH` usually lacks Homebrew's `bin`.
            assert!(
                log.contains(&format!(
                    "hooks {} PATH={}:",
                    remote.brew_binary().display(),
                    remote.brew_prefix.join("bin").display()
                )),
                "{brew_at:?}: hooks install must run with Homebrew's bin first on PATH: {log}"
            );
            result.unwrap_or_else(|e| panic!("{brew_at:?}: upgrade must succeed: {e}"));
            assert!(
                out.contains("Upgraded remote 'mac' to version 0.43.0."),
                "{brew_at:?}: {out}"
            );

            let entry = remote.entry();
            assert_eq!(entry.version, "0.43.0");
            assert_eq!(entry.install.as_deref(), Some(INSTALL_HOMEBREW));
            assert_eq!(
                entry.remote_binary(),
                remote.brew_binary().display().to_string(),
                "{brew_at:?}: `connect` must run the Homebrew binary from now on"
            );
        }
    }

    /// PRD #1487 review: the release lands and then `hooks install` fails. The
    /// binary is not rolled back, so the error says the new version is
    /// installed and which later step failed — never that nothing changed —
    /// and the deck list still records the version it had.
    #[test]
    fn a_failure_after_the_binary_landed_names_the_installed_version_and_the_step() {
        let remote = Remote::new(Fixture {
            brew: None,
            local_bin: Some("0.40.0"),
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");
        // The release that lands reports its version, but its hook install fails.
        let landed = "#!/bin/sh\ncase \"$1\" in\n--version) echo \"dot-agent-deck 0.43.0\" ;;\nhooks) echo 'hooks: settings.json is not writable' >&2; exit 3 ;;\nesac\n";
        write_script(
            &remote.root.join("stubs/curl"),
            &format!(
                "#!/bin/sh\nwhile [ $# -gt 0 ]; do\nif [ \"$1\" = -o ]; then out=\"$2\"; fi\nshift\ndone\ncat > \"$out\" <<'EOF'\n{landed}EOF\n"
            ),
        );

        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.43.0", false);
        let err = result.expect_err("a failed hook install must fail the upgrade");
        assert_eq!(err.installed_version(), Some("0.43.0"));
        assert!(
            matches!(
                &err,
                RemoteUpgradeError::AfterInstall { step: "reinstalling the hooks", source, .. }
                    if matches!(**source, RemoteUpgradeError::Inner(RemoteAddError::HooksInstallFailed { .. }))
            ),
            "{err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.starts_with("0.43.0 was installed, but reinstalling the hooks failed:"),
            "{msg}"
        );
        assert!(msg.contains("settings.json is not writable"), "{msg}");
        assert_eq!(
            remote.version_of(&remote.local_bin_copy()),
            "dot-agent-deck 0.43.0"
        );
        assert_eq!(remote.entry().version, "0.40.0");

        // A failure before anything landed carries no installed version.
        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.44.0", true);
        assert_eq!(
            result.expect_err("version mismatch").installed_version(),
            None
        );
    }

    /// Control: a remote with no Homebrew install keeps today's behaviour —
    /// the release binary lands in `~/.local/bin` — and says so in the entry.
    #[test]
    fn upgrade_on_a_local_bin_remote_still_installs_to_local_bin() {
        let remote = Remote::new(Fixture {
            brew: None,
            local_bin: Some("0.40.0"),
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");

        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.43.0", false);
        let entry = result.expect("upgrade must succeed");
        let log = remote.log();

        assert_eq!(entry.version, "0.43.0");
        assert_eq!(
            remote.version_of(&remote.local_bin_copy()),
            "dot-agent-deck 0.43.0"
        );
        assert!(
            log.contains("curl "),
            "the release must be downloaded: {log}"
        );
        assert!(!log.contains("brew "), "no brew on this remote: {log}");
        assert!(
            log.contains(&format!("hooks {}", remote.local_bin_copy().display())),
            "hooks must be installed by the ~/.local/bin binary: {log}"
        );
        assert_eq!(entry.install.as_deref(), Some(INSTALL_LOCAL_BIN));
        assert_eq!(entry.binary, None);
        assert_eq!(entry.remote_binary(), REMOTE_INSTALL_PATH);
    }

    /// Scenario: an upgrade starts against the deck-list row it read for
    /// `mac` (`user@mac`), and while it runs that row is removed and re-added
    /// for another machine (`user@elsewhere`). Every ssh command still goes to
    /// the machine the upgrade started on, and the result is refused rather
    /// than recorded over the moved row — the error says so in plain words and
    /// that the build was installed (PRD #1487, Greptile 4208066970).
    #[test]
    fn an_upgrade_whose_row_moves_to_another_machine_mid_run_installs_on_the_first_and_records_nothing()
     {
        struct MovesTheRow<'a> {
            inner: SandboxShell,
            targets: std::cell::RefCell<Vec<SshTarget>>,
            registry: &'a Path,
        }
        impl SshExecutor for MovesTheRow<'_> {
            fn run(&self, target: &SshTarget, command: &str) -> Result<SshOutput, SshError> {
                if self.targets.borrow().is_empty() {
                    // Another client removes `mac` and adds it back for a
                    // different machine, while this upgrade is under way.
                    let mut file = RemotesFile::load(self.registry).unwrap();
                    file.remotes[0].host = "user@elsewhere".to_string();
                    file.save(self.registry).unwrap();
                }
                self.targets.borrow_mut().push(target.clone());
                self.inner.run(target, command)
            }
        }

        let remote = Remote::new(Fixture {
            brew: None,
            local_bin: Some("0.40.0"),
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");
        let pinned = remote.entry();
        let executor = MovesTheRow {
            inner: remote.shell(BrewAt::PrefixOnly),
            targets: Default::default(),
            registry: &remote.registry,
        };
        let opts = UpgradeOptions {
            name: "mac".to_string(),
            version: "0.43.0".to_string(),
            no_install: false,
            release_base: "https://example.test/releases/download".to_string(),
        };
        let mut out = Vec::new();
        let error =
            upgrade_entry_reporting_to(&opts, &pinned, &executor, &remote.registry, &mut out)
                .expect_err("a row moved to another machine must not be recorded over");

        let targets = executor.targets.borrow();
        assert!(!targets.is_empty());
        assert!(
            targets.iter().all(|t| *t == pinned.ssh_target()),
            "every command must reach the machine the upgrade started on: {targets:?}"
        );
        assert!(
            matches!(
                &error,
                RemoteUpgradeError::AfterInstall { source, .. }
                    if matches!(**source, RemoteUpgradeError::RouteChanged { .. })
            ),
            "got {error:?}"
        );
        assert_eq!(error.installed_version(), Some("0.43.0"));
        let message = error.to_string();
        assert!(
            message.contains("was changed to reach a different machine"),
            "{message}"
        );
        let row = remote.entry();
        assert_eq!(row.host, "user@elsewhere", "the re-added row is kept");
        assert_eq!(row.version, "0.40.0", "nothing recorded over it");
        assert_eq!(row.upgraded_at, None);
    }

    /// A row whose route is unchanged is the same machine: [`RemoteEntry::same_route`]
    /// ignores what an upgrade itself records, and notices every address field.
    #[test]
    fn same_route_compares_the_address_and_socket_only() {
        let remote = Remote::new(Fixture {
            brew: None,
            local_bin: None,
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");
        let base = remote.entry();
        let mut recorded = base.clone();
        recorded.version = "0.43.0".into();
        recorded.upgraded_at = Some("2026-10-07T00:00:00Z".into());
        recorded.install = Some(INSTALL_LOCAL_BIN.into());
        assert!(base.same_route(&recorded));
        let moved: [fn(&mut RemoteEntry); 6] = [
            |e| e.host = "user@elsewhere".into(),
            |e| e.port = 2222,
            |e| e.key = Some("/k/id".into()),
            |e| e.user = Some("other".into()),
            |e| e.jump_host = Some("bastion".into()),
            |e| e.socket = Some("/run/other.sock".into()),
        ];
        for change in moved {
            let mut other = base.clone();
            change(&mut other);
            assert!(!base.same_route(&other), "{other:?}");
        }
    }

    /// The state the bug leaves behind — a Homebrew install AND a
    /// `~/.local/bin` copy. The upgrade still goes through Homebrew, the other
    /// copy is left alone, and the output names both and which one is used.
    #[test]
    fn upgrade_with_both_installs_uses_homebrew_and_reports_both() {
        let remote = Remote::new(Fixture {
            brew: Some("0.40.0"),
            local_bin: Some("0.41.0"),
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.41.0");

        let (result, out) = remote.upgrade(BrewAt::PrefixOnly, "0.43.0", false);
        result.expect("upgrade must succeed");
        let log = remote.log();

        assert!(log.contains("brew upgrade dot-agent-deck"), "{log}");
        assert!(!log.contains("curl "), "{log}");
        assert_eq!(
            remote.version_of(&remote.local_bin_copy()),
            "dot-agent-deck 0.41.0",
            "the other copy must be left untouched"
        );
        let brew_binary = remote.brew_binary().display().to_string();
        assert!(
            out.contains("has two dot-agent-deck installs")
                && out.contains(&brew_binary)
                && out.contains(REMOTE_INSTALL_PATH)
                && out.contains("uses the Homebrew one")
                && out.contains("`ssh user@mac 'rm ~/.local/bin/dot-agent-deck'`"),
            "both installs, the one in use and the cleanup must be named: {out}"
        );
        assert_eq!(remote.entry().remote_binary(), brew_binary);
    }

    /// `remote add` registers a Homebrew install as it is: no download, no
    /// `brew upgrade`, nothing in `~/.local/bin`, and the version Homebrew has
    /// is what is recorded, with the difference from the requested one said.
    #[test]
    fn add_on_a_homebrew_remote_registers_the_brew_install() {
        let remote = Remote::new(Fixture {
            brew: Some("0.40.0"),
            local_bin: None,
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        let opts = AddOptions {
            name: "mac".to_string(),
            remote_type: "ssh".to_string(),
            target: "user@mac".to_string(),
            port: 22,
            key: None,
            version: "0.43.0".to_string(),
            no_install: false,
            release_base: "https://example.test/releases/download".to_string(),
        };
        let mut out = Vec::new();
        let entry = add_reporting_to(
            &opts,
            &remote.shell(BrewAt::PrefixOnly),
            &remote.registry,
            &mut out,
        )
        .expect("add must succeed");
        let out = String::from_utf8(out).unwrap();
        let log = remote.log();

        assert!(!remote.local_bin_copy().exists(), "{log}");
        assert!(!log.contains("curl "), "{log}");
        assert!(
            !log.contains("brew upgrade"),
            "`add` must not upgrade: {log}"
        );
        assert!(
            log.contains(&format!("hooks {}", remote.brew_binary().display())),
            "{log}"
        );
        assert_eq!(entry, remote.entry());
        assert_eq!(entry.version, "0.40.0");
        assert_eq!(entry.install.as_deref(), Some(INSTALL_HOMEBREW));
        assert_eq!(
            entry.remote_binary(),
            remote.brew_binary().display().to_string()
        );
        assert!(
            out.contains("runs dot-agent-deck 0.40.0 from Homebrew")
                && out.contains("not the requested 0.43.0")
                && out.contains("remote upgrade"),
            "{out}"
        );
    }

    /// Homebrew installs its tap's latest and cannot install a chosen
    /// version. The upgrade reports which version landed and records it.
    #[test]
    fn upgrade_through_homebrew_records_the_version_that_landed() {
        let remote = Remote::new(Fixture {
            brew: Some("0.40.0"),
            local_bin: None,
            tap: "0.42.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");

        let (result, out) = remote.upgrade(BrewAt::PrefixOnly, "0.43.0", false);
        let entry = result.expect("upgrade must succeed");

        assert_eq!(entry.version, "0.42.0");
        assert!(
            out.contains("runs dot-agent-deck 0.42.0 from Homebrew")
                && out.contains("not the requested 0.43.0")
                && out.contains("tap's latest"),
            "{out}"
        );
        assert!(
            out.contains("Upgraded remote 'mac' to version 0.42.0."),
            "{out}"
        );
    }

    /// `--no-install` on a Homebrew remote verifies the Homebrew binary and
    /// changes nothing on the remote — before #1372 it verified a
    /// `~/.local/bin` copy that a brew-only remote does not have, and failed.
    #[test]
    fn upgrade_no_install_on_a_homebrew_remote_verifies_the_brew_binary() {
        let remote = Remote::new(Fixture {
            brew: Some("0.43.0"),
            local_bin: None,
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");

        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.43.0", true);
        let entry = result.expect("upgrade --no-install must succeed");
        let log = remote.log();

        assert!(!log.contains("brew upgrade"), "{log}");
        assert!(!log.contains("curl "), "{log}");
        assert!(!remote.local_bin_copy().exists());
        assert_eq!(entry.version, "0.43.0");
        assert_eq!(entry.install.as_deref(), Some(INSTALL_HOMEBREW));

        // A different version fails the pre-flight, as it does for a
        // `~/.local/bin` install, and leaves the registry alone (PR #1373
        // review).
        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.44.0", true);
        let err = result.expect_err("--no-install must require the requested version");
        assert!(
            matches!(
                err,
                RemoteUpgradeError::Inner(RemoteAddError::VersionMismatch { .. })
            ),
            "{err:?}"
        );
        assert_eq!(remote.entry().version, "0.43.0");
    }

    /// A failing `brew upgrade` fails the command and never falls back to a
    /// download, and the registry is left as it was.
    #[test]
    fn a_failed_brew_upgrade_fails_without_a_fallback_download() {
        let remote = Remote::new(Fixture {
            brew: Some("0.40.0"),
            local_bin: None,
            tap: "0.43.0",
            brew_upgrade_fails: true,
        });
        remote.register_legacy_entry("0.40.0");

        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.43.0", false);
        let err = result.expect_err("a failed brew upgrade must fail the command");
        let msg = err.to_string();

        assert!(
            matches!(
                err,
                RemoteUpgradeError::Inner(RemoteAddError::BrewUpgradeFailed { status: 1, .. })
            ),
            "{err:?}"
        );
        assert!(msg.contains("dot-agent-deck is pinned"), "{msg}");
        assert!(!remote.local_bin_copy().exists());
        assert!(!remote.log().contains("curl "));
        let entry = remote.entry();
        assert_eq!(entry.version, "0.40.0");
        assert_eq!(entry.install, None);
    }

    /// PRD #1487 review (Qodo 4200041523): the download moved into place and
    /// then failed its version check. The binary is not rolled back, so the
    /// error says it was replaced and carries the version the check read —
    /// which the upgrade outcome reports as installed — and the registry is
    /// left as it was.
    #[test]
    fn a_download_that_lands_the_wrong_version_says_the_binary_was_replaced() {
        let remote = Remote::new(Fixture {
            brew: None,
            local_bin: Some("0.40.0"),
            tap: "0.43.0",
            brew_upgrade_fails: false,
        });
        remote.register_legacy_entry("0.40.0");

        let (result, _) = remote.upgrade(BrewAt::PrefixOnly, "0.44.0", false);
        let err = result.expect_err("a version check that fails must fail the command");
        let msg = err.to_string();

        assert!(
            matches!(
                &err,
                RemoteUpgradeError::Inner(RemoteAddError::ReplacedButUnverified { on_disk, source, .. })
                    if on_disk == "0.43.0"
                        && matches!(**source, RemoteAddError::VersionMismatch { .. })
            ),
            "{err:?}"
        );
        assert_eq!(err.installed_version(), Some("0.43.0"));
        assert!(msg.contains("was replaced"), "{msg}");
        assert!(
            msg.contains("reports `0.43.0` but expected `0.44.0`"),
            "{msg}"
        );
        assert!(
            msg.contains("What is installed there now is 0.43.0"),
            "{msg}"
        );
        assert_eq!(
            remote.version_of(&remote.local_bin_copy()),
            "dot-agent-deck 0.43.0"
        );
        assert_eq!(remote.entry().version, "0.40.0");
    }

    /// An entry written before #1372 has neither field and runs
    /// [`REMOTE_INSTALL_PATH`], exactly as it did.
    #[test]
    fn a_legacy_registry_entry_runs_the_local_bin_path() {
        let file: RemotesFile = toml::from_str(
            "[[remotes]]\nname = \"old\"\ntype = \"ssh\"\nhost = \"u@h\"\nport = 22\nversion = \"0.40.0\"\nadded_at = \"2026-01-01T00:00:00Z\"\n",
        )
        .expect("a legacy entry must parse");
        let entry = &file.remotes[0];
        assert_eq!(entry.install, None);
        assert_eq!(entry.binary, None);
        assert_eq!(entry.remote_binary(), REMOTE_INSTALL_PATH);
    }

    /// The recorded binary is interpolated into remote commands unquoted, so a
    /// value that could inject one is refused when `remotes.toml` is read
    /// rather than handed to the remote shell.
    #[test]
    fn a_registry_binary_with_shell_metacharacters_is_refused() {
        let legacy = "[[remotes]]\nname = \"old\"\ntype = \"ssh\"\nhost = \"u@h\"\nport = 22\nversion = \"0.40.0\"\nadded_at = \"2026-01-01T00:00:00Z\"\ninstall = \"homebrew\"\n";
        for hostile in [
            "/opt/homebrew/bin/dot-agent-deck; rm -rf ~",
            "/opt/homebrew/bin/$(id)",
            "/opt/home brew/bin/dot-agent-deck",
            "opt/homebrew/bin/dot-agent-deck",
            "/opt/homebrew/bin/dot-agent-deck\n",
        ] {
            let text = format!("{legacy}binary = {hostile:?}\n");
            assert!(
                toml::from_str::<RemotesFile>(&text).is_err(),
                "{hostile:?} must be refused"
            );
        }
        let text = format!("{legacy}binary = \"/opt/homebrew/bin/dot-agent-deck\"\n");
        let file: RemotesFile = toml::from_str(&text).expect("a safe path must parse");
        assert_eq!(
            file.remotes[0].remote_binary(),
            "/opt/homebrew/bin/dot-agent-deck"
        );
        let round_trip: RemotesFile =
            toml::from_str(&toml::to_string_pretty(&file).unwrap()).unwrap();
        assert_eq!(round_trip, file);
    }
}
