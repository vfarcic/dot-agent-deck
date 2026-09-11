//! PRD #741 M10 — `Test connection`, per endpoint, in settings.
//!
//! It exercises the whole transport stack — resolve the endpoint, open the
//! tunnel, speak `Hello`, classify the answer — without any screen having to
//! work first, which is what makes it the early user-testable milestone and
//! also the thing that says whether M5 and M6 got it right.
//!
//! # Every outcome is a distinct named state
//!
//! [`EndpointTestState`] has twelve variants and not one of them is "failed".
//! That is the whole design: "your ssh config has never seen this host's key"
//! and "the deck over there is not running" and "this build and that daemon
//! disagree about the wire" are three different things for a user to do next,
//! and collapsing them into one red banner is how a user ends up re-typing a
//! hostname that was never wrong. Two of the twelve — [`EndpointTestState::UnknownDeck`]
//! and [`EndpointTestState::NoRemoteSocket`] — are `SelectionFallback`'s own
//! two cases, reported honestly rather than as "connected to local".
//!
//! # The three things M6 and the audit left here
//!
//! 1. **Socket discovery.** A remote attach socket path cannot be derived:
//!    OpenSSH expands neither `~` nor an environment variable on the remote side
//!    of `-L`, and the far host's `XDG_RUNTIME_DIR` and uid are not knowable
//!    from here. So this probe asks, over the round trip it is making anyway —
//!    see `remote_tunnel::REMOTE_SOCKET_PROBE` — and hands the answer back for
//!    the panel to write into the row, so the next connection needs no probe.
//!    **The write-back is the webview's**, deliberately: `useDesktopSettings`
//!    already serialises the document's read-modify-write, and a second writer
//!    in Rust would be the two-process race [#828](https://github.com/vfarcic/dot-agent-deck/issues/828)
//!    tracks, created on purpose.
//!
//! 2. **The forwards disclosure.** `remote_tunnel`'s audit **A3** is a residual
//!    the argv cannot close: `ClearAllForwardings` would clear our own `-L`, and
//!    OpenSSH has no per-direction alternative, so the user's `LocalForward`,
//!    `RemoteForward` and `DynamicForward` are inherited for the tunnel's whole
//!    life. `remote doctor` refuses to *create* such a forward and calls it a
//!    criterion violation; the tunnel makes the same exposure for orders of
//!    magnitude longer and has said nothing. This is the one place a user can
//!    find out what their own ssh config is doing on their behalf.
//!
//! 3. **`SelectionFallback`, reported honestly.** See above.
//!
//! # What it does not leave behind
//!
//! A probe may be run against a deck that is not the selection, so the lease it
//! takes is released before it returns unless the tested endpoint *is* the
//! selection — rule 3 of `endpoint_tunnels`. The generated `-F` config is a
//! [`ProbeConfig`], removed on drop and swept by `reap_orphaned_tunnels` if a
//! SIGKILL lands between the two.

use serde::Serialize;

use dot_agent_deck::daemon_client::Endpoint;
use dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;

use crate::daemon_bridge::{HandshakeInfo, hello};
use crate::dto::{ConnectionStatus, safe_display_text};
use crate::endpoint_tunnels::EndpointTunnels;
use crate::settings::{DesktopSettings, EndpointId, LOCAL_SELECTION_TOKEN, RemoteEndpointSettings};

/// What a `Test connection` found, as one named state.
///
/// Ordered as the probe reaches them: the things that stop it before it leaves
/// this machine, then the ssh hop, then the deck at the other end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointTestState {
    /// The selection names a deck this document no longer holds — a row the
    /// user removed, or a token a newer build wrote. `SelectionFallback::UnknownDeck`.
    UnknownDeck,
    /// No `ssh` program was found at any of the standard absolute locations.
    /// Nothing about the endpoint is wrong; this machine has no client.
    SshUnavailable,
    /// The row has no remote socket path and discovery could not produce one.
    /// `SelectionFallback::NoRemoteSocket`, and the state the panel turns into
    /// "not configured yet — press Test connection".
    NoRemoteSocket,
    /// ssh could not reach the host at all: refused, unresolvable, no route, or
    /// timed out.
    HostUnreachable,
    /// The host's key is not in this machine's `known_hosts`, or has changed.
    /// Carries the run-`ssh`-once remedy, built from the endpoint the tunnel
    /// actually uses — bastion and port included.
    HostKeyUnverified,
    /// ssh reached the host and was refused: no usable key, wrong user.
    AuthFailed,
    /// ssh authenticated and something else about the transport failed — a
    /// forward that would not bind, a config that would not write, a child that
    /// died saying something this build cannot name.
    TransportFailed,
    /// The ssh connection is up and nothing is listening on the remote socket
    /// path: the tunnel came up but no forwarded socket appeared, or the far
    /// end closed it. Usually "the deck is not running over there".
    DeckNotAnswering,
    /// A daemon answered and refused the `Hello` itself.
    HandshakeRefused,
    /// A daemon answered and the wire versions disagree. The two numbers are in
    /// the report; this is the one state no override can reach.
    ProtocolRefused,
    /// The wire agreed and the two git-describe build stamps did not.
    BuildStampDiffers,
    /// The deck answered and this build can talk to it.
    Reachable,
}

impl EndpointTestState {
    /// Whether this state means the deck can be used right now.
    ///
    /// `BuildStampDiffers` is deliberately **not** ok: the protocol agreed, so
    /// it is overridable, but an override is a judgement the user makes on the
    /// connection screen and not something a green tick here should pre-empt.
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Reachable)
    }
}

/// What a `Test connection` reports back.
///
/// Every text field is scrubbed through [`safe_display_text`] — control **and**
/// bidi — because three of them (`detail`, and the forwards) carry bytes a
/// remote host or a planted `~/.ssh/config` wrote.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointTestReport {
    /// The `Selection` token this report is about: `local`, or a row's id.
    pub endpoint_id: String,
    /// How the deck is named to a user — `RemoteEndpoint::describe`, or the
    /// local socket path.
    pub deck: String,
    pub state: EndpointTestState,
    /// Whether [`Self::state`] means the deck is usable right now.
    ///
    /// Emitted beside the state rather than derived in the webview, so the
    /// twelve-way classification stays in one place: a panel that decided for
    /// itself which states count as success would be a second copy of that
    /// judgement, and the first new state would leave the two disagreeing.
    ///
    /// Stamped once, by [`Self::sealed`] on the way out of [`test_endpoint`],
    /// rather than maintained beside each of the nine assignments to
    /// [`Self::state`] — a derived field kept in step by hand is a derived field
    /// that eventually is not.
    pub ok: bool,
    /// One sentence saying what happened, in the user's terms.
    pub message: String,
    /// A command to run, when there is one. Today only the host-key state has
    /// one, and it is the endpoint's own — the bastion and the port survive,
    /// which is M5 audit A5's fix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    /// ssh's own words, scrubbed. Absent when there were none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The remote attach socket path this probe learned, when it learned one.
    /// The panel writes it into the row; see the module docs for why the
    /// write-back is not made here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discovered_socket: Option<String>,
    /// Whether [`Self::forwards`] is an answer or an absence. `false` means the
    /// resolution did not run or could not be read — which is not the same
    /// claim as "there are none", and the panel must not render it as one.
    pub forwards_known: bool,
    /// The forwards this endpoint's tunnel will inherit from the user's ssh
    /// config, one readable line each. Empty **and** `forwards_known` is the
    /// only combination that means "none".
    pub forwards: Vec<String>,
    pub client_protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_protocol_version: Option<u32>,
    pub client_build_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_build_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_agent_count: Option<usize>,
}

impl EndpointTestReport {
    /// Bring [`Self::ok`] into step with [`Self::state`]. The one exit.
    fn sealed(mut self) -> Self {
        self.ok = self.state.is_ok();
        self
    }

    /// A report carrying nothing but an identity and a verdict — the base every
    /// path below fills in.
    fn new(endpoint_id: &str, deck: String, state: EndpointTestState, message: String) -> Self {
        Self {
            endpoint_id: endpoint_id.to_string(),
            deck,
            state,
            ok: state.is_ok(),
            message,
            remedy: None,
            detail: None,
            discovered_socket: None,
            forwards_known: false,
            forwards: Vec::new(),
            client_protocol_version: PROTOCOL_VERSION,
            server_protocol_version: None,
            client_build_version: dot_agent_deck::build_id::local_build_id(),
            daemon_build_version: None,
            running_agent_count: None,
        }
    }
}

/// Classify a handshake this build already took (PRD #741 M10).
///
/// Pure, and written over [`HandshakeInfo`] rather than over an
/// `AttachResponse`, so it reuses `daemon_bridge`'s classifier instead of
/// growing a second one that could disagree with the connection banner about
/// the same daemon. The split it adds on top is the one a *test* needs and a
/// banner does not: the banner has one "incompatible", and a user pressing
/// Test connection needs to know whether the wire disagreed (nothing to be done
/// from here) or only the stamps did (overridable on the connection screen).
pub(crate) fn state_from_handshake(info: &HandshakeInfo) -> EndpointTestState {
    if info.status == ConnectionStatus::Connected && info.error.is_none() {
        return EndpointTestState::Reachable;
    }
    if info.build_stamp_mismatch_only {
        return EndpointTestState::BuildStampDiffers;
    }
    if info.server_protocol_version != Some(PROTOCOL_VERSION) {
        return EndpointTestState::ProtocolRefused;
    }
    EndpointTestState::HandshakeRefused
}

/// Classify a transport failure into one of the named states, with the remedy
/// where there is one (PRD #741 M10).
///
/// Pure over the error, so every arm is reachable from a test without an ssh
/// binary, a network or a credential — which matters because these are exactly
/// the states the browser tiers cannot see at all.
#[cfg(unix)]
pub(crate) fn state_from_tunnel_error(
    error: &dot_agent_deck::remote_tunnel::TunnelError,
) -> (EndpointTestState, Option<String>) {
    use dot_agent_deck::remote::SshError;
    use dot_agent_deck::remote_tunnel::TunnelError;
    match error {
        TunnelError::SshNotFound { .. } | TunnelError::SshPathNotAbsolute { .. } => {
            (EndpointTestState::SshUnavailable, None)
        }
        // The ssh connection came up and the forward did not. Either nothing is
        // listening on the remote socket path, or the path is wrong — both of
        // which are things about the deck over there rather than about ssh.
        TunnelError::ForwardTimeout { .. } | TunnelError::ForwardFailed { .. } => {
            (EndpointTestState::DeckNotAnswering, None)
        }
        TunnelError::Ssh { source, .. } => match source {
            SshError::HostKeyVerificationFailed { remedy, .. } => (
                EndpointTestState::HostKeyUnverified,
                Some(remedy.to_string()),
            ),
            SshError::ConnectionRefused { .. } => (EndpointTestState::HostUnreachable, None),
            SshError::AuthFailed { .. } => (EndpointTestState::AuthFailed, None),
            SshError::Io { .. } | SshError::Other { .. } => {
                (EndpointTestState::TransportFailed, None)
            }
        },
        // Everything else is this machine failing to prepare a tunnel: a socket
        // path the OS or ssh could not use, a config that would not write, a
        // spawn that failed. None of it says anything about the far host.
        _ => (EndpointTestState::TransportFailed, None),
    }
}

/// One sentence per state, in the user's terms and naming the deck.
///
/// Written here rather than in the panel because the vocabulary is this crate's
/// — protocol versions, build stamps, forwarded sockets — and because a state
/// whose sentence lives beside its classification cannot drift from it.
fn message_for(state: EndpointTestState, deck: &str, info: Option<&HandshakeInfo>) -> String {
    match state {
        EndpointTestState::Reachable => format!("{deck} answered and is compatible with this app."),
        EndpointTestState::BuildStampDiffers => info
            .and_then(|info| info.error.clone())
            .unwrap_or_else(|| format!("{deck} answered; it was built from a different commit.")),
        EndpointTestState::ProtocolRefused => info
            .and_then(|info| info.error.clone())
            .unwrap_or_else(|| format!("{deck} speaks a different protocol version.")),
        EndpointTestState::HandshakeRefused => info
            .and_then(|info| info.error.clone())
            .unwrap_or_else(|| format!("{deck} refused the connection.")),
        EndpointTestState::DeckNotAnswering => format!(
            "The ssh connection to {deck} works, but nothing is listening on its deck socket over there. Start Agent Deck on that machine, then test again."
        ),
        EndpointTestState::HostUnreachable => {
            format!("ssh could not reach {deck}.")
        }
        EndpointTestState::HostKeyUnverified => format!(
            "This machine has not verified {deck}'s host key. Run the command below once in a terminal, then test again."
        ),
        EndpointTestState::AuthFailed => {
            format!("ssh reached {deck} and the login was refused. Check the user and the key.")
        }
        EndpointTestState::TransportFailed => format!("The ssh tunnel to {deck} could not be established."),
        EndpointTestState::SshUnavailable => {
            "No ssh program was found on this machine, so no remote deck can be reached. Install an OpenSSH client.".to_string()
        }
        EndpointTestState::NoRemoteSocket => format!(
            "{deck} has no deck socket path yet, and this test could not discover one."
        ),
        EndpointTestState::UnknownDeck => {
            "That deck is no longer in this settings document.".to_string()
        }
    }
}

/// Fill a report's handshake half from a classified `Hello`.
fn apply_handshake(report: &mut EndpointTestReport, info: &HandshakeInfo) {
    report.state = state_from_handshake(info);
    report.message = message_for(report.state, &report.deck, Some(info));
    report.server_protocol_version = info.server_protocol_version;
    report.daemon_build_version = info.daemon_build_version.clone();
    report.running_agent_count = info.running_agent_count;
}

/// Run a `Test connection` against the deck `selection` names.
///
/// `selection` is a [`crate::settings::Selection`] token: `local`, or a row's
/// id. Taking the token rather than an endpoint is what lets the two
/// `SelectionFallback` states — a row that is gone, a row with no socket — be
/// reported as themselves.
pub(crate) async fn test_endpoint(
    settings: &DesktopSettings,
    selection: &str,
    tunnels: &EndpointTunnels,
) -> EndpointTestReport {
    unsealed(settings, selection, tunnels).await.sealed()
}

/// [`test_endpoint`] before [`EndpointTestReport::sealed`] stamps the derived
/// `ok`. Split so there is exactly one exit to stamp.
async fn unsealed(
    settings: &DesktopSettings,
    selection: &str,
    tunnels: &EndpointTunnels,
) -> EndpointTestReport {
    if selection.eq_ignore_ascii_case(LOCAL_SELECTION_TOKEN) {
        return test_local(tunnels).await;
    }
    let Ok(id) = EndpointId::parse(selection) else {
        return EndpointTestReport::new(
            selection,
            safe_display_text(selection),
            EndpointTestState::UnknownDeck,
            message_for(EndpointTestState::UnknownDeck, "", None),
        );
    };
    let row = settings
        .endpoints
        .as_ref()
        .and_then(|endpoints| endpoints.find(&id))
        .cloned();
    let Some(row) = row else {
        return EndpointTestReport::new(
            id.as_str(),
            safe_display_text(id.to_string()),
            EndpointTestState::UnknownDeck,
            message_for(EndpointTestState::UnknownDeck, "", None),
        );
    };
    test_remote(&id, &row, tunnels).await
}

/// The local deck: no ssh, no tunnel, no forwards — just the handshake.
///
/// It is here rather than left out because "test this deck" should mean the
/// same thing whichever deck is selected, and because it is the one arm of this
/// function a developer can run with nothing configured.
async fn test_local(tunnels: &EndpointTunnels) -> EndpointTestReport {
    let endpoint = Endpoint::local();
    let deck = safe_display_text(endpoint.describe());
    let mut report = EndpointTestReport::new(
        LOCAL_SELECTION_TOKEN,
        deck.clone(),
        EndpointTestState::DeckNotAnswering,
        String::new(),
    );
    // A local deck has no forwards to disclose, and saying so is an answer:
    // `forwards_known` without `forwards` is what the panel renders as "none".
    report.forwards_known = true;
    match tunnels.acquire(&endpoint).await {
        Ok(lease) => match hello(lease.address()).await {
            Ok((info, _)) => apply_handshake(&mut report, &info),
            Err(error) => {
                report.state = EndpointTestState::DeckNotAnswering;
                report.detail = Some(safe_display_text(error));
                report.message = format!(
                    "No deck answered at {deck}. Start Agent Deck on this machine, then test again."
                );
            }
        },
        Err(error) => {
            report.state = EndpointTestState::DeckNotAnswering;
            report.detail = Some(safe_display_text(error));
            report.message = format!(
                "No deck answered at {deck}. Start Agent Deck on this machine, then test again."
            );
        }
    }
    release_if_not_selected(tunnels, &endpoint).await;
    report
}

#[cfg(unix)]
async fn test_remote(
    id: &EndpointId,
    row: &RemoteEndpointSettings,
    tunnels: &EndpointTunnels,
) -> EndpointTestReport {
    use dot_agent_deck::remote_tunnel::SshProgram;

    let destination = row.destination();
    let deck = safe_display_text(destination.describe());
    let mut report = EndpointTestReport::new(
        id.as_str(),
        deck.clone(),
        EndpointTestState::TransportFailed,
        String::new(),
    );

    let ssh = match SshProgram::resolve() {
        Ok(ssh) => ssh,
        Err(error) => {
            report.state = EndpointTestState::SshUnavailable;
            report.detail = Some(safe_display_text(error.to_string()));
            report.message = message_for(report.state, &deck, None);
            return report;
        }
    };

    // The disclosure first, and never fatal: it resolves configuration and
    // connects to nothing, so it is the cheapest thing here and the one piece
    // of the report that is still worth having when the connection fails.
    let (forwards_known, forwards) = resolve_forwards(&ssh, &destination).await;
    report.forwards_known = forwards_known;
    report.forwards = forwards;

    let socket = match row.socket.clone() {
        Some(socket) => socket,
        None => match discover_socket(&ssh, &destination).await {
            Ok(socket) => {
                report.discovered_socket = Some(socket.as_str().to_string());
                socket
            }
            Err(failure) => {
                report.state = failure.state;
                report.remedy = failure.remedy;
                report.detail = failure.detail;
                report.message = message_for(report.state, &deck, None);
                return report;
            }
        },
    };

    let endpoint = Endpoint::Remote(row.endpoint_at(socket));
    let lease = match tunnels.acquire(&endpoint).await {
        Ok(lease) => lease,
        Err(error) => {
            // `acquire` has already flattened the typed error to a string, so
            // the classification is re-derived from the tunnel the same way the
            // discovery probe's is: by asking `TunnelError` itself. The
            // round-trip through a string is the cost of one seam for both
            // kinds of endpoint, and it is bounded — every state below is
            // reachable through the text ssh itself wrote.
            report.state = EndpointTestState::TransportFailed;
            report.detail = Some(safe_display_text(&error));
            report.message = message_for(report.state, &deck, None);
            reclassify_from_detail(&mut report, &destination, &error, &deck);
            return report;
        }
    };

    match hello(lease.address()).await {
        Ok((info, _)) => apply_handshake(&mut report, &info),
        Err(error) => {
            report.state = EndpointTestState::DeckNotAnswering;
            report.detail = Some(safe_display_text(error));
            report.message = message_for(report.state, &deck, None);
        }
    }

    release_if_not_selected(tunnels, &endpoint).await;
    report
}

/// Give a probe's transport back unless the app is actually using it
/// (`endpoint_tunnels` rule 3, teardown trigger 3).
///
/// A deck that is **not** the selection is released, because a tunnel per
/// Test-connection click is exactly the process leak this milestone is about and
/// nothing would otherwise close it until the app exits.
///
/// A deck that **is** the selection keeps its transport, and that is the more
/// interesting half. Releasing it would only drop the *map's* handle — a live
/// link still holds a lease, so the child would survive — and the next
/// `establish()` would then open a **second** `ssh` child beside the first for
/// as long as the old link stayed fresh. Testing the deck you are connected to
/// would cost a duplicate authenticated session, which is the same defect this
/// function exists to avoid, arrived at from the other side.
async fn release_if_not_selected(tunnels: &EndpointTunnels, endpoint: &Endpoint) {
    if endpoint.describe() != crate::dto::selected_endpoint().describe() {
        tunnels.release(endpoint).await;
    }
}

/// Why discovery could not produce a socket path.
#[cfg(unix)]
struct DiscoveryFailure {
    state: EndpointTestState,
    remedy: Option<String>,
    detail: Option<String>,
}

/// Ask the far host where its deck listens.
///
/// One ssh exec, bounded in time and in captured bytes by `run_probe`. A
/// non-zero exit or unparsable output is *not* folded into a generic failure:
/// ssh's own stderr is classified exactly as the tunnel's is, so a host-key
/// problem discovered here reads the same as one discovered a step later.
#[cfg(unix)]
async fn discover_socket(
    ssh: &dot_agent_deck::remote_tunnel::SshProgram,
    destination: &dot_agent_deck::remote_tunnel::SshDestination,
) -> Result<dot_agent_deck::remote_tunnel::RemoteSocketPath, DiscoveryFailure> {
    use dot_agent_deck::remote_tunnel::{
        ProbeConfig, REMOTE_SOCKET_PROBE, RemoteSocketPath, remote_command_args, run_probe,
    };

    let probe_ssh = ssh.clone();
    let probe_destination = destination.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let config = ProbeConfig::create()?;
        let args = remote_command_args(&probe_destination, config.path(), REMOTE_SOCKET_PROBE);
        run_probe(&probe_ssh, &args)
    })
    .await;

    let output = match probe {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            let (state, remedy) = state_from_tunnel_error(&error);
            return Err(DiscoveryFailure {
                state,
                remedy,
                detail: Some(safe_display_text(error.to_string())),
            });
        }
        Err(error) => {
            return Err(DiscoveryFailure {
                state: EndpointTestState::TransportFailed,
                remedy: None,
                detail: Some(safe_display_text(format!(
                    "the discovery probe task failed: {error}"
                ))),
            });
        }
    };

    if output.status == Some(0) && !output.truncated {
        // The probe prints exactly one line. Anything else — a login banner, a
        // shell that echoes — means the value cannot be trusted, so the LAST
        // non-empty line is taken and then validated by the same deserializer a
        // hand-edited document goes through.
        if let Some(path) = output
            .stdout
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty())
            && let Ok(socket) = RemoteSocketPath::parse(path)
        {
            return Ok(socket);
        }
    }

    // Nothing usable came back. Classify what ssh said rather than reporting
    // "discovery failed", because the reason is almost always the connection.
    let target = destination.ssh_target();
    let classified = dot_agent_deck::remote::classify_ssh_error_with_remedy(
        &target,
        &output.stderr,
        &destination.host_key_remedy(),
    );
    let detail = output.stderr_text();
    let detail = (!detail.is_empty()).then(|| safe_display_text(detail));
    let (state, remedy) =
        state_from_tunnel_error(&dot_agent_deck::remote_tunnel::TunnelError::Ssh {
            deck: destination.describe(),
            source: classified,
        });
    // `classify_ssh_error` folds anything it cannot name into `Other`, which
    // lands on `TransportFailed`. For a probe that connected and simply printed
    // nothing useful, the honest state is the one the PRD names: the row has no
    // socket path and this run could not learn one.
    let state = if state == EndpointTestState::TransportFailed && output.stderr.trim().is_empty() {
        EndpointTestState::NoRemoteSocket
    } else {
        state
    };
    Err(DiscoveryFailure {
        state,
        remedy,
        detail,
    })
}

/// Re-derive a transport state from the message `acquire` flattened.
#[cfg(unix)]
fn reclassify_from_detail(
    report: &mut EndpointTestReport,
    destination: &dot_agent_deck::remote_tunnel::SshDestination,
    detail: &str,
    deck: &str,
) {
    let classified = dot_agent_deck::remote::classify_ssh_error_with_remedy(
        &destination.ssh_target(),
        detail,
        &destination.host_key_remedy(),
    );
    let (state, remedy) =
        state_from_tunnel_error(&dot_agent_deck::remote_tunnel::TunnelError::Ssh {
            deck: destination.describe(),
            source: classified,
        });
    if state != EndpointTestState::TransportFailed {
        report.state = state;
        report.remedy = remedy;
        report.message = message_for(state, deck, None);
        return;
    }
    // `TunnelError`'s own `Display` names the forward cases in words this
    // classifier does not look for, so they are matched here rather than left
    // as a generic transport failure.
    let lower = detail.to_ascii_lowercase();
    if lower.contains("did not produce a forwarded socket")
        || lower.contains("the forward could not be established")
    {
        report.state = EndpointTestState::DeckNotAnswering;
        report.message = message_for(report.state, deck, None);
    }
}

/// Resolve what the tunnel will inherit from the user's ssh config.
///
/// Returns `(known, forwards)`. `known` is `false` whenever the resolution did
/// not run or could not be read, and that is **not** the same claim as "there
/// are none" — a disclosure that quietly says "no forwards" when it could not
/// look is worse than one that says it does not know.
#[cfg(unix)]
async fn resolve_forwards(
    ssh: &dot_agent_deck::remote_tunnel::SshProgram,
    destination: &dot_agent_deck::remote_tunnel::SshDestination,
) -> (bool, Vec<String>) {
    use dot_agent_deck::remote_doctor::parse_ssh_g;
    use dot_agent_deck::remote_tunnel::{ProbeConfig, resolved_config_args, run_probe};

    let probe_ssh = ssh.clone();
    let probe_destination = destination.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let config = ProbeConfig::create()?;
        let args = resolved_config_args(&probe_destination, config.path());
        run_probe(&probe_ssh, &args)
    })
    .await;

    let Ok(Ok(output)) = probe else {
        return (false, Vec::new());
    };
    if output.status != Some(0) || output.truncated {
        return (false, Vec::new());
    }
    let resolved = parse_ssh_g(&output.stdout);
    (
        true,
        resolved
            .forwards
            .iter()
            .map(|forward| safe_display_text(forward.to_string()))
            .collect(),
    )
}

// The tunnel is Unix-only (`remote_tunnel`'s `mod tunnel` is `#[cfg(unix)]`),
// so on Windows a remote deck is reported as exactly that rather than as a
// transport failure. Windows desktop needs #754 and is out of PRD #741's scope.
#[cfg(not(unix))]
async fn test_remote(
    id: &EndpointId,
    row: &RemoteEndpointSettings,
    _tunnels: &EndpointTunnels,
) -> EndpointTestReport {
    let deck = safe_display_text(row.destination().describe());
    let mut report = EndpointTestReport::new(
        id.as_str(),
        deck,
        EndpointTestState::SshUnavailable,
        "Remote decks are not supported on this platform yet.".to_string(),
    );
    report.forwards_known = false;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::daemon_protocol::{AttachResponse, RunningAgentsSummary};

    /// The handshake states, driven through `daemon_bridge`'s own classifier so
    /// this cannot disagree with the connection banner about the same daemon.
    fn info(response: &AttachResponse, client_build: &str) -> HandshakeInfo {
        crate::daemon_bridge::classify_handshake_for_test(response, client_build)
    }

    fn hello_with_build(build: Option<&str>) -> AttachResponse {
        let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        reply.build_version = build.map(str::to_string);
        reply
    }

    /// A matching build and a matching wire is the one state that reads as ok.
    #[test]
    fn a_matching_deck_is_reachable() {
        let response = hello_with_build(Some("0.39.0-gabc1234"));
        let state = state_from_handshake(&info(&response, "0.39.0-gabc1234"));
        assert_eq!(state, EndpointTestState::Reachable);
        assert!(state.is_ok());
    }

    /// A stamp difference across releases is its own state, distinct from a
    /// protocol refusal — the user can override one and not the other.
    #[test]
    fn a_build_stamp_difference_is_its_own_state() {
        let response = hello_with_build(Some("0.38.0-gdead111"));
        let state = state_from_handshake(&info(&response, "0.39.0-gabc1234"));
        assert_eq!(state, EndpointTestState::BuildStampDiffers);
        assert!(
            !state.is_ok(),
            "a stamp difference must not read as a clean pass: the override is a judgement the \
             user makes on the connection screen"
        );
    }

    /// The wire disagreeing is the state no override reaches, and the report
    /// carries both numbers so the sentence can name them.
    #[test]
    fn a_protocol_difference_is_named_and_carries_both_versions() {
        let mut response = hello_with_build(Some("0.39.0-gabc1234"));
        response.server_version = Some(PROTOCOL_VERSION + 1);
        let classified = info(&response, "0.39.0-gabc1234");
        assert_eq!(
            state_from_handshake(&classified),
            EndpointTestState::ProtocolRefused
        );

        let mut report = EndpointTestReport::new(
            "deck1",
            "deploy@build-box".into(),
            EndpointTestState::TransportFailed,
            String::new(),
        );
        apply_handshake(&mut report, &classified);
        assert_eq!(report.state, EndpointTestState::ProtocolRefused);
        assert_eq!(report.client_protocol_version, PROTOCOL_VERSION);
        assert_eq!(report.server_protocol_version, Some(PROTOCOL_VERSION + 1));
        assert!(
            report.message.contains(&PROTOCOL_VERSION.to_string()),
            "the sentence must name the version: {}",
            report.message
        );
    }

    /// A daemon that rejects `Hello` outright is not a version problem and does
    /// not read as one.
    #[test]
    fn a_rejected_hello_is_a_handshake_refusal() {
        let mut response = hello_with_build(Some("0.39.0-gabc1234"));
        response.ok = false;
        response.error = Some("attach is disabled".into());
        assert_eq!(
            state_from_handshake(&info(&response, "0.39.0-gabc1234")),
            EndpointTestState::HandshakeRefused
        );
    }

    /// Every transport failure this build can meet maps to a distinct state,
    /// and the host-key one keeps its remedy — which is the whole reason that
    /// state exists separately from "auth failed".
    #[cfg(unix)]
    #[test]
    fn each_transport_failure_is_a_distinct_named_state() {
        use dot_agent_deck::remote::SshError;
        use dot_agent_deck::remote_tunnel::TunnelError;

        let deck = "deploy@build-box".to_string();
        let ssh = |source| TunnelError::Ssh {
            deck: deck.clone(),
            source,
        };

        let (state, remedy) = state_from_tunnel_error(&ssh(SshError::HostKeyVerificationFailed {
            target: deck.clone(),
            remedy: "ssh -J bastion -p 2222 deploy@build-box".into(),
        }));
        assert_eq!(state, EndpointTestState::HostKeyUnverified);
        assert_eq!(
            remedy.as_deref(),
            Some("ssh -J bastion -p 2222 deploy@build-box"),
            "the remedy must name the endpoint the tunnel uses — bastion and port included (M5 \
             audit A5)"
        );

        assert_eq!(
            state_from_tunnel_error(&ssh(SshError::ConnectionRefused {
                host: "build-box".into(),
                port: 22,
                detail: String::new(),
            }))
            .0,
            EndpointTestState::HostUnreachable
        );
        assert_eq!(
            state_from_tunnel_error(&ssh(SshError::AuthFailed {
                target: deck.clone(),
                detail: String::new(),
            }))
            .0,
            EndpointTestState::AuthFailed
        );
        assert_eq!(
            state_from_tunnel_error(&ssh(SshError::Other {
                target: deck.clone(),
                detail: String::new(),
            }))
            .0,
            EndpointTestState::TransportFailed
        );
        assert_eq!(
            state_from_tunnel_error(&TunnelError::ForwardTimeout {
                deck: deck.clone(),
                secs: 10,
                remote: "/run/user/1000/dot-agent-deck-attach.sock".into(),
            })
            .0,
            EndpointTestState::DeckNotAnswering,
            "an ssh connection that came up with no forward is the deck being down, not ssh \
             failing"
        );
        assert_eq!(
            state_from_tunnel_error(&TunnelError::SshNotFound {
                searched: "/usr/bin/ssh".into(),
            })
            .0,
            EndpointTestState::SshUnavailable
        );
        assert_eq!(
            state_from_tunnel_error(&TunnelError::SocketPathHasColon {
                path: "/tmp/a:b.sock".into(),
            })
            .0,
            EndpointTestState::TransportFailed
        );
    }

    /// A selection naming a row this document does not hold is reported as
    /// itself — not as "connected to local", and not as a transport failure.
    #[tokio::test]
    async fn a_selection_naming_a_missing_row_reports_unknown_deck() {
        let settings = DesktopSettings::default();
        let tunnels = EndpointTunnels::default();
        let report = test_endpoint(&settings, "0123456789abcdef", &tunnels).await;
        assert_eq!(report.state, EndpointTestState::UnknownDeck);
        assert_eq!(report.endpoint_id, "0123456789abcdef");
        assert!(!report.state.is_ok());
    }

    /// A malformed token is the same answer. It is reachable from a hand-edited
    /// document, which is the only writer that can produce one.
    #[tokio::test]
    async fn a_malformed_selection_token_reports_unknown_deck() {
        let settings = DesktopSettings::default();
        let tunnels = EndpointTunnels::default();
        let report = test_endpoint(&settings, "not a valid id", &tunnels).await;
        assert_eq!(report.state, EndpointTestState::UnknownDeck);
    }

    /// Every text field a remote or a planted ssh config can influence is
    /// stripped of bidi characters, not only of control characters.
    #[test]
    fn report_text_is_bidi_safe() {
        let report = EndpointTestReport::new(
            "deck1",
            safe_display_text("build\u{202e}box"),
            EndpointTestState::TransportFailed,
            String::new(),
        );
        assert_eq!(report.deck, "buildbox");
    }

    // -----------------------------------------------------------------------
    // Against a real socket
    //
    // The tier that can actually exercise a connection (see
    // `docs/develop/desktop-gui.md`): a scripted daemon on a Unix socket, the
    // real `Hello` frame, and the real classifier. The browser tiers cannot see
    // any of this, which is why the properties that matter live here and run in
    // the required `build` job.
    // -----------------------------------------------------------------------

    /// The attach-socket override is process-global, so every test that points
    /// `Endpoint::local()` at a scripted socket takes this first. Under nextest
    /// each test owns its process and the lock is free; under a plain
    /// `cargo test` the module shares one.
    ///
    /// An **async** mutex, and not for contention: the guard is deliberately
    /// held across the `await`s that run the probe, which is exactly what a
    /// `std::sync::Mutex` must not do (`clippy::await_holding_lock`). Narrowing
    /// it to the two `set_var` calls would not serialise anything — the whole
    /// point is that the override stays in force for the probe.
    #[cfg(unix)]
    static ATTACH_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(unix)]
    const ATTACH_SOCKET_ENV: &str = "DOT_AGENT_DECK_ATTACH_SOCKET";

    /// A private directory plus a socket path inside it, short enough for
    /// `sun_path`.
    #[cfg(unix)]
    fn scratch_socket(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "dad-m10-{tag}-{}-{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.subsec_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let socket = dir.join("attach.sock");
        (dir, socket)
    }

    /// Bind at a mode the local trust check accepts — `establish()` runs
    /// `verify_endpoint_trusted` on the inode before it connects, so a socket
    /// left at the ambient umask would be refused before any handshake.
    #[cfg(unix)]
    fn bind_trusted(socket: &std::path::Path) -> tokio::net::UnixListener {
        use std::os::unix::fs::PermissionsExt;
        let listener = tokio::net::UnixListener::bind(socket).expect("bind the scripted daemon");
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");
        listener
    }

    /// A scripted daemon that answers `replies.len()` connections, one request
    /// each. One request per connection is not a simplification: the real
    /// daemon's `handle_connection` reads exactly ONE frame and returns.
    #[cfg(unix)]
    async fn scripted_daemon(listener: tokio::net::UnixListener, replies: Vec<AttachResponse>) {
        use dot_agent_deck::daemon_protocol::{KIND_REQ, KIND_RESP, read_frame, write_frame};
        for reply in replies {
            let (stream, _peer) = listener.accept().await.expect("accept one client");
            let (mut reader, mut writer) = stream.into_split();
            let (kind, _payload) = read_frame(&mut reader)
                .await
                .expect("read the request frame")
                .expect("the client sent a frame");
            assert_eq!(kind, KIND_REQ);
            let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
            write_frame(&mut writer, KIND_RESP, &encoded)
                .await
                .expect("answer the client");
        }
    }

    /// Point `Endpoint::local()` at `socket`, run `body`, restore the
    /// environment, and hand back the report.
    #[cfg(unix)]
    async fn against_local_socket(socket: &std::path::Path) -> EndpointTestReport {
        let guard = ATTACH_ENV_LOCK.lock().await;
        // SAFETY: the whole mutation happens under ATTACH_ENV_LOCK and the
        // prior value is restored before it is released.
        let prior = std::env::var(ATTACH_SOCKET_ENV).ok();
        unsafe { std::env::set_var(ATTACH_SOCKET_ENV, socket) };
        let tunnels = EndpointTunnels::default();
        let report = test_endpoint(&DesktopSettings::default(), "local", &tunnels).await;
        unsafe {
            match prior {
                Some(value) => std::env::set_var(ATTACH_SOCKET_ENV, value),
                None => std::env::remove_var(ATTACH_SOCKET_ENV),
            }
        }
        drop(guard);
        report
    }

    /// A deck that answers with this build's own stamp: the one green state.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_that_answers_compatibly_is_reachable() {
        let (dir, socket) = scratch_socket("reachable");
        let listener = bind_trusted(&socket);
        let mut reply = hello_with_build(Some(&dot_agent_deck::build_id::local_build_id()));
        reply.running_agents = Some(RunningAgentsSummary::default());
        let daemon = tokio::spawn(scripted_daemon(listener, vec![reply]));

        let report = against_local_socket(&socket).await;
        daemon.await.expect("the scripted daemon must not panic");

        assert_eq!(report.state, EndpointTestState::Reachable);
        assert!(report.ok);
        assert_eq!(report.server_protocol_version, Some(PROTOCOL_VERSION));
        assert!(
            report.forwards_known && report.forwards.is_empty(),
            "a local deck inherits no ssh forwards, and saying so is an answer rather than an              absence"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A wire disagreement over a real socket, named and carrying both numbers.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_wire_disagreement_over_a_real_socket_names_the_versions() {
        let (dir, socket) = scratch_socket("protocol");
        let listener = bind_trusted(&socket);
        let mut reply = hello_with_build(Some(&dot_agent_deck::build_id::local_build_id()));
        reply.server_version = Some(PROTOCOL_VERSION + 1);
        let daemon = tokio::spawn(scripted_daemon(listener, vec![reply]));

        let report = against_local_socket(&socket).await;
        daemon.await.expect("no panic");

        assert_eq!(report.state, EndpointTestState::ProtocolRefused);
        assert!(!report.ok);
        assert_eq!(report.client_protocol_version, PROTOCOL_VERSION);
        assert_eq!(report.server_protocol_version, Some(PROTOCOL_VERSION + 1));
        assert!(
            report.message.contains(&(PROTOCOL_VERSION + 1).to_string()),
            "the sentence must name what the daemon reported: {}",
            report.message
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A stamp difference over a real socket is a different state from a wire
    /// disagreement, which is the distinction the panel needs in order to say
    /// anything useful.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_stamp_difference_over_a_real_socket_is_not_a_wire_disagreement() {
        let (dir, socket) = scratch_socket("stamp");
        let listener = bind_trusted(&socket);
        let daemon = tokio::spawn(scripted_daemon(
            listener,
            vec![hello_with_build(Some("0.1.0-gfeedface"))],
        ));

        let report = against_local_socket(&socket).await;
        daemon.await.expect("no panic");

        assert_eq!(report.state, EndpointTestState::BuildStampDiffers);
        assert_eq!(
            report.daemon_build_version.as_deref(),
            Some("0.1.0-gfeedface")
        );
        assert_eq!(report.server_protocol_version, Some(PROTOCOL_VERSION));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Nothing listening is its own state, and it is the one a user meets most
    /// often. It must not read as a version problem.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_deck_that_is_not_listening_is_reported_as_not_answering() {
        let (dir, socket) = scratch_socket("silent");
        // Deliberately never bound: the path does not exist at all.
        let report = against_local_socket(&socket).await;

        assert_eq!(report.state, EndpointTestState::DeckNotAnswering);
        assert!(!report.ok);
        assert!(report.server_protocol_version.is_none());
        assert!(
            report.detail.is_some(),
            "the reason the connection failed belongs in the report"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    /// Both halves of teardown trigger 3, on one scripted socket.
    ///
    /// Testing a deck that is **not** the selection must leave nothing held —
    /// that is the process leak the ownership rules exist to close. Testing the
    /// deck the app is actually using must keep its transport, because
    /// releasing it would only drop the map's handle and the next `establish()`
    /// would then open a second `ssh` child beside the one a live link still
    /// holds.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_probe_releases_a_deck_it_is_not_connected_to_and_keeps_the_one_it_is() {
        let (dir, socket) = scratch_socket("release");
        let listener = bind_trusted(&socket);
        let matching = || hello_with_build(Some(&dot_agent_deck::build_id::local_build_id()));
        let daemon = tokio::spawn(scripted_daemon(listener, vec![matching(), matching()]));

        let guard = ATTACH_ENV_LOCK.lock().await;
        // SAFETY: as in `against_local_socket` — the mutation is under the lock
        // and the prior value is restored before it is released. The selection
        // is a process-global for the same reason and is restored with it.
        let prior = std::env::var(ATTACH_SOCKET_ENV).ok();
        unsafe { std::env::set_var(ATTACH_SOCKET_ENV, &socket) };

        // A document whose selection resolves to a REMOTE deck, so the local
        // deck this test probes is genuinely not the one in use.
        let elsewhere = document_selecting_a_remote_deck();
        crate::dto::apply_settings_selection(&elsewhere);
        let tunnels = EndpointTunnels::default();
        let unselected = test_endpoint(&elsewhere, "local", &tunnels).await;
        let held_after_unselected = tunnels.held().await;

        // Now the local deck IS the selection.
        crate::dto::apply_settings_selection(&DesktopSettings::default());
        let selected = test_endpoint(&DesktopSettings::default(), "local", &tunnels).await;
        let held_after_selected = tunnels.held().await;

        unsafe {
            match prior {
                Some(value) => std::env::set_var(ATTACH_SOCKET_ENV, value),
                None => std::env::remove_var(ATTACH_SOCKET_ENV),
            }
        }
        drop(guard);
        daemon.await.expect("no panic");

        assert_eq!(unselected.state, EndpointTestState::Reachable);
        assert_eq!(
            held_after_unselected, 0,
            "a deck that is not the selection must not keep a transport: one tunnel per \
             Test-connection click is the process leak this milestone is about"
        );
        assert_eq!(selected.state, EndpointTestState::Reachable);
        assert_eq!(
            held_after_selected, 1,
            "the deck the app is connected to keeps its transport, or the next establishment \
             opens a second ssh child beside the one a live link still holds"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A document whose `[endpoints]` selection names a connectable remote row.
    fn document_selecting_a_remote_deck() -> DesktopSettings {
        use crate::settings::{EndpointSettings, Selection};
        use dot_agent_deck::remote_tunnel::{Hostname, RemoteSocketPath};

        let id = EndpointId::parse("deck0000000000aa").expect("a valid id");
        let mut row = RemoteEndpointSettings::new(
            id.clone(),
            Hostname::parse("build-box").expect("a valid host"),
        );
        row.socket = Some(
            RemoteSocketPath::parse("/run/user/1000/dot-agent-deck-attach.sock").expect("path"),
        );
        DesktopSettings {
            endpoints: Some(EndpointSettings {
                remote: vec![row],
                selection: Selection::One(id),
            }),
            ..DesktopSettings::default()
        }
    }
}
