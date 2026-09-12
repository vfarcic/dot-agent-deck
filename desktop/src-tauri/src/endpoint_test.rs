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
//! 2. **The `ssh -G` disclosure.** `remote_tunnel`'s audit **A3** is a residual
//!    the argv cannot close: `ClearAllForwardings` would clear our own `-L`, and
//!    OpenSSH has no per-direction alternative, so the user's `LocalForward`,
//!    `RemoteForward` and `DynamicForward` are inherited for the tunnel's whole
//!    life. `remote doctor` refuses to *create* such a forward and calls it a
//!    criterion violation; the tunnel makes the same exposure for orders of
//!    magnitude longer and has said nothing. This is the one place a user can
//!    find out what their own ssh config is doing on their behalf.
//!
//!    The **host-key sources** ride the same resolution (PRD #741 final audit
//!    **F2**). `forced_options` forces `StrictHostKeyChecking=yes` onto both
//!    hops, which forces the *check*; `KnownHostsCommand`, `UserKnownHostsFile`
//!    and `GlobalKnownHostsFile` are inherited, so where ssh looks for the key
//!    it is strict about stays the user's config's to decide. Forcing those
//!    too was measured to work and deliberately not done — it breaks the CA and
//!    inventory fleets `KnownHostsCommand` exists for — so the answer is the
//!    same one A3 got: disclose it. The `ssh -G` run was already happening.
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

use crate::daemon_bridge::{HandshakeInfo, StampPolicy, hello};
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
    /// Whether this state means the deck can be used right now, under the
    /// build-stamp policy for its kind (PRD #741 M8).
    ///
    /// `BuildStampDiffers` is the one state whose answer depends on the policy,
    /// and it depends on it because the *connection banner's* answer does:
    ///
    /// - [`StampPolicy::Enforced`] — a local deck. Deliberately **not** ok: the
    ///   protocol agreed, so it is overridable, but an override is a judgement
    ///   the user makes on the connection screen and not something a green tick
    ///   here should pre-empt.
    /// - [`StampPolicy::Informational`] — a remote deck. Ok, because the app
    ///   *will* connect to it: M8 demoted the stamp to a badge for a deck this
    ///   app cannot replace. A probe reporting "unusable" for a deck the banner
    ///   is about to connect to would be the two screens disagreeing about one
    ///   daemon, which is the failure this module was written to avoid.
    ///
    /// The message says what differs either way — the verdict changes, the
    /// disclosure does not.
    pub fn is_ok(self, stamps: StampPolicy) -> bool {
        match self {
            Self::Reachable => true,
            Self::BuildStampDiffers => stamps == StampPolicy::Informational,
            _ => false,
        }
    }
}

/// What a `Test connection` reports back.
///
/// Every text field that carries bytes this app did not write is scrubbed
/// through [`safe_display_text`] — control **and** bidi. Enumerated, because
/// the wider claim that used to stand here ("every text field") was false for
/// two of them and a reader would have used it to judge a *new* render site
/// safe (PRD #741 final audit **F4**):
///
/// - `deck`, `detail`, `forwards`, `known_hosts`, `message` and
///   `daemon_build_version` — scrubbed at every assignment. The last two carry
///   the **remote** daemon's `build_version`, which is an unvalidated
///   `Option<String>` on the wire, so they need it most and had it least.
/// - `remedy` is built from validated ASCII newtypes, and `discovered_socket`
///   is a `RemoteSocketPath` whose charset is `[A-Za-z0-9._/-]`. Neither can
///   hold a control or bidi byte, so neither is scrubbed.
/// - `endpoint_id`, the versions and the counts are not text this app renders
///   as prose.
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
    /// Whether [`Self::forwards`] and [`Self::known_hosts`] are answers or
    /// absences. `false` means the `ssh -G` resolution did not run or could not
    /// be read — which is not the same claim as "there are none", and the panel
    /// must not render it as one.
    ///
    /// One flag for both lists because there is one fact behind them: a single
    /// `ssh -G` either ran and parsed or it did not. A second boolean written
    /// from the same expression is a derived field kept in step by hand, which
    /// is the shape [`Self::ok`] above exists to avoid.
    pub disclosure_known: bool,
    /// The forwards this endpoint's tunnel will inherit from the user's ssh
    /// config, one readable line each. Empty **and** `disclosure_known` is the
    /// only combination that means "none".
    ///
    /// Complete as of PRD #741 final audit **F1**: `parse_ssh_g` keeps a
    /// forward line it cannot split rather than dropping it, so this list is
    /// never quietly shorter than what ssh resolved.
    pub forwards: Vec<String>,
    /// Where ssh resolved the host keys it checks this endpoint against, one
    /// readable line each (PRD #741 final audit **F2**).
    ///
    /// Additive context rather than a claim: an empty list under
    /// `disclosure_known` means ssh named no source — which for a local deck
    /// means no `ssh -G` was run at all — and the panel renders nothing rather
    /// than asserting that no host-key file is configured.
    pub known_hosts: Vec<String>,
    pub client_protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_protocol_version: Option<u32>,
    pub client_build_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_build_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_agent_count: Option<usize>,
    /// The build-stamp policy [`Self::state`] was classified under, carried so
    /// [`Self::sealed`] can stamp [`Self::ok`] without deciding it a second time
    /// (PRD #741 M8).
    ///
    /// Not serialised: it is an input to `ok`, which is what the panel reads.
    /// Written only by [`apply_handshake`], which is the only producer of the
    /// only state whose verdict consults it — so on every path that never
    /// handshakes this holds the placeholder [`StampPolicy::Enforced`] that
    /// [`Self::new`] wrote, and is never read.
    #[serde(skip)]
    stamps: StampPolicy,
}

impl EndpointTestReport {
    /// Bring [`Self::ok`] into step with [`Self::state`]. The one exit.
    fn sealed(mut self) -> Self {
        self.ok = self.state.is_ok(self.stamps);
        self
    }

    /// A report carrying nothing but an identity and a verdict — the base every
    /// path below fills in.
    fn new(endpoint_id: &str, deck: String, state: EndpointTestState, message: String) -> Self {
        Self {
            endpoint_id: endpoint_id.to_string(),
            deck,
            state,
            // Stamped by [`Self::sealed`], which is the one exit and the only
            // place that knows the deck's build-stamp policy. `false` here is a
            // placeholder, not a verdict.
            ok: false,
            message,
            remedy: None,
            detail: None,
            discovered_socket: None,
            // Never consulted unless `apply_handshake` overwrites it — no
            // other path can reach the one state that reads it. `Enforced` is
            // the fail-safe of the two, so a future state that did consult it
            // would withhold a green tick rather than invent one.
            stamps: StampPolicy::Enforced,
            disclosure_known: false,
            forwards: Vec::new(),
            known_hosts: Vec::new(),
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

/// Fill a report's handshake half from a classified `Hello`, under the policy
/// `stamps` — the same value the `hello` that produced `info` was given.
///
/// The policy travels with the handshake rather than being re-derived at the
/// exit because this is the only place [`EndpointTestState::BuildStampDiffers`]
/// can be produced, and that is the only state whose verdict reads the policy
/// (PRD #741 M8). Taking it here is what lets [`EndpointTestReport::sealed`]
/// stamp `ok` without a second copy of the local/remote decision.
fn apply_handshake(report: &mut EndpointTestReport, info: &HandshakeInfo, stamps: StampPolicy) {
    report.stamps = stamps;
    report.state = state_from_handshake(info);
    // Scrubbed here rather than trusted from `HandshakeInfo` (PRD #741 final
    // audit **F4**). Three of the states below take their sentence verbatim
    // from `info.error`, which `daemon_bridge` builds with `safe_message` —
    // general category `Cc` only — around the **remote** daemon's
    // `build_version`, an unvalidated `Option<String>` on the wire. `Cf`, the
    // bidi controls, passes straight through that. For a remote deck the
    // stamp-mismatch sentence is the ordinary path, not an edge case: a
    // released daemon never matches a branch build.
    //
    // `strip_control_and_bidi` removes rather than escapes, so scrubbing a
    // string that is already scrubbed — `deck` is — changes nothing.
    report.message = safe_display_text(message_for(report.state, &report.deck, Some(info)));
    report.server_protocol_version = info.server_protocol_version;
    report.daemon_build_version = info.daemon_build_version.as_deref().map(safe_display_text);
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
    // PRD #741 M8: no policy is decided here. It was, from the selection token,
    // which made this a third spelling of a rule [`StampPolicy::for_endpoint`]
    // already owns — the same one-implementation-three-callers shape M4(b) took
    // out of the daemon's session attach. The two arms below each hold a real
    // endpoint and ask `for_endpoint` about it; the report carries the answer up
    // to `sealed`.
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
    // A local deck has nothing to disclose — no ssh runs, so there are no
    // inherited forwards and no host-key source — and saying so is an answer:
    // `disclosure_known` with empty lists is what the panel renders as "none".
    report.disclosure_known = true;
    // PRD #741 M8: asked of the endpoint rather than written out, because
    // `Test connection` must classify a handshake exactly as the connection
    // banner does and the banner asks the same function. Writing `Enforced` here
    // would be a second copy of the rule that could drift from it.
    let stamps = StampPolicy::for_endpoint(&endpoint);
    match tunnels.acquire(&endpoint).await {
        Ok(lease) => match hello(lease.address(), stamps).await {
            Ok((info, _)) => apply_handshake(&mut report, &info, stamps),
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
            report.detail = Some(safe_display_text(error.to_string()));
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

    // The disclosure first, and never fatal: it is the cheapest thing here and
    // the one piece of the report that is still worth having when the
    // connection fails.
    //
    // **`ssh -G` opens no network connection, and that is the narrow claim**
    // (PRD #741 final audit **F7**). It is not inert locally: a `Match exec` in
    // the user's own config *runs* under `-G` — measured, the marker file was
    // created — and `PermitLocalCommand=no` does not reach it, because that
    // option governs `LocalCommand`, a different mechanism. `remote_tunnel`'s
    // `PROBE_DEADLINE_SECS` already names `Match exec` as the case its deadline
    // exists for, which is what bounds this.
    //
    // Running it first is still right, and no longer rests on that sentence:
    // every step below — `discover_socket`, then `acquire` — spawns ssh against
    // the same config and runs the same `Match exec`, so the disclosure adds no
    // local execution the button press did not already authorise. It changes
    // the order, not the set.
    let disclosure = resolve_disclosure(&ssh, &destination).await;
    report.disclosure_known = disclosure.known;
    report.forwards = disclosure.forwards;
    report.known_hosts = disclosure.known_hosts;

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
    // PRD #741 M8, as in `test_local`: the policy is the endpoint's to state. A
    // probe that refused where the banner would connect would be telling the
    // user the deck is unusable when it is about to work, and the way to keep
    // the two screens agreeing is to have them consult one function.
    let stamps = StampPolicy::for_endpoint(&endpoint);
    let lease = match tunnels.acquire(&endpoint).await {
        Ok(lease) => lease,
        Err(error) => {
            // Classified from the TYPED error, which is the whole reason
            // `acquire` hands one over (PRD #741 final audit **F3**). The
            // verdict inside it was derived by `classify_exit` from ssh's RAW
            // stderr; re-deriving it from the flattened string would be
            // matching text `scrub_remote_text` had already rewritten, and
            // stripping can only create matches the raw bytes lacked. The
            // forward cases land on `DeckNotAnswering` through
            // `state_from_tunnel_error`'s own arms rather than through a
            // substring search of `TunnelError`'s prose.
            let (state, remedy) = match &error {
                crate::endpoint_tunnels::AcquireError::Tunnel(tunnel) => {
                    state_from_tunnel_error(tunnel)
                }
                // Nothing about the far host: no ssh here, or a task that died.
                crate::endpoint_tunnels::AcquireError::Local(_) => {
                    (EndpointTestState::TransportFailed, None)
                }
            };
            report.state = state;
            report.remedy = remedy;
            report.detail = Some(safe_display_text(error.to_string()));
            report.message = message_for(state, &deck, None);
            return report;
        }
    };

    match hello(lease.address(), stamps).await {
        Ok((info, _)) => apply_handshake(&mut report, &info, stamps),
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

/// What one `ssh -G` run disclosed about the resolved configuration.
///
/// `known` is `false` whenever the resolution did not run or could not be read,
/// and that is **not** the same claim as "there are none" — a disclosure that
/// quietly says "no forwards" when it could not look is worse than one that
/// says it does not know. Both lists are gated by it because both come from the
/// one run.
#[cfg(unix)]
#[derive(Default)]
struct ConfigDisclosure {
    known: bool,
    forwards: Vec<String>,
    known_hosts: Vec<String>,
}

/// Resolve what the tunnel will inherit from the user's ssh config: the
/// forwards it will carry, and the host keys it will be checked against.
///
/// **Nothing ssh printed is dropped on the way here** (PRD #741 final audit
/// **F1**). `parse_ssh_g` keeps a forward line whose value it cannot split as a
/// `ResolvedForward::Unsplit` rather than skipping it, so a `LocalForward`
/// whose path contains a space — a macOS home directory is enough, no adversary
/// needed — reaches this list instead of vanishing into a `(true, [])` that the
/// panel would have rendered as silence.
#[cfg(unix)]
async fn resolve_disclosure(
    ssh: &dot_agent_deck::remote_tunnel::SshProgram,
    destination: &dot_agent_deck::remote_tunnel::SshDestination,
) -> ConfigDisclosure {
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
        return ConfigDisclosure::default();
    };
    if output.status != Some(0) || output.truncated {
        return ConfigDisclosure::default();
    }
    let resolved = parse_ssh_g(&output.stdout);
    ConfigDisclosure {
        known: true,
        forwards: resolved
            .forwards
            .iter()
            .map(|forward| safe_display_text(forward.to_string()))
            .collect(),
        known_hosts: resolved
            .known_hosts_lines()
            .into_iter()
            .map(safe_display_text)
            .collect(),
    }
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
    report.disclosure_known = false;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use dot_agent_deck::daemon_protocol::{AttachResponse, RunningAgentsSummary};

    /// The handshake states, driven through `daemon_bridge`'s own classifier so
    /// this cannot disagree with the connection banner about the same daemon.
    ///
    /// The local policy, because that is the one every existing case here is
    /// about; the M8 remote demotion has its own cases and names its own policy.
    fn info(response: &AttachResponse, client_build: &str) -> HandshakeInfo {
        crate::daemon_bridge::classify_handshake_for_test(
            response,
            client_build,
            StampPolicy::Enforced,
        )
    }

    fn hello_with_build(build: Option<&str>) -> AttachResponse {
        let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
            .with_running_agents(RunningAgentsSummary::default());
        reply.build_version = build.map(str::to_string);
        reply
    }

    /// PRD #741 final audit **F4**: the handshake sentence and the daemon's own
    /// build stamp are the two fields that carry the REMOTE daemon's
    /// unvalidated `build_version`, and `safe_message` (category `Cc`) lets the
    /// bidi controls straight through. A right-to-left override in a stamp
    /// reverses everything the banner prints after it.
    #[test]
    fn a_handshake_scrubs_the_remote_stamp_of_bidi_as_well_as_control() {
        let hostile = "0.38.0-g5a56361\u{202e}drowssap";
        let response = hello_with_build(Some(hostile));
        let classified = info(&response, "0.39.0-gabc1234");
        let mut report = EndpointTestReport::new(
            "deck1",
            "deploy@build-box".into(),
            EndpointTestState::TransportFailed,
            String::new(),
        );

        apply_handshake(&mut report, &classified, StampPolicy::Enforced);

        assert_eq!(report.state, EndpointTestState::BuildStampDiffers);
        assert!(
            !report.message.contains('\u{202e}'),
            "the sentence still carries the override: {:?}",
            report.message
        );
        assert!(
            !report
                .daemon_build_version
                .as_deref()
                .unwrap_or_default()
                .contains('\u{202e}'),
            "the stamp still carries the override: {:?}",
            report.daemon_build_version
        );
        assert!(
            report.daemon_build_version.as_deref() == Some("0.38.0-g5a56361drowssap"),
            "scrubbing must strip the control and keep the rest: {:?}",
            report.daemon_build_version
        );
    }

    /// PRD #741 final audit **F3**: classify the TYPED tunnel error, never the
    /// flattened string. `SshError`'s `detail` is `scrub_remote_text`-ed, and
    /// stripping can only CREATE matches the raw bytes lacked — so a peer that
    /// writes `Host key verification fai\x01led.` onto the local client's stderr
    /// must not be able to promote an unrelated failure into a host-key verdict
    /// carrying a copy-paste `ssh` remedy.
    #[test]
    fn a_scrubbed_detail_cannot_promote_a_failure_into_a_host_key_verdict() {
        use dot_agent_deck::remote::SshError;
        use dot_agent_deck::remote_tunnel::TunnelError;

        // What the flattening produced: the raw bytes classified as `Other`,
        // and the scrubbed copy inside the message reading as the host-key
        // sentence.
        let error = TunnelError::Ssh {
            deck: "deploy@build-box".into(),
            source: SshError::Other {
                target: "build-box".into(),
                detail: "Host key verification failed.".into(),
            },
        };

        let (state, remedy) = state_from_tunnel_error(&error);

        assert_eq!(
            state,
            EndpointTestState::TransportFailed,
            "the typed verdict is `Other`, so the report must not claim a host-key failure"
        );
        assert!(
            remedy.is_none(),
            "a remedy the user would paste into a terminal must follow the typed verdict"
        );
        assert!(
            error.to_string().contains("Host key verification failed."),
            "the flattened string is exactly what a substring classifier would have matched"
        );
    }

    /// A matching build and a matching wire is the one state that reads as ok.
    #[test]
    fn a_matching_deck_is_reachable() {
        let response = hello_with_build(Some("0.39.0-gabc1234"));
        let state = state_from_handshake(&info(&response, "0.39.0-gabc1234"));
        assert_eq!(state, EndpointTestState::Reachable);
        assert!(state.is_ok(StampPolicy::Enforced));
    }

    /// A stamp difference across releases is its own state, distinct from a
    /// protocol refusal — the user can override one and not the other.
    #[test]
    fn a_build_stamp_difference_is_its_own_state() {
        let response = hello_with_build(Some("0.38.0-gdead111"));
        let state = state_from_handshake(&info(&response, "0.39.0-gabc1234"));
        assert_eq!(state, EndpointTestState::BuildStampDiffers);
        assert!(
            !state.is_ok(StampPolicy::Enforced),
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
        apply_handshake(&mut report, &classified, StampPolicy::Enforced);
        assert_eq!(report.state, EndpointTestState::ProtocolRefused);
        assert_eq!(report.client_protocol_version, PROTOCOL_VERSION);
        assert_eq!(report.server_protocol_version, Some(PROTOCOL_VERSION + 1));
        assert!(
            report.message.contains(&PROTOCOL_VERSION.to_string()),
            "the sentence must name the version: {}",
            report.message
        );
    }

    /// The verdict a report seals with is the policy its HANDSHAKE was
    /// classified under — the wiring that replaced `test_endpoint`'s own copy of
    /// the local/remote split (PRD #741 M8).
    ///
    /// Both directions on one state, because one stamp difference reading two
    /// ways by deck kind is the entire behaviour, and `sealed` taking no
    /// argument is what stops that rule being spelled a third time. The remote
    /// half is the one nothing else covers: `test_remote` needs ssh, so this is
    /// where the `Informational` path through `sealed` is pinned.
    #[test]
    fn a_report_seals_under_the_policy_its_handshake_used() {
        // Classified AND sealed under the same policy, which is the only
        // pairing production makes: both call sites now derive one `stamps`
        // from `for_endpoint` and hand that same value to `hello` and to
        // `apply_handshake`.
        let seal = |stamps| {
            let classified = crate::daemon_bridge::classify_handshake_for_test(
                &hello_with_build(Some("0.1.0-gfeedface")),
                "0.39.0-gabc1234",
                stamps,
            );
            let mut report = EndpointTestReport::new(
                "deck1",
                "deploy@build-box".into(),
                EndpointTestState::TransportFailed,
                String::new(),
            );
            apply_handshake(&mut report, &classified, stamps);
            report.sealed()
        };

        let remote = seal(StampPolicy::Informational);
        assert_eq!(remote.state, EndpointTestState::BuildStampDiffers);
        assert!(
            remote.ok,
            "a remote deck's stamp difference must seal as usable: the banner is about to connect \
             to it, and the two screens disagreeing about one daemon is what this module exists \
             to avoid"
        );

        let local = seal(StampPolicy::Enforced);
        assert_eq!(local.state, EndpointTestState::BuildStampDiffers);
        assert!(
            !local.ok,
            "the same state under the local policy is not a clean pass — the override is a \
             judgement the user makes on the connection screen"
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
        assert!(!report.state.is_ok(StampPolicy::Enforced));
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
            report.disclosure_known && report.forwards.is_empty(),
            "a local deck inherits no ssh forwards, and saying so is an answer rather than an              absence"
        );
        assert!(
            report.known_hosts.is_empty(),
            "a local deck runs no ssh, so it has no resolved host-key source to name"
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
        assert!(
            !report.ok,
            "the local policy must survive the whole probe: `test_endpoint` no longer decides it, \
             so this is the end-to-end check that `for_endpoint`'s answer reaches `sealed`"
        );
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
