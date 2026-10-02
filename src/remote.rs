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
    pub fn host_key_remedy(&self) -> String {
        if self.port == DEFAULT_SSH_PORT {
            format!("ssh {}", self.user_host())
        } else {
            format!("ssh -p {} {}", self.port, self.user_host())
        }
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
    /// option (PR #1373 review).
    pub fn command_line(&self, remote_command: &str) -> String {
        let mut line = String::from("ssh");
        if self.port != DEFAULT_SSH_PORT {
            line.push_str(&format!(" -p {}", self.port));
        }
        if let Some(key) = &self.key {
            line.push_str(&format!(" -i {}", shell_word(&key.to_string_lossy())));
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
fn shell_word(word: &str) -> String {
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
fn scrub_remote_text(s: &str) -> String {
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
}

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
        }
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

    /// Build the `ssh` command without spawning it. Exposed for tests so we
    /// can verify argument quoting without forking a subprocess.
    pub fn build_command(&self, target: &SshTarget, remote_command: &str) -> Command {
        let mut cmd = Command::new("ssh");
        // BatchMode=yes makes ssh fail fast on missing keys/known_hosts
        // instead of hanging on a TTY prompt. Users who haven't trusted the
        // host yet will see an actionable error rather than the deck CLI
        // wedging.
        cmd.arg("-o").arg("BatchMode=yes");
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
///   while the main loop polls `child.try_wait()`. This is what
///   `Command::output()` does internally, and it's required for any child
///   producing more output than a single pipe buffer (~64 KiB on Linux):
///   without concurrent draining, the child blocks in `write(2)` before it
///   can exit, `try_wait` keeps returning `None`, and the wallclock fires
///   even though the child wasn't actually stalled.
/// - Applies `max_capture_bytes` to *each* stream independently — stdout and
///   stderr are separate attack vectors, and a hostile peer that floods stderr
///   drives memory growth just as easily as one that floods stdout. A drainer
///   stops reading altogether at its cap; if the child keeps writing it fills
///   the kernel pipe buffer, blocks in `write(2)`, and the deadline reaps it.
/// - Polls `child.try_wait()` every 50ms until the deadline. Polling cadence
///   is a wallclock-vs-CPU tradeoff; 50ms keeps the worst-case overshoot
///   under a tick while costing ~20 syscalls/sec.
/// - On deadline: SIGKILL via `child.kill()`, reap with `child.wait()`, and
///   return `timed_out: true`. **The kill reaches the child only.** `ssh -G`
///   evaluates `Match exec`, so a config with `Match exec "sleep 30"` has
///   already forked a descendant that this does not signal; such a descendant
///   is orphaned and reaped by init when it exits on its own. The bound this
///   helper offers is on *our* wait and *our* memory, not on what the user's
///   own configuration chose to spawn.
/// - Computes the deadline with `Instant::checked_add` so an absurd `secs`
///   (e.g. `u64::MAX`) can never panic between `spawn` and the polling
///   loop and leak the child — probe callers already clamp to a sane upper
///   bound, this is belt-and-suspenders.
pub fn run_local_bounded(
    cmd: &mut Command,
    secs: u64,
    max_capture_bytes: usize,
) -> std::io::Result<LocalCapture> {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    // `usize::MAX` is the wrapper's spelling of "no cap"; map it back to
    // `None` so the uncapped install path keeps its plain `read_to_end`
    // instead of looping through the chunked capped drainer for nothing.
    let cap = (max_capture_bytes != usize::MAX).then_some(max_capture_bytes);
    let stdout_handle = child
        .stdout
        .take()
        .map(|s| std::thread::spawn(move || drain_pipe(s, cap)));
    let stderr_handle = child
        .stderr
        .take()
        .map(|s| std::thread::spawn(move || drain_pipe(s, cap)));

    let deadline = Instant::now()
        .checked_add(Duration::from_secs(secs))
        .unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
    let poll_interval = Duration::from_millis(50);

    let join_pipes = |stdout_handle: Option<std::thread::JoinHandle<Vec<u8>>>,
                      stderr_handle: Option<std::thread::JoinHandle<Vec<u8>>>|
     -> (Vec<u8>, Vec<u8>) {
        let stdout = stdout_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr = stderr_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        (stdout, stderr)
    };

    // A drainer stops exactly AT its cap, so a stream that reached it is a
    // prefix of what the child wanted to say. An output that happens to be
    // exactly `max_capture_bytes` long is reported truncated too; that errs
    // toward "I could not read all of it", which is the safe direction for
    // every caller here.
    let truncated = |stdout: &[u8], stderr: &[u8]| {
        stdout.len() >= max_capture_bytes || stderr.len() >= max_capture_bytes
    };

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Child already exited; close the pipes by joining the
                // drain threads (they will see EOF once the kernel reaps
                // the writers).
                let (stdout, stderr) = join_pipes(stdout_handle, stderr_handle);
                let truncated = truncated(&stdout, &stderr);
                return Ok(LocalCapture {
                    status: Some(status),
                    stdout,
                    stderr,
                    truncated,
                    timed_out: false,
                });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    // Best-effort cleanup: ignore secondary errors. SIGKILL
                    // is unblockable so the child is guaranteed to be
                    // reaped, and `wait` collects the zombie. Joining the
                    // drain threads after kill ensures their pipe handles
                    // don't outlive this function.
                    let _ = child.kill();
                    let _ = child.wait();
                    let (stdout, stderr) = join_pipes(stdout_handle, stderr_handle);
                    let truncated = truncated(&stdout, &stderr);
                    return Ok(LocalCapture {
                        status: None,
                        stdout,
                        stderr,
                        truncated,
                        timed_out: true,
                    });
                }
                std::thread::sleep(poll_interval);
            }
            Err(source) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipes(stdout_handle, stderr_handle);
                return Err(source);
            }
        }
    }
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

/// Drain a child-process pipe into a `Vec<u8>`, optionally capping how much
/// is retained. When `cap` is `Some(n)`, at most `n` bytes are buffered and
/// the helper returns immediately once that bound is hit — no further `read`
/// syscalls are issued. If the child keeps writing it will fill the kernel
/// pipe buffer and then block in `write(2)`; the surrounding wallclock kill
/// is the documented fallback that reaps such children. Errors are
/// swallowed: a half-read pipe still returns the bytes that did land,
/// matching the behavior `Command::output()` exhibits when the kernel closes
/// the writer.
fn drain_pipe<R: std::io::Read>(mut reader: R, cap: Option<usize>) -> Vec<u8> {
    match cap {
        None => {
            let mut buf = Vec::new();
            let _ = reader.read_to_end(&mut buf);
            buf
        }
        Some(cap) => {
            let mut buf: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                if buf.len() >= cap {
                    break;
                }
                let needed = cap - buf.len();
                let take = chunk.len().min(needed);
                match reader.read(&mut chunk[..take]) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
            buf
        }
    }
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
        target
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

/// Pull the version number out of `dot-agent-deck --version` output.
/// Expected shape: `dot-agent-deck X.Y.Z` (possibly with trailing whitespace).
fn parse_version_output(stdout: &str) -> Option<String> {
    stdout.split_whitespace().nth(1).map(|s| s.to_string())
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
    Ok(())
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

    let landed = remote_binary_version(executor, target, binary.as_str(), version)?;
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
    let version = validate_version_string(&opts.version)?;

    // 2. Lookup. Unknown name short-circuits before any ssh work.
    let registry = RemotesFile::load(remotes_path)?;
    let existing = registry
        .remotes
        .iter()
        .find(|r| r.name == opts.name)
        .ok_or_else(|| RemoteUpgradeError::UnknownName {
            name: opts.name.clone(),
        })?;
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
    //    fails loud rather than persisting half-finished state.
    install_remote_hooks(executor, &target, &installed, out)?;

    // 6. Update registry. `added_at` stays at the original registration
    //    timestamp; `upgraded_at` records the most recent upgrade so users
    //    can see both moments without losing the registration history. The
    //    install method and binary are re-recorded on every upgrade, which is
    //    what moves an entry written before issue #1372 onto the method that
    //    actually owns the remote's install.
    let now = chrono::Utc::now().to_rfc3339();
    let updated = crate::deck_list::update(
        remotes_path,
        crate::deck_list::DeckRef::Name(&opts.name),
        |entry| {
            entry.version = installed.version.clone();
            entry.upgraded_at = Some(now);
            entry.install = Some(installed.method.to_string());
            entry.binary = installed.binary.clone();
        },
    )?
    .ok_or_else(|| RemoteUpgradeError::UnknownName {
        name: opts.name.clone(),
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
