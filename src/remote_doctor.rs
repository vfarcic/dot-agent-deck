//! `dot-agent-deck remote doctor <name>` — read-only diagnosis of a remote's
//! ssh setup (PRD #345).
//!
//! The command exists because the reverse-tunnel recipe from issue #97 fails
//! in four ways that are opaque or actively misleading from the client side:
//!
//! 1. `AllowTcpForwarding no` on the remote and a **port collision** produce
//!    byte-identical client errors. Only the remote's own `sshd -T` separates
//!    them, which is why this command exists at all.
//! 2. Without `ExitOnForwardFailure yes`, a forward that cannot bind is
//!    *silent* — ssh brings the session up without it.
//! 3. `DynamicForward` reads like the right directive and puts the SOCKS
//!    listener on the laptop instead of the remote.
//! 4. An unreaped forward listener fails the deck's own version probe, which
//!    used to be reported as an unreachable host (issue #344, fixed in
//!    [`crate::connect::is_forward_failure_detail`]).
//!
//! Everything here **probes and reports**. Nothing in this module writes to
//! the registry, to `~/.ssh/config`, to the remote's sshd configuration, or to
//! the remote at all: every remote command is a read-only query, and
//! `remote/doctor/005` enforces that by rejecting any mutating form in the
//! recorded ssh argv.
//!
//! The parsing and classification half is deliberately pure — `ssh -G` and
//! `sshd -T` output in, ordered [`CheckResult`]s out — so the interesting
//! decisions are covered by fast-tier tests rather than by spawning ssh.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::connect::{
    REMOTE_INSTALL_PATH, RemoteConnectError, is_forward_failure_detail, lookup_remote,
    probe_remote_protocol, probe_remote_version, probe_timeout_secs, ssh_error_detail,
};
use crate::remote::{SshExecutor, SshTarget, SystemSshExecutor};

/// One forwarding directive from ssh's resolved (`ssh -G`) configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedForward {
    /// A SOCKS listener on the remote, emitted by ssh as `[socks]:0`.
    RemoteDynamic { listen: String },
    /// A remote listener forwarding to a concrete host and port.
    Remote { listen: String, destination: String },
    /// A SOCKS listener on the laptop (the wrong direction for PRD #97).
    Dynamic { listen: String },
    /// A laptop-side local forward.
    Local { listen: String, destination: String },
}

/// The subset of resolved client-side ssh configuration used by the doctor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSshConfig {
    pub forwards: Vec<ResolvedForward>,
    pub exit_on_forward_failure: Option<bool>,
    pub forward_agent: Option<bool>,
}

/// Resolved values accepted by sshd for `AllowTcpForwarding`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowTcpForwarding {
    Yes,
    No,
    Local,
    Remote,
    All,
}

impl AllowTcpForwarding {
    /// Whether this policy lets a client open a reverse (`-R`) forward.
    ///
    /// `local` is a blocker exactly as `no` is: it permits `-L` and refuses
    /// `-R`, which is the direction the #97 recipe depends on.
    fn permits_reverse(self) -> bool {
        matches!(self, Self::Yes | Self::Remote | Self::All)
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
            Self::Local => "local",
            Self::Remote => "remote",
            Self::All => "all",
        }
    }
}

/// The subset of remote-side `sshd -T` output used by the doctor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedSshdConfig {
    pub allow_tcp_forwarding: Option<AllowTcpForwarding>,
    pub client_alive_interval: Option<u64>,
}

/// Stable identities for the doctor's dependency-ordered checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckId {
    HostReachable,
    RemoteBinary,
    ProtocolCompatible,
    RemoteForward,
    DynamicForward,
    ExitOnForwardFailure,
    AllowTcpForwarding,
    ClientAliveInterval,
    ForwardBound,
    ForwardAgent,
}

impl CheckId {
    /// The stable label printed in the report. Kept identical to the variant
    /// name so a user can grep the output and the docs for the same token.
    pub fn label(self) -> &'static str {
        match self {
            Self::HostReachable => "HostReachable",
            Self::RemoteBinary => "RemoteBinary",
            Self::ProtocolCompatible => "ProtocolCompatible",
            Self::RemoteForward => "RemoteForward",
            Self::DynamicForward => "DynamicForward",
            Self::ExitOnForwardFailure => "ExitOnForwardFailure",
            Self::AllowTcpForwarding => "AllowTcpForwarding",
            Self::ClientAliveInterval => "ClientAliveInterval",
            Self::ForwardBound => "ForwardBound",
            Self::ForwardAgent => "ForwardAgent",
        }
    }
}

/// User-visible status of one doctor check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
    Unknown,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Whether this aggregate verdict means "nothing to act on".
    ///
    /// `Warn` counts as clear: the advisories are legitimate choices, not
    /// defects. `Unknown` never does — an incomplete diagnosis reported as
    /// all-clear is the failure mode the PRD calls out explicitly.
    pub fn is_clear(self) -> bool {
        matches!(self, Self::Pass | Self::Warn)
    }
}

/// One user-visible doctor result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    pub check: CheckId,
    pub verdict: Verdict,
    pub headline: String,
    pub fix: String,
}

impl CheckResult {
    fn new(check: CheckId, verdict: Verdict, headline: &str, fix: &str) -> Self {
        Self {
            check,
            verdict,
            headline: headline.to_string(),
            fix: fix.to_string(),
        }
    }
}

/// All observations consumed by the pure classifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorInputs {
    pub host_reachable: Option<bool>,
    pub remote_binary_present: Option<bool>,
    pub protocol_compatible: Option<bool>,
    pub ssh: ResolvedSshConfig,
    pub sshd: ResolvedSshdConfig,
    pub forward_bound: Option<bool>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Interpret one of ssh's boolean values. `None` for anything unrecognised, so
/// a future spelling never turns into a confident wrong answer.
fn parse_ssh_bool(raw: Option<&&str>) -> Option<bool> {
    match raw?.to_ascii_lowercase().as_str() {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Parse the known subset of `ssh -G` output, ignoring everything else.
///
/// `ssh -G <destination>` prints ssh's *resolved* client configuration as
/// `lowercasekey value` lines without connecting, so this is both cheap and
/// non-invasive: ssh stays the single source of truth for its own option
/// grammar and the deck never parses `~/.ssh/config` itself.
///
/// Leniency is load-bearing (PRD #345 Risks). A real dump carries ~80 keys
/// this cares nothing about, plus keys from OpenSSH releases that do not exist
/// yet. Unknown keys, malformed values and valueless lines are skipped
/// individually; nothing aborts the parse, and there is no error path at all.
/// A later occurrence of a key wins, which matches ssh's own "print the
/// resolved value" semantics.
pub fn parse_ssh_g(stdout: &str) -> ResolvedSshConfig {
    let mut resolved = ResolvedSshConfig::default();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        let values: Vec<&str> = fields.collect();
        match key.to_ascii_lowercase().as_str() {
            "exitonforwardfailure" => {
                if let Some(value) = parse_ssh_bool(values.first()) {
                    resolved.exit_on_forward_failure = Some(value);
                }
            }
            "forwardagent" => {
                if let Some(value) = parse_ssh_bool(values.first()) {
                    resolved.forward_agent = Some(value);
                }
            }
            // ssh labels a reverse-DYNAMIC (SOCKS) forward unambiguously as
            // `[socks]:0`, so the correct #97 configuration and a concrete
            // reverse tunnel are distinguishable without any guessing.
            "remoteforward" => {
                if let [listen, destination] = values.as_slice() {
                    resolved
                        .forwards
                        .push(if *destination == SOCKS_DESTINATION {
                            ResolvedForward::RemoteDynamic {
                                listen: listen.to_string(),
                            }
                        } else {
                            ResolvedForward::Remote {
                                listen: listen.to_string(),
                                destination: destination.to_string(),
                            }
                        });
                }
            }
            "dynamicforward" => {
                if let [listen] = values.as_slice() {
                    resolved.forwards.push(ResolvedForward::Dynamic {
                        listen: listen.to_string(),
                    });
                }
            }
            "localforward" => {
                if let [listen, destination] = values.as_slice() {
                    resolved.forwards.push(ResolvedForward::Local {
                        listen: listen.to_string(),
                        destination: destination.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    resolved
}

/// How ssh spells the destination of a reverse-dynamic (SOCKS) forward.
const SOCKS_DESTINATION: &str = "[socks]:0";

/// Markers in `sshd -T`'s stderr that mean "we were not allowed to look", as
/// opposed to a benign warning printed alongside a real dump.
///
/// `sshd -T` typically requires root. Run as an ordinary user it exits
/// non-zero, and on some hosts it prints a partial-looking dump alongside a
/// permission complaint — which is the dangerous case, because the dump reads
/// like an answer. Anything here forces UNKNOWN.
const SSHD_UNAVAILABLE_MARKERS: &[&str] = &[
    "permission denied",
    "operation not permitted",
    "must be run as root",
    "no hostkeys available",
    "not found",
    "no such file",
];

/// Parse the known subset of a completed `sshd -T` invocation.
///
/// UNKNOWN, never PASS, is the whole point of the signature taking `status`
/// and `stderr`: a non-zero exit or a permission complaint yields `None` for
/// every field, because "a diagnostic that silently reports 'fine' when it
/// could not look is worse than one that admits it does not know" (PRD #345).
///
/// A *successful* but partial dump is preserved per field — the caller gets a
/// real verdict for whatever sshd did print and UNKNOWN only for what it
/// didn't, rather than an all-or-nothing answer.
pub fn parse_sshd_t(status: i32, stdout: &str, stderr: &str) -> ResolvedSshdConfig {
    if status != 0 {
        return ResolvedSshdConfig::default();
    }
    let lower_stderr = stderr.to_ascii_lowercase();
    if SSHD_UNAVAILABLE_MARKERS
        .iter()
        .any(|marker| lower_stderr.contains(marker))
    {
        return ResolvedSshdConfig::default();
    }

    let mut resolved = ResolvedSshdConfig::default();
    for line in stdout.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        let Some(value) = fields.next() else {
            continue;
        };
        match key.to_ascii_lowercase().as_str() {
            "allowtcpforwarding" => {
                resolved.allow_tcp_forwarding = match value.to_ascii_lowercase().as_str() {
                    "yes" => Some(AllowTcpForwarding::Yes),
                    "no" => Some(AllowTcpForwarding::No),
                    "local" => Some(AllowTcpForwarding::Local),
                    "remote" => Some(AllowTcpForwarding::Remote),
                    "all" => Some(AllowTcpForwarding::All),
                    // An unrecognised value is a future sshd, not a policy we
                    // may assume is permissive.
                    _ => None,
                };
            }
            "clientaliveinterval" => {
                if let Ok(seconds) = value.parse::<u64>() {
                    resolved.client_alive_interval = Some(seconds);
                }
            }
            _ => {}
        }
    }
    resolved
}

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// The listen spec of the first reverse forward ssh resolved, if any. This is
/// the endpoint the live-bind probe targets and the port every port-specific
/// message names.
fn reverse_listen(ssh: &ResolvedSshConfig) -> Option<&str> {
    ssh.forwards.iter().find_map(|forward| match forward {
        ResolvedForward::RemoteDynamic { listen } | ResolvedForward::Remote { listen, .. } => {
            Some(listen.as_str())
        }
        _ => None,
    })
}

/// `port 1080` when a listen spec is known, a neutral phrase when it isn't.
///
/// Every string built from this deliberately avoids the words PASS / WARN /
/// FAIL / UNKNOWN, and avoids spelling any other check's identity — the report
/// contract is one verdict token and one check identity per line, and prose
/// that says "remote forwarding" would read as the `RemoteForward` check.
fn listener_phrase(listen: Option<&str>) -> String {
    match listen {
        Some(spec) => format!("port {spec}"),
        None => "the reverse listener".to_string(),
    }
}

/// Classify parsed observations into dependency-ordered user-visible checks.
///
/// The order is the reading order: reachability, then the binary and protocol,
/// then what ssh resolved locally, then the remote's sshd policy, then
/// liveness, then advisories. A user reads down until the first FAIL and
/// stops, so a cause must never appear below its own symptom.
///
/// Every unobserved input becomes [`Verdict::Unknown`] with a hint rather than
/// a confident PASS.
pub fn classify(inputs: &DoctorInputs) -> Vec<CheckResult> {
    let listen = reverse_listen(&inputs.ssh);
    let listener = listener_phrase(listen);
    let has_reverse = listen.is_some();
    let laptop_socks = inputs
        .ssh
        .forwards
        .iter()
        .find_map(|forward| match forward {
            ResolvedForward::Dynamic { listen } => Some(listen.as_str()),
            _ => None,
        });
    let sshd_permits_reverse = inputs
        .sshd
        .allow_tcp_forwarding
        .map(AllowTcpForwarding::permits_reverse);

    let mut checks = Vec::with_capacity(10);

    // --- reachability -----------------------------------------------------
    checks.push(match inputs.host_reachable {
        Some(true) => CheckResult::new(
            CheckId::HostReachable,
            Verdict::Pass,
            "ssh connected and authenticated",
            "",
        ),
        Some(false) => CheckResult::new(
            CheckId::HostReachable,
            Verdict::Fail,
            "ssh could not open a session to this host",
            "Check `~/.ssh/config`, that the host is up, that its ssh port is open, and that your key is accepted.",
        ),
        None => CheckResult::new(
            CheckId::HostReachable,
            Verdict::Unknown,
            "the ssh probe did not run",
            "`ssh` itself could not be started. Check that it is installed and on `PATH`.",
        ),
    });

    // --- the deck's own install ------------------------------------------
    checks.push(match inputs.remote_binary_present {
        Some(true) => CheckResult::new(
            CheckId::RemoteBinary,
            Verdict::Pass,
            "the deck answered on the remote",
            "",
        ),
        Some(false) => CheckResult::new(
            CheckId::RemoteBinary,
            Verdict::Fail,
            "the deck was not found at its expected install path on the remote",
            "Run `dot-agent-deck remote upgrade <name>` to (re)install it.",
        ),
        // The hint has to follow the evidence. A probe that never ran because
        // ssh could not open a session at all is a different story from one
        // ssh aborted because a forward could not bind — and the second is
        // failure mode 4, which is worth naming when it applies and
        // misleading when it does not.
        None if inputs.host_reachable == Some(true) => CheckResult::new(
            CheckId::RemoteBinary,
            Verdict::Unknown,
            "the deck's version probe did not complete",
            "The deck's own probes go through the same `Host` block as your tunnel, so a listener that cannot bind aborts them too (issue #344). Settle the forwarding checks below and re-run.",
        ),
        None => CheckResult::new(
            CheckId::RemoteBinary,
            Verdict::Unknown,
            "the deck's version probe did not complete",
            "ssh never opened a session, so the probe had nothing to ask. Settle the reachability check above and re-run.",
        ),
    });

    // Issue #491 argues the laptop<->remote protocol comparison guards
    // nothing, since the remote's TUI and daemon are the same install and the
    // laptop is only ssh plus a terminal. That issue is out of scope here, so
    // connect.rs keeps its behaviour untouched — but the doctor declines to
    // assert a verdict the issue shows to be unfounded, and reports a
    // difference as advisory.
    checks.push(match inputs.protocol_compatible {
        Some(true) => CheckResult::new(
            CheckId::ProtocolCompatible,
            Verdict::Pass,
            "the attach protocol version matches this laptop's",
            "",
        ),
        Some(false) => CheckResult::new(
            CheckId::ProtocolCompatible,
            Verdict::Warn,
            "the attach protocol version differs from this laptop's (informational)",
            "The remote's TUI and daemon come from one install, so a laptop-side version difference is not itself a defect (issue #491). Run `dot-agent-deck remote upgrade <name>` if you want them aligned.",
        ),
        None => CheckResult::new(
            CheckId::ProtocolCompatible,
            Verdict::Unknown,
            "the attach protocol handshake did not complete",
            "The handshake rides the same session, so it cannot answer until whatever else this report flagged is resolved.",
        ),
    });

    // --- what ssh actually resolved locally ------------------------------
    checks.push(match reverse_forward_summary(&inputs.ssh) {
        Some(summary) => CheckResult::new(CheckId::RemoteForward, Verdict::Pass, &summary, ""),
        None => CheckResult::new(
            CheckId::RemoteForward,
            Verdict::Fail,
            "ssh resolved no reverse tunnel for this destination",
            "Add `RemoteForward 1080` (a port with no destination, which is reverse-dynamic SOCKS) to this host's `Host` block in `~/.ssh/config`. See docs/remote-recipes.md.",
        ),
    });

    checks.push(match laptop_socks {
        Some(port) if has_reverse => CheckResult::new(
            CheckId::DynamicForward,
            Verdict::Warn,
            &format!(
                "`DynamicForward {port}` also points the wrong direction, though a reverse tunnel is configured too"
            ),
            "`DynamicForward` opens the SOCKS listener on this laptop, egressing via the remote — the opposite of lending the remote your network. Remove it unless you want it for something else.",
        ),
        Some(port) => CheckResult::new(
            CheckId::DynamicForward,
            Verdict::Fail,
            &format!(
                "`DynamicForward {port}` points the wrong direction — that SOCKS listener lands on this laptop"
            ),
            "Replace it with `RemoteForward <port>` (no destination) in `~/.ssh/config`: the reverse form is what puts the SOCKS listener on the remote. This exact mistake appeared in issue #97's original proposal.",
        ),
        None => CheckResult::new(
            CheckId::DynamicForward,
            Verdict::Pass,
            "no laptop-side SOCKS listener is configured",
            "",
        ),
    });

    checks.push(match inputs.ssh.exit_on_forward_failure {
        Some(true) => CheckResult::new(
            CheckId::ExitOnForwardFailure,
            Verdict::Pass,
            "`ExitOnForwardFailure yes` is set, so a tunnel that cannot bind aborts the session loudly",
            "",
        ),
        setting => {
            let headline = if setting == Some(false) {
                "`ExitOnForwardFailure` is off, so a tunnel that cannot bind is silent"
            } else {
                "`ExitOnForwardFailure` was not present in the resolved configuration"
            };
            CheckResult::new(
                CheckId::ExitOnForwardFailure,
                // Only a real hazard once there is something to be silent
                // about: without it ssh brings the session up anyway and
                // agents then report git errors instead of tunnel errors.
                if has_reverse {
                    Verdict::Fail
                } else {
                    Verdict::Warn
                },
                headline,
                "Add `ExitOnForwardFailure yes` to this host's `Host` block in `~/.ssh/config` so a tunnel that cannot bind aborts the session instead of coming up without it.",
            )
        }
    });

    // --- the remote's sshd policy ----------------------------------------
    checks.push(match inputs.sshd.allow_tcp_forwarding {
        Some(policy) if policy.permits_reverse() => CheckResult::new(
            CheckId::AllowTcpForwarding,
            Verdict::Pass,
            &format!(
                "the remote's sshd permits reverse (`-R`) tunnels (`AllowTcpForwarding {}`)",
                policy.as_str()
            ),
            "",
        ),
        Some(policy) => CheckResult::new(
            CheckId::AllowTcpForwarding,
            Verdict::Fail,
            &format!(
                "the remote's sshd refuses reverse (`-R`) tunnels (`AllowTcpForwarding {}`)",
                policy.as_str()
            ),
            "Set `AllowTcpForwarding yes` in the remote's sshd_config and reload sshd. sshd honours the FIRST value it finds for a keyword, so rewrite the existing line — appending a new one at the end does nothing. Alpine's openssh package and most hardening baselines ship this disabled.",
        ),
        None => CheckResult::new(
            CheckId::AllowTcpForwarding,
            Verdict::Unknown,
            "could not read the remote's sshd policy",
            "`sshd -T` needs root on most hosts and is unavailable otherwise. Re-run it on the remote with elevated permission (`sudo sshd -T | grep allowtcpforwarding`), or ask whoever administers the host.",
        ),
    });

    checks.push(match inputs.sshd.client_alive_interval {
        Some(0) => CheckResult::new(
            CheckId::ClientAliveInterval,
            Verdict::Warn,
            "the remote's sshd never probes idle sessions (`ClientAliveInterval 0`)",
            "Set `ClientAliveInterval 15` and `ClientAliveCountMax 3` in the remote's sshd_config so it reaps sessions orphaned by a sleeping laptop. Otherwise a stale listener can hold the port long enough to block your next session.",
        ),
        Some(seconds) => CheckResult::new(
            CheckId::ClientAliveInterval,
            Verdict::Pass,
            &format!("the remote's sshd probes idle sessions every {seconds}s"),
            "",
        ),
        None => CheckResult::new(
            CheckId::ClientAliveInterval,
            Verdict::Unknown,
            "could not read the remote's sshd keepalive policy",
            "`sshd -T` needs root on most hosts and is unavailable otherwise. Re-run it on the remote with elevated permission, or ask whoever administers the host.",
        ),
    });

    // --- liveness ---------------------------------------------------------
    //
    // The headline diagnostic. `AllowTcpForwarding no` and a port collision
    // produce byte-identical client errors, so the ONLY thing that separates
    // them is whether the remote's own sshd said it permits reverse tunnels.
    checks.push(match inputs.forward_bound {
        Some(true) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Pass,
            &format!("{listener} is bound and accepting connections on the remote"),
            "",
        ),
        Some(false) if sshd_permits_reverse == Some(false) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Fail,
            &format!("{listener} is not bound on the remote, which the sshd policy above explains"),
            "This is the remote's policy refusing the tunnel, not a busy port. Fix `AllowTcpForwarding` on the remote first, then re-run this command.",
        ),
        Some(false) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Fail,
            &format!("{listener} is not bound on the remote, though its sshd permits the tunnel"),
            "That port on the remote is already taken — a collision, not a policy refusal. Give this laptop its own listen port, or drop the older session still holding it. Forward ports are per-remote, so two laptops using the same one collide.",
        ),
        None => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Unknown,
            "the live bind state on the remote was not observed",
            "Nothing readable answered the loopback probe: either no reverse tunnel is configured to probe, or `bash` (used for a read-only `/dev/tcp` connect) is missing on the remote.",
        ),
    });

    // --- advisories -------------------------------------------------------
    //
    // Never a defect. Agent forwarding is a legitimate choice; the docs
    // recommend against it, which is advice, not a verdict.
    checks.push(match inputs.ssh.forward_agent {
        Some(true) => CheckResult::new(
            CheckId::ForwardAgent,
            Verdict::Warn,
            "agent forwarding is enabled for this destination",
            "Consider `ForwardAgent no` and a scoped deploy key on the remote instead: with agent forwarding, every agent on the remote can use this laptop's ssh-agent for as long as you stay connected, with no per-agent scoping and no way to revoke one agent short of disconnecting.",
        ),
        Some(false) => CheckResult::new(
            CheckId::ForwardAgent,
            Verdict::Pass,
            "agent forwarding is off for this destination",
            "",
        ),
        None => CheckResult::new(
            CheckId::ForwardAgent,
            Verdict::Unknown,
            "agent forwarding was not present in the resolved configuration",
            "Run `ssh -G <host>` yourself and look for the `forwardagent` line.",
        ),
    });

    checks
}

/// A one-line description of the reverse tunnels ssh resolved, or `None` when
/// it resolved none.
fn reverse_forward_summary(ssh: &ResolvedSshConfig) -> Option<String> {
    let described: Vec<String> = ssh
        .forwards
        .iter()
        .filter_map(|forward| match forward {
            ResolvedForward::RemoteDynamic { listen } => {
                Some(format!("reverse-dynamic SOCKS on {listen}"))
            }
            ResolvedForward::Remote {
                listen,
                destination,
            } => Some(format!("{listen} to {destination}")),
            _ => None,
        })
        .collect();
    if described.is_empty() {
        return None;
    }
    Some(format!("ssh resolved {}", described.join(", ")))
}

/// Collapse checks without ever treating an UNKNOWN observation as PASS.
///
/// Precedence is FAIL, then UNKNOWN, then WARN, then PASS. UNKNOWN outranking
/// WARN is the load-bearing part: the command exits non-zero on an incomplete
/// diagnosis, because "everything I could look at is fine" is not the same
/// claim as "everything is fine".
pub fn overall_verdict(checks: &[CheckResult]) -> Verdict {
    if checks.iter().any(|c| c.verdict == Verdict::Fail) {
        return Verdict::Fail;
    }
    if checks.iter().any(|c| c.verdict == Verdict::Unknown) {
        return Verdict::Unknown;
    }
    if checks.iter().any(|c| c.verdict == Verdict::Warn) {
        return Verdict::Warn;
    }
    Verdict::Pass
}

// ---------------------------------------------------------------------------
// Read-only probes
// ---------------------------------------------------------------------------

/// The remote command that dumps sshd's resolved configuration. Read-only:
/// `-T` makes sshd parse its config, print it, and exit without touching
/// anything or serving a connection.
const SSHD_DUMP_COMMAND: &str = "sshd -T";

/// Ask the ssh client to print its resolved configuration for `target`.
///
/// `-G` does not connect, so this costs nothing and cannot fail because the
/// host is down — which is why the resolved-forwards half of the report still
/// renders when every other probe has given up.
fn ssh_config_dump_command(target: &SshTarget) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-G");
    cmd.arg("-p").arg(target.port.to_string());
    if let Some(key) = &target.key {
        cmd.arg("-i").arg(key);
    }
    cmd.arg("--");
    cmd.arg(target.user_host());
    cmd
}

/// A read-only TCP connect against the remote's loopback, used to decide
/// whether a configured reverse listener is actually there.
///
/// `bash`'s `/dev/tcp` pseudo-device is the smallest read-only way to ask "is
/// something listening": it opens a socket and closes it. No file is created,
/// no listener state is changed, nothing is written. When `bash` is absent the
/// remote shell exits 127 and the check degrades to UNKNOWN rather than
/// claiming the port is free.
fn forward_probe_command(host: &str, port: u16) -> String {
    format!("bash -c 'exec 3<>/dev/tcp/{host}/{port}'")
}

/// Resolve a `ssh -G` listen spec into the loopback endpoint to probe.
///
/// A bare port means ssh binds the remote's loopback, which is where the #97
/// recipe expects the SOCKS listener. A wildcard bind is probed on loopback
/// too — that is the address an agent on the remote would use.
fn probe_endpoint(listen: &str) -> Option<(String, u16)> {
    const LOOPBACK: &str = "127.0.0.1";
    if let Ok(port) = listen.parse::<u16>() {
        return Some((LOOPBACK.to_string(), port));
    }
    let (host, port) = listen.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = match host {
        "" | "*" | "0.0.0.0" | "::" => LOOPBACK,
        other => other,
    };
    Some((host.to_string(), port))
}

/// Run the loopback probe and translate its exit status into an observation.
fn probe_forward_bound(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    listen: &str,
) -> Option<bool> {
    let (host, port) = probe_endpoint(listen)?;
    match executor.run(target, &forward_probe_command(&host, port)) {
        Ok(output) if output.status == 0 => Some(true),
        Ok(output) if output.status == 1 => {
            // bash exits 1 both for a refused connect and for a build without
            // net redirections, where it reports the pseudo-path as missing.
            // Only the former is evidence about the port.
            let lower = output.stderr.to_ascii_lowercase();
            if lower.contains("no such file") || lower.contains("not supported") {
                None
            } else {
                Some(false)
            }
        }
        // 127 is "bash missing on the remote" — an absent tool, not a free
        // port.
        Ok(_) => None,
        Err(err) if is_forward_failure_detail(&ssh_error_detail(&err)) => Some(false),
        Err(_) => None,
    }
}

/// Everything one `remote doctor` run observed, plus the raw ssh complaint (if
/// any) so the report can quote what ssh itself said.
struct Observations {
    inputs: DoctorInputs,
    ssh_detail: Option<String>,
}

/// Run every read-only probe against `target`.
///
/// Ordering is chosen to spend the fewest ssh round-trips on a broken remote:
/// `ssh -G` never connects so it always runs, the protocol handshake is
/// skipped when the version probe already showed the binary is not answering,
/// and the remote-side probes are skipped when ssh could not open a session at
/// all.
fn observe(executor: &dyn SshExecutor, target: &SshTarget, name: &str) -> Observations {
    let mut inputs = DoctorInputs::default();
    let mut ssh_detail = None;

    // `ssh -G` first: it is the one probe that cannot fail because of the
    // network, so the resolved-configuration half of the report survives an
    // unreachable host.
    match ssh_config_dump_command(target).output() {
        Ok(output) => inputs.ssh = parse_ssh_g(&String::from_utf8_lossy(&output.stdout)),
        Err(err) => ssh_detail = Some(format!("could not run `ssh -G`: {err}")),
    }

    match probe_remote_version(executor, target, name, REMOTE_INSTALL_PATH) {
        Ok(_) => {
            inputs.host_reachable = Some(true);
            inputs.remote_binary_present = Some(true);
        }
        Err(RemoteConnectError::HostUnreachable { detail, .. }) => {
            inputs.host_reachable = Some(false);
            ssh_detail = Some(detail);
        }
        // Issue #344's payoff: ssh got far enough to negotiate a forward, so
        // the host IS reachable and authentication DID work. Reporting this as
        // an unreachable host is the exact misdiagnosis the doctor exists to
        // eliminate.
        Err(RemoteConnectError::ForwardFailed { detail, .. }) => {
            inputs.host_reachable = Some(true);
            ssh_detail = Some(detail);
        }
        Err(RemoteConnectError::RemoteBinaryMissing { .. }) => {
            inputs.host_reachable = Some(true);
            inputs.remote_binary_present = Some(false);
        }
        Err(RemoteConnectError::SpawnFailed { source, .. }) => {
            ssh_detail = Some(source.to_string());
        }
        Err(other) => {
            inputs.host_reachable = Some(true);
            ssh_detail = Some(other.to_string());
        }
    }

    if inputs.remote_binary_present == Some(true) {
        inputs.protocol_compatible =
            match probe_remote_protocol(executor, target, name, REMOTE_INSTALL_PATH) {
                Ok(_) => Some(true),
                Err(RemoteConnectError::ProtocolMismatch { .. }) => Some(false),
                Err(_) => None,
            };
    }

    if inputs.host_reachable != Some(false) {
        if let Ok(output) = executor.run(target, SSHD_DUMP_COMMAND) {
            inputs.sshd = parse_sshd_t(output.status, &output.stdout, &output.stderr);
        }
        if let Some(listen) = reverse_listen(&inputs.ssh) {
            inputs.forward_bound = probe_forward_bound(executor, target, listen);
        }
    }

    Observations { inputs, ssh_detail }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Column width for the verdict token, wide enough for `UNKNOWN`.
const VERDICT_WIDTH: usize = 7;
/// Column width for the check identity, wide enough for `ExitOnForwardFailure`.
const CHECK_WIDTH: usize = 20;

/// Write the report.
///
/// The shape is a contract the L2 tests depend on: **one verdict-bearing line
/// per check**, carrying that check's identity and exactly one of PASS / WARN
/// / FAIL / UNKNOWN. The fix deliberately goes on its own continuation line,
/// which keeps a phrase like "reverse tunnels" or "`AllowTcpForwarding yes`"
/// out of the line that is being matched for a single identity and a single
/// verdict.
fn render(
    out: &mut impl Write,
    name: &str,
    target: &SshTarget,
    observations: &Observations,
    checks: &[CheckResult],
    overall: Verdict,
) -> io::Result<()> {
    writeln!(
        out,
        "Diagnosing remote '{name}' at {}:{} (read-only)",
        target.user_host(),
        target.port
    )?;
    if let Some(detail) = &observations.ssh_detail {
        // First line only, and capped: ssh's own complaint is useful context
        // but a multi-line dump would drown the report.
        let first = detail.lines().next().unwrap_or_default().trim();
        if !first.is_empty() {
            let quoted: String = first.chars().take(200).collect();
            writeln!(out, "ssh itself said: {quoted}")?;
        }
    }
    writeln!(out)?;

    for check in checks {
        writeln!(
            out,
            "{verdict:<VERDICT_WIDTH$} {check_id:<CHECK_WIDTH$} {headline}",
            verdict = check.verdict.label(),
            check_id = check.check.label(),
            headline = check.headline,
        )?;
        if !check.fix.is_empty() {
            let indent = " ".repeat(VERDICT_WIDTH + 1);
            writeln!(out, "{indent}-> {}", check.fix.replace("<name>", name))?;
        }
    }

    writeln!(out)?;
    writeln!(out, "Overall: {}", overall.label())?;
    if !overall.is_clear() {
        writeln!(
            out,
            "See docs/remote-recipes.md for the reverse-tunnel recipe and its troubleshooting table."
        )?;
    }
    Ok(())
}

/// Run `remote doctor <name>` end to end and write the report to `out`.
///
/// Registry resolution happens **before any ssh work**, so an unknown name
/// costs zero ssh invocations — the command must not go probing a host the
/// user never registered.
///
/// Returns the aggregate verdict; the caller maps it to an exit code.
pub fn run_doctor(
    name: &str,
    registry_path: &Path,
    out: &mut impl Write,
) -> Result<Verdict, RemoteConnectError> {
    let entry = lookup_remote(name, registry_path)?;
    let target = entry.ssh_target();

    let executor = SystemSshExecutor::with_wallclock_timeout(probe_timeout_secs());
    let observations = observe(&executor, &target, name);
    let checks = classify(&observations.inputs);
    let overall = overall_verdict(&checks);

    render(out, name, &target, &observations, &checks, overall)?;
    Ok(overall)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REALISTIC_SSH_G: &str = r#"
host localhost
user vfarcic
hostname localhost
port 22
addressfamily any
batchmode no
canonicalizefallbacklocal yes
canonicalizehostname false
checkhostip no
compression no
controlmaster false
clearallforwardings no
exitonforwardfailure no
fingerprinthash SHA256
forwardx11 no
gatewayports no
gssapiauthentication yes
hashknownhosts yes
hostbasedauthentication no
identitiesonly no
kbdinteractiveauthentication yes
passwordauthentication yes
permitlocalcommand no
proxyusefdpass no
pubkeyauthentication true
requesttty auto
sessiontype default
stdinnull no
forkafterauthentication no
streamlocalbindunlink no
stricthostkeychecking ask
tcpkeepalive yes
serveralivecountmax 3
serveraliveinterval 0
identityfile ~/.ssh/id_rsa
identityfile ~/.ssh/id_ed25519
sendenv LANG
sendenv LC_*
permitremoteopen any
forwardagent no
connecttimeout none
controlpersist no
thiskeyisfromafutureopenssh frobnicate
remoteforward missing-destination
line-without-a-value
exitonforwardfailure yes
forwardagent yes
remoteforward 1080 [socks]:0
"#;

    fn healthy_inputs() -> DoctorInputs {
        DoctorInputs {
            host_reachable: Some(true),
            remote_binary_present: Some(true),
            protocol_compatible: Some(true),
            ssh: ResolvedSshConfig {
                forwards: vec![ResolvedForward::RemoteDynamic {
                    listen: "1080".to_string(),
                }],
                exit_on_forward_failure: Some(true),
                forward_agent: Some(false),
            },
            sshd: ResolvedSshdConfig {
                allow_tcp_forwarding: Some(AllowTcpForwarding::Yes),
                client_alive_interval: Some(30),
            },
            forward_bound: Some(true),
        }
    }

    fn check(results: &[CheckResult], id: CheckId) -> &CheckResult {
        results
            .iter()
            .find(|result| result.check == id)
            .unwrap_or_else(|| panic!("missing {id:?} check in {results:#?}"))
    }

    /// Scenario: Parse all forward forms emitted by `ssh -G` and keep laptop-side,
    /// concrete reverse, and reverse-dynamic SOCKS forwards distinguishable.
    #[test]
    fn ssh_g_parses_forward_directions_and_boolean_settings() {
        let parsed = parse_ssh_g(
            "remoteforward 1080 [socks]:0\n\
             remoteforward 127.0.0.1:8080 db.internal:5432\n\
             dynamicforward 9099\n\
             localforward 8080 localhost:80\n\
             exitonforwardfailure yes\n\
             forwardagent no\n",
        );

        assert_eq!(
            parsed.forwards,
            vec![
                ResolvedForward::RemoteDynamic {
                    listen: "1080".to_string(),
                },
                ResolvedForward::Remote {
                    listen: "127.0.0.1:8080".to_string(),
                    destination: "db.internal:5432".to_string(),
                },
                ResolvedForward::Dynamic {
                    listen: "9099".to_string(),
                },
                ResolvedForward::Local {
                    listen: "8080".to_string(),
                    destination: "localhost:80".to_string(),
                },
            ]
        );
        assert_eq!(parsed.exit_on_forward_failure, Some(true));
        assert_eq!(parsed.forward_agent, Some(false));
    }

    /// Scenario: Parse both boolean values for the two client settings instead
    /// of conflating an explicit `no` with an absent or unreadable setting.
    #[test]
    fn ssh_g_accepts_yes_and_no_boolean_values() {
        for (raw, expected) in [("yes", true), ("no", false)] {
            let parsed = parse_ssh_g(&format!("exitonforwardfailure {raw}\nforwardagent {raw}\n"));
            assert_eq!(
                parsed.exit_on_forward_failure,
                Some(expected),
                "ExitOnForwardFailure {raw}"
            );
            assert_eq!(parsed.forward_agent, Some(expected), "ForwardAgent {raw}");
        }
    }

    /// Scenario: Feed a captured, mostly-unrelated `ssh -G localhost` dump plus
    /// future and malformed lines; known settings after the bad lines still parse.
    #[test]
    fn ssh_g_ignores_unknown_and_malformed_lines_in_realistic_dump() {
        let parsed = parse_ssh_g(REALISTIC_SSH_G);

        assert_eq!(parsed.exit_on_forward_failure, Some(true));
        assert_eq!(parsed.forward_agent, Some(true));
        assert_eq!(
            parsed.forwards,
            vec![ResolvedForward::RemoteDynamic {
                listen: "1080".to_string(),
            }]
        );
    }

    /// Scenario: Parse a resolved config with no `RemoteForward` directives and
    /// return an empty inventory rather than rejecting otherwise-valid output.
    #[test]
    fn ssh_g_without_remote_forwards_returns_empty_inventory() {
        let parsed = parse_ssh_g("host localhost\nexitonforwardfailure yes\nforwardagent no\n");

        assert!(parsed.forwards.is_empty());
        assert_eq!(parsed.exit_on_forward_failure, Some(true));
    }

    /// Scenario: Parse every value accepted by `AllowTcpForwarding`; classify
    /// `no` and `local` as blocking a remote (`-R`) forward.
    #[test]
    fn sshd_t_parses_all_allow_tcp_forwarding_values_and_remote_blockers() {
        let cases = [
            ("yes", AllowTcpForwarding::Yes, Verdict::Pass),
            ("no", AllowTcpForwarding::No, Verdict::Fail),
            ("local", AllowTcpForwarding::Local, Verdict::Fail),
            ("remote", AllowTcpForwarding::Remote, Verdict::Pass),
            ("all", AllowTcpForwarding::All, Verdict::Pass),
        ];

        for (raw, expected, verdict) in cases {
            let sshd = parse_sshd_t(0, &format!("allowtcpforwarding {raw}\n"), "");
            assert_eq!(sshd.allow_tcp_forwarding, Some(expected), "value {raw}");

            let mut inputs = healthy_inputs();
            inputs.sshd = sshd;
            let results = classify(&inputs);
            assert_eq!(
                check(&results, CheckId::AllowTcpForwarding).verdict,
                verdict,
                "remote-forward semantics for {raw}"
            );
        }
    }

    /// Scenario: Parse zero (disabled) and a positive `ClientAliveInterval`
    /// as integers so classification can distinguish stale-listener risk.
    #[test]
    fn sshd_t_parses_client_alive_interval_zero_and_nonzero() {
        assert_eq!(
            parse_sshd_t(0, "clientaliveinterval 0\n", "").client_alive_interval,
            Some(0)
        );
        assert_eq!(
            parse_sshd_t(0, "clientaliveinterval 45\n", "").client_alive_interval,
            Some(45)
        );
    }

    /// Scenario: Simulate empty, failed, and permission-denied `sshd -T`
    /// probes; none may turn plausible-looking settings into known PASS data.
    #[test]
    fn sshd_t_unavailable_is_unknown_never_pass() {
        let unavailable = [
            parse_sshd_t(0, "", ""),
            parse_sshd_t(
                1,
                "allowtcpforwarding yes\nclientaliveinterval 30\n",
                "sshd: no hostkeys available -- exiting",
            ),
            parse_sshd_t(
                0,
                "allowtcpforwarding yes\nclientaliveinterval 30\n",
                "Permission denied",
            ),
        ];

        for sshd in unavailable {
            assert_eq!(sshd.allow_tcp_forwarding, None);
            assert_eq!(sshd.client_alive_interval, None);

            let mut inputs = healthy_inputs();
            inputs.sshd = sshd;
            let results = classify(&inputs);
            assert_eq!(
                check(&results, CheckId::AllowTcpForwarding).verdict,
                Verdict::Unknown
            );
            assert_eq!(
                check(&results, CheckId::ClientAliveInterval).verdict,
                Verdict::Unknown
            );
            assert_ne!(overall_verdict(&results), Verdict::Pass);
        }
    }

    /// Scenario: Parse a successful but partial sshd dump per setting; the
    /// present keepalive gets a real verdict while the absent forwarding key is UNKNOWN.
    #[test]
    fn sshd_t_partial_output_preserves_per_check_knowledge() {
        let sshd = parse_sshd_t(0, "clientaliveinterval 30\n", "");
        assert_eq!(sshd.allow_tcp_forwarding, None);
        assert_eq!(sshd.client_alive_interval, Some(30));

        let mut inputs = healthy_inputs();
        inputs.sshd = sshd;
        let results = classify(&inputs);
        assert_eq!(
            check(&results, CheckId::AllowTcpForwarding).verdict,
            Verdict::Unknown
        );
        assert_eq!(
            check(&results, CheckId::ClientAliveInterval).verdict,
            Verdict::Pass
        );
    }

    /// Scenario: Compare the two failures that share ssh's client error text:
    /// sshd refusing remote forwards and an allowed forward whose port is unavailable.
    #[test]
    fn classify_distinguishes_sshd_block_from_port_collision() {
        let mut blocked = healthy_inputs();
        blocked.sshd.allow_tcp_forwarding = Some(AllowTcpForwarding::No);
        blocked.forward_bound = Some(false);
        let blocked_results = classify(&blocked);
        let blocked_check = check(&blocked_results, CheckId::AllowTcpForwarding);

        let mut collision = healthy_inputs();
        collision.forward_bound = Some(false);
        let collision_results = classify(&collision);
        let collision_check = check(&collision_results, CheckId::ForwardBound);

        assert_eq!(blocked_check.verdict, Verdict::Fail);
        assert_eq!(collision_check.verdict, Verdict::Fail);
        assert!(blocked_check.fix.contains("AllowTcpForwarding"));
        assert!(collision_check.fix.to_ascii_lowercase().contains("port"));
        assert_ne!(blocked_check.headline, collision_check.headline);
        assert_ne!(blocked_check.fix, collision_check.fix);
    }

    /// Scenario: Classify both an explicit `no` and an absent
    /// `ExitOnForwardFailure`; each reports the exact setting that prevents silent failure.
    #[test]
    fn classify_reports_exit_on_forward_failure_missing_or_disabled() {
        for setting in [Some(false), None] {
            let mut inputs = healthy_inputs();
            inputs.ssh.exit_on_forward_failure = setting;
            let results = classify(&inputs);
            let result = check(&results, CheckId::ExitOnForwardFailure);

            assert!(matches!(result.verdict, Verdict::Fail | Verdict::Warn));
            assert!(
                result.fix.contains("ExitOnForwardFailure yes"),
                "specific fix missing from {result:#?}"
            );
        }
    }

    /// Scenario: Compare a laptop-side `DynamicForward` mistake with no forward
    /// at all; the user must receive different headlines and fixes.
    #[test]
    fn classify_distinguishes_wrong_direction_from_no_forward() {
        let mut wrong_direction = healthy_inputs();
        wrong_direction.ssh.forwards = vec![ResolvedForward::Dynamic {
            listen: "9099".to_string(),
        }];
        wrong_direction.forward_bound = None;
        let wrong_results = classify(&wrong_direction);
        let wrong = check(&wrong_results, CheckId::DynamicForward);

        let mut missing = healthy_inputs();
        missing.ssh.forwards.clear();
        missing.forward_bound = None;
        let missing_results = classify(&missing);
        let absent = check(&missing_results, CheckId::RemoteForward);

        assert_ne!(wrong.verdict, Verdict::Pass);
        assert_ne!(absent.verdict, Verdict::Pass);
        assert!(wrong.headline.to_ascii_lowercase().contains("direction"));
        assert_ne!(wrong.headline, absent.headline);
        assert_ne!(wrong.fix, absent.fix);
    }

    /// Scenario: A correctly configured reverse-dynamic forward fails its live
    /// remote bind probe; report the unbound forward and a port-specific fix.
    #[test]
    fn classify_reports_configured_forward_not_bound() {
        let mut inputs = healthy_inputs();
        inputs.forward_bound = Some(false);
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardBound);

        assert_eq!(result.verdict, Verdict::Fail);
        assert!(result.headline.to_ascii_lowercase().contains("bound"));
        assert!(result.fix.to_ascii_lowercase().contains("port"));
    }

    /// Scenario: Enable agent forwarding on an otherwise healthy remote; the
    /// doctor emits a security advisory without turning it into a failure.
    #[test]
    fn classify_forward_agent_is_advisory_not_failure() {
        let mut inputs = healthy_inputs();
        inputs.ssh.forward_agent = Some(true);
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardAgent);

        assert_eq!(result.verdict, Verdict::Warn);
        assert_ne!(result.verdict, Verdict::Fail);
        assert!(result.fix.contains("ForwardAgent no"));
    }

    /// Scenario: Classify a healthy observation set and preserve dependency
    /// order: reachability first, resolved forwards next, then remote sshd policy.
    #[test]
    fn classify_orders_checks_by_dependency() {
        let results = classify(&healthy_inputs());
        let position = |id| {
            results
                .iter()
                .position(|result| result.check == id)
                .unwrap_or_else(|| panic!("missing {id:?} in {results:#?}"))
        };

        assert!(position(CheckId::HostReachable) < position(CheckId::RemoteForward));
        assert!(position(CheckId::RemoteForward) < position(CheckId::AllowTcpForwarding));
    }

    /// Scenario: Leave one remote-side observation unavailable while all other
    /// checks are healthy; aggregate status remains UNKNOWN rather than all-clear PASS.
    #[test]
    fn overall_verdict_does_not_treat_unknown_as_pass() {
        let mut inputs = healthy_inputs();
        inputs.sshd.allow_tcp_forwarding = None;
        let results = classify(&inputs);

        assert_eq!(
            check(&results, CheckId::AllowTcpForwarding).verdict,
            Verdict::Unknown
        );
        assert_eq!(overall_verdict(&results), Verdict::Unknown);
    }
}
