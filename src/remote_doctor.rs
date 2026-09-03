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
//! **Every ssh session this module opens is an *observation* session**, built
//! through [`SystemSshExecutor::for_observation`] — see
//! `remote::apply_observation_options` for the flags and the full reasoning.
//! The short version: an ordinary session applies the user's `Host` block, so
//! the doctor would **establish the very reverse forward it then checks**.
//! `ForwardBound` would report PASS because the doctor bound the port, not
//! because a user session had it bound, and the run would stop being read-only
//! (a reverse-*dynamic* forward briefly exposes the laptop's reachable network
//! through a SOCKS listener on the remote; `ControlPersist` can leave a master
//! connection *and its forwards* alive after the command exits;
//! `UpdateHostKeys` writes `known_hosts`; `LocalCommand` runs). Do not
//! "helpfully" drop those flags. Host-key *verification* is deliberately left
//! alone — that is a security control, not a mutation.
//!
//! The one probe that deliberately does **not** carry them is `ssh -G`: it
//! never connects, and `ClearAllForwardings` would erase the very forward
//! inventory the dump is read for.
//!
//! Two limitations are accepted rather than fixed, and are worth knowing
//! before reading a report:
//!
//! 1. **`ForwardBound` observes pre-existing state.** Over a session that
//!    creates no forwards, the probe answers "what is already listening on
//!    that port on the remote?" — not "would a forward bind?". Asking the
//!    other question would mean binding the port ourselves, which is exactly
//!    the mutation this module refuses. The remote's own `AllowTcpForwarding`
//!    is what separates a policy refusal from a busy port, and it is read
//!    independently of this probe.
//!
//!    A bare TCP connect could not even answer the first question usefully —
//!    "something is listening" covers a live tunnel of yours and a squatter
//!    equally, so a foreign service on an otherwise-perfect configuration
//!    rendered entirely PASS, exit 0. For the recipe's reverse-**dynamic**
//!    forward the probe therefore speaks SOCKS5: `05 01 00` out, `05 00`
//!    back, which a foreign service will not produce. That is the module's one
//!    write, it is scoped to ports the user declared to be SOCKS (see
//!    [`ForwardProbe`]), and it is recorded as a decision in the PRD. A
//!    **concrete** `RemoteForward` gets a connect and nothing else — its port
//!    carries whatever the user tunnelled — so an accepting listener there is
//!    honestly UNKNOWN rather than a PASS nobody verified.
//! 2. **Only the FIRST reverse forward is bind-checked.** That covers the
//!    single-tunnel recipe of issue #97. A `Host` block with several reverse
//!    forwards is still reported in full by the `RemoteForward` check; only
//!    its first listener is probed for liveness.
//!
//! The parsing and classification half is deliberately pure — `ssh -G` and
//! `sshd -T` output in, ordered [`CheckResult`]s out — so the interesting
//! decisions are covered by fast-tier tests rather than by spawning ssh.

use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

use crate::connect::{
    REMOTE_INSTALL_PATH, RemoteConnectError, lookup_remote, probe_remote_protocol,
    probe_remote_version, probe_timeout_secs,
};
use crate::remote::{SshExecutor, SshTarget, SystemSshExecutor, run_local_bounded};
use crate::untrusted_text::escape_control_and_bidi;

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

    /// The process exit code this aggregate verdict maps to (review note N1).
    ///
    /// Three codes, not two:
    ///
    /// - **0** — clear. Every check PASSed, or at most raised an advisory WARN.
    /// - **1** — a check FAILed. Something is wrong and the report names it.
    /// - **2** — incomplete. No FAIL, but at least one check is UNKNOWN.
    ///
    /// Both non-zero codes satisfy the PRD's "UNKNOWN must never read as PASS".
    /// Splitting them is what makes the single most common real-world outcome
    /// — a healthy tunnel on a host where `sshd -T` needs root you do not have
    /// — a stable, scriptable `2` instead of being indistinguishable from a
    /// broken tunnel. A wrapper script can then treat 2 as "as much as I could
    /// see is fine" without having to treat a genuine FAIL the same way.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Pass | Self::Warn => 0,
            Self::Fail => 1,
            Self::Unknown => 2,
        }
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

/// What the live loopback probe learned about the configured reverse listener.
///
/// A `bool` carried this until PRD #345's attribution work and could not:
/// "something answered" spans a verified tunnel, a foreign service, and a
/// service that accepts and then says nothing, and those three want three
/// different verdicts. Splitting them is what stops a squatter on an
/// otherwise-perfect configuration from rendering entirely PASS, exit 0 —
/// the flagship scenario reporting healthy while broken (`remote/doctor/006`).
///
/// `None` on [`DoctorInputs::forward_bound`] still means "not observed", which
/// stays UNKNOWN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardObservation {
    /// The listener accepted a SOCKS5 no-auth handshake, so it **is a SOCKS
    /// proxy** — the form a reverse-dynamic `RemoteForward` puts there. The
    /// only observation this module treats as a confident PASS.
    ///
    /// Note the ceiling, which the headline is held to: `05 00` proves *a*
    /// SOCKS5 proxy is listening, not that it is *this user's*. Two SOCKS
    /// proxies on one port are indistinguishable from the laptop, and the
    /// docs' limits list says so plainly — "if you deliberately run one for
    /// something else, expect a PASS". Claiming attribution here would have
    /// the code assert what the documentation two sections later disclaims.
    SocksVerified,
    /// The listener accepted the connection and answered the handshake with
    /// something that is not a SOCKS5 acceptance — a foreign service holding
    /// the port.
    Foreign,
    /// The listener accepted the connection and never answered at all. The
    /// quietest shape of the same collision, and the one a connect-only probe
    /// reads as success.
    Silent,
    /// Something is listening on a **concrete** reverse forward's port. That
    /// port carries whatever the user tunnelled, so there is no greeting we
    /// may send to attribute it — see [`ForwardProbe`].
    Unattributable,
    /// Nothing is listening: the connect was **recognisably** refused.
    ///
    /// Only a stderr [`is_recognised_refusal`] accepts produces this. A probe
    /// failure the module cannot classify is UNKNOWN with the stderr quoted,
    /// never this — see [`probe_forward_bound`].
    Refused,
}

/// All observations consumed by the pure classifier.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorInputs {
    pub host_reachable: Option<bool>,
    pub remote_binary_present: Option<bool>,
    pub protocol_compatible: Option<bool>,
    pub ssh: ResolvedSshConfig,
    pub sshd: ResolvedSshdConfig,
    pub forward_bound: Option<ForwardObservation>,
    /// The remote's own stderr when the loopback probe ran and failed in a
    /// shape [`probe_forward_bound`] could not classify.
    ///
    /// `Some` is strictly a *reason* attached to an unobserved
    /// [`Self::forward_bound`], never an observation: it reports UNKNOWN
    /// carrying the evidence, so the user can see what the deck saw instead of
    /// being handed remediation for a cause nothing established. Producer
    /// controlled — [`render`] escapes it on the way out.
    pub forward_probe_unclassified: Option<String>,
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
///
/// Deliberately narrow (review note N4). This list is only consulted on a
/// **status-0** run, so it is looking for the one pathological shape where
/// sshd printed a dump *and* complained; `"not found"` and `"no such file"`
/// used to be here and were removed, because a benign status-0 warning
/// containing either phrase discarded a perfectly readable dump. The absent
/// binary they were aiming at exits 127, which
/// [`parse_sshd_t`]'s status check already catches, and the permission
/// markers below carry the rest of the weight.
const SSHD_UNAVAILABLE_MARKERS: &[&str] = &[
    "permission denied",
    "operation not permitted",
    "must be run as root",
    "no hostkeys available",
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

/// The listen spec of `forward` when it is a *reverse* forward, `None` for the
/// laptop-side directions the liveness probe has no business targeting.
fn reverse_forward_listen(forward: &ResolvedForward) -> Option<&str> {
    match forward {
        ResolvedForward::RemoteDynamic { listen } | ResolvedForward::Remote { listen, .. } => {
            Some(listen.as_str())
        }
        _ => None,
    }
}

/// The first reverse forward ssh resolved, if any.
///
/// The probe needs the whole forward, not just its listen spec: whether it is
/// reverse-*dynamic* decides whether a handshake may be written into it at all
/// (see [`ForwardProbe`]).
fn reverse_forward(ssh: &ResolvedSshConfig) -> Option<&ResolvedForward> {
    ssh.forwards
        .iter()
        .find(|forward| reverse_forward_listen(forward).is_some())
}

/// The listen spec of the first reverse forward ssh resolved, if any. This is
/// the endpoint the live-bind probe targets and the port every port-specific
/// message names.
fn reverse_listen(ssh: &ResolvedSshConfig) -> Option<&str> {
    reverse_forward(ssh).and_then(reverse_forward_listen)
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

    // What this check measures changed with issue #491, and the verdicts did
    // not. `connect` no longer compares the remote's protocol version against
    // the laptop's — the two constants never share a wire, since `connect`
    // ssh's in and runs the remote's own TUI against the remote's own daemon —
    // so a version *difference* is no longer observable here and no longer
    // fatal anywhere. What `probe_remote_protocol` still reports, and what
    // `run_connect` still refuses on, is a remote that cannot answer
    // `daemon hello` at all: too old for the subcommand, or a broken install.
    // That remains genuinely unattachable, so FAIL stays FAIL — a remote you
    // literally cannot attach to must not be diagnosed `Overall: WARN`,
    // exit 0, "clear", which is precisely the "reports fine when it is not"
    // failure this command exists to prevent.
    //
    // `CheckId::ProtocolCompatible` keeps its name deliberately. The identity
    // is part of the report's scriptable output contract (pinned by
    // `tests/e2e_remote_doctor.rs`), it still reads true — can the remote speak
    // our handshake at all — and renaming it would break anyone grepping the
    // report for zero diagnostic gain. Only the prose moved.
    checks.push(match inputs.protocol_compatible {
        Some(true) => CheckResult::new(
            CheckId::ProtocolCompatible,
            Verdict::Pass,
            "the remote answered the attach handshake",
            "",
        ),
        Some(false) => CheckResult::new(
            CheckId::ProtocolCompatible,
            Verdict::Fail,
            "the remote did not answer the attach handshake, so attaching is refused",
            "Run `dot-agent-deck remote upgrade <name>` to reinstall — `dot-agent-deck connect <name>` refuses a remote whose `daemon hello` it cannot get an answer from, so this is not advisory. Either the installed binary predates the handshake or the install is broken. A version *difference* is not what this reports: since issue #491 the remote's protocol version is never compared against this laptop's, because the remote's TUI and daemon are one install and the laptop is only ssh plus a terminal.",
        ),
        None => CheckResult::new(
            CheckId::ProtocolCompatible,
            Verdict::Unknown,
            "the attach handshake did not complete",
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
            // Only a real hazard once there is something to be silent about:
            // without it ssh brings the session up anyway and agents then
            // report git errors instead of tunnel errors.
            let verdict = if has_reverse {
                Verdict::Fail
            } else {
                Verdict::Warn
            };
            // The two verdicts get DIFFERENT headlines (review note Q3). An
            // identical headline separated only by the verdict token reads as
            // inconsistency to anyone comparing two runs; the `DynamicForward`
            // pair above spells out why its WARN is milder, and so does this.
            let headline = match (setting, verdict) {
                (Some(false), Verdict::Warn) => "`ExitOnForwardFailure` is off, so a tunnel that cannot bind would be silent — harmless until you configure a tunnel".to_string(),
                (Some(false), _) => "`ExitOnForwardFailure` is off, so a tunnel that cannot bind is silent".to_string(),
                (_, Verdict::Warn) => "`ExitOnForwardFailure` was not present in the resolved configuration — harmless until you configure a tunnel".to_string(),
                _ => "`ExitOnForwardFailure` was not present in the resolved configuration".to_string(),
            };
            CheckResult::new(
                CheckId::ExitOnForwardFailure,
                verdict,
                &headline,
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
        // The only confident PASS, and it carries its evidence. A bare TCP
        // connect answers "is *something* listening", which is not the
        // question — see `ForwardProbe` for why the handshake is the
        // discriminator and why only this forward kind gets one.
        //
        // Pitched at what `05 00` actually proves: a SOCKS proxy is there, in
        // the form this recipe's tunnel takes. It does NOT prove the proxy is
        // this user's — a second SOCKS proxy on the same port answers
        // identically, which the docs' limits list states outright. The
        // headline used to say "verified as this recipe's own tunnel", an
        // identity claim the evidence does not support.
        Some(ForwardObservation::SocksVerified) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Pass,
            &format!(
                "{listener} answered the SOCKS5 no-auth handshake, so the listener is a SOCKS proxy, consistent with this recipe's tunnel"
            ),
            "",
        ),
        Some(ForwardObservation::Foreign) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Fail,
            &format!(
                "{listener} is held by something else — it answered the SOCKS5 handshake with bytes no SOCKS proxy sends"
            ),
            "A service that is not a SOCKS proxy already owns that port on the remote, so your tunnel cannot bind it. Give this laptop its own listen port, or stop whatever holds this one — forward ports are per-remote, so two laptops on the same one collide.",
        ),
        // Connected and said nothing. The shape most likely to be read as
        // success by a probe that only checks the connect, and the most likely
        // real squatter: a great many services speak only when spoken to in
        // their own protocol.
        Some(ForwardObservation::Silent) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Fail,
            &format!(
                "{listener} is held by something that accepted the connection and then never answered the SOCKS5 handshake"
            ),
            "A live SOCKS proxy replies in microseconds over loopback, so silence means the port belongs to another service. Give this laptop its own listen port, or stop whatever holds this one — forward ports are per-remote, so two laptops on the same one collide.",
        ),
        // UNKNOWN rather than PASS *or* FAIL, and the middle answer is the
        // honest one. PASS and WARN both exit 0 via `Verdict::is_clear`, which
        // would be false confidence; FAIL would be a lie, because for a
        // concrete forward an accepting listener usually IS the user's tunnel
        // working, and calling a healthy configuration broken trains people to
        // ignore the tool.
        Some(ForwardObservation::Unattributable) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Unknown,
            &format!(
                "{listener} has a listener, but a tunnel to a concrete destination carries no greeting this probe may use to attribute it"
            ),
            "Your configuration is right and something is listening; the deck just cannot prove that something is yours. Only the reverse-dynamic form (`RemoteForward <port>` with no destination) puts a SOCKS proxy there, whose no-auth handshake the deck can safely speak. Confirm ownership on the remote yourself (`ss -ltnp`), or switch to the reverse-dynamic form this recipe uses.",
        ),
        Some(ForwardObservation::Refused) if sshd_permits_reverse == Some(false) => {
            CheckResult::new(
                CheckId::ForwardBound,
                Verdict::Fail,
                &format!(
                    "{listener} is not bound on the remote, which the sshd policy above explains"
                ),
                "This is the remote's policy refusing the tunnel, not a busy port. Fix `AllowTcpForwarding` on the remote first, then re-run this command.",
            )
        }
        // The probe rides a session that creates no forwards of its own, so
        // "not bound" is a statement about PRE-EXISTING remote state. Two very
        // different situations produce it and the fix has to own that rather
        // than assert the collision reading, which was written when the
        // doctor's own session established the listener it then measured.
        Some(ForwardObservation::Refused) => CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Fail,
            &format!("{listener} is not bound on the remote, though its sshd permits the tunnel"),
            "Nothing is listening there right now. If a session to this remote is up as you read this, its tunnel did not bind — that port is taken by something else, so give this laptop its own listen port or drop whatever still holds it (forward ports are per-remote, so two laptops on the same one collide). If you are not connected, expect this: the tunnel exists only while a session does, so re-run this while connected to learn anything more.",
        ),
        // Three shapes of "not observed", and they are three different
        // answers. A probe that ran and failed in an unrecognised way has
        // evidence to show; a listen spec we refuse to probe has a value to
        // name; anything else has neither. Collapsing them would put the user
        // back where a confident FAIL left them — told a cause, shown nothing.
        None => match (
            inputs.forward_probe_unclassified.as_deref(),
            listen.filter(|spec| probe_endpoint(spec).is_none()),
        ) {
            // Deliberately UNKNOWN and not FAIL: the probe came back in a
            // shape this module cannot read, so the port is undetermined. The
            // stderr goes in the FIX line, never the verdict line — `render`
            // escapes both, but only the verdict line must carry exactly one
            // verdict token and exactly one check identity.
            (Some(detail), _) => CheckResult::new(
                CheckId::ForwardBound,
                Verdict::Unknown,
                &format!("the loopback probe against {listener} failed in a way the deck cannot read, so nothing was established about that port"),
                &if detail.is_empty() {
                    "The remote's shell rejected the read-only connect and printed nothing to explain it, so the deck will not guess a cause. Try the connect yourself on the remote (`bash -c 'exec 3<>/dev/tcp/127.0.0.1/<port>'`) to see what it says.".to_string()
                } else {
                    format!("The remote's shell said: `{detail}`. The deck does not recognise that as a refused connection, so it will not tell you the port is taken — run the read-only connect yourself on the remote (`bash -c 'exec 3<>/dev/tcp/127.0.0.1/<port>'`) to see it directly.")
                },
            ),
            (None, Some(spec)) => CheckResult::new(
                CheckId::ForwardBound,
                Verdict::Unknown,
                &format!(
                    "ssh resolved the listen spec `{spec}`, which is not a shape this probe will target"
                ),
                "The probe is refused rather than guessed at: a bind address is interpolated into a command the remote shell parses, so only an IPv4 literal, a non-link-local IPv6 literal, or a hostname of letters, digits, `.` and `-` is accepted, with a port in range. A link-local address (`fe80::…`) is excluded because it needs a zone index to connect and the deck will not interpolate one. Rewrite the `RemoteForward` listen address in `~/.ssh/config` as one of those, or probe the endpoint yourself.",
            ),
            (None, None) => CheckResult::new(
                CheckId::ForwardBound,
                Verdict::Unknown,
                "the live bind state on the remote was not observed",
                "Nothing readable answered the loopback probe: either no reverse tunnel is configured to probe, or the remote is missing `bash` (for the read-only `/dev/tcp` connect) or `timeout`, `head` and `od` (for the bounded reply the deck reads back).",
            ),
        },
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

/// Maximum bytes kept from either stream of the remote `sshd -T` dump.
///
/// A real dump is ~100 short lines. 64 KiB leaves room for a verbose future
/// sshd while bounding what a hostile remote can make the laptop hold: the
/// wallclock deadline caps *duration*, not *bytes*, and
/// `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` can stretch that to an hour. Output
/// that actually reaches the cap is treated as UNKNOWN by [`observe`] — a
/// truncated dump must never be parsed as an authoritative answer.
const SSHD_DUMP_CAP: usize = 64 * 1024;

/// Ask the ssh client to print its resolved configuration for `target`.
///
/// `-G` does not connect, so this costs nothing and cannot fail because the
/// host is down — which is why the resolved-forwards half of the report still
/// renders when every other probe has given up.
///
/// Note the absence of the observation `-o` flags every *connecting* probe
/// carries: `ClearAllForwardings=yes` would erase exactly the inventory this
/// dump exists to read. `-G` opens no session, so none of the reasons for
/// those flags applies here.
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

/// Maximum bytes kept from either stream of the local `ssh -G` dump.
///
/// A real dump is ~80 short lines, a few KiB at most. 256 KiB is far more than
/// any plausible configuration and still bounds the capture, which matters
/// because `-G` runs a **local** subprocess whose output an included config
/// file controls.
const SSH_CONFIG_DUMP_CAP: usize = 256 * 1024;

/// Read ssh's resolved client configuration for `target`, or explain why not.
///
/// The `Err` arm is the point. `-G` does not connect, but it is not
/// consequence-free: OpenSSH **evaluates `Match exec`**, so an included
/// `Match exec "sleep 30"` blocks it, and its output is not bounded by
/// anything ssh does. Worse, the old implementation called `Command::output()`
/// and parsed stdout **regardless of exit status**, so a failed or partial
/// dump was presented as *definitive missing configuration* — the report would
/// say "ssh resolved no reverse tunnel" when the truth was "I could not read
/// your config". That is the same false confidence the PRD forbids for
/// `sshd -T`, and the caller turns an `Err` here into UNKNOWN for every check
/// derived from the dump.
///
/// Three ways to get an `Err`, all reported rather than papered over: the
/// deadline fired (`run_local_bounded` SIGKILLs ssh — note that a descendant
/// `Match exec` already forked is not signalled and is left to init), the exit
/// status was non-zero, or a stream hit [`SSH_CONFIG_DUMP_CAP`] so what
/// arrived is a prefix.
fn read_resolved_ssh_config(target: &SshTarget) -> Result<ResolvedSshConfig, String> {
    let secs = probe_timeout_secs();
    let capture = run_local_bounded(
        &mut ssh_config_dump_command(target),
        secs,
        SSH_CONFIG_DUMP_CAP,
    )
    .map_err(|err| format!("`ssh -G` could not be started ({err})"))?;
    if capture.timed_out {
        return Err(format!(
            "`ssh -G` did not finish within {secs}s and was stopped; a `Match exec` directive in your ssh config can block it"
        ));
    }
    if capture.truncated {
        return Err(format!(
            "`ssh -G` printed more than {} KiB, so the dump is a fragment and was not parsed",
            SSH_CONFIG_DUMP_CAP / 1024
        ));
    }
    match capture.status.and_then(|status| status.code()) {
        Some(0) => Ok(parse_ssh_g(&String::from_utf8_lossy(&capture.stdout))),
        Some(code) => Err(format!("`ssh -G` exited with status {code}")),
        None => Err("`ssh -G` was terminated by a signal".to_string()),
    }
}

/// Replace every check derived from the `ssh -G` dump with UNKNOWN, because
/// the dump itself could not be read (review item 6).
///
/// Kept out of [`classify`] on purpose: the classifier's contract is pure
/// observations in, ordered results out, and "the tool that produces the
/// observations failed" is knowledge the probe layer has and the classifier
/// does not. `ForwardBound` is absent from the list because it needs a listen
/// spec that only the dump could have supplied, so it is already UNKNOWN.
fn mark_ssh_config_unreadable(checks: &mut [CheckResult], reason: &str) {
    const DERIVED: &[CheckId] = &[
        CheckId::RemoteForward,
        CheckId::DynamicForward,
        CheckId::ExitOnForwardFailure,
        CheckId::ForwardAgent,
    ];
    for check in checks.iter_mut().filter(|c| DERIVED.contains(&c.check)) {
        check.verdict = Verdict::Unknown;
        check.headline = "ssh's resolved configuration could not be read".to_string();
        check.fix = format!(
            "{reason}. Until that is fixed this says nothing about your configuration — run `ssh -G <host>` yourself to see what ssh resolves."
        );
    }
}

/// Which of the two remote probes to build for a resolved reverse forward.
///
/// **This is a safety gate, not a tuning knob.** The SOCKS5 greeting is three
/// bytes *written* into the listening socket, and only a
/// [`ResolvedForward::RemoteDynamic`] port is one the user declared to be
/// SOCKS. A **concrete** `RemoteForward` carries whatever they tunnelled — a
/// database, an internal API — so writing `05 01 00` into it means writing
/// arbitrary bytes into someone's Postgres, which a line-oriented daemon can
/// log or parse. [`Self::for_forward`] therefore defaults every non-dynamic
/// forward to [`Self::ConnectOnly`], and the fast-tier test
/// `connect_only_probe_never_writes_the_socks_greeting` pins that the built
/// command carries no spelling of the greeting we emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardProbe {
    /// Connect, send the SOCKS5 no-auth greeting, and print the reply as hex.
    /// Reverse-**dynamic** forwards only.
    SocksHandshake,
    /// Connect and stop. Everything else.
    ConnectOnly,
}

impl ForwardProbe {
    /// The probe `forward` may be subjected to.
    ///
    /// Deliberately a match on the one permitted variant with a catch-all
    /// default, not the other way round: a forward kind added later gets the
    /// safe probe until someone decides otherwise.
    fn for_forward(forward: &ResolvedForward) -> Self {
        match forward {
            ResolvedForward::RemoteDynamic { .. } => Self::SocksHandshake,
            _ => Self::ConnectOnly,
        }
    }
}

/// Seconds the handshake waits for the listener's reply before giving up.
///
/// `bash`'s `/dev/tcp` has **no read timeout**, so an unbounded `head -c 2`
/// against a listener that accepts and never speaks — [`ForwardObservation::Silent`],
/// and the most likely real squatter — would block until the ssh wallclock
/// deadline, which `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` lets a user stretch
/// to an hour. Two seconds is enormous for the case that matters: a SOCKS
/// proxy on the remote's own loopback answers in microseconds.
///
/// **Scope, stated precisely because the first wording of it was wrong** (PRD
/// #345 second audit): this bounds the **reply read** and nothing else. DNS
/// resolution of the bind host and the `/dev/tcp` connect both happen *before*
/// `timeout` is reached, so they are bounded only by the outer ssh probe
/// wallclock (`DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS`, 10s by default and
/// clamped to an hour), not by two seconds. That is the correct division of
/// labour — a stalled connect is a transport problem the ssh deadline already
/// owns — but it is not what "the probe is bounded at two seconds" implies.
const SOCKS_REPLY_TIMEOUT_SECS: u8 = 2;

/// Seconds `timeout` waits after its own deadline before escalating TERM to
/// KILL (`timeout -k`).
///
/// Without it the read is not actually bounded. GNU `timeout` sends SIGTERM at
/// the deadline and then **waits**, so a child that ignores or blocks TERM
/// keeps the probe open until the ssh wallclock fires — reproduced locally
/// against a TERM-ignoring child during the PRD #345 second audit, which is
/// the whole scenario [`SOCKS_REPLY_TIMEOUT_SECS`] exists to prevent. `-k`
/// makes the escalation unconditional.
///
/// One second is ample: the escalation only ever runs for a `head` that is
/// already refusing to die, and SIGKILL is unblockable.
const SOCKS_REPLY_KILL_AFTER_SECS: u8 = 1;

/// A read-only probe against the remote's loopback, used to decide whether a
/// configured reverse listener is actually there **and whether it is ours**.
///
/// `bash`'s `/dev/tcp` pseudo-device is the smallest read-only way to open the
/// socket: no file is created, no listener state is changed, and for
/// [`ForwardProbe::ConnectOnly`] nothing at all is written. When `bash` is
/// absent the remote shell exits 127 and the check degrades to UNKNOWN rather
/// than claiming the port is free.
///
/// The handshake form adds the five bytes that make the answer *attributable*.
/// The #97 recipe's forward is reverse-**dynamic**, i.e. a SOCKS5 listener, and
/// the no-auth handshake is definitive — `05 01 00` out, `05 00` back. A
/// foreign service will not produce that, so a reply of `05 00` proves the
/// listener is the tunnel rather than merely present. Writing those three
/// bytes is the one thing this module sends rather than reads; it is scoped to
/// ports the user declared to be SOCKS (see [`ForwardProbe`]) and recorded as
/// a deliberate decision in `prds/345-remote-doctor.md`.
///
/// Three properties of the command matter and are easy to lose in an edit:
///
/// 1. **The exit status reflects the CONNECT, not the read.** `|| exit 1`
///    carries a refusal out, and the trailing `exit 0` discards the status of
///    everything after it. That is what makes an empty reply mean "a listener
///    accepted and said nothing" (FAIL) rather than being confusable with
///    broken tooling (UNKNOWN, from a non-zero status).
/// 2. **The read is bounded** by `timeout -k`; see
///    [`SOCKS_REPLY_TIMEOUT_SECS`] for what the bound does and does not cover,
///    and [`SOCKS_REPLY_KILL_AFTER_SECS`] for why the escalation is not
///    optional.
/// 3. **Every tool the pipeline needs is guarded, so absent tooling exits
///    127**, landing in the same UNKNOWN as a missing `bash` instead of
///    looking like a silent listener. All three of `timeout`, `head` and `od`
///    are named. `head` was the gap: without it in the guard, `timeout`
///    reports it could not exec, `od` sees EOF, the trailing `exit 0` keeps
///    the status at zero, and an empty reply becomes a confident
///    **silent-listener FAIL** — a false FAIL manufactured out of a missing
///    coreutil. False-FAIL is the wrong direction for a guard to be relaxed
///    in, and one extra `command -v` clause removes the path entirely.
///
/// **What the guard deliberately does not do**, recorded rather than chased:
/// `command -v` proves a *name resolves*, not that it resolves to the utility
/// we mean. A `od` shadowed by a shell function, an alias, or an earlier
/// `PATH` entry could print `0500` and force a PASS. That buys an attacker
/// nothing here — a remote that can shadow `od` is already executing the
/// `bash` we sent and controls every other observation this module makes, so
/// the marginal trust boundary is empty. Hardening it (absolute paths,
/// `command -p`) would cost portability across the remotes this has to run on
/// for no security gain.
///
/// The reply is rendered as hex because bash cannot hold the NUL byte of
/// `05 00` in a variable. Which tool spells it is deliberately not a contract —
/// [`socks_reply`] normalises whitespace, so `05 00`, ` 05 00` and `0500` are
/// one observation.
///
/// A future robustness option, noted rather than taken: have the remote print
/// a literal marker token before the hex, so a stray line from a remote shell
/// profile could never be mistaken for the reply. The exit-status route above
/// achieves the separation that actually matters without it.
fn forward_probe_command(probe: ForwardProbe, host: &str, port: u16) -> String {
    let endpoint = format!("/dev/tcp/{host}/{port}");
    match probe {
        ForwardProbe::ConnectOnly => format!("bash -c 'exec 3<>{endpoint}'"),
        // `-k` is written as a short option because that spelling is the one
        // every implementation the probe can land on understands: GNU
        // coreutils (since 7.0), BusyBox and toybox all take `-k SECS`, while
        // `--kill-after=` is GNU-only.
        ForwardProbe::SocksHandshake => format!(
            "bash -c 'exec 3<>{endpoint} || exit 1; \
             command -v timeout >/dev/null 2>&1 && command -v head >/dev/null 2>&1 && command -v od >/dev/null 2>&1 || exit 127; \
             printf \"\\005\\001\\000\" >&3; \
             timeout -k {SOCKS_REPLY_KILL_AFTER_SECS} {SOCKS_REPLY_TIMEOUT_SECS} head -c 2 <&3 | od -An -tx1; \
             exit 0'"
        ),
    }
}

/// The SOCKS5 no-auth acceptance (`05 00`), normalised the way
/// [`socks_reply`] normalises what the remote printed.
const SOCKS5_NO_AUTH_ACCEPTED: &str = "0500";

/// What the listener said back to the SOCKS5 greeting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocksReply {
    /// Exactly the no-auth acceptance.
    Accepted,
    /// Some other bytes.
    Other,
    /// Nothing at all before the read deadline.
    Silent,
}

/// Read the probe's stdout as the listener's reply.
///
/// **Whitespace is normalised away before comparing**, deliberately: `od -An
/// -tx1` prints ` 05 00`, `xxd -p` prints `0500`, and a future edit that swaps
/// the tool must not change the verdict. A parser that accepts one spelling
/// is brittle against exactly the coreutils differences the remotes vary in.
fn socks_reply(stdout: &str) -> SocksReply {
    let hex: String = stdout
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if hex.is_empty() {
        return SocksReply::Silent;
    }
    if hex == SOCKS5_NO_AUTH_ACCEPTED {
        SocksReply::Accepted
    } else {
        SocksReply::Other
    }
}

/// Longest bind host accepted. The DNS ceiling for a fully qualified name;
/// anything past it is not a hostname anyone configured on purpose.
const MAX_BIND_HOST_LEN: usize = 255;

/// Whether `host` is a bind address safe to interpolate into the remote
/// command, and one of the forms the probe actually supports.
///
/// **This is a security boundary, not tidiness.** [`forward_probe_command`]
/// puts `host` inside `bash -c 'exec 3<>/dev/tcp/{host}/{port}'`, and passing
/// that whole string as one local `ssh` argv element protects only the *local*
/// shell: OpenSSH hands the element to the remote **login shell**, which
/// strips the outer quotes, and the nested `bash` then parses the interpolated
/// host again. Two levels of shell parsing, both on the remote.
///
/// The bind host is attacker-reachable because `ssh -G` will happily resolve
/// one out of `~/.ssh/config` — `RemoteForward "evil';id;#:1080"` resolves to
/// `remoteforward [evil';id;#]:1080 [socks]:0`, which built
/// `bash -c 'exec 3<>/dev/tcp/evil';id;#/1080'` and ran `id` on the remote
/// under the authenticated account (verified against OpenSSH 10.2). A `$(…)`
/// host does not even need to break the outer quote — the nested bash
/// evaluates it in place.
///
/// So: **validate, don't quote.** Getting quoting right across two shell
/// levels is possible and fragile, and no legitimate bind host contains a
/// shell metacharacter. Accepted, and nothing else:
///
/// - an IPv4 literal (`127.0.0.1`),
/// - an IPv6 literal (`::1`, `2001:db8::1` — brackets are stripped by the caller),
/// - a hostname of ASCII letters, digits, `.` and `-`, at most
///   [`MAX_BIND_HOST_LEN`] bytes.
///
/// Every accepted form is drawn from `[0-9a-zA-Z.:-]`, none of which is a
/// metacharacter to either shell. Everything else — quotes, `$`, backticks,
/// `;`, `|`, `&`, `(`, `)`, `<`, `>`, `#`, `*`, `?`, `\`, `/`, whitespace,
/// control characters, non-ASCII — is rejected, and the caller turns a
/// rejection into UNKNOWN naming the value rather than into a constructed
/// command or a silent PASS. `/` is refused along with the rest: it would
/// escape the `/dev/tcp/host/port` path shape even without a shell.
///
/// **This answers "is this inert to interpolate?", not "is this connectable?"**
/// The two questions have drifted apart: an IPv6 link-local literal is
/// perfectly inert and still cannot be connected to, because it needs a zone
/// index and `%` is not in the allowlist. That second question belongs to
/// [`probe_endpoint`], which is where the link-local refusal lives — widening
/// this allowlist would put a `%` into a two-shell-level interpolation to fix
/// a problem that is not an injection problem. A future reader may want to
/// give the probe its own predicate rather than leaning on this one twice.
fn is_probeable_bind_host(host: &str) -> bool {
    if host.is_empty() || host.len() > MAX_BIND_HOST_LEN {
        return false;
    }
    if host.parse::<std::net::Ipv4Addr>().is_ok() || host.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    host.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Resolve a `ssh -G` listen spec into the loopback endpoint to probe, or
/// `None` when the spec is not a shape this is willing to probe.
///
/// A bare port means ssh binds the remote's loopback, which is where the #97
/// recipe expects the SOCKS listener. A wildcard bind is probed on loopback
/// too — that is the address an agent on the remote would use.
///
/// **The loopback has to match the family of the wildcard.** An IPv6
/// unspecified bind (`[::]:1080`, or its long spelling
/// `[0:0:0:0:0:0:0:0]:1080` — `ssh -G` echoes IPv6 hosts verbatim rather than
/// canonicalising them) is probed at `::1`, not at `127.0.0.1`. Sending the
/// IPv4 loopback at it is refused wherever the listener is genuinely v6-only
/// — `net.ipv6.bindv6only=1`, OpenBSD, `AddressFamily inet6` — and a refused
/// connect is read as [`ForwardObservation::Refused`], so a *working* tunnel
/// was reported FAIL with the port-collision remediation attached. Measured
/// against a real `IPV6_V6ONLY` listener on `[::]`: `127.0.0.1` is refused
/// while `::1` connects. Dual-stack Linux with the default
/// `bindv6only=0` masks it, because a `[::]` listener also accepts v4-mapped
/// IPv4 — which is why it survived review and eleven L2 tests.
///
/// The empty and `*` spellings carry no family, so they keep the IPv4
/// loopback.
///
/// **A link-local IPv6 bind (`fe80::/10`) is refused rather than probed.** It
/// passes [`is_probeable_bind_host`] — it is inert to interpolate — but it is
/// not *connectable*: a link-local address needs a zone index (`fe80::1%eth0`)
/// to name an interface, `%` is not in the allowlist and is not going to be,
/// so the command built from one can only ever come back as bash's
/// `Invalid argument`. That is an exit 1 the probe would have to guess at, and
/// guessing produced the collision remediation for a tunnel that may be fine.
/// Refusing here lands it on the same UNKNOWN-naming-the-spec path a rejected
/// spec already takes. Global and unique-local IPv6 literals are untouched —
/// those connect.
///
/// The port is narrowed to a `u16`, which makes it inert; the bind host goes
/// through [`is_probeable_bind_host`], which is what keeps a hostile listen
/// spec out of the remote shell. Pure and total, so [`classify`] re-runs it to
/// decide whether an unobserved forward means "refused to probe this spec" or
/// "there was nothing to probe".
fn probe_endpoint(listen: &str) -> Option<(String, u16)> {
    const LOOPBACK: &str = "127.0.0.1";
    const LOOPBACK_V6: &str = "::1";
    if let Ok(port) = listen.parse::<u16>() {
        return Some((LOOPBACK.to_string(), port));
    }
    let (host, port) = listen.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = match host {
        // Neither spelling names a family, so both keep the IPv4 loopback.
        "" | "*" => LOOPBACK,
        // Decided by parsing rather than by matching spellings, because
        // `ssh -G` does not canonicalise an IPv6 host: `[::]` and
        // `[0:0:0:0:0:0:0:0]` both reach here exactly as written.
        other => match other.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(v4)) if v4.is_unspecified() => LOOPBACK,
            Ok(std::net::IpAddr::V6(v6)) if v6.is_unspecified() => LOOPBACK_V6,
            _ => other,
        },
    };
    if !is_probeable_bind_host(host) || is_unprobeable_link_local(host) {
        return None;
    }
    Some((host.to_string(), port))
}

/// Whether `host` is an IPv6 unicast link-local literal (`fe80::/10`).
///
/// Hand-rolled because `Ipv6Addr::is_unicast_link_local` is still unstable
/// (`#![feature(ip)]`), and this is the whole of its definition: the top ten
/// bits are `1111111010`. Scoped to IPv6 deliberately — IPv4's link-local
/// block (`169.254.0.0/16`) carries no zone index and connects like any other
/// literal, so refusing it would be inventing a case.
fn is_unprobeable_link_local(host: &str) -> bool {
    matches!(
        host.parse::<std::net::IpAddr>(),
        Ok(std::net::IpAddr::V6(v6)) if (v6.segments()[0] & 0xffc0) == 0xfe80
    )
}

/// Maximum bytes kept from either stream of the liveness probe.
///
/// The command prints nothing on success and one short line on failure, so
/// 4 KiB is generous. Capping matters because the wallclock deadline bounds
/// *duration*, not *bytes*, and `DOT_AGENT_DECK_SSH_PROBE_TIMEOUT_SECS` lets a
/// user stretch that window to an hour — long enough for a hostile remote
/// streaming at line rate to matter.
const FORWARD_PROBE_CAP: usize = 4 * 1024;

/// Run the loopback probe and translate what it observed.
///
/// Returns [`ProbeOutcome::NotObserved`] — UNKNOWN, never a claim about the
/// port — when the listen spec is not one [`probe_endpoint`] will target, when
/// the remote has no usable `bash` or hex tooling, or when the reply was too
/// long to be the reply this command produces.
///
/// **Only a recognised refusal produces [`ForwardObservation::Refused`].** A
/// non-zero exit whose stderr this module does not understand is
/// [`ProbeOutcome::Unclassified`], which the report renders as UNKNOWN quoting
/// that stderr. The inverse default — anything unrecognised is a refusal —
/// shipped a confident FAIL, with port-collision remediation attached, built
/// on an observation nothing had classified; a link-local bind reaching bash
/// as `Invalid argument` was one instance, and the next unrecognised bash
/// failure would have been the next. It is the same rule this module already
/// applies to `sshd -T`, in the other direction: a diagnostic that reports
/// fine when it could not look is bad, and one that reports broken when it
/// could not tell is worse, because it also prescribes a cure for a cause it
/// never established.
///
/// The exit status is read as a statement about the **connect** because
/// [`forward_probe_command`] makes it one; the reply bytes only ever refine an
/// exit-0 observation. So a listener that accepted and stayed silent is
/// [`ForwardObservation::Silent`] (FAIL) rather than being blurred into the
/// same UNKNOWN as tooling that never ran (non-zero status).
///
/// There is deliberately no arm mapping ssh's own "remote port forwarding
/// failed" stderr onto an observation. It used to exist (`Some(false)`), and
/// it is gone because the doctor's sessions are built by
/// [`SystemSshExecutor::for_observation`], which passes
/// `ClearAllForwardings=yes`: no session this module opens ever *requests* a
/// forward, so OpenSSH has nothing to fail to bind and cannot emit that
/// stderr. Keeping it would have been worse than dead — it turned an error
/// about the doctor's own session into a confident claim about the *user's*
/// listener, which is the misdiagnosis this whole module exists to eliminate.
/// (`crate::connect`'s sessions do carry forwards, so
/// [`crate::connect::is_forward_failure_detail`] is very much alive there.)
fn probe_forward_bound(
    executor: &dyn SshExecutor,
    target: &SshTarget,
    forward: &ResolvedForward,
) -> ProbeOutcome {
    let Some(listen) = reverse_forward_listen(forward) else {
        return ProbeOutcome::NotObserved;
    };
    let Some((host, port)) = probe_endpoint(listen) else {
        return ProbeOutcome::NotObserved;
    };
    let probe = ForwardProbe::for_forward(forward);
    match executor.run_capped(
        target,
        &forward_probe_command(probe, &host, port),
        FORWARD_PROBE_CAP,
    ) {
        // A probe that hit its cap is not the terse reply this command
        // produces, so nothing about the port can be concluded from it. The
        // executor decides this now (`CappedOutput::truncated`) rather than
        // this call site re-deriving it from the returned lengths, which is
        // the same signal the version and protocol probes had been missing.
        Ok(capped) if capped.truncated => ProbeOutcome::NotObserved,
        Ok(capped) if capped.output.status == 0 => ProbeOutcome::Observed(match probe {
            // Connected, and that is the whole of what we are allowed to
            // learn: nothing was sent, so nothing can attribute the listener.
            ForwardProbe::ConnectOnly => ForwardObservation::Unattributable,
            ForwardProbe::SocksHandshake => match socks_reply(&capped.output.stdout) {
                SocksReply::Accepted => ForwardObservation::SocksVerified,
                SocksReply::Other => ForwardObservation::Foreign,
                SocksReply::Silent => ForwardObservation::Silent,
            },
        }),
        Ok(capped) if capped.output.status == 1 => {
            // bash exits 1 for a refused connect, for a build without net
            // redirections, and for every other way the redirection can fail.
            // Only a RECOGNISED refusal is evidence about the port; the rest
            // is an observation this module did not understand, and is
            // reported as one.
            if is_recognised_refusal(&capped.output.stderr) {
                ProbeOutcome::Observed(ForwardObservation::Refused)
            } else {
                ProbeOutcome::Unclassified(first_line_capped(&capped.output.stderr))
            }
        }
        // 127 is "bash, `timeout`, `head` or `od` missing on the remote" —
        // absent tooling, not a free port and not a silent listener. Left on
        // the plain UNKNOWN path deliberately: unlike the exit-1 arm above it
        // was never a confident claim, and its existing hint already names the
        // missing tooling more usefully than the remote's stderr would.
        Ok(_) => ProbeOutcome::NotObserved,
        Err(_) => ProbeOutcome::NotObserved,
    }
}

/// What one run of the loopback probe yielded.
///
/// The third variant is the point: before it existed, "the probe failed in a
/// way I do not recognise" had nowhere to go except
/// [`ForwardObservation::Refused`], and `Refused` is a confident FAIL that
/// ships port-collision remediation. This module already refuses to let an
/// unreadable `sshd -T` become a PASS; the mirror is that an unreadable probe
/// must not become a FAIL.
enum ProbeOutcome {
    /// The probe ran and this is what it learned about the port.
    Observed(ForwardObservation),
    /// The probe ran and failed in a shape this module cannot classify.
    /// Carries the remote's own first stderr line (possibly empty) so the
    /// report can quote the evidence instead of guessing a cause.
    Unclassified(String),
    /// There was nothing to probe, or nothing worth quoting: no reverse
    /// forward, a listen spec [`probe_endpoint`] refuses, an over-long reply,
    /// or ssh itself failing to run.
    NotObserved,
}

/// Whether `stderr` is bash reporting a **refused connect** — the one exit-1
/// shape that is evidence the port has no listener.
///
/// Deliberately one phrase, not a family. `Connection refused` is ECONNREFUSED
/// and nothing else: over loopback it means the kernel had no socket to hand
/// the SYN to. Every other failure names a different cause and must not be
/// read as an empty port — `Network is unreachable` is IPv6 switched off,
/// `Invalid argument` is a link-local address with no zone, `Connection timed
/// out` is something dropping packets, `No such file or directory` and
/// `Operation not supported` are a bash built without net redirections. Those
/// all become UNKNOWN with the text quoted.
///
/// A non-English remote locale translates the message and so reads as
/// unclassified. That is the safe direction, and it degrades to "the deck
/// admits it does not know" rather than to a wrong verdict.
fn is_recognised_refusal(stderr: &str) -> bool {
    stderr.to_ascii_lowercase().contains("connection refused")
}

/// The first non-empty line of producer-controlled text, trimmed and capped.
///
/// Capped at [`QUOTED_STDERR_CAP`] **characters before escaping**, the same
/// order [`render`] uses for ssh's own complaint, so an escape that expands to
/// several characters cannot smuggle bytes past the cap. Returns an empty
/// string when there is nothing to quote; the caller words that case.
fn first_line_capped(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(QUOTED_STDERR_CAP).collect())
        .unwrap_or_default()
}

/// How many characters of remote stderr a hint may quote.
const QUOTED_STDERR_CAP: usize = 200;

/// Everything one `remote doctor` run observed, plus the raw ssh complaint (if
/// any) so the report can quote what ssh itself said.
struct Observations {
    inputs: DoctorInputs,
    ssh_detail: Option<String>,
    /// Why `ssh -G` could not be read, when it could not. `Some` turns every
    /// dump-derived check UNKNOWN via [`mark_ssh_config_unreadable`] instead
    /// of letting an empty parse read as "nothing is configured".
    ssh_config_unreadable: Option<String>,
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
    let mut ssh_config_unreadable = None;

    // `ssh -G` first: it is the one probe that cannot fail because of the
    // network, so the resolved-configuration half of the report survives an
    // unreachable host.
    match read_resolved_ssh_config(target) {
        Ok(config) => inputs.ssh = config,
        Err(reason) => ssh_config_unreadable = Some(reason),
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
        //
        // KEPT, though currently unreachable from here (PRD #345): the version
        // probe rides the same `for_observation` executor as everything else
        // in this module, so `ClearAllForwardings=yes` means it never requests
        // a forward and OpenSSH never emits the stderr
        // `connect::map_probe_ssh_error` matches on. It stays because it is
        // free, it is exactly right the moment those flags change, and the
        // catch-all below would otherwise quietly blur `host_reachable` for
        // the one error class this arm was added to get right. That is a
        // different judgement from the arm deleted in `probe_forward_bound`,
        // which converted the same unreachable error into a confident claim
        // about the USER's listener rather than about the doctor's session.
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
                Err(RemoteConnectError::RemoteHandshakeUnsupported { .. }) => Some(false),
                Err(_) => None,
            };
    }

    if inputs.host_reachable != Some(false) {
        if let Ok(capped) = executor.run_capped(target, SSHD_DUMP_COMMAND, SSHD_DUMP_CAP) {
            // A dump that reached its cap is a fragment, and a fragment parsed
            // as if complete is exactly the "confident wrong answer" the PRD
            // forbids for this probe: the missing tail is indistinguishable
            // from a key sshd never printed. Leave both fields UNKNOWN.
            if !capped.truncated {
                let output = capped.output;
                inputs.sshd = parse_sshd_t(output.status, &output.stdout, &output.stderr);
            }
        }
        // Cloned so the probe can borrow the forward while `inputs` is
        // written back; a resolved forward is two short strings.
        if let Some(forward) = reverse_forward(&inputs.ssh).cloned() {
            match probe_forward_bound(executor, target, &forward) {
                ProbeOutcome::Observed(observation) => inputs.forward_bound = Some(observation),
                // Both leave `forward_bound` unobserved; only this one has
                // evidence worth quoting back.
                ProbeOutcome::Unclassified(detail) => {
                    inputs.forward_probe_unclassified = Some(detail)
                }
                ProbeOutcome::NotObserved => {}
            }
        }
    }

    Observations {
        inputs,
        ssh_detail,
        ssh_config_unreadable,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Column width for the verdict token, wide enough for `UNKNOWN`.
const VERDICT_WIDTH: usize = 7;
/// Column width for the check identity, wide enough for `ExitOnForwardFailure`.
const CHECK_WIDTH: usize = 20;

/// Every verdict token that may appear in a report line, for the shape
/// invariant [`report_shape_violation`] enforces.
const VERDICT_TOKENS: &[&str] = &["PASS", "WARN", "FAIL", "UNKNOWN"];

/// Every check identity, for the same invariant.
const ALL_CHECKS: &[CheckId] = &[
    CheckId::HostReachable,
    CheckId::RemoteBinary,
    CheckId::ProtocolCompatible,
    CheckId::RemoteForward,
    CheckId::DynamicForward,
    CheckId::ExitOnForwardFailure,
    CheckId::AllowTcpForwarding,
    CheckId::ClientAliveInterval,
    CheckId::ForwardBound,
    CheckId::ForwardAgent,
];

/// Whether `line` carries `token` as a whole alphabetic word, case-insensitively.
///
/// Word-wise, not substring-wise, and matching how the L2 tests read a report:
/// `failover` must not count as `FAIL`.
fn carries_word(line: &str, token: &str) -> bool {
    line.split(|c: char| !c.is_ascii_alphabetic())
        .any(|word| word.eq_ignore_ascii_case(token))
}

/// Whether `line` names `check`, matching how the L2 tests read a report:
/// strip everything but alphanumerics, lowercase, then look for the identity.
fn names_check(line: &str, check: CheckId) -> bool {
    let normalize = |s: &str| -> String {
        s.chars()
            .filter(char::is_ascii_alphanumeric)
            .flat_map(char::to_lowercase)
            .collect()
    };
    normalize(line).contains(&normalize(check.label()))
}

/// The report's shape contract, or `None` when `check`'s lines honour it
/// (review note N2).
///
/// **One verdict-bearing line per check, naming exactly that one check.** The
/// L2 tests locate a check's result by scanning for the single line that
/// carries both its identity and a verdict token, so a second such line makes
/// the check unfindable rather than merely untidy. That is why the fix goes on
/// its own continuation line: `ForwardBound`'s fix deliberately names
/// `AllowTcpForwarding`, which is only safe because a fix line carries no
/// verdict token. The contract was real and load-bearing and written down
/// nowhere; this is where it is written down, and `render`'s `debug_assert!`
/// is where a future edit that breaks it fails fast instead of silently.
///
/// Scope, deliberately: this guards text **this module authors**. Headlines
/// also interpolate values resolved out of the user's ssh config (a listen
/// spec, a destination), and `ForwardBound`'s fix quotes the remote's own
/// probe stderr when it could not classify it, so a host literally named
/// `fail` — or a remote whose shell says `UNKNOWN` — could trip it in a debug
/// build. That is why it is a `debug_assert!` — compiled out of the release
/// binary users run — and the maintainer guard it buys is worth far more than
/// the theoretical debug-build panic it costs. Neutering the quote to dodge it
/// would be sanitising the evidence, which is the opposite of what quoting it
/// is for.
fn report_shape_violation(
    check: &CheckResult,
    verdict_line: &str,
    fix_line: &str,
) -> Option<String> {
    let verdicts: Vec<&str> = VERDICT_TOKENS
        .iter()
        .copied()
        .filter(|token| carries_word(verdict_line, token))
        .collect();
    if verdicts.len() != 1 {
        return Some(format!(
            "a check's verdict line must carry exactly one of PASS/WARN/FAIL/UNKNOWN, found {verdicts:?} in: {verdict_line}"
        ));
    }
    let named: Vec<&str> = ALL_CHECKS
        .iter()
        .filter(|id| names_check(verdict_line, **id))
        .map(|id| id.label())
        .collect();
    if named != [check.check.label()] {
        return Some(format!(
            "a check's verdict line must name exactly its own identity, found {named:?} in: {verdict_line}"
        ));
    }
    let stray: Vec<&str> = VERDICT_TOKENS
        .iter()
        .copied()
        .filter(|token| carries_word(fix_line, token))
        .collect();
    if !stray.is_empty() {
        return Some(format!(
            "a fix line must carry no verdict token (it would become a second report line for whichever check it names), found {stray:?} in: {fix_line}"
        ));
    }
    None
}

/// Write the report.
///
/// The shape is a contract the L2 tests depend on: **one verdict-bearing line
/// per check**, carrying that check's identity and exactly one of PASS / WARN
/// / FAIL / UNKNOWN. The fix deliberately goes on its own continuation line,
/// which keeps a phrase like "reverse tunnels" or "`AllowTcpForwarding yes`"
/// out of the line that is being matched for a single identity and a single
/// verdict. [`report_shape_violation`] states that contract precisely and the
/// `debug_assert!` below enforces it.
///
/// **Everything a producer controls is escaped on the way out**, through
/// [`escape_control_and_bidi`]: the registry name and target, the listen specs
/// and destinations `ssh -G` resolved, and above all ssh's own stderr, which
/// this quotes back at the user. A malicious remote binary, shell startup file
/// or ssh endpoint can otherwise emit CSI/OSC sequences that clear or repaint
/// the report, retitle the terminal, forge a hyperlink or drive the clipboard
/// — and the sharpest version of that is a diagnostic whose own security
/// conclusions the endpoint being diagnosed can visually falsify, needing no
/// local config write at all. Escaping rather than stripping is the deliberate
/// choice for a diagnostic: the user should be able to *see* that the remote
/// sent something peculiar. (Raw `sshd -T` output never reaches here —
/// `AllowTcpForwarding` becomes a closed enum and `ClientAliveInterval` a
/// `u64`. Keep it that way.)
fn render(
    out: &mut impl Write,
    name: &str,
    target: &SshTarget,
    observations: &Observations,
    checks: &[CheckResult],
    overall: Verdict,
) -> io::Result<()> {
    let safe_name = escape_control_and_bidi(name);
    writeln!(
        out,
        "Diagnosing remote '{safe_name}' at {}:{} (read-only)",
        escape_control_and_bidi(&target.user_host()),
        target.port
    )?;
    if let Some(detail) = &observations.ssh_detail {
        // First line only, and capped: ssh's own complaint is useful context
        // but a multi-line dump would drown the report. Escape AFTER taking
        // the first line and the first 200 characters, so an escape expanding
        // to several characters cannot smuggle bytes past the cap.
        let first = detail.lines().next().unwrap_or_default().trim();
        if !first.is_empty() {
            let quoted: String = first.chars().take(200).collect();
            writeln!(out, "ssh itself said: {}", escape_control_and_bidi(&quoted))?;
        }
    }
    writeln!(out)?;

    for check in checks {
        // `<name>` is substituted in BOTH the headline and the fix (review
        // note N3). Only fixes use the placeholder today; substituting both
        // removes the trap where a headline that adopts it silently renders
        // the literal `<name>`.
        let headline = escape_control_and_bidi(&check.headline).replace("<name>", &safe_name);
        let verdict_line = format!(
            "{verdict:<VERDICT_WIDTH$} {check_id:<CHECK_WIDTH$} {headline}",
            verdict = check.verdict.label(),
            check_id = check.check.label(),
        );
        let fix_line = if check.fix.is_empty() {
            String::new()
        } else {
            let indent = " ".repeat(VERDICT_WIDTH + 1);
            let fix = escape_control_and_bidi(&check.fix).replace("<name>", &safe_name);
            format!("{indent}-> {fix}")
        };
        debug_assert!(
            report_shape_violation(check, &verdict_line, &fix_line).is_none(),
            "{}",
            report_shape_violation(check, &verdict_line, &fix_line).unwrap_or_default()
        );

        writeln!(out, "{verdict_line}")?;
        if !fix_line.is_empty() {
            writeln!(out, "{fix_line}")?;
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

    // `for_observation`, not `with_wallclock_timeout`: see the module doc. An
    // ordinary executor would make every probe apply the user's `Host` block,
    // so the doctor would create the forwards it is here to inspect.
    let executor = SystemSshExecutor::for_observation(probe_timeout_secs());
    let observations = observe(&executor, &target, name);
    let mut checks = classify(&observations.inputs);
    if let Some(reason) = &observations.ssh_config_unreadable {
        mark_ssh_config_unreadable(&mut checks, reason);
    }
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
            forward_bound: Some(ForwardObservation::SocksVerified),
            forward_probe_unclassified: None,
        }
    }

    /// An [`SshExecutor`] that answers every command with one fixed result,
    /// so a probe's translation of a status/stderr pair can be tested without
    /// spawning ssh.
    struct FixedExecutor {
        status: i32,
        stderr: String,
    }

    impl FixedExecutor {
        fn exit(status: i32, stderr: &str) -> Self {
            Self {
                status,
                stderr: stderr.to_string(),
            }
        }
    }

    impl crate::remote::SshExecutor for FixedExecutor {
        fn run(
            &self,
            _target: &SshTarget,
            _command: &str,
        ) -> Result<crate::remote::SshOutput, crate::remote::SshError> {
            Ok(crate::remote::SshOutput {
                status: self.status,
                stdout: String::new(),
                stderr: self.stderr.clone(),
            })
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
        blocked.forward_bound = Some(ForwardObservation::Refused);
        let blocked_results = classify(&blocked);
        let blocked_check = check(&blocked_results, CheckId::AllowTcpForwarding);

        let mut collision = healthy_inputs();
        collision.forward_bound = Some(ForwardObservation::Foreign);
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
        inputs.forward_bound = Some(ForwardObservation::Refused);
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

    // -----------------------------------------------------------------
    // Audit regressions (PRD #345 review + security audit)
    // -----------------------------------------------------------------

    /// Every listen spec whose bind host must never reach the remote shell.
    ///
    /// The first entry is the audit's live proof of concept: a legal
    /// `RemoteForward "evil';id;#:1080"` in `~/.ssh/config` resolves, under
    /// OpenSSH 10.2, to `remoteforward [evil';id;#]:1080 [socks]:0`, and the
    /// old builder turned that into
    /// `bash -c 'exec 3<>/dev/tcp/evil';id;#/1080'` — running `id` on the
    /// remote under the authenticated account. The rest cover the other ways
    /// two levels of shell parsing can be reached.
    const HOSTILE_LISTEN_SPECS: &[&str] = &[
        "[evil';id;#]:1080",       // the audit's proof of concept
        "[$(id)]:1080",            // survives the outer quote entirely
        "[`id`]:1080",             // backtick substitution
        "[a;id]:1080",             // bare command separator
        "[a|id]:1080",             // pipeline
        "[a&id]:1080",             // background / list operator
        "[a b]:1080",              // whitespace splits the nested argv
        "[a\tb]:1080",             // and so does a tab
        "[../../etc/passwd]:1080", // `/` escapes the /dev/tcp path shape
        "[a/b]:1080",              // any `/` at all
        "[a>b]:1080",              // redirection
        "[a<b]:1080",
        "[a*]:1080", // glob
        "[a?]:1080",
        "[a\\b]:1080",       // backslash
        "[a\"b]:1080",       // double quote
        "[a'b]:1080",        // single quote on its own
        "[a$b]:1080",        // parameter expansion
        "[a\nb]:1080",       // embedded newline
        "[a\rb]:1080",       // carriage return
        "[a\x1bb]:1080",     // ESC
        "[a\0b]:1080",       // NUL
        "[a\u{202e}b]:1080", // bidi override
        "[héllo]:1080",      // non-ASCII
    ];

    /// Scenario: Feed `probe_endpoint` every hostile listen spec the audit
    /// found or implied. Each is refused outright, so no remote command is ever
    /// constructed from it and no probe silently reports the port as free.
    #[test]
    fn probe_endpoint_rejects_every_shell_metacharacter_in_a_bind_host() {
        for spec in HOSTILE_LISTEN_SPECS {
            assert_eq!(
                probe_endpoint(spec),
                None,
                "hostile listen spec {spec:?} must be refused before a command is built"
            );
        }

        // The port half was already inert (`u16`), but prove it stays that way
        // rather than falling through to some other parse.
        for spec in ["1080; id", "[127.0.0.1]:$(id)", "[127.0.0.1]:70000"] {
            assert_eq!(probe_endpoint(spec), None, "bad port in {spec:?}");
        }
    }

    /// Scenario: Feed `probe_endpoint` the address forms the recipe actually
    /// uses; each resolves to a loopback-or-literal endpoint, and the command
    /// built from it contains nothing either shell would interpret.
    #[test]
    fn probe_endpoint_accepts_the_supported_forms_and_builds_an_inert_command() {
        let cases = [
            ("1080", "127.0.0.1", 1080u16),
            ("127.0.0.1:1080", "127.0.0.1", 1080),
            ("localhost:1080", "localhost", 1080),
            ("db-1.internal.example:8080", "db-1.internal.example", 8080),
            ("[::1]:1080", "::1", 1080),
            // A GLOBAL IPv6 literal. The link-local block used to be pinned
            // here too and is now refused — see
            // `probe_endpoint_refuses_a_link_local_bind_instead_of_guessing`.
            ("[2001:db8::1]:1080", "2001:db8::1", 1080),
            // Wildcard binds are probed on loopback: that is the address an
            // agent ON the remote would use. The loopback matches the FAMILY
            // of the wildcard — see
            // `probe_endpoint_probes_an_ipv6_wildcard_on_the_ipv6_loopback`,
            // which is where the reasoning for the `::` rows lives.
            ("*:1080", "127.0.0.1", 1080),
            ("0.0.0.0:1080", "127.0.0.1", 1080),
            ("[::]:1080", "::1", 1080),
            ("[0:0:0:0:0:0:0:0]:1080", "::1", 1080),
            (":1080", "127.0.0.1", 1080),
        ];

        for (spec, host, port) in cases {
            assert_eq!(
                probe_endpoint(spec),
                Some((host.to_string(), port)),
                "supported listen spec {spec:?}"
            );
            for probe in [ForwardProbe::ConnectOnly, ForwardProbe::SocksHandshake] {
                let command = forward_probe_command(probe, host, port);
                // The remote login shell strips the outer quotes and a nested
                // bash parses the result, so the interpolated endpoint has to
                // be inert to BOTH. Only the endpoint comes from the user's
                // config; the rest of the command is a literal this module
                // authors, pinned verbatim by
                // `forward_probe_command_is_pinned_for_both_forward_kinds`.
                let endpoint = format!("/dev/tcp/{host}/{port}");
                assert!(
                    command.contains(&endpoint),
                    "{probe:?} must target the resolved endpoint: {command:?}"
                );
                let inert = endpoint
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-/:.".contains(c));
                assert!(
                    inert,
                    "interpolated endpoint carries a metacharacter: {endpoint:?}"
                );
            }
        }
    }

    /// Scenario: Resolve every wildcard listen spec `ssh -G` can emit and
    /// check the probe targets the loopback of the SAME address family, then
    /// pin the command built for an IPv6 endpoint. An IPv6-only `[::]`
    /// listener does not answer at `127.0.0.1`, so the old IPv4 mapping
    /// turned a working tunnel into a confident `ForwardBound` FAIL.
    #[test]
    fn probe_endpoint_probes_an_ipv6_wildcard_on_the_ipv6_loopback() {
        // The regression itself. This suite previously pinned
        // `("[::]:1080", "127.0.0.1", 1080)` — it did not miss the case, it
        // ENCODED the defect, which is worse than no coverage because it
        // promoted a bug to a guarantee.
        //
        // Measured against a real `IPV6_V6ONLY` listener bound to `[::]`:
        // `bash -c 'exec 3<>/dev/tcp/127.0.0.1/<port>'` is refused while the
        // same command against `::1` connects and completes the SOCKS5
        // greeting. Dual-stack Linux with the default `net.ipv6.bindv6only=0`
        // hides this, because a `[::]` listener there also accepts IPv4 via
        // v4-mapped addresses.
        assert_eq!(
            probe_endpoint("[::]:1080"),
            Some(("::1".to_string(), 1080)),
            "an IPv6 wildcard bind must be probed at the IPv6 loopback"
        );

        // `ssh -G` echoes an IPv6 listen host verbatim instead of
        // canonicalising it (verified against OpenSSH 10.2:
        // `RemoteForward [0:0:0:0:0:0:0:0]:1080` resolves to
        // `remoteforward [0:0:0:0:0:0:0:0]:1080`), so the long spelling of
        // the unspecified address reaches here unchanged and needs the same
        // treatment. Matching on parsed bits rather than on spelling is what
        // makes that fall out instead of needing its own arm.
        assert_eq!(
            probe_endpoint("[0:0:0:0:0:0:0:0]:1080"),
            Some(("::1".to_string(), 1080)),
            "the long spelling of the IPv6 wildcard is the same wildcard"
        );

        // The IPv4 wildcard is unchanged, and an explicit literal of either
        // family is never rewritten — only the *unspecified* address is a
        // wildcard.
        for (spec, host) in [
            ("0.0.0.0:1080", "127.0.0.1"),
            ("*:1080", "127.0.0.1"),
            (":1080", "127.0.0.1"),
            ("[::1]:1080", "::1"),
            ("[2001:db8::1]:1080", "2001:db8::1"),
        ] {
            assert_eq!(
                probe_endpoint(spec),
                Some((host.to_string(), 1080)),
                "{spec:?} must resolve to {host:?}"
            );
        }

        // The command built for an IPv6 endpoint, pinned in the shape that
        // was measured to work: a BARE IPv6 literal in the `/dev/tcp` path,
        // with no brackets. Bash 5.3 connects to `/dev/tcp/::1/<port>` and
        // completes the handshake; brackets would make it a different,
        // untested path shape.
        assert_eq!(
            forward_probe_command(ForwardProbe::ConnectOnly, "::1", 1080),
            "bash -c 'exec 3<>/dev/tcp/::1/1080'"
        );
        let handshake = forward_probe_command(ForwardProbe::SocksHandshake, "::1", 1080);
        assert!(
            handshake.starts_with("bash -c 'exec 3<>/dev/tcp/::1/1080 || exit 1;"),
            "the IPv6 endpoint must be interpolated bare: {handshake:?}"
        );
        assert!(
            !handshake.contains('[') && !handshake.contains(']'),
            "brackets around the IPv6 host are not the measured shape: {handshake:?}"
        );
    }

    /// Scenario: Build both remote probes for one endpoint and pin them
    /// verbatim. The three properties an edit is most likely to lose — the
    /// connect deciding the exit status, the bounded read, and the 127 for
    /// absent tooling — are all spelling, so they are asserted as spelling.
    /// Two of them are spelling the PRD #345 second audit had to correct:
    /// `head` must be in the `command -v` guard (its absence manufactured a
    /// silent-listener FAIL out of a missing coreutil) and `timeout` must
    /// carry `-k` (a TERM-ignoring child otherwise outlives its deadline).
    #[test]
    fn forward_probe_command_is_pinned_for_both_forward_kinds() {
        assert_eq!(
            forward_probe_command(ForwardProbe::ConnectOnly, "127.0.0.1", 1080),
            "bash -c 'exec 3<>/dev/tcp/127.0.0.1/1080'"
        );
        assert_eq!(
            forward_probe_command(ForwardProbe::SocksHandshake, "127.0.0.1", 1080),
            "bash -c 'exec 3<>/dev/tcp/127.0.0.1/1080 || exit 1; \
             command -v timeout >/dev/null 2>&1 && command -v head >/dev/null 2>&1 && command -v od >/dev/null 2>&1 || exit 127; \
             printf \"\\005\\001\\000\" >&3; \
             timeout -k 1 2 head -c 2 <&3 | od -An -tx1; \
             exit 0'"
        );

        // Every tool the pipeline execs is guarded. Asserted by name rather
        // than by the pinned string above so a rewrite of the command that
        // drops one clause fails with the reason rather than with a diff.
        let handshake = forward_probe_command(ForwardProbe::SocksHandshake, "127.0.0.1", 1080);
        for tool in ["timeout", "head", "od"] {
            assert!(
                handshake.contains(&format!("command -v {tool} >/dev/null 2>&1")),
                "{tool} must be guarded, or its absence becomes a false FAIL: {handshake:?}"
            );
        }

        // `remote/doctor/005`'s argv denylist, restated here so a failing
        // spelling is caught in the fast tier rather than in the L2 run.
        for mutating_form in [
            " rm ",
            " mv ",
            " cp ",
            " touch ",
            " mkdir ",
            " chmod ",
            " chown ",
            " sed -i",
            " tee ",
            "systemctl ",
            "service ",
            "sshd_config",
            ">>",
        ] {
            assert!(
                !handshake.to_ascii_lowercase().contains(mutating_form),
                "the probe must stay read-only, found {mutating_form:?} in {handshake:?}"
            );
        }
    }

    /// Scenario: Build the probe for a CONCRETE reverse forward and confirm it
    /// carries no spelling of the SOCKS5 greeting. That port holds whatever the
    /// user tunnelled — a database, an internal API — so the three bytes must
    /// never be written into it.
    #[test]
    fn connect_only_probe_never_writes_the_socks_greeting() {
        let concrete = ResolvedForward::Remote {
            listen: "1080".to_string(),
            destination: "db.internal.example:5432".to_string(),
        };
        assert_eq!(
            ForwardProbe::for_forward(&concrete),
            ForwardProbe::ConnectOnly
        );
        // A forward kind added later must inherit the SAFE probe, not the
        // handshake, so the laptop-side directions are checked too.
        for other in [
            ResolvedForward::Dynamic {
                listen: "9099".to_string(),
            },
            ResolvedForward::Local {
                listen: "8080".to_string(),
                destination: "localhost:80".to_string(),
            },
        ] {
            assert_eq!(ForwardProbe::for_forward(&other), ForwardProbe::ConnectOnly);
        }
        assert_eq!(
            ForwardProbe::for_forward(&ResolvedForward::RemoteDynamic {
                listen: "1080".to_string()
            }),
            ForwardProbe::SocksHandshake
        );

        let command = forward_probe_command(ForwardProbe::ConnectOnly, "127.0.0.1", 1080);
        // The denylist is over the spellings THIS module could emit, which is
        // why it lives next to the emitter rather than in a test that would
        // have to guess: the octal escapes above, the hex the parser compares
        // against, and `printf` / `>&3` as the mechanism either would need.
        for greeting in [
            "\\005", "\\001", "\\000", "\\x05", "\\x01", "\\x00", "05 01 00", "050100", "printf",
            ">&3",
        ] {
            assert!(
                !command.contains(greeting),
                "a connect-only probe must write nothing, found {greeting:?} in {command:?}"
            );
        }
    }

    /// Scenario: Read the same SOCKS5 acceptance as `od -An -tx1`, `xxd -p`
    /// and a bare hex pair spell it. The deck must not care which tool the
    /// remote had, so whitespace is normalised before comparing.
    #[test]
    fn socks_reply_normalises_whitespace_before_comparing() {
        for spelling in [" 05 00", "05 00", "05 00\n", "0500", "0500\n", " 05 00 \n"] {
            assert_eq!(
                socks_reply(spelling),
                SocksReply::Accepted,
                "an acceptance spelled {spelling:?} must read as one observation"
            );
        }
        for spelling in [" 48 54", "4854", "05", "05 01", "ff ff"] {
            assert_eq!(
                socks_reply(spelling),
                SocksReply::Other,
                "a non-acceptance spelled {spelling:?} is a foreign listener"
            );
        }
        for silence in ["", "\n", "   ", " \n \n"] {
            assert_eq!(
                socks_reply(silence),
                SocksReply::Silent,
                "an empty reply spelled {silence:?} is a silent listener"
            );
        }
    }

    /// Scenario: Classify each of the five listener observations. A verified
    /// SOCKS handshake is the only clear one; a foreign or silent squatter
    /// fails; an unattributable concrete listener is UNKNOWN, never a
    /// confident answer in either direction.
    #[test]
    fn classify_maps_every_listener_observation_to_its_own_verdict() {
        let cases = [
            (ForwardObservation::SocksVerified, Verdict::Pass, 0),
            (ForwardObservation::Foreign, Verdict::Fail, 1),
            (ForwardObservation::Silent, Verdict::Fail, 1),
            (ForwardObservation::Unattributable, Verdict::Unknown, 2),
            (ForwardObservation::Refused, Verdict::Fail, 1),
        ];
        let mut headlines = Vec::new();
        for (observation, verdict, exit) in cases {
            let mut inputs = healthy_inputs();
            inputs.forward_bound = Some(observation);
            let results = classify(&inputs);
            let result = check(&results, CheckId::ForwardBound);
            assert_eq!(result.verdict, verdict, "{observation:?}: {result:#?}");
            let overall = overall_verdict(&results);
            assert_eq!(overall.exit_code(), exit, "{observation:?}: {overall:?}");
            headlines.push(result.headline.clone());
        }
        // Each observation has to explain itself; two of them sharing a
        // sentence would hand the user the same line for different causes.
        for (i, one) in headlines.iter().enumerate() {
            for other in &headlines[i + 1..] {
                assert_ne!(one, other, "two observations share a headline: {one}");
            }
        }
    }

    /// Scenario: A verified SOCKS listener is the one PASS this check can
    /// give, so its prose must carry the evidence — that the handshake was
    /// answered — rather than claiming the port was merely reachable.
    #[test]
    fn verified_socks_pass_states_its_evidence() {
        let result = check(&classify(&healthy_inputs()), CheckId::ForwardBound).clone();
        assert_eq!(result.verdict, Verdict::Pass);
        let headline = result.headline.to_ascii_lowercase();
        assert!(headline.contains("socks"), "{result:#?}");
        assert!(
            headline.contains("verified")
                || headline.contains("handshake")
                || headline.contains("confirmed"),
            "a confident PASS must say WHY it is confident: {result:#?}"
        );
    }

    /// Scenario: Something accepts the connection on a concrete reverse
    /// forward. PASS and WARN both exit 0 and FAIL would call a healthy
    /// configuration broken, so the honest answer is UNKNOWN with a caveat
    /// saying the listener could not be attributed.
    #[test]
    fn concrete_forward_with_a_listener_is_unknown_not_pass_or_fail() {
        let mut inputs = healthy_inputs();
        inputs.ssh.forwards = vec![ResolvedForward::Remote {
            listen: "1080".to_string(),
            destination: "db.internal.example:5432".to_string(),
        }];
        inputs.forward_bound = Some(ForwardObservation::Unattributable);
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardBound);

        assert_eq!(result.verdict, Verdict::Unknown, "{result:#?}");
        let headline = result.headline.to_ascii_lowercase();
        assert!(
            headline.contains("attribute")
                || headline.contains("verify")
                || headline.contains("ownership"),
            "the caveat must say why this is not a confident answer: {result:#?}"
        );
        assert_eq!(overall_verdict(&results), Verdict::Unknown);
        assert_eq!(overall_verdict(&results).exit_code(), 2);
    }

    /// Scenario: Resolve every spelling of an IPv6 link-local listen spec.
    /// Each is refused before a command is built, so it reaches the report as
    /// UNKNOWN naming the value instead of as a bash `Invalid argument` the
    /// probe would have read as a confident FAIL.
    #[test]
    fn probe_endpoint_refuses_a_link_local_bind_instead_of_guessing() {
        // `fe80::/10`, so the whole block and not just the `fe80:` spelling.
        // A link-local address needs a zone index to name an interface, `%` is
        // not in `is_probeable_bind_host`'s allowlist and is not being added
        // to one that feeds a two-shell-level interpolation, so a command
        // built from any of these can only come back as `Invalid argument`.
        for spec in [
            "[fe80::1]:1080",
            "[fe80::1%eth0]:1080",
            "[fe80:0:0:0:0:0:0:1]:1080",
            "[febf::1]:1080",
            "[fe80::a00:27ff:fe4e:66a1]:1080",
        ] {
            assert_eq!(
                probe_endpoint(spec),
                None,
                "a link-local bind is not connectable without a zone index, so it must not be probed: {spec:?}"
            );
        }

        // The refusal is `fe80::/10` and nothing wider. Global, unique-local,
        // loopback and both wildcards were measured working and stay probed —
        // "anything unusual" would have been a much bigger refusal than the
        // evidence supports.
        for (spec, host) in [
            ("[2001:db8::1]:1080", "2001:db8::1"),
            ("[fec0::1]:1080", "fec0::1"),
            ("[fd00::1]:1080", "fd00::1"),
            ("[fe7f::1]:1080", "fe7f::1"),
            ("[fc00::1]:1080", "fc00::1"),
            ("[::1]:1080", "::1"),
            ("[::]:1080", "::1"),
            ("169.254.1.1:1080", "169.254.1.1"),
        ] {
            assert_eq!(
                probe_endpoint(spec),
                Some((host.to_string(), 1080)),
                "{spec:?} connects without a zone index and must still be probed"
            );
        }

        // And it lands on the path that already exists for a spec we will not
        // probe: UNKNOWN naming the value, never a verdict about the port.
        let mut inputs = healthy_inputs();
        inputs.ssh.forwards = vec![ResolvedForward::RemoteDynamic {
            listen: "[fe80::1]:1080".to_string(),
        }];
        inputs.forward_bound = None;
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardBound);
        assert_eq!(
            result.verdict,
            Verdict::Unknown,
            "a link-local bind must not produce a confident verdict: {result:#?}"
        );
        assert!(
            result.headline.contains("fe80::1"),
            "the hint must name the spec the user has to change: {result:#?}"
        );
    }

    /// Scenario: The remote's shell fails the loopback connect with an error
    /// the deck has no reading for. `ForwardBound` is UNKNOWN quoting that
    /// error, not FAIL with port-collision remediation — a recognised
    /// `Connection refused` is still the only thing that produces `Refused`.
    #[test]
    fn an_unrecognised_probe_failure_is_unknown_carrying_its_evidence() {
        // The one shape that IS evidence about the port. The L2 stub emits
        // exactly this line (`remote/doctor/008`), and it must keep meaning
        // FAIL.
        for stderr in [
            "bash: connect: Connection refused",
            "bash: /dev/tcp/127.0.0.1/1080: Connection refused",
            "BASH: CONNECT: CONNECTION REFUSED",
        ] {
            assert!(
                is_recognised_refusal(stderr),
                "a refused connect is the module's one confident exit-1 reading: {stderr:?}"
            );
        }

        // Everything else names a different cause, and none of them says the
        // port is free. `Invalid argument` is the link-local case that started
        // this; `Network is unreachable` is IPv6 switched off on the remote,
        // which would have called a working v4 tunnel broken.
        for stderr in [
            "bash: /dev/tcp/fe80::1/1080: Invalid argument",
            "bash: connect: Network is unreachable",
            "bash: connect: Connection timed out",
            "bash: connect: No route to host",
            "bash: /dev/tcp/127.0.0.1/1080: No such file or directory",
            "bash: /dev/tcp/127.0.0.1/1080: Operation not supported",
            "bash: connexion refusee",
            "",
        ] {
            assert!(
                !is_recognised_refusal(stderr),
                "an unclassified failure must not be read as a refusal: {stderr:?}"
            );
        }

        // Drive the probe itself, because `is_recognised_refusal` only
        // matters through the arm that consults it. A recognised refusal is
        // still an observation; everything else now carries its evidence out
        // instead of becoming `Refused`.
        let forward = ResolvedForward::RemoteDynamic {
            listen: "1080".to_string(),
        };
        let target = SshTarget {
            host: "prod.example.test".to_string(),
            user: None,
            port: 22,
            key: None,
        };
        assert!(
            matches!(
                probe_forward_bound(
                    &FixedExecutor::exit(1, "bash: connect: Connection refused"),
                    &target,
                    &forward,
                ),
                ProbeOutcome::Observed(ForwardObservation::Refused)
            ),
            "a recognised refusal must stay the confident FAIL the L2 suite pins"
        );
        for stderr in [
            "bash: /dev/tcp/fe80::1/1080: Invalid argument",
            "bash: connect: Network is unreachable",
            "bash: /dev/tcp/127.0.0.1/1080: No such file or directory",
            "",
        ] {
            let outcome = probe_forward_bound(&FixedExecutor::exit(1, stderr), &target, &forward);
            match outcome {
                ProbeOutcome::Unclassified(detail) => assert_eq!(
                    detail,
                    stderr.trim(),
                    "the evidence must reach the report verbatim: {stderr:?}"
                ),
                other => panic!(
                    "an unrecognised exit-1 must not become an observation about the port: \
                     {stderr:?} produced {}",
                    match other {
                        ProbeOutcome::Observed(o) => format!("{o:?}"),
                        ProbeOutcome::NotObserved => "NotObserved".to_string(),
                        ProbeOutcome::Unclassified(_) => unreachable!(),
                    }
                ),
            }
        }

        // What the user is shown: UNKNOWN, the observed stderr quoted, and
        // none of the collision remediation the `Refused` arm carries.
        let mut inputs = healthy_inputs();
        inputs.forward_bound = None;
        inputs.forward_probe_unclassified =
            Some("bash: /dev/tcp/fe80::1/1080: Invalid argument".to_string());
        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardBound);

        assert_eq!(
            result.verdict,
            Verdict::Unknown,
            "an observation the probe could not classify cannot be a confident FAIL: {result:#?}"
        );
        assert!(
            result.fix.contains("Invalid argument"),
            "the hint must carry the evidence the verdict was withheld on: {result:#?}"
        );
        let text = format!("{} {}", result.headline, result.fix).to_ascii_lowercase();
        assert!(
            !text.contains("give this laptop its own listen port"),
            "remediation for a cause nothing established is the defect being fixed: {result:#?}"
        );
        assert_eq!(overall_verdict(&results), Verdict::Unknown);
        assert_eq!(overall_verdict(&results).exit_code(), 2);

        // An exit-1 with nothing on stderr is still not a refusal, and the
        // hint has to word the absence rather than quote an empty string.
        let mut silent = healthy_inputs();
        silent.forward_bound = None;
        silent.forward_probe_unclassified = Some(String::new());
        let results = classify(&silent);
        let result = check(&results, CheckId::ForwardBound);
        assert_eq!(result.verdict, Verdict::Unknown, "{result:#?}");
        assert!(
            !result.fix.contains("``"),
            "an empty stderr must not render as an empty quote: {result:#?}"
        );

        // The evidence is producer-controlled, so it must be escaped on the
        // way out like every other quoted remote string.
        let mut hostile = healthy_inputs();
        hostile.forward_bound = None;
        hostile.forward_probe_unclassified =
            Some("bash: \u{1b}[2Jcleared: Invalid argument".to_string());
        let report = rendered(
            "prod",
            &Observations {
                inputs: hostile,
                ssh_detail: None,
                ssh_config_unreadable: None,
            },
        );
        assert!(
            !report.contains('\u{1b}'),
            "the quoted stderr must be escaped, not emitted raw: {report}"
        );
        assert!(
            report.contains("Invalid argument"),
            "escaping must not swallow the evidence: {report}"
        );
    }

    /// Scenario: A `ssh -G` dump resolves a bind host the probe refuses. The
    /// live-bind check reports UNKNOWN and names the exact rejected spec, so
    /// the user can find it — never PASS, and never a constructed command.
    #[test]
    fn classify_names_the_rejected_listen_spec_instead_of_probing_it() {
        let mut inputs = healthy_inputs();
        inputs.ssh.forwards = vec![ResolvedForward::RemoteDynamic {
            listen: "[evil';id;#]:1080".to_string(),
        }];
        // What `observe` would have recorded: refused, so never observed.
        inputs.forward_bound = None;

        let results = classify(&inputs);
        let result = check(&results, CheckId::ForwardBound);

        assert_eq!(result.verdict, Verdict::Unknown);
        assert!(
            result.headline.contains("evil';id;#"),
            "the hint must name the rejected value: {result:#?}"
        );
        assert_ne!(overall_verdict(&results), Verdict::Pass);
    }

    /// Scenario: The remote cannot answer `daemon hello` at all — too old for
    /// the subcommand, or a broken install. That is fatal to `connect`, so the
    /// doctor reports FAIL — a remote you cannot attach to must never read as
    /// an all-clear diagnosis.
    #[test]
    fn classify_unanswered_handshake_is_fatal_not_advisory() {
        let mut inputs = healthy_inputs();
        inputs.protocol_compatible = Some(false);
        let results = classify(&inputs);
        let result = check(&results, CheckId::ProtocolCompatible);

        assert_eq!(
            result.verdict,
            Verdict::Fail,
            "`connect` refuses a remote whose handshake it cannot get an answer from \
             (src/connect.rs `RemoteHandshakeUnsupported`), so the doctor must not downgrade \
             it below what connect enforces: {result:#?}"
        );
        assert_eq!(overall_verdict(&results), Verdict::Fail);
        assert_eq!(overall_verdict(&results).exit_code(), 1);
        assert!(result.fix.contains("remote upgrade"), "{result:#?}");
        // Issue #491: a version *difference* is no longer what this reports,
        // and the fix text must not send the user chasing one.
        assert!(
            !result.headline.contains("version differs"),
            "the headline must not claim a version comparison the code no longer makes: \
             {result:#?}"
        );
    }

    /// Scenario: Map each aggregate verdict to its exit code. A failed check
    /// and an incomplete diagnosis are both non-zero but distinguishable, so
    /// "healthy tunnel, sshd unreadable without root" is scriptable.
    #[test]
    fn verdict_exit_codes_separate_failure_from_incompleteness() {
        assert_eq!(Verdict::Pass.exit_code(), 0);
        assert_eq!(Verdict::Warn.exit_code(), 0);
        assert_eq!(Verdict::Fail.exit_code(), 1);
        assert_eq!(Verdict::Unknown.exit_code(), 2);

        // The case the split exists for: everything readable is healthy, but
        // `sshd -T` needed root we did not have.
        let mut inputs = healthy_inputs();
        inputs.sshd = ResolvedSshdConfig::default();
        let verdict = overall_verdict(&classify(&inputs));
        assert_eq!(verdict, Verdict::Unknown);
        assert_eq!(verdict.exit_code(), 2);

        // And it is still distinguishable from a genuinely broken tunnel.
        let mut broken = healthy_inputs();
        broken.forward_bound = Some(ForwardObservation::Refused);
        assert_eq!(overall_verdict(&classify(&broken)).exit_code(), 1);

        // Both non-zero: an UNKNOWN never reads as PASS.
        assert_ne!(Verdict::Unknown.exit_code(), 0);
    }

    /// Scenario: `ExitOnForwardFailure` is unset with and without a reverse
    /// tunnel configured. The two verdicts get different headlines, so someone
    /// comparing two runs is not left with only the verdict token to go on.
    #[test]
    fn exit_on_forward_failure_headlines_differ_between_fail_and_warn() {
        for setting in [Some(false), None] {
            let mut with_tunnel = healthy_inputs();
            with_tunnel.ssh.exit_on_forward_failure = setting;
            let failing = check(&classify(&with_tunnel), CheckId::ExitOnForwardFailure).clone();

            let mut without_tunnel = with_tunnel.clone();
            without_tunnel.ssh.forwards.clear();
            without_tunnel.forward_bound = None;
            let warning = check(&classify(&without_tunnel), CheckId::ExitOnForwardFailure).clone();

            assert_eq!(failing.verdict, Verdict::Fail, "{failing:#?}");
            assert_eq!(warning.verdict, Verdict::Warn, "{warning:#?}");
            assert_ne!(
                failing.headline, warning.headline,
                "the two forms must not be separable only by their verdict token: {failing:#?}"
            );
            assert!(
                warning
                    .headline
                    .contains("harmless until you configure a tunnel"),
                "the WARN form must say why it is milder: {warning:#?}"
            );
        }
    }

    /// Scenario: A status-0 `sshd -T` run prints a real dump alongside a benign
    /// warning containing "not found". The dump is still parsed — only the
    /// permission markers, and a non-zero status, discard it.
    #[test]
    fn sshd_t_benign_status_zero_warning_does_not_discard_a_real_dump() {
        let dump = "allowtcpforwarding yes\nclientaliveinterval 30\n";
        for benign in [
            "/etc/ssh/sshd_config.d/50-cloud.conf: line 3: Deprecated option; key not found",
            "Could not open /etc/ssh/moduli: no such file, continuing",
        ] {
            let sshd = parse_sshd_t(0, dump, benign);
            assert_eq!(
                sshd.allow_tcp_forwarding,
                Some(AllowTcpForwarding::Yes),
                "a benign warning must not discard a readable dump: {benign:?}"
            );
            assert_eq!(sshd.client_alive_interval, Some(30));
        }

        // The markers that DO mean "we were not allowed to look" still bite.
        for denial in ["Permission denied", "Operation not permitted"] {
            assert_eq!(parse_sshd_t(0, dump, denial).allow_tcp_forwarding, None);
        }
    }

    /// Scenario: `ssh -G` could not be read at all. Every check derived from it
    /// becomes UNKNOWN with the reason, rather than reporting an empty parse as
    /// definitive "no forward is configured".
    #[test]
    fn unreadable_ssh_config_is_unknown_not_definitive_absence() {
        // What the old code did: an unreadable dump parsed to an empty config,
        // which classifies as a confident FAIL.
        let confident = classify(&DoctorInputs::default());
        assert_eq!(
            check(&confident, CheckId::RemoteForward).verdict,
            Verdict::Fail
        );

        let mut checks = confident.clone();
        mark_ssh_config_unreadable(&mut checks, "`ssh -G` exited with status 255");

        for id in [
            CheckId::RemoteForward,
            CheckId::DynamicForward,
            CheckId::ExitOnForwardFailure,
            CheckId::ForwardAgent,
        ] {
            let result = check(&checks, id);
            assert_eq!(result.verdict, Verdict::Unknown, "{result:#?}");
            assert!(result.fix.contains("status 255"), "{result:#?}");
        }
        // Checks that never came from the dump are untouched.
        assert_eq!(
            check(&checks, CheckId::HostReachable).verdict,
            check(&confident, CheckId::HostReachable).verdict
        );

        // And the substituted text still honours the report's shape contract,
        // which `render`'s `debug_assert!` enforces on the way through.
        let target = SshTarget {
            host: "prod.example.test".to_string(),
            user: None,
            port: 22,
            key: None,
        };
        let observations = Observations {
            inputs: DoctorInputs::default(),
            ssh_detail: None,
            ssh_config_unreadable: Some("`ssh -G` exited with status 255".to_string()),
        };
        let mut out: Vec<u8> = Vec::new();
        let overall = overall_verdict(&checks);
        render(&mut out, "prod", &target, &observations, &checks, overall)
            .expect("rendering to a Vec cannot fail");
        assert!(String::from_utf8_lossy(&out).contains("could not be read"));
    }

    fn rendered(name: &str, observations: &Observations) -> String {
        let target = SshTarget {
            host: "prod.example.test".to_string(),
            user: Some("deck".to_string()),
            port: 2222,
            key: None,
        };
        let checks = classify(&observations.inputs);
        let overall = overall_verdict(&checks);
        let mut out: Vec<u8> = Vec::new();
        render(&mut out, name, &target, observations, &checks, overall)
            .expect("rendering to a Vec cannot fail");
        String::from_utf8(out).expect("the report is UTF-8")
    }

    /// Scenario: A hostile remote answers the version probe with terminal
    /// escape sequences and a bidi override, and the registry name carries them
    /// too. The report escapes every one of them, so the endpoint being
    /// diagnosed cannot repaint the diagnosis of itself.
    #[test]
    fn render_escapes_producer_controlled_text_instead_of_emitting_it() {
        let mut inputs = healthy_inputs();
        inputs.ssh.forwards = vec![ResolvedForward::Remote {
            // The listen spec and destination come straight from `ssh -G`.
            listen: "\u{202e}1080".to_string(),
            destination: "db\x1b]0;pwn\x07.internal:5432".to_string(),
        }];
        let observations = Observations {
            inputs,
            // ssh's own stderr, quoted back at the user verbatim before this.
            ssh_detail: Some(
                "\x1b[2J\x1b[Hcleared the screen\r\nPASS AllowTcpForwarding all fine".to_string(),
            ),
            ssh_config_unreadable: None,
        };
        let report = rendered("prod\x1b[31m\u{202e}", &observations);

        assert!(
            !report
                .chars()
                .any(|c| (c.is_control() && c != '\n')
                    || crate::untrusted_text::is_bidi_format_char(c)),
            "a live control or bidi character survived into the report:\n{report:?}"
        );
        // Escaped, not stripped: a diagnostic should SHOW that the remote sent
        // something peculiar.
        assert!(report.contains("\\u{1b}"), "{report}");
        assert!(report.contains("\\u{202e}"), "{report}");
        // The quote is still first-line-only, so the forged verdict line that
        // followed the CR/LF never reaches the report at all.
        assert!(
            !report.contains("all fine"),
            "only ssh's first line may be quoted:\n{report}"
        );
    }

    /// Scenario: Render a healthy report and read it back the way the L2 tests
    /// do. Every check identity appears on exactly one verdict-bearing line,
    /// and `ForwardBound`'s fix names another check without becoming a second
    /// such line.
    #[test]
    fn render_holds_the_one_check_one_verdict_per_line_contract() {
        let mut inputs = healthy_inputs();
        // The load-bearing case: this FAIL's fix names `AllowTcpForwarding`.
        inputs.forward_bound = Some(ForwardObservation::Refused);
        inputs.sshd.allow_tcp_forwarding = Some(AllowTcpForwarding::No);
        let observations = Observations {
            inputs,
            ssh_detail: None,
            ssh_config_unreadable: None,
        };
        let report = rendered("prod", &observations);

        assert!(
            report.contains("AllowTcpForwarding` on the remote first"),
            "the fix that makes this contract load-bearing is gone:\n{report}"
        );
        for id in ALL_CHECKS {
            let matches: Vec<&str> = report
                .lines()
                .filter(|line| {
                    names_check(line, *id) && VERDICT_TOKENS.iter().any(|t| carries_word(line, t))
                })
                .collect();
            assert_eq!(
                matches.len(),
                1,
                "check {} must appear on exactly one verdict-bearing line, found {matches:#?}\n\n{report}",
                id.label()
            );
        }
    }

    /// Scenario: Hand the shape invariant the two mistakes it exists to catch —
    /// a fix line that says FAIL, and a headline that names a second check —
    /// and confirm each is reported rather than silently shipped.
    #[test]
    fn report_shape_violation_catches_a_fix_that_carries_a_verdict() {
        let result = CheckResult::new(
            CheckId::ForwardBound,
            Verdict::Fail,
            "port 1080 is not bound on the remote",
            "fix it",
        );
        let good = "FAIL    ForwardBound         port 1080 is not bound on the remote";
        assert_eq!(
            report_shape_violation(&result, good, "        -> Fix `AllowTcpForwarding` first."),
            None,
            "naming another check in a FIX line is allowed and load-bearing"
        );

        // A fix that also carries a verdict token becomes a second report line
        // for whichever check it names.
        assert!(
            report_shape_violation(
                &result,
                good,
                "        -> Fix `AllowTcpForwarding` or this will FAIL."
            )
            .is_some()
        );
        // A headline that names a second check makes both unfindable.
        assert!(
            report_shape_violation(
                &result,
                "FAIL    ForwardBound         AllowTcpForwarding is to blame",
                ""
            )
            .is_some()
        );
        // And a verdict line with two verdict tokens is ambiguous.
        assert!(
            report_shape_violation(&result, "FAIL    ForwardBound  it did not PASS", "").is_some()
        );
    }
}
