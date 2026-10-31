//! PRD #1666 part 2: the held client's requests that act on a deck's agents
//! (posting a message, an inbox Retry or Cancel, answering a question, stopping
//! or renaming an agent) and the senders the earlier swap pins did not reach
//! (turn-reply subscriptions, orchestration preparation, prepared and authoring
//! starts) are each admitted on their own connection by the desktop's full
//! classification.
//!
//! Every pin drives a deck whose daemon is replaced at its socket after the app
//! has classified it `Connected`, by one of three replacements: the same build
//! (the control, which must be served), the next attach protocol, and the same
//! protocol one declared contract break behind. Both refused replacements list
//! an agent under the id the first daemon used, as a fresh daemon's counter
//! would, so a request that reached one would act on an unrelated agent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use dot_agent_deck::daemon_client::{
    AgentRecord, AnswerReport, Endpoint, LocalEndpoint, StartAgentOptions,
};
use dot_agent_deck::daemon_protocol::{
    AttachResponse, InboxActionOutcome, InboxActionReply, KIND_REQ, KIND_RESP, PROTOCOL_VERSION,
    RunningAgentsSummary, read_frame, write_frame,
};

use crate::OrchestrationDaemon;
use crate::daemon_bridge::{
    AllowanceGuard, DaemonLinks, allow_build_mismatch_this_session,
    hello_from_a_deck_one_declared_break_behind,
};
use crate::voice::reading::{DaemonTurnEvents, ReadingTarget, TurnEventSource};

/// The agent id both daemons list: the first daemon's agent, and the unrelated
/// agent a replacement minted from its own counter.
const REUSED_ID: &str = "1";
const PANE: &str = "pane-1";

/// The requests a refused replacement must never receive. `Hello` is absent:
/// it is how the gate asks.
const ACTING_OPS: &[&str] = &[
    "post-message",
    "inbox-retry",
    "inbox-cancel",
    "answer-question",
    "stop-agent",
    "set-agent-label",
    "subscribe-turn-replies",
    "prepare-orchestration",
    "start-prepared-agent",
    "start-agent",
    "list-agents",
    "subscribe-events",
];

/// When the daemon at the socket is replaced.
#[derive(Clone, Copy)]
enum SwapAt {
    /// After the app's first connection, its classifying handshake.
    FirstConnection,
    /// After the connection on which the first daemon served this op, so the
    /// request under test follows a successful gated one.
    AfterOp(&'static str),
}

/// Every capability the senders under test withhold on, plus the follow-on.
fn capable(mut reply: AttachResponse) -> AttachResponse {
    use dot_agent_deck::daemon_protocol::{
        CAP_ANSWER_QUESTION, CAP_AUTHORING_KIND, CAP_HELLO_FOLLOW_ON, CAP_INBOX, CAP_INBOX_ACTIONS,
        CAP_PREPARE_ORCHESTRATION, CAP_PREPARED_ROLE_COMMAND, CAP_START_PREPARED_AGENT,
        CAP_TURN_REPLIES,
    };
    let capabilities = reply.capabilities.get_or_insert_with(Vec::new);
    for capability in [
        CAP_HELLO_FOLLOW_ON,
        CAP_INBOX,
        CAP_INBOX_ACTIONS,
        CAP_ANSWER_QUESTION,
        CAP_TURN_REPLIES,
        CAP_PREPARE_ORCHESTRATION,
        CAP_START_PREPARED_AGENT,
        CAP_PREPARED_ROLE_COMMAND,
        CAP_AUTHORING_KIND,
    ] {
        if !capabilities.iter().any(|held| held == capability) {
            capabilities.push(capability.to_string());
        }
    }
    reply
}

/// The daemon process every fixture's `Hello` names, the original and its
/// replacements alike: what these pins tell apart is the replacement's
/// contract, not its process, which `restart_binding_tests` pins (PRD #1666
/// part 2, G-fix4/G-fix5). A daemon that names no process is refused outright.
const FIXTURE_PROCESS: &str = "fixture-process";

/// The incarnation an action on one of the fixtures' agents is bound to.
fn listed_process() -> dot_agent_deck::daemon_client::Incarnation {
    dot_agent_deck::daemon_client::Incarnation::from(Some(FIXTURE_PROCESS.to_string()))
}

fn matching_hello() -> AttachResponse {
    let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
        .with_running_agents(RunningAgentsSummary::default());
    reply.build_version = Some(dot_agent_deck::build_id::local_build_id());
    reply.instance_id = Some(FIXTURE_PROCESS.to_string());
    reply
}

/// The same build, the next attach protocol, one declared break behind — and
/// whether the replacement is served.
fn replacements() -> [(&'static str, AttachResponse, bool); 3] {
    let mut newer = matching_hello();
    newer.server_version = Some(PROTOCOL_VERSION + 1);
    let mut behind = hello_from_a_deck_one_declared_break_behind();
    behind.instance_id = matching_hello().instance_id;
    [
        ("the same build", matching_hello(), true),
        ("the next attach protocol", newer, false),
        ("one declared contract break behind", behind, false),
    ]
}

fn listed(id: &str) -> AgentRecord {
    AgentRecord {
        input_held: None,
        id: id.into(),
        pane_id_env: Some(PANE.into()),
        display_name: None,
        cwd: Some("/original".into()),
        tab_membership: None,
        agent_type: None,
        rows: 24,
        cols: 80,
        live: None,
        spawned_at_ms: None,
        cli_name: None,
        prompt_keys: None,
        crashed: None,
        orchestrator_context_path: None,
        authoring_kind: None,
        inbox: None,
    }
}

/// What either daemon answers a request that is not a `Hello`: what a daemon
/// that kept the familiar variants would, so a request that reached a refused
/// replacement would succeed there.
fn serve(op: &str) -> AttachResponse {
    match op {
        "list-agents" => AttachResponse::agent_records(vec![listed(REUSED_ID)]),
        "start-agent" | "start-prepared-agent" => AttachResponse::with_id("started".into()),
        "post-message" => AttachResponse::with_id("m-posted".into()),
        "prepare-orchestration" => AttachResponse::prepared(
            dot_agent_deck::event::PreparedOrchestration {
                path: "/project".into(),
                context_path: "/project/.dot-agent-deck/context.md".into(),
                token: "prep-token".into(),
                roles: vec![dot_agent_deck::event::ProjectRole {
                    name: "orchestrator".into(),
                    start: true,
                }],
                prompt: "Read .dot-agent-deck/context.md and carry out your task.".into(),
            },
            dot_agent_deck::daemon_protocol::PrepareSpelling::Orchestration,
        ),
        "inbox-retry" | "inbox-cancel" => {
            let mut reply = AttachResponse::ok();
            reply.inbox_action = Some(InboxActionReply {
                outcome: InboxActionOutcome::Applied,
                revision: Some(2),
            });
            reply
        }
        _ => AttachResponse::ok(),
    }
}

/// A deck whose daemon is replaced at its socket at [`SwapAt`]. Records, for
/// every request the replacement receives, its op and whether it arrived on a
/// connection that opened with a `Hello`.
struct ReplacedDeck {
    dir: std::path::PathBuf,
    socket: std::path::PathBuf,
    sent: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    replacement_hellos: Arc<AtomicUsize>,
    server: tokio::task::JoinHandle<()>,
}

impl ReplacedDeck {
    fn start(tag: &str, swap_at: SwapAt, replacement: AttachResponse) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("dad-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let socket = dir.join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the deck");
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let replacement_hellos = Arc::new(AtomicUsize::new(0));
        let replacement = capable(replacement);
        let original = capable(matching_hello());
        let server = {
            let sent = Arc::clone(&sent);
            let hellos = Arc::clone(&replacement_hellos);
            tokio::spawn(async move {
                let mut swapped = false;
                // Held so a subscription stays open while the test reads.
                let mut open = Vec::new();
                while let Ok((stream, _peer)) = listener.accept().await {
                    let (mut reader, mut writer) = stream.into_split();
                    let mut follow_on = true;
                    let mut checked = false;
                    let mut swap_after_this = matches!(swap_at, SwapAt::FirstConnection);
                    while std::mem::take(&mut follow_on) {
                        let Ok(Some((KIND_REQ, payload))) = read_frame(&mut reader).await else {
                            break;
                        };
                        let request: serde_json::Value =
                            serde_json::from_slice(&payload).unwrap_or_default();
                        let op = request["op"].as_str().unwrap_or_default().to_string();
                        let reply = if op == "hello" {
                            checked = true;
                            follow_on = request["follow_on"] == true;
                            if swapped {
                                hellos.fetch_add(1, Ordering::SeqCst);
                                replacement.clone()
                            } else {
                                original.clone()
                            }
                        } else {
                            if swapped {
                                sent.lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .push((op.clone(), checked));
                            } else if matches!(swap_at, SwapAt::AfterOp(after) if after == op) {
                                swap_after_this = true;
                            }
                            serve(&op)
                        };
                        let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
                        let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
                    }
                    open.push(writer);
                    swapped |= swap_after_this;
                }
            })
        };
        Self {
            dir,
            socket,
            sent,
            replacement_hellos,
            server,
        }
    }

    fn endpoint(&self) -> Endpoint {
        Endpoint::Local(LocalEndpoint::at(&self.socket))
    }

    /// The acting requests the replacement received, and how many of them
    /// arrived on a connection that asked no `Hello` first.
    fn finish(self) -> Outcome {
        let sent = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let hellos = self.replacement_hellos.load(Ordering::SeqCst);
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
        Outcome {
            acted: sent
                .iter()
                .filter(|(op, _)| ACTING_OPS.contains(&op.as_str()))
                .map(|(op, _)| op.clone())
                .collect(),
            unchecked: sent
                .iter()
                .filter(|(op, checked)| ACTING_OPS.contains(&op.as_str()) && !checked)
                .count(),
            hellos,
        }
    }
}

struct Outcome {
    acted: Vec<String>,
    unchecked: usize,
    hellos: usize,
}

/// The one shape every pin asserts: no acting request on an unchecked
/// connection, the replacement asked first, and then either exactly `expected`
/// served (with `ok`) or nothing acting sent (with `ok` false).
fn assert_admitted(
    name: &str,
    what: &str,
    outcome: &Outcome,
    served: bool,
    ok: bool,
    expected: &[&str],
) {
    assert_eq!(
        outcome.unchecked, 0,
        "{name}: {what} reached the replacement on an unchecked connection"
    );
    assert!(
        outcome.hellos > 0,
        "{name}: precondition: {what} went to the replacement"
    );
    if served {
        assert!(ok, "{name}: {what} failed against the same build");
        assert_eq!(outcome.acted, expected, "{name}: {what}");
    } else {
        assert!(
            !ok,
            "{name}: {what} succeeded against a daemon the app would refuse"
        );
        assert_eq!(
            outcome.acted,
            Vec::<String>::new(),
            "{name}: {what}: the refused replacement was sent an acting request"
        );
    }
}

/// The deck's held client, classified against the first daemon.
async fn held(
    links: &DaemonLinks,
    endpoint: &Endpoint,
) -> Arc<crate::daemon_bridge::TrustedDaemon> {
    let daemon = links.trusted(endpoint).await.expect("the first daemon");
    daemon
        .require_compatible()
        .expect("precondition: the first daemon is compatible");
    daemon
}

fn role_options() -> StartAgentOptions {
    StartAgentOptions {
        cwd: Some("/project".into()),
        display_name: Some("role".into()),
        rows: 24,
        cols: 80,
        env: vec![(
            dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID.into(),
            crate::dto::mint_desktop_pane_id(),
        )],
        ..Default::default()
    }
}

/// Scenario: an orchestration launch has started its coordinator on a deck the
/// app classified `Connected`, and the daemon is replaced before the app posts
/// the coordinator its remit. Against a replacement on the next protocol or a
/// declared break behind, the post fails and the replacement is sent no
/// message; against the same build it is posted on a checked connection.
#[tokio::test]
async fn a_coordinator_post_after_a_swap_sends_the_replacement_nothing() {
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-post", SwapAt::FirstConnection, replacement);
        let links = DaemonLinks::default();
        let daemon = held(&links, &deck.endpoint()).await;
        let posted = OrchestrationDaemon::post_coordinator_prompt(
            &*daemon.client,
            PANE,
            "your remit",
            REUSED_ID,
            Duration::from_secs(10),
        )
        .await;
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the coordinator post",
            &outcome,
            served,
            posted.is_ok(),
            &["post-message"],
        );
    }
}

/// Scenario: the user presses Retry, then Cancel, on a message an agent's card
/// lists, and the daemon was replaced since the app classified it. Against a
/// refused replacement both presses fail and the replacement receives neither;
/// against the same build each is applied on a checked connection.
#[tokio::test]
async fn an_inbox_retry_or_cancel_after_a_swap_sends_the_replacement_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let _guard = AllowanceGuard::acquire();
    for (verb, op) in [("retry", "inbox-retry"), ("cancel", "inbox-cancel")] {
        for (name, replacement, served) in replacements() {
            let deck = ReplacedDeck::start("g3-inbox", SwapAt::FirstConnection, replacement);
            let _local_deck = crate::local_deck::test_override::point_local_deck_at(&deck.socket);
            crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
            let state = crate::terminal::DesktopState::default();
            let wire = crate::dto::deck_wire_id(&deck.endpoint());
            let acted = crate::inbox_action_on(
                &state,
                &wire,
                REUSED_ID,
                PANE,
                "m-1",
                1,
                verb,
                &listed_process(),
            )
            .await;
            let outcome = deck.finish();
            assert_admitted(name, verb, &outcome, served, acted.is_ok(), &[op]);
        }
    }
}

/// Scenario: an answer to the question an agent is waiting on is sent after
/// the daemon was replaced. A refused replacement receives no answer and the
/// call reports an error rather than `Answered`; the same build is answered on
/// a checked connection. If the user withdraws the answer while that connection
/// is being admitted, no answer is sent and the call reports `Superseded`.
#[tokio::test]
async fn an_answer_after_a_swap_sends_the_replacement_nothing() {
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-answer", SwapAt::FirstConnection, replacement);
        let links = DaemonLinks::default();
        let daemon = held(&links, &deck.endpoint()).await;
        let answered = daemon
            .client
            .answer_question_while(
                || true,
                REUSED_ID,
                "q-1",
                Some(1),
                vec![dot_agent_deck::question::QuestionAnswer {
                    question_index: 0,
                    option_indices: vec![1],
                    text: None,
                }],
                false,
            )
            .await;
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the answer",
            &outcome,
            served,
            matches!(answered, Ok(AnswerReport::Answered)),
            &["answer-question"],
        );
    }

    // The replacement's Hello is a deterministic cancellation barrier: the
    // answer is wanted before it responds, and withdrawn as it responds.
    let deck = ReplacedDeck::start("g3-withdraw", SwapAt::FirstConnection, matching_hello());
    let links = DaemonLinks::default();
    let daemon = held(&links, &deck.endpoint()).await;
    let hellos = Arc::clone(&deck.replacement_hellos);
    let answered = daemon
        .client
        .answer_question_while(
            || hellos.load(Ordering::SeqCst) == 0,
            REUSED_ID,
            "q-withdrawn",
            Some(1),
            Vec::new(),
            false,
        )
        .await;
    let outcome = deck.finish();
    assert!(outcome.hellos > 0, "the replacement must answer the gate");
    assert!(
        matches!(answered, Ok(AnswerReport::Superseded)),
        "an answer withdrawn during admission must be Superseded: {answered:?}"
    );
    assert_eq!(
        outcome.acted,
        Vec::<String>::new(),
        "the replacement received an answer withdrawn during admission"
    );
}

/// Scenario: the user stops agent 1 from its card after the daemon was
/// replaced by one that also runs an agent 1, an unrelated one. Against a
/// refused replacement the stop fails and that agent is not stopped; against
/// the same build the stop is sent on a checked connection. An orchestration
/// launch's rollback stop of the same id behaves the same way, including when
/// a card or rollback stop is repeated against the same build.
#[tokio::test]
async fn a_stop_after_a_swap_cannot_hit_the_replacements_reused_id() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-stop", SwapAt::FirstConnection, replacement);
        let _local_deck = crate::local_deck::test_override::point_local_deck_at(&deck.socket);
        crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
        let state = crate::terminal::DesktopState::default();
        let wire = crate::dto::deck_wire_id(&deck.endpoint());
        let stopped = crate::stop_agent_action(&state, &wire, REUSED_ID, &listed_process()).await;
        if served {
            crate::stop_agent_action(&state, &wire, REUSED_ID, &listed_process())
                .await
                .expect("a repeated card stop against the same build");
        }
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the card's stop",
            &outcome,
            served,
            stopped.is_ok(),
            &["stop-agent", "stop-agent"],
        );
    }
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-rollback", SwapAt::FirstConnection, replacement);
        let links = DaemonLinks::default();
        let daemon = held(&links, &deck.endpoint()).await;
        let stopped =
            OrchestrationDaemon::stop_orchestration_agent(&*daemon.client, REUSED_ID).await;
        if served {
            OrchestrationDaemon::stop_orchestration_agent(&*daemon.client, REUSED_ID)
                .await
                .expect("a repeated rollback stop against the same build");
        }
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the rollback stop",
            &outcome,
            served,
            stopped.is_ok(),
            &["stop-agent", "stop-agent"],
        );
    }
}

/// Scenario: the user renames agent 1, and the daemon is replaced after the
/// rename has read the agent's directory from the first daemon's listing but
/// before it relabels the agent. A refused replacement, which also runs an
/// agent 1, is not relabelled and the rename fails; the same build is
/// relabelled on a checked connection.
#[tokio::test]
async fn a_rename_after_its_listing_cannot_relabel_the_replacements_reused_id() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-rename", SwapAt::AfterOp("list-agents"), replacement);
        let _local_deck = crate::local_deck::test_override::point_local_deck_at(&deck.socket);
        crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
        let state = crate::terminal::DesktopState::default();
        let renamed = crate::rename_agent_action(
            &state.daemon,
            REUSED_ID,
            "renamed".into(),
            &listed_process(),
        )
        .await;
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the rename",
            &outcome,
            served,
            renamed.is_ok(),
            &["set-agent-label"],
        );
    }
}

/// Scenario: voice starts reading an agent's replies: its status subscription
/// is served by the first daemon, and the daemon is replaced before the reply
/// subscription. A refused replacement is sent no reply subscription and
/// reading does not start; the same build is subscribed on a checked
/// connection.
#[tokio::test]
async fn a_turn_reply_subscription_after_a_swap_sends_the_replacement_nothing() {
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start(
            "g3-replies",
            SwapAt::AfterOp("subscribe-events"),
            replacement,
        );
        let links = DaemonLinks::default();
        let daemon = held(&links, &deck.endpoint()).await;
        let source = DaemonTurnEvents::new(daemon.client.clone());
        let target = ReadingTarget {
            deck_id: crate::dto::deck_wire_id(&deck.endpoint()),
            agent_id: REUSED_ID.into(),
            agent_type: None,
        };
        let subscribed = source.subscribe(&target).await;
        let ok = subscribed.is_ok();
        drop(subscribed);
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the reply subscription",
            &outcome,
            served,
            ok,
            &["subscribe-turn-replies"],
        );
    }
}

/// Scenario: an orchestration launch prepares the orchestration after the
/// daemon was replaced. A refused replacement is sent no preparation; the same
/// build receives it on a checked connection and returns the preparation the
/// launch needs.
#[tokio::test]
async fn a_preparation_after_a_swap_sends_the_replacement_nothing() {
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-prep", SwapAt::FirstConnection, replacement);
        let links = DaemonLinks::default();
        let daemon = held(&links, &deck.endpoint()).await;
        let prepared = OrchestrationDaemon::prepare_orchestration(
            &*daemon.client,
            "/project",
            "team",
            "the task",
            None,
        )
        .await;
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the preparation",
            &outcome,
            served,
            prepared.is_ok(),
            &["prepare-orchestration"],
        );
        if let Ok(prepared) = prepared {
            assert_eq!(prepared.token, "prep-token", "{name}: preparation token");
            assert_eq!(prepared.path, "/project", "{name}: preparation project");
        }
    }
}

/// Scenario: an orchestration launch starts a prepared role, by token and with
/// the role's configured command, after the daemon was replaced. A refused
/// replacement starts neither; the same build starts each on a checked
/// connection.
#[tokio::test]
async fn a_prepared_role_start_after_a_swap_sends_the_replacement_nothing() {
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-role", SwapAt::FirstConnection, replacement);
        let links = DaemonLinks::default();
        let daemon = held(&links, &deck.endpoint()).await;
        let by_token = OrchestrationDaemon::start_orchestration_agent(
            &*daemon.client,
            StartAgentOptions {
                command: Some("cat".into()),
                ..role_options()
            },
            Some("prep-token"),
        )
        .await;
        let configured = OrchestrationDaemon::start_configured_role(
            &*daemon.client,
            role_options(),
            "prep-token",
        )
        .await;
        let outcome = deck.finish();
        let ok = by_token.is_ok()
            && matches!(
                configured,
                Ok(dot_agent_deck::daemon_client::GatedQuery::Answered(_))
            );
        let failed = by_token.is_err() && configured.is_err();
        assert_admitted(
            name,
            "the prepared starts",
            &outcome,
            served,
            if served { ok } else { !failed },
            &["start-prepared-agent", "start-prepared-agent"],
        );
    }
}

/// Scenario: the user starts a `schedule` authoring agent from the New agent
/// dialog after the daemon was replaced; the form's own capability handshake
/// reaches the replacement first. A refused replacement starts nothing; the
/// same build starts it on a checked connection.
#[tokio::test]
async fn an_authoring_start_after_a_swap_sends_the_replacement_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let _guard = AllowanceGuard::acquire();
    for (name, replacement, served) in replacements() {
        let deck = ReplacedDeck::start("g3-author", SwapAt::FirstConnection, replacement);
        let _local_deck = crate::local_deck::test_override::point_local_deck_at(&deck.socket);
        crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
        let state = crate::terminal::DesktopState::default();
        let wire = crate::dto::deck_wire_id(&deck.endpoint());
        let started = crate::start_agent_action(
            &state,
            &wire,
            crate::StartAgentRequest {
                command: Some("claude".into()),
                cwd: Some("/project".into()),
                display_name: Some("author".into()),
                rows: Some(24),
                cols: Some(80),
                authoring_kind: Some(dot_agent_deck::authoring_seeds::AuthoringKind::Schedule),
            },
        )
        .await;
        let outcome = deck.finish();
        assert_admitted(
            name,
            "the authoring start",
            &outcome,
            served,
            started.is_ok(),
            &["start-agent"],
        );
    }
}

/// Scenario: with Connect anyway chosen for the session, every acting request
/// above reaches a replacement one declared contract break behind, each on a
/// checked connection, and none reaches a replacement on the next protocol.
#[tokio::test]
async fn connect_anyway_admits_acting_requests_a_break_behind_but_not_another_protocol() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let _guard = AllowanceGuard::acquire();
    allow_build_mismatch_this_session();
    let [_, (_, newer, _), (_, behind, _)] = replacements();
    for (name, replacement, served) in [
        ("next protocol", newer, false),
        ("a break behind", behind, true),
    ] {
        let deck = ReplacedDeck::start("g3-allow", SwapAt::FirstConnection, replacement);
        let _local_deck = crate::local_deck::test_override::point_local_deck_at(&deck.socket);
        crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
        let state = crate::terminal::DesktopState::default();
        let endpoint = deck.endpoint();
        let wire = crate::dto::deck_wire_id(&endpoint);
        let daemon = held(&state.daemon, &endpoint).await;
        let posted = OrchestrationDaemon::post_coordinator_prompt(
            &*daemon.client,
            PANE,
            "your remit",
            REUSED_ID,
            Duration::from_secs(10),
        )
        .await
        .is_ok();
        let retried = crate::inbox_action_on(
            &state,
            &wire,
            REUSED_ID,
            PANE,
            "m-1",
            1,
            "retry",
            &listed_process(),
        )
        .await
        .is_ok();
        let answered = matches!(
            daemon
                .client
                .answer_question_while(|| true, REUSED_ID, "q-1", Some(1), Vec::new(), false)
                .await,
            Ok(AnswerReport::Answered)
        );
        let stopped = crate::stop_agent_action(&state, &wire, REUSED_ID, &listed_process())
            .await
            .is_ok();
        let outcome = deck.finish();
        let results = [posted, retried, answered, stopped];
        assert_eq!(
            outcome.unchecked, 0,
            "{name}: an acting request on an unchecked connection"
        );
        if served {
            assert_eq!(
                results, [true; 4],
                "{name}: Connect anyway admits the break"
            );
            assert_eq!(
                outcome.acted,
                vec![
                    "post-message",
                    "inbox-retry",
                    "answer-question",
                    "stop-agent"
                ],
                "{name}"
            );
        } else {
            assert_eq!(
                results, [false; 4],
                "{name}: Connect anyway bypassed the protocol"
            );
            assert_eq!(outcome.acted, Vec::<String>::new(), "{name}");
        }
    }
}
