//! PRD #1666 part 2 (G-fix4): an action the app takes on an agent reaches the
//! daemon process that listed the agent, or nothing.
//!
//! Every pin drives a deck whose daemon restarts at its socket as the SAME
//! build: every compatibility check the app makes passes, and only the process
//! its `Hello` names changes. Both processes number their agent `1` in pane
//! `pane-1`, as two daemons counting from 1 would, so a request that reached
//! the restarted one would act on an unrelated agent. Each pin has a control
//! that acts on the restarted daemon's own listing and must be served.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dot_agent_deck::daemon_client::{AGENT_GONE_AFTER_RESTART, Endpoint, LocalEndpoint};
use dot_agent_deck::daemon_protocol::{
    AttachResponse, InboxActionOutcome, InboxActionReply, KIND_REQ, KIND_RESP, PROTOCOL_VERSION,
    RunningAgentsSummary, read_frame, write_frame,
};

use crate::dto::DesktopAgent;
use crate::terminal::DesktopState;

#[derive(Default)]
struct Script {
    current: &'static str,
    /// `(op, next)`: once the current process serves `op`, connections
    /// accepted afterwards are served by `next`.
    restart_after: Option<(&'static str, &'static str)>,
    hellos: HashMap<&'static str, usize>,
    terminal_probe: bool,
    /// Agent `1`'s live session state on every listing, when set — the voice
    /// answer pin's pending question rides in it.
    live: Option<serde_json::Value>,
    answer_capability: Option<bool>,
    answer_delay: Option<std::time::Duration>,
    answer_requests: Vec<serde_json::Value>,
    /// Issue #1958: whether the deck advertises `keep-worktree`, when set;
    /// unset advertises what this build does.
    keep_capability: Option<bool>,
    /// Every `stop-agent` request, as sent.
    stop_requests: Vec<serde_json::Value>,
    /// Issue #1958: a `Hello` is read and never answered, the connection held
    /// open — a deck that accepts and then goes quiet.
    silent_hello: bool,
    generate_capability: Option<bool>,
    generate_reply: Option<AttachResponse>,
    silent_generate: bool,
    generate_requests: Vec<serde_json::Value>,
    /// Issue #2399 re-review: an `invoke-card-action-batch` is read and never
    /// answered, the connection held open.
    silent_batch: bool,
    /// Every `invoke-card-action-batch` request, as sent.
    batch_requests: Vec<serde_json::Value>,
    /// Issue #2399: an `invoke-card-action-batch` is read and the connection
    /// closed without an answer — a deck that goes away mid-batch.
    drop_batch: bool,
    /// Issue #2399: a `Hello` that asks to be followed by a request is read
    /// and the connection closed — a deck that goes away before a request
    /// is written.
    drop_follow_on_hello: bool,
    /// Issue #2399: an `invoke-card-action-batch` is answered with this
    /// refusal of the whole request.
    refuse_batch: Option<&'static str>,
    /// PR #2491 review: an `invoke-card-action-batch` is answered with this
    /// per-entry result.
    batch_answer: Option<dot_agent_deck::agent_cards::CardBatchResult>,
    /// Issue #2399 (PR #2491 scope extension): whether the deck advertises
    /// `agent-cards-catalog-edit`, when set; unset advertises what this build
    /// does.
    catalog_edit_capability: Option<bool>,
    /// Every `card-catalog-edit` request, as sent.
    catalog_edit_requests: Vec<serde_json::Value>,
    /// A `card-catalog-edit` is read and never answered, the connection held
    /// open.
    silent_catalog_edit: bool,
    /// A `card-catalog-edit` is answered with this refusal.
    refuse_catalog_edit: Option<&'static str>,
    /// A `list-agent-cards` is answered with an empty card list (issue #2399
    /// review: the cards snapshot carries the editor's capability).
    list_cards: bool,
    /// PR #2491 review: every `Hello` names this attach protocol, when set —
    /// a deck the establishment handshake classifies incompatible.
    hello_protocol: Option<u32>,
    /// PR #2491 review: a `Hello` that asks to be followed by a request names
    /// this attach protocol, when set, while the establishment `Hello` names
    /// this build's.
    follow_on_hello_protocol: Option<u32>,
}

/// A deck whose daemon restarts at its socket as the same build. Every request
/// that is not a `Hello` or a host sample is recorded as `(process, op)`.
pub(crate) struct RestartingDeck {
    dir: std::path::PathBuf,
    pub(crate) socket: std::path::PathBuf,
    recorded: Arc<Mutex<Vec<(&'static str, String)>>>,
    script: Arc<Mutex<Script>>,
    server: tokio::task::JoinHandle<()>,
}

fn hello_from(process: &'static str) -> AttachResponse {
    let mut reply = AttachResponse::hello(PROTOCOL_VERSION)
        .with_capabilities()
        .with_running_agents(RunningAgentsSummary::default());
    reply.build_version = Some(dot_agent_deck::build_id::local_build_id());
    reply.instance_id = Some(process.to_string());
    reply
}

fn serve(op: &str) -> AttachResponse {
    match op {
        "list-agents" => AttachResponse::agent_records(vec![
            serde_json::from_value(serde_json::json!({
                "id": "1",
                "pane_id_env": "pane-1",
                "cwd": "/project",
                "rows": 24,
                "cols": 80,
            }))
            .expect("a minimal agent record"),
            serde_json::from_value(serde_json::json!({
                "id": "2",
                "pane_id_env": "pane-2",
                "cwd": "/project",
                "rows": 24,
                "cols": 80,
            }))
            .expect("the second role's record"),
        ]),
        "start-agent" | "start-prepared-agent" => AttachResponse::with_id("1".into()),
        "resolve-project" => serde_json::from_value(serde_json::json!({
            "ok": true, "project": {"path":"/project", "orchestrations":[]}
        }))
        .unwrap(),
        "prepare-orchestration" => AttachResponse::prepared(
            dot_agent_deck::event::PreparedOrchestration {
                path: "/project".into(),
                token: "prep-1".into(),
                context_path: "/project/.dot-agent-deck/context.md".into(),
                prompt: "Read the context and do the task".into(),
                roles: vec![dot_agent_deck::event::ProjectRole {
                    name: "orchestrator".into(),
                    start: true,
                }],
            },
            dot_agent_deck::daemon_protocol::PrepareSpelling::Orchestration,
        ),
        "attach-stream" | "resize" => {
            let mut reply = AttachResponse::ok();
            reply.viewer = Some("v1".into());
            reply.applied_rows = Some(24);
            reply.applied_cols = Some(80);
            reply
        }
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

/// `reply` with agent `1`'s live session state set to `live`, when it lists
/// agents and `live` is set.
fn with_live(reply: AttachResponse, live: Option<serde_json::Value>) -> AttachResponse {
    let Some(live) = live else {
        return reply;
    };
    let mut value = serde_json::to_value(&reply).expect("serialize the reply");
    if let Some(record) = value
        .get_mut("agent_records")
        .and_then(|records| records.get_mut(0))
    {
        record["live"] = live;
    }
    serde_json::from_value(value).expect("a reply with the live state")
}

impl RestartingDeck {
    /// Serve as the process `first` until told to restart.
    pub(crate) fn start(tag: &str, first: &'static str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        // No tag in the path: macOS's temp folder is about 50 bytes long and a
        // socket's path must stay under its 104, which a long tag overran.
        let dir = std::env::temp_dir().join(format!("dad-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let socket = dir.join("s");
        let listener = tokio::net::UnixListener::bind(&socket)
            .unwrap_or_else(|e| panic!("bind the deck {tag} at {}: {e}", socket.display()));
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("restate 0o600 on the socket inode");
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(Script {
            current: first,
            ..Script::default()
        }));
        let server = {
            let recorded = Arc::clone(&recorded);
            let script = Arc::clone(&script);
            tokio::spawn(async move {
                while let Ok((stream, _peer)) = listener.accept().await {
                    let process = script.lock().unwrap().current;
                    let recorded = Arc::clone(&recorded);
                    let script = Arc::clone(&script);
                    tokio::spawn(async move {
                        let (mut reader, mut writer) = stream.into_split();
                        let mut follow_on = true;
                        while std::mem::take(&mut follow_on) {
                            let Ok(Some((KIND_REQ, payload))) = read_frame(&mut reader).await
                            else {
                                return;
                            };
                            let request: serde_json::Value =
                                serde_json::from_slice(&payload).unwrap_or_default();
                            let op = request["op"].as_str().unwrap_or_default().to_string();
                            if op == "generate-config" {
                                script
                                    .lock()
                                    .unwrap()
                                    .generate_requests
                                    .push(request.clone());
                                if script.lock().unwrap().silent_generate {
                                    std::future::pending::<()>().await;
                                }
                            }
                            if op == "invoke-card-action-batch" {
                                script.lock().unwrap().batch_requests.push(request.clone());
                                if script.lock().unwrap().silent_batch {
                                    std::future::pending::<()>().await;
                                }
                                if script.lock().unwrap().drop_batch {
                                    return;
                                }
                            }
                            if op == "card-catalog-edit" {
                                script
                                    .lock()
                                    .unwrap()
                                    .catalog_edit_requests
                                    .push(request.clone());
                                if script.lock().unwrap().silent_catalog_edit {
                                    std::future::pending::<()>().await;
                                }
                            }
                            if op == "hello"
                                && request["follow_on"] == true
                                && script.lock().unwrap().drop_follow_on_hello
                            {
                                return;
                            }
                            if op == "stop-agent" {
                                script.lock().unwrap().stop_requests.push(request.clone());
                            }
                            if op == "answer-question" {
                                let delay = {
                                    let mut script = script.lock().unwrap();
                                    script.answer_requests.push(request.clone());
                                    if script.answer_delay.is_some()
                                        && let Some(live) = script.live.as_mut()
                                    {
                                        live["pending_question"]["answer_queued"] = true.into();
                                    }
                                    script.answer_delay
                                };
                                if let Some(delay) = delay {
                                    tokio::time::sleep(delay).await;
                                }
                            }
                            if op == "hello" && script.lock().unwrap().silent_hello {
                                std::future::pending::<()>().await;
                            }
                            let reply = if op == "hello" {
                                *script.lock().unwrap().hellos.entry(process).or_default() += 1;
                                follow_on = request["follow_on"] == true;
                                let mut hello = hello_from(process);
                                if let Some(advertise) = script.lock().unwrap().generate_capability
                                {
                                    let capabilities = hello.capabilities.get_or_insert_default();
                                    capabilities.retain(|cap| {
                                        cap != dot_agent_deck::daemon_protocol::CAP_GENERATE_CONFIG
                                    });
                                    if advertise {
                                        capabilities.push(
                                            dot_agent_deck::daemon_protocol::CAP_GENERATE_CONFIG
                                                .into(),
                                        );
                                    }
                                }
                                if let Some(advertise) =
                                    script.lock().unwrap().catalog_edit_capability
                                {
                                    let capabilities = hello.capabilities.get_or_insert_default();
                                    capabilities.retain(|cap| {
                                        cap != dot_agent_deck::daemon_protocol::CAP_AGENT_CARDS_CATALOG_EDIT
                                    });
                                    if advertise {
                                        capabilities.push(
                                            dot_agent_deck::daemon_protocol::CAP_AGENT_CARDS_CATALOG_EDIT
                                                .into(),
                                        );
                                    }
                                }
                                if let Some(advertise) = script.lock().unwrap().keep_capability {
                                    let capabilities = hello.capabilities.get_or_insert_default();
                                    capabilities.retain(|cap| {
                                        cap != dot_agent_deck::daemon_protocol::CAP_KEEP_WORKTREE
                                    });
                                    if advertise {
                                        capabilities.push(
                                            dot_agent_deck::daemon_protocol::CAP_KEEP_WORKTREE
                                                .into(),
                                        );
                                    }
                                }
                                {
                                    let script = script.lock().unwrap();
                                    let protocol = if request["follow_on"] == true {
                                        script.follow_on_hello_protocol.or(script.hello_protocol)
                                    } else {
                                        script.hello_protocol
                                    };
                                    if let Some(protocol) = protocol {
                                        hello.server_version = Some(protocol);
                                    }
                                }
                                if let Some(advertise) = script.lock().unwrap().answer_capability {
                                    let capabilities = hello.capabilities.get_or_insert_default();
                                    capabilities.retain(|cap| cap != dot_agent_deck::daemon_protocol::CAP_ANSWER_SETTLEMENT);
                                    if advertise {
                                        capabilities.push(
                                            dot_agent_deck::daemon_protocol::CAP_ANSWER_SETTLEMENT
                                                .into(),
                                        );
                                    }
                                }
                                hello
                            } else {
                                // A snapshot's host sample is observation, not
                                // an action on an agent, and rides beside the
                                // listing concurrently: left out of the record.
                                if op != "host-metrics" {
                                    recorded.lock().unwrap().push((process, op.clone()));
                                }
                                let live = script.lock().unwrap().live.clone();
                                let reply = if op == "generate-config" {
                                    script
                                        .lock()
                                        .unwrap()
                                        .generate_reply
                                        .clone()
                                        .unwrap_or_else(|| {
                                            AttachResponse::with_id("m-config".into())
                                        })
                                } else if op == "list-agent-cards"
                                    && script.lock().unwrap().list_cards
                                {
                                    AttachResponse {
                                        agent_cards: Some(
                                            dot_agent_deck::agent_cards::AgentCardList {
                                                instance_id: process.to_string(),
                                                revision: 1,
                                                entries: Vec::new(),
                                                catalog_errors: Vec::new(),
                                            },
                                        ),
                                        ..AttachResponse::ok()
                                    }
                                } else if op == "card-catalog-edit" {
                                    match script.lock().unwrap().refuse_catalog_edit {
                                        Some(refusal) => AttachResponse::err(refusal),
                                        None => AttachResponse {
                                            card_catalog_edit: Some(
                                                dot_agent_deck::card_catalog::editable_catalog(
                                                    None,
                                                ),
                                            ),
                                            ..AttachResponse::ok()
                                        },
                                    }
                                } else if let Some(refusal) = script
                                    .lock()
                                    .unwrap()
                                    .refuse_batch
                                    .filter(|_| op == "invoke-card-action-batch")
                                {
                                    AttachResponse::err(refusal)
                                } else if let Some(answer) = script
                                    .lock()
                                    .unwrap()
                                    .batch_answer
                                    .clone()
                                    .filter(|_| op == "invoke-card-action-batch")
                                {
                                    AttachResponse {
                                        card_action_batch: Some(answer),
                                        ..AttachResponse::ok()
                                    }
                                } else {
                                    with_live(serve(&op), live)
                                };
                                // The restart lands before the reply is
                                // written: no connection the client opens
                                // after reading it reaches `process`, and a
                                // `restart_after` set after the client read
                                // it never fires on this request.
                                let mut script = script.lock().unwrap();
                                if script.current == process
                                    && let Some((after, next)) = script.restart_after
                                    && after == op
                                {
                                    script.current = next;
                                    script.restart_after = None;
                                }
                                reply
                            };
                            let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
                            let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
                            if op != "hello" {
                                if op == "attach-stream" && script.lock().unwrap().terminal_probe {
                                    let _ = write_frame(
                                        &mut writer,
                                        dot_agent_deck::daemon_protocol::KIND_STREAM_OUT,
                                        b"SUCCESSOR-PTY-1666",
                                    )
                                    .await;
                                }
                                // Hold an attach or subscription open, as a
                                // daemon would, until the client leaves.
                                while let Ok(Some((kind, bytes))) = read_frame(&mut reader).await {
                                    if kind == dot_agent_deck::daemon_protocol::KIND_STREAM_IN {
                                        recorded.lock().unwrap().push((
                                            process,
                                            format!(
                                                "pty-input:{}",
                                                String::from_utf8_lossy(&bytes)
                                            ),
                                        ));
                                    }
                                }
                            }
                        }
                    });
                }
            })
        };
        Self {
            dir,
            socket,
            recorded,
            script,
            server,
        }
    }

    pub(crate) fn endpoint(&self) -> Endpoint {
        Endpoint::Local(LocalEndpoint::at(&self.socket))
    }

    pub(crate) fn config_generation(&self, advertised: bool, reply: Option<AttachResponse>) {
        let mut script = self.script.lock().unwrap();
        script.generate_capability = Some(advertised);
        script.generate_reply = reply;
        script.generate_requests.clear();
    }

    pub(crate) fn config_requests(&self) -> Vec<serde_json::Value> {
        self.script.lock().unwrap().generate_requests.clone()
    }

    pub(crate) fn silence_config_generation(&self) {
        self.script.lock().unwrap().silent_generate = true;
    }

    /// The daemon restarts now, as the process `next`.
    pub(crate) fn restart_as(&self, next: &'static str) {
        self.script.lock().unwrap().current = next;
    }

    pub(crate) fn enable_terminal_probe(&self) {
        self.script.lock().unwrap().terminal_probe = true;
    }

    /// Agent `1` waits on `question` in every listing from now on.
    pub(crate) fn ask(&self, question: &dot_agent_deck::question::PendingQuestion) {
        self.script.lock().unwrap().live = Some(serde_json::json!({
            "status": "WaitingForInput",
            "tool_count": 0,
            "pending_question": question,
        }));
    }

    /// The daemon restarts as `next` right after the current process serves
    /// `op`.
    pub(crate) fn restart_after(&self, op: &'static str, next: &'static str) {
        self.script.lock().unwrap().restart_after = Some((op, next));
    }

    /// The ops `process` received, in order.
    pub(crate) fn received_by(&self, process: &str) -> Vec<String> {
        self.recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|(by, _)| *by == process)
            .map(|(_, op)| op.clone())
            .collect()
    }

    /// How many `Hello`s `process` answered.
    pub(crate) fn hellos(&self, process: &str) -> usize {
        self.script
            .lock()
            .unwrap()
            .hellos
            .get(process)
            .copied()
            .unwrap_or(0)
    }
}

impl Drop for RestartingDeck {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The deck's row for agent `1`, as the snapshot the webview renders lists it
/// — the incarnation it carries is what the webview sends back.
async fn listed_row(deck: &RestartingDeck, state: &DesktopState) -> DesktopAgent {
    crate::daemon_bridge::snapshot_of(&deck.endpoint(), &state.daemon)
        .await
        .agents
        .into_iter()
        .find(|agent| agent.id == "1")
        .expect("the deck lists agent 1")
}

/// Point the selected deck at `deck`, as the production path resolves it.
fn select(deck: &RestartingDeck) -> crate::local_deck::test_override::LocalDeckOverride {
    let guard = crate::local_deck::test_override::point_local_deck_at(&deck.socket);
    crate::dto::apply_settings_selection(&crate::settings::DesktopSettings::default());
    guard
}

/// Scenario: the Runs screen starts an orchestration, and the same-build daemon restarts after preparing it but before the first role starts. No role is started on the successor and the launch reports the gone sentence.
#[tokio::test]
async fn handshake_013_runs_launch_never_starts_on_the_preparations_successor() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g5-runs", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    deck.restart_after("prepare-orchestration", "b");
    let result = crate::activate_orchestration_action(
        &state,
        crate::ActivateOrchestrationRequest {
            deck_id: None,
            name: "review".into(),
            display_title: None,
            cwd: "/project".into(),
            roles: vec![crate::dto::OrchestrationRoleInput {
                role: "orchestrator".into(),
                command: "claude".into(),
                start: true,
            }],
            rows: Some(24),
            cols: Some(80),
            config_revision: None,
        },
    )
    .await;
    assert_eq!(
        deck.received_by("b"),
        Vec::<String>::new(),
        "Runs launch must not start or roll back successor agents"
    );
    let failure = result.expect_err("a restarted launch must fail");
    assert!(
        failure.message().contains(AGENT_GONE_AFTER_RESTART),
        "{}",
        failure.message()
    );
    assert!(
        deck.received_by("a")
            .contains(&"prepare-orchestration".to_string()),
        "precondition: real launch caller prepared on a"
    );
}

/// Scenario: the New Agent dialog launches a configured orchestration and its daemon restarts after preparing it. The dialog must report the gone sentence without starting or stopping anything on the successor.
#[tokio::test]
async fn handshake_013_configured_launch_never_starts_on_the_preparations_successor() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g5-configured", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    deck.restart_after("prepare-orchestration", "b");
    let result = crate::start_orchestration_action(
        &state,
        &wire,
        crate::StartOrchestrationRequest {
            path: "/project".into(),
            orchestration: "review".into(),
            display_title: None,
            config_revision: None,
            rows: Some(24),
            cols: Some(80),
        },
    )
    .await;
    assert_eq!(
        deck.received_by("b"),
        Vec::<String>::new(),
        "configured launch must not start or roll back successor agents"
    );
    let failure = match result {
        Err(failure) => failure,
        Ok(_) => panic!("a restarted launch must fail"),
    };
    assert!(
        failure.message().contains(AGENT_GONE_AFTER_RESTART),
        "{}",
        failure.message()
    );
    assert!(
        deck.received_by("a")
            .contains(&"prepare-orchestration".to_string()),
        "precondition: real launch caller prepared on a"
    );
}

/// Scenario: the user opens the Stop confirmation on a card for agent `1`,
/// the deck restarts as the same build and starts an unrelated agent `1`, and
/// the user confirms. Nothing is stopped and the action fails as "that agent
/// is gone (the deck restarted)". Confirming a Stop on the restarted deck's own
/// card stops its agent (control).
#[tokio::test]
async fn handshake_013_a_stop_confirmed_after_a_restart_stops_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g4-stop", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());

    let card = listed_row(&deck, &state).await;
    assert_eq!(
        card.incarnation.as_deref(),
        Some("a"),
        "the row carries the process that listed it"
    );
    deck.restart_as("b");
    let stopped = crate::stop_agent_action(&state, &wire, &card.id, &card.incarnation, false).await;
    assert_eq!(
        stopped.err().as_deref(),
        Some(AGENT_GONE_AFTER_RESTART),
        "the user sees why nothing was stopped"
    );
    assert!(deck.hellos("b") > 0, "precondition: the stop asked b");
    assert_eq!(
        deck.received_by("b"),
        Vec::<String>::new(),
        "the restarted daemon was sent a stop for the agent before it"
    );

    let card = listed_row(&deck, &state).await;
    crate::stop_agent_action(&state, &wire, &card.id, &card.incarnation, false)
        .await
        .expect("control: the restarted deck's own card stops its agent");
    assert_eq!(
        deck.received_by("b"),
        vec!["list-agents".to_string(), "stop-agent".to_string()]
    );
}

/// Scenario: an orchestration close is confirmed for two roles listed before
/// the deck restarted as the same build. Neither is stopped on the restarted
/// deck, and the close reports both as unconfirmed with the restart's
/// sentence; the restarted deck's own roles are stopped (control).
#[tokio::test]
async fn handshake_013_an_orchestration_close_after_a_restart_stops_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g4-close", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let role = |card: &DesktopAgent| crate::dto::StopOrchestrationRole {
        agent_id: card.id.clone(),
        name: if card.id == "1" { "planner" } else { "builder" }.into(),
        incarnation: card.incarnation.clone(),
    };

    let cards = crate::daemon_bridge::snapshot_of(&deck.endpoint(), &state.daemon)
        .await
        .agents;
    assert_eq!(cards.len(), 2, "precondition: two roles were listed");
    let roles: Vec<_> = cards.iter().map(role).collect();
    deck.restart_as("b");
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles, false).await;
    let failure = closed.expect_err("a stale role's stop is not confirmed");
    assert_eq!(
        failure.unconfirmed_stops,
        vec!["planner".to_string(), "builder".to_string()]
    );
    assert!(
        failure.message.contains(AGENT_GONE_AFTER_RESTART),
        "{}",
        failure.message
    );
    assert_eq!(deck.received_by("b"), Vec::<String>::new());

    let cards = crate::daemon_bridge::snapshot_of(&deck.endpoint(), &state.daemon)
        .await
        .agents;
    let roles: Vec<_> = cards.iter().map(role).collect();
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles, false).await;
    closed.expect("control: the restarted deck's own role is stopped");
    assert_eq!(
        deck.received_by("b"),
        vec![
            "list-agents".to_string(),
            "stop-agent".to_string(),
            "stop-agent".to_string()
        ]
    );
}

/// Scenario (issue #1958): the user picks "Close agent, keep worktree" and then
/// "Close all 2 roles, keep worktree" on a deck that does not advertise the
/// keep. Both are refused with the client's sentence and the deck is sent no
/// stop at all — never a plain stop, which would remove the worktree the user
/// asked to keep. The same closes without the keep go out (control).
#[tokio::test]
async fn a_keep_close_on_a_deck_that_cannot_keep_stops_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("keep-refused", "a");
    deck.script.lock().unwrap().keep_capability = Some(false);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let cards = crate::daemon_bridge::snapshot_of(&deck.endpoint(), &state.daemon)
        .await
        .agents;
    assert_eq!(cards.len(), 2, "precondition: two roles were listed");
    let roles: Vec<_> = cards
        .iter()
        .map(|card| crate::dto::StopOrchestrationRole {
            agent_id: card.id.clone(),
            name: format!("role-{}", card.id),
            incarnation: card.incarnation.clone(),
        })
        .collect();

    let stopped =
        crate::stop_agent_action(&state, &wire, &cards[0].id, &cards[0].incarnation, true).await;
    assert_eq!(
        stopped.err().as_deref(),
        Some(dot_agent_deck::daemon_client::KEEP_WORKTREE_UNSUPPORTED)
    );
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles, true).await;
    let failure = closed.expect_err("an orchestration keep close is refused");
    assert_eq!(
        failure.message,
        dot_agent_deck::daemon_client::KEEP_WORKTREE_UNSUPPORTED
    );
    assert!(
        failure.unconfirmed_stops.is_empty(),
        "nothing was asked to stop"
    );
    assert!(
        !deck.received_by("a").contains(&"stop-agent".to_string()),
        "a keep close the deck cannot honour sends no stop: {:?}",
        deck.received_by("a")
    );

    crate::stop_agent_action(&state, &wire, &cards[0].id, &cards[0].incarnation, false)
        .await
        .expect("control: the same close without the keep stops the agent");
    let requests = deck.script.lock().unwrap().stop_requests.clone();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].get("keep_worktree").is_none(), "{requests:?}");
}

/// Scenario (issue #1958): on a deck that advertises the keep, "Close agent,
/// keep worktree" sends its stop with the keep, and "Close all 2 roles, keep
/// worktree" sends the keep on EVERY role's stop — the deck keeps the tree at
/// whichever of them it handles first.
#[tokio::test]
async fn a_keep_close_sends_the_keep_on_every_stop() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("keep-sent", "a");
    deck.script.lock().unwrap().keep_capability = Some(true);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let cards = crate::daemon_bridge::snapshot_of(&deck.endpoint(), &state.daemon)
        .await
        .agents;
    assert_eq!(cards.len(), 2, "precondition: two roles were listed");

    crate::stop_agent_action(&state, &wire, &cards[0].id, &cards[0].incarnation, true)
        .await
        .expect("the keep stop is served");
    let roles: Vec<_> = cards
        .iter()
        .map(|card| crate::dto::StopOrchestrationRole {
            agent_id: card.id.clone(),
            name: format!("role-{}", card.id),
            incarnation: card.incarnation.clone(),
        })
        .collect();
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles, true).await;
    closed.expect("the keep close is served");

    let requests = deck.script.lock().unwrap().stop_requests.clone();
    assert_eq!(requests.len(), 3, "{requests:?}");
    for request in &requests {
        assert_eq!(request["keep_worktree"], true, "{requests:?}");
    }
    let mut ids: Vec<_> = requests[1..]
        .iter()
        .map(|request| request["id"].as_str().unwrap_or_default().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, ["1", "2"], "every role is stopped with the keep");
}

/// Scenario (issue #1958, PR #2268 review): the user picks "Close all 2 roles,
/// keep worktree" on a deck the app already trusts, but whose capabilities
/// have to be asked again and which never answers that `Hello`. The close is
/// refused once the role-stop bound runs out, saying the daemon did not answer
/// whether it can keep the worktree, and no role is sent a stop.
#[tokio::test]
async fn a_keep_close_on_a_deck_that_never_answers_hello_is_refused_in_bound() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("keep-silent", "a");
    deck.script.lock().unwrap().keep_capability = Some(true);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let cards = crate::daemon_bridge::snapshot_of(&deck.endpoint(), &state.daemon)
        .await
        .agents;
    assert_eq!(cards.len(), 2, "precondition: two roles were listed");
    let roles: Vec<_> = cards
        .iter()
        .map(|card| crate::dto::StopOrchestrationRole {
            agent_id: card.id.clone(),
            name: format!("role-{}", card.id),
            incarnation: card.incarnation.clone(),
        })
        .collect();
    // A failed request elsewhere cleared the capability cache, so the close's
    // check is a fresh `Hello` — and the deck stops answering them.
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted")
        .client
        .invalidate_capabilities();
    deck.script.lock().unwrap().silent_hello = true;

    // Paused, so the bound elapses as soon as nothing but the silent `Hello`
    // is left to wait on.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles, true).await;
    let waited = started.elapsed();
    tokio::time::resume();

    let failure = closed.expect_err("a keep close the deck never confirms is refused");
    assert_eq!(failure.message, crate::keep_capability_unanswered_message());
    assert!(
        failure.unconfirmed_stops.is_empty(),
        "nothing was asked to stop: {:?}",
        failure.unconfirmed_stops
    );
    assert!(
        waited >= crate::ORCHESTRATION_ROLE_STOP_TIMEOUT
            && waited < crate::ORCHESTRATION_ROLE_STOP_TIMEOUT * 2,
        "the refusal comes from the role-stop bound: {waited:?}"
    );
    assert!(
        deck.script.lock().unwrap().stop_requests.is_empty(),
        "a keep close whose capability check went unanswered sends no stop"
    );
}

/// Scenario: the user renames agent `1`. Once with the deck restarted as the
/// same build before the rename reads the agent's directory, and once with it
/// restarted between that read and the relabel. Either way the restarted
/// daemon is neither asked for its agent's directory nor relabelled, and the
/// rename fails as the restart; renaming the restarted deck's own agent
/// relabels it (control).
#[tokio::test]
async fn handshake_013_a_rename_across_a_restart_relabels_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    for (case, before_the_read) in [("before the read", true), ("after the read", false)] {
        let deck = RestartingDeck::start("g4-rename", "a");
        let _local = select(&deck);
        let state = DesktopState::default();
        let card = listed_row(&deck, &state).await;
        if before_the_read {
            deck.restart_as("b");
        } else {
            deck.restart_after("list-agents", "b");
        }
        let renamed = crate::rename_agent_action(
            &state.daemon,
            &card.id,
            "renamed".into(),
            &card.incarnation,
        )
        .await;
        assert_eq!(
            renamed.err().as_deref(),
            Some(AGENT_GONE_AFTER_RESTART),
            "{case}"
        );
        assert!(deck.hellos("b") > 0, "{case}: precondition: b was asked");
        assert_eq!(deck.received_by("b"), Vec::<String>::new(), "{case}");
        assert!(
            !deck
                .received_by("a")
                .contains(&"set-agent-label".to_string()),
            "{case}: nothing was relabelled"
        );

        let card = listed_row(&deck, &state).await;
        crate::rename_agent_action(&state.daemon, &card.id, "renamed".into(), &card.incarnation)
            .await
            .expect("control: the restarted deck's own agent is renamed");
        assert!(
            deck.received_by("b")
                .ends_with(&["list-agents".to_string(), "set-agent-label".to_string()]),
            "{case}: control: {:?}",
            deck.received_by("b")
        );
    }
}

/// Scenario: the user presses Retry on a message of agent `1` after the deck
/// restarted as the same build. The restarted daemon is sent no Retry and the
/// press fails as the restart; Retry on the restarted deck's own card reaches
/// it (control).
#[tokio::test]
async fn handshake_013_an_inbox_retry_after_a_restart_reaches_nobody() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g4-inbox", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());

    let card = listed_row(&deck, &state).await;
    deck.restart_as("b");
    let retried = crate::inbox_action_on(
        &state,
        &wire,
        &card.id,
        "pane-1",
        "m-1",
        1,
        "retry",
        &card.incarnation,
    )
    .await;
    assert_eq!(retried.err().as_deref(), Some(AGENT_GONE_AFTER_RESTART));
    assert_eq!(deck.received_by("b"), Vec::<String>::new());

    let card = listed_row(&deck, &state).await;
    crate::inbox_action_on(
        &state,
        &wire,
        &card.id,
        "pane-1",
        "m-1",
        1,
        "retry",
        &card.incarnation,
    )
    .await
    .expect("control");
    assert_eq!(
        deck.received_by("b"),
        vec!["list-agents".to_string(), "inbox-retry".to_string()]
    );
}

/// Scenario: an orchestration launch binds its run and starts a role, the deck
/// restarts as the same build and numbers an unrelated agent the same, and
/// the launch then rolls back. The restarted daemon is sent no stop, and the
/// rollback reports the role as unconfirmed with the restart's sentence; a
/// launch bound after the restart rolls its own role back there (control).
#[tokio::test]
async fn handshake_013_a_rollback_after_a_restart_stops_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g4-rollback", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let daemon = state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("the deck is trusted");
    let options = || dot_agent_deck::daemon_client::StartAgentOptions {
        command: Some("cat".into()),
        ..Default::default()
    };

    let launch = crate::bind_launch(&daemon.client)
        .await
        .expect("bind the run");
    let started = launch.start_agent(options()).await.expect("a role on a");
    deck.restart_as("b");
    let rollback = crate::rollback_orchestration_agents(
        &launch,
        &[crate::StartedRole {
            agent_id: started,
            role: "planner".into(),
        }],
    )
    .await;
    assert_eq!(
        rollback.unconfirmed.len(),
        1,
        "the stale role is unconfirmed"
    );
    assert!(
        rollback.unconfirmed[0]
            .reason
            .contains(AGENT_GONE_AFTER_RESTART),
        "{}",
        rollback.unconfirmed[0].reason
    );
    assert!(deck.hellos("b") > 0, "precondition: the rollback asked b");
    assert_eq!(deck.received_by("b"), Vec::<String>::new());

    let launch = crate::bind_launch(&daemon.client)
        .await
        .expect("bind again");
    let started = launch.start_agent(options()).await.expect("a role on b");
    let rollback = crate::rollback_orchestration_agents(
        &launch,
        &[crate::StartedRole {
            agent_id: started,
            role: "planner".into(),
        }],
    )
    .await;
    assert!(rollback.unconfirmed.is_empty(), "control");
    assert_eq!(
        deck.received_by("b"),
        vec!["start-agent".to_string(), "stop-agent".to_string()]
    );
}

/// A permission prompt agent `1` waits on, answered through the keys its
/// option table names.
fn permission_question() -> dot_agent_deck::question::PendingQuestion {
    use dot_agent_deck::question::{
        AnswerChannel, OptionRole, PendingQuestion, Question, QuestionKind, QuestionOption,
    };
    let option = |index: u32, label: &str, role: OptionRole| QuestionOption {
        index,
        label: label.to_string(),
        description: None,
        role,
        keyboard_only: false,
        scope: None,
        maybe_hidden: false,
    };
    PendingQuestion {
        id: "q-g5".to_string(),
        kind: QuestionKind::Permission,
        questions: vec![Question {
            prompt: "Allow Bash?".to_string(),
            header: None,
            options: vec![
                option(1, "Yes", OptionRole::AllowOnce),
                option(2, "No", OptionRole::Deny),
            ],
            multi_select: false,
        }],
        raised_at_ms: 1,
        tool: None,
        channel: AnswerChannel::Keys,
        subagent_id: None,
        revision: Some(3),
        answer_queued: false,
    }
}

/// Scenario: agent `1` waits on a permission question and the voice panel answers it through the real `desktop_voice_answer_question` command, invoked over IPC on Tauri's mock runtime, with the same-build deck restarting before the question is refreshed or between that refresh and the answer. The answer is refused with the gone sentence and the successor receives no action, while a read-only listing may refresh the question; with no restart, the deck that listed the question receives the answer (control).
#[test]
fn handshake_013_a_voice_answer_after_a_restart_answers_nothing() {
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder};
    use tauri::webview::InvokeRequest;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the scripted deck");
    let _selection = runtime.block_on(crate::dto::SELECTION_LOCK.lock());
    enum Restart {
        Never,
        BeforeQuestionRead,
        AfterQuestionRead,
    }
    for restart in [
        Restart::Never,
        Restart::BeforeQuestionRead,
        Restart::AfterQuestionRead,
    ] {
        let restarted = !matches!(restart, Restart::Never);
        let deck = {
            let _entered = runtime.enter();
            RestartingDeck::start("g5-voice", "a")
        };
        deck.ask(&permission_question());
        let _local = select(&deck);
        // The row the voice panel resolved its answer against.
        let row = runtime.block_on(listed_row(&deck, &DesktopState::default()));
        let question = row
            .pending_question
            .clone()
            .expect("agent 1 waits on a question");
        assert_eq!(row.incarnation.as_deref(), Some("a"));
        match restart {
            Restart::Never => {}
            Restart::BeforeQuestionRead => deck.restart_as("b"),
            // The command reads the question afresh, then sends the answer:
            // the deck restarts between the two.
            Restart::AfterQuestionRead => deck.restart_after("list-agents", "b"),
        }

        let app = mock_builder()
            .manage(DesktopState::default())
            .invoke_handler(tauri::generate_handler![
                crate::desktop_voice_answer_question
            ])
            .build(crate::app_context())
            .expect("a mock app");
        let main = tauri::WebviewWindowBuilder::new(
            &app,
            crate::pr_browser::MAIN_WEBVIEW_LABEL,
            tauri::WebviewUrl::default(),
        )
        .build()
        .expect("the main webview");
        let answer = get_ipc_response(
            &main,
            InvokeRequest {
                cmd: "desktop_voice_answer_question".into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "tauri://localhost".parse().expect("the app origin"),
                body: InvokeBody::Json(serde_json::json!({
                    "deckId": crate::dto::deck_wire_id(&deck.endpoint()),
                    "agentId": "1",
                    "agent": "planner",
                    "questionId": question.id,
                    "form": [{"questionIndex": 0, "optionIndices": [1]}],
                    "confirmedAlways": false,
                    "lease": "7",
                    "revision": question.revision,
                    "incarnation": row.incarnation,
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        );
        if restarted {
            // Refreshing the question may list the successor before its
            // incarnation is compared with the held row. That read is not an
            // answer; every other recorded request (including PTY input)
            // must still be absent.
            let actions: Vec<_> = deck
                .received_by("b")
                .into_iter()
                .filter(|op| op != "list-agents")
                .collect();
            assert_eq!(
                actions,
                Vec::<String>::new(),
                "a voice answer reached the successor; answer={answer:?}"
            );
            assert_eq!(
                answer.as_ref().err(),
                Some(&serde_json::Value::String(
                    AGENT_GONE_AFTER_RESTART.to_string()
                )),
                "{:?}",
                answer.as_ref().map(|body| format!("{body:?}"))
            );
        } else {
            assert!(answer.is_ok(), "control: {answer:?}");
            assert!(
                deck.received_by("a")
                    .contains(&"answer-question".to_string()),
                "control: the answer reaches the deck that listed the question; {:?}",
                deck.received_by("a")
            );
        }
    }
}

/// Scenario: Invoke the real Tauri voice-answer command over mock-runtime IPC against a scripted daemon that holds the answer past its 15-second deadline. The command returns unconfirmed, its queued snapshot crosses the desktop projection, and each eventual outcome preserves the request's own token; an older daemon receives the same answer without a token.
#[test]
fn question_answer_024_tauri_timeout_and_settlement_bridge() {
    use tauri::Manager;
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder};
    use tauri::webview::InvokeRequest;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _selection = runtime.block_on(crate::dto::SELECTION_LOCK.lock());
    for supported in [true, false] {
        let deck = {
            let _entered = runtime.enter();
            RestartingDeck::start("answer-settlement", "answer-process")
        };
        deck.ask(&permission_question());
        {
            let mut script = deck.script.lock().unwrap();
            script.answer_capability = Some(supported);
            script.answer_delay = supported.then_some(std::time::Duration::from_secs(16));
        }
        let _local = select(&deck);
        let state = DesktopState::default();
        let row = runtime.block_on(listed_row(&deck, &state));
        let question = row.pending_question.unwrap();
        let app = mock_builder()
            .manage(state)
            .invoke_handler(tauri::generate_handler![
                crate::desktop_voice_answer_question
            ])
            .build(crate::app_context())
            .unwrap();
        let main = tauri::WebviewWindowBuilder::new(
            &app,
            crate::pr_browser::MAIN_WEBVIEW_LABEL,
            tauri::WebviewUrl::default(),
        )
        .build()
        .unwrap();
        let answer = get_ipc_response(&main, InvokeRequest {
            cmd: "desktop_voice_answer_question".into(), callback: CallbackFn(0), error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::Json(serde_json::json!({
                "deckId": crate::dto::deck_wire_id(&deck.endpoint()), "agentId": "1", "agent": "planner",
                "questionId": question.id, "form": [{"questionIndex": 0, "optionIndices": [1]}],
                "confirmedAlways": false, "lease": "41", "revision": question.revision, "incarnation": row.incarnation,
            })),
            headers: Default::default(), invoke_key: INVOKE_KEY.to_string(),
        }).expect("the Tauri command returns an outcome").deserialize::<serde_json::Value>().unwrap();
        let requests = deck.script.lock().unwrap().answer_requests.clone();
        assert_eq!(requests.len(), 1);
        if !supported {
            assert!(
                requests[0].get("client_token").is_none(),
                "an older daemon must not receive a token"
            );
            assert_eq!(answer["kind"], "answered");
            continue;
        }
        assert_eq!(requests[0]["client_token"], 41);
        assert_eq!(answer["kind"], "unconfirmed");
        assert_eq!(
            answer["sentence"],
            "The answer may have been sent, but planner did not confirm it — check planner before answering again."
        );
        let state = app.state::<DesktopState>();
        let queued = runtime.block_on(listed_row(&deck, &state));
        assert!(queued.pending_question.unwrap().answer_queued);
        for outcome in [
            "written",
            "partial",
            "superseded",
            "retired",
            "write_failed",
        ] {
            {
                let mut script = deck.script.lock().unwrap();
                let live = script.live.as_mut().unwrap();
                live["pending_question"]["answer_queued"] = false.into();
                live["answer_settlements"] = serde_json::json!([{
                    "question_id": question.id, "revision": question.revision, "client_token": 41, "outcome": outcome,
                }]);
            }
            let settled = runtime.block_on(listed_row(&deck, &state));
            let dto = serde_json::to_value(settled).unwrap();
            assert_eq!(
                dto["answerSettlements"][0],
                serde_json::json!({
                    "questionId": question.id, "revision": question.revision, "clientToken": 41, "outcome": outcome,
                })
            );
            assert_ne!(dto["pendingQuestion"]["answerQueued"], true);
        }
    }
}

/// Scenario: agent `1` waits on a permission question on deck process `a`, and the voice panel holds its row. The deck restarts as the same build and lists an unrelated agent `1` waiting on a question of its own. Reading the question for the held row — what resolving and answering a spoken answer both do first — refuses with "that agent is gone (the deck restarted)", and a row held with no process refuses with the identity sentence; before the restart, the held row's question is read (control).
#[tokio::test]
async fn handshake_013_a_voice_question_after_a_restart_reads_nothing() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("g6-voice-question", "a");
    deck.ask(&permission_question());
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let card = listed_row(&deck, &state).await;

    let read = crate::pending_question_on(&state, &wire, &card.id, &card.incarnation)
        .await
        .expect("the deck answers");
    assert!(
        matches!(read, Ok(Some(_))),
        "control: the held row's own question is read: {read:?}"
    );

    deck.restart_as("b");
    let read = crate::pending_question_on(&state, &wire, &card.id, &card.incarnation)
        .await
        .expect("the deck answers");
    assert_eq!(
        read.err(),
        Some(AGENT_GONE_AFTER_RESTART),
        "the successor's question was read as the held agent's"
    );
    let read = crate::pending_question_on(
        &state,
        &wire,
        &card.id,
        &dot_agent_deck::daemon_client::Incarnation::UNNAMED,
    )
    .await
    .expect("the deck answers");
    assert_eq!(
        read.err(),
        Some(dot_agent_deck::daemon_client::AGENT_IDENTITY_UNKNOWN)
    );
}

/// Scenario: reading is switched on and its notice shown, and the webview asks the real `desktop_voice_reading_start` command, over IPC on Tauri's mock runtime, to read agent `1` as listed by deck process `a`. After the same-build deck restarted, it answers that the agent is gone and asks the successor for nothing but its listing; a row held with no process is gone too. Without the restart the same request is not answered gone (control).
#[test]
fn handshake_013_a_reading_start_after_a_restart_reads_nothing() {
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{INVOKE_KEY, get_ipc_response, mock_builder};
    use tauri::webview::InvokeRequest;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the scripted deck");
    let _selection = runtime.block_on(crate::dto::SELECTION_LOCK.lock());
    let _settings = crate::settings::IsolatedSettingsEnv::new();
    let mut settings = crate::settings::DesktopSettings::default();
    settings.voice = Some(crate::settings::VoiceSettings {
        reading: crate::settings::ReadingConsent::On,
        reading_notice: crate::settings::ReadingNotice::Shown,
        ..Default::default()
    });
    crate::settings::save_to(&crate::settings::settings_path(), None, &settings)
        .expect("reading switched on");
    for (restarted, held) in [(false, Some("a")), (true, Some("a")), (false, None)] {
        let deck = {
            let _entered = runtime.enter();
            RestartingDeck::start("g7-reading", "a")
        };
        let _local = select(&deck);
        if restarted {
            deck.restart_as("b");
        }
        let app = mock_builder()
            .manage(DesktopState::default())
            .manage(crate::VoiceState::default())
            .invoke_handler(tauri::generate_handler![crate::desktop_voice_reading_start])
            .build(crate::app_context())
            .expect("a mock app");
        let main = tauri::WebviewWindowBuilder::new(
            &app,
            crate::pr_browser::MAIN_WEBVIEW_LABEL,
            tauri::WebviewUrl::default(),
        )
        .build()
        .expect("the main webview");
        let answer = get_ipc_response(
            &main,
            InvokeRequest {
                cmd: "desktop_voice_reading_start".into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "tauri://localhost".parse().expect("the app origin"),
                body: InvokeBody::Json(serde_json::json!({
                    "deckId": crate::dto::deck_wire_id(&deck.endpoint()),
                    "agentId": "1",
                    "agentLabel": "planner",
                    "onSentence": "__CHANNEL__:1",
                    "incarnation": held,
                })),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|body| {
            body.deserialize::<serde_json::Value>()
                .expect("a JSON answer")
        });
        let kind = answer
            .as_ref()
            .ok()
            .and_then(|answer| answer["kind"].as_str().map(str::to_string));
        let gone = kind.as_deref() == Some("gone");
        if restarted || held.is_none() {
            assert!(gone, "restarted={restarted} held={held:?}: {answer:?}");
            assert_eq!(
                deck.received_by("b")
                    .into_iter()
                    .filter(|op| op != "list-agents")
                    .collect::<Vec<_>>(),
                Vec::<String>::new(),
                "only the successor's listing is read"
            );
        } else {
            assert!(answer.is_ok() && !gone, "control: {answer:?}");
        }
    }
}

/// Scenario: the New agent dialog starts a plain agent on deck process `a`, and the deck restarts as the same build right after answering the start. The start's result names `a` — the process that minted the id the dialog then waits for and opens — not the restarted deck answering now; with no restart it names `a` too (control). An orchestration launch's result names the process its run's handle is bound to.
#[tokio::test]
async fn handshake_013_a_start_result_names_the_process_that_answered_it() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    for restarted in [false, true] {
        let deck = RestartingDeck::start("g7-start", "a");
        let _local = select(&deck);
        let state = DesktopState::default();
        let wire = crate::dto::deck_wire_id(&deck.endpoint());
        if restarted {
            deck.restart_after("start-agent", "b");
        }
        let started = crate::start_agent_action(
            &state,
            &wire,
            crate::StartAgentRequest {
                command: Some("cat".into()),
                cwd: Some("/project".into()),
                display_name: None,
                rows: Some(24),
                cols: Some(80),
                authoring_kind: None,
            },
        )
        .await
        .expect("the start is accepted");
        assert_eq!(started.agent_id, "1");
        assert_eq!(
            started.incarnation.as_deref(),
            Some("a"),
            "restarted={restarted}: the start's result does not name the process that answered it"
        );
    }

    // An orchestration launch's result takes the process its run's handle is
    // bound to, which minted every role id it learned.
    use crate::OrchestrationDaemon;
    let deck = RestartingDeck::start("g7-launch", "a");
    let client = dot_agent_deck::daemon_client::DaemonClient::new(deck.socket.clone());
    let bound = client.bound_to(Some("a".to_string()).into());
    assert_eq!(bound.launch_incarnation().as_deref(), Some("a"));
    assert!(client.launch_incarnation().is_unnamed());
}

/// Scenario (issue #2399 re-review): the user sends "Go ahead" to two agents
/// on a deck that takes the batch request and never answers it. When the
/// batch's bound runs out each agent's result is "unknown", with a sentence
/// saying the deck may still deliver it, never "failed"; the request was sent
/// once and not again. A preview the deck never answers fails at the ordinary
/// round-trip bound instead, since a preview sends nothing.
#[tokio::test]
async fn a_card_batch_the_deck_never_answers_ends_unknown_at_its_bound_and_is_not_resent() {
    use dot_agent_deck::agent_cards::{CardBatchEntry, CardBatchMode};
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("batch-silent", "a");
    deck.script.lock().unwrap().silent_batch = true;
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");
    let entries: Vec<_> = ["1", "2"]
        .iter()
        .map(|id| CardBatchEntry {
            pane_id: format!("pane-{id}"),
            agent_id: (*id).to_string(),
            action_token: "token".into(),
            instance_id: "a".into(),
        })
        .collect();
    let deadline = crate::card_batch_reply_timeout(CardBatchMode::Execute, entries.len());
    assert_eq!(
        deadline,
        crate::daemon_bridge::DECK_REPLY_TIMEOUT + crate::CARD_BATCH_ENTRY_ALLOWANCE * 2,
        "an execution's bound grows with its entries"
    );
    // PR #2491 review: and stops growing at the ceiling, so a large batch
    // cannot hold the app for long.
    let largest = crate::card_batch_reply_timeout(
        CardBatchMode::Execute,
        dot_agent_deck::agent_cards::MAX_BATCH_ENTRIES,
    );
    assert_eq!(largest, crate::CARD_BATCH_EXECUTE_CEILING);
    assert!(
        largest <= std::time::Duration::from_secs(180),
        "the ceiling is at most three minutes: {largest:?}"
    );
    assert_eq!(
        crate::card_batch_reply_timeout(CardBatchMode::Execute, usize::MAX),
        crate::CARD_BATCH_EXECUTE_CEILING,
        "however many entries a request names"
    );

    // Paused, so the bound elapses as soon as nothing but the silent batch is
    // left to wait on.
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let sent = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-silent-1",
        entries.clone(),
    )
    .await;
    let waited = started.elapsed();
    tokio::time::resume();

    let result = sent.expect("an unanswered execution still answers each entry");
    assert_eq!(result.request_id, "batch-silent-1");
    let agents: Vec<_> = result
        .entries
        .iter()
        .map(|entry| entry.agent_id.as_str())
        .collect();
    assert_eq!(agents, ["1", "2"], "every entry sent is answered");
    for entry in &result.entries {
        assert_eq!(entry.outcome, crate::CARD_BATCH_UNKNOWN, "{entry:?}");
        assert_ne!(entry.outcome, dot_agent_deck::agent_cards::BATCH_FAILED);
        assert_eq!(
            entry.code.as_deref(),
            Some(crate::CODE_CARD_BATCH_NO_ANSWER)
        );
        assert_eq!(
            entry.message.as_deref(),
            Some(crate::card_batch_no_answer_message(deadline).as_str())
        );
    }
    assert!(
        crate::card_batch_no_answer_message(deadline).contains("may still deliver"),
        "the sentence does not claim nothing was sent"
    );
    assert!(
        waited >= deadline && waited < deadline + crate::daemon_bridge::DECK_REPLY_TIMEOUT,
        "the answer comes from the batch's bound: {waited:?}"
    );
    {
        let script = deck.script.lock().unwrap();
        assert_eq!(
            script.batch_requests.len(),
            1,
            "the batch is not sent again"
        );
        assert_eq!(script.batch_requests[0]["mode"], "execute");
    }

    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let previewed = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Preview,
        "batch-silent-2",
        entries,
    )
    .await;
    let waited = started.elapsed();
    tokio::time::resume();
    let failure = previewed.expect_err("an unanswered preview fails: it sent nothing");
    assert!(failure.contains("did not answer the preview"), "{failure}");
    let bound = crate::card_batch_reply_timeout(CardBatchMode::Preview, 2);
    assert_eq!(bound, crate::daemon_bridge::DECK_REPLY_TIMEOUT);
    assert!(
        waited >= bound && waited < bound * 2,
        "the preview's bound is the round trip's: {waited:?}"
    );
    assert_eq!(deck.script.lock().unwrap().batch_requests.len(), 2);

    // PR #2491 review: an execution large enough that its per-entry bound
    // passes the ceiling is answered at the ceiling.
    let many: Vec<_> = (0..40)
        .map(|id| CardBatchEntry {
            pane_id: format!("pane-{id}"),
            agent_id: id.to_string(),
            action_token: "token".into(),
            instance_id: "a".into(),
        })
        .collect();
    assert!(
        crate::daemon_bridge::DECK_REPLY_TIMEOUT
            + crate::CARD_BATCH_ENTRY_ALLOWANCE * many.len() as u32
            > crate::CARD_BATCH_EXECUTE_CEILING,
        "precondition: uncapped, this batch would wait past the ceiling"
    );
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let large = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-silent-3",
        many,
    )
    .await;
    let waited = started.elapsed();
    tokio::time::resume();
    let large = large.expect("an unanswered execution still answers each entry");
    assert_eq!(large.entries.len(), 40);
    let ceiling = crate::CARD_BATCH_EXECUTE_CEILING;
    for entry in &large.entries {
        assert_eq!(entry.outcome, crate::CARD_BATCH_UNKNOWN, "{entry:?}");
        assert_eq!(
            entry.message.as_deref(),
            Some(crate::card_batch_no_answer_message(ceiling).as_str())
        );
    }
    assert!(
        waited >= ceiling && waited < ceiling + crate::daemon_bridge::DECK_REPLY_TIMEOUT,
        "a large batch waits no longer than the ceiling: {waited:?}"
    );
    assert_eq!(deck.script.lock().unwrap().batch_requests.len(), 3);
}

/// Two batch entries for agents `1` and `2`, as their preview on process `a`
/// answered them.
fn batch_entries() -> Vec<dot_agent_deck::agent_cards::CardBatchEntry> {
    ["1", "2"]
        .iter()
        .map(|id| dot_agent_deck::agent_cards::CardBatchEntry {
            pane_id: format!("pane-{id}"),
            agent_id: (*id).to_string(),
            action_token: "token".into(),
            instance_id: "a".into(),
        })
        .collect()
}

/// Scenario (issue #2399): the user sends "Go ahead" to two agents on a deck
/// that reads the batch request and then closes the connection without
/// answering. Each agent's result is "unknown" with a sentence saying the
/// connection broke after the request was sent and it is not known whether
/// the agent got it, never "failed"; the request was sent once and not again.
#[tokio::test]
async fn a_card_batch_whose_connection_breaks_after_sending_ends_unknown_and_is_not_resent() {
    use dot_agent_deck::agent_cards::CardBatchMode;
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("batch-dropped", "a");
    deck.script.lock().unwrap().drop_batch = true;
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");

    let result = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-dropped-1",
        batch_entries(),
    )
    .await
    .expect("an execution whose connection broke still answers each entry");

    assert_eq!(result.request_id, "batch-dropped-1");
    let agents: Vec<_> = result
        .entries
        .iter()
        .map(|entry| entry.agent_id.as_str())
        .collect();
    assert_eq!(agents, ["1", "2"], "every entry sent is answered");
    for entry in &result.entries {
        assert_eq!(entry.outcome, crate::CARD_BATCH_UNKNOWN, "{entry:?}");
        assert_eq!(
            entry.code.as_deref(),
            Some(crate::CODE_CARD_BATCH_CONNECTION_LOST)
        );
        let message = entry.message.as_deref().unwrap_or_default();
        assert!(
            message.starts_with(
                "the deck's answer was lost or could not be read after the request was sent"
            ) && message.contains("not known whether this agent got it"),
            "{message}"
        );
        assert!(!message.contains("nothing was sent"), "{message}");
    }
    let script = deck.script.lock().unwrap();
    assert_eq!(
        script.batch_requests.len(),
        1,
        "the batch is not sent again"
    );
    assert_eq!(script.batch_requests[0]["mode"], "execute");
}

/// Scenario (issue #2399): the user sends "Go ahead" to two agents on a deck
/// that closes the connection on the `Hello` the batch would follow, so the
/// request is never written. Each agent's result is "failed", saying nothing
/// was sent, and no batch request reached the deck. A deck that reads the
/// request and refuses it whole also answers each agent "failed": it ran none.
#[tokio::test]
async fn a_card_batch_refused_before_sending_fails_and_sends_nothing() {
    use dot_agent_deck::agent_cards::{BATCH_FAILED, CardBatchMode};
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("batch-unsent", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");
    deck.script.lock().unwrap().drop_follow_on_hello = true;

    let result = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-unsent-1",
        batch_entries(),
    )
    .await
    .expect("an execution that was not sent still answers each entry");
    assert_eq!(result.entries.len(), 2);
    for entry in &result.entries {
        assert_eq!(entry.outcome, BATCH_FAILED, "{entry:?}");
        assert_eq!(entry.code.as_deref(), Some(crate::CODE_CARD_BATCH_NOT_SENT));
        let message = entry.message.as_deref().unwrap_or_default();
        assert!(message.ends_with("; nothing was sent"), "{message}");
    }
    assert!(
        deck.script.lock().unwrap().batch_requests.is_empty(),
        "no batch request was written"
    );

    // Control: the same deck reads the request and refuses it whole.
    {
        let mut script = deck.script.lock().unwrap();
        script.drop_follow_on_hello = false;
        script.refuse_batch = Some("invoke-card-action-batch: request_invalid");
    }
    let result = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-unsent-2",
        batch_entries(),
    )
    .await
    .expect("a refused execution still answers each entry");
    for entry in &result.entries {
        assert_eq!(entry.outcome, BATCH_FAILED, "{entry:?}");
        assert_eq!(
            entry.code.as_deref(),
            Some(crate::CODE_CARD_BATCH_DECK_REFUSED)
        );
        let message = entry.message.as_deref().unwrap_or_default();
        assert!(
            message.contains("request_invalid") && message.ends_with("; nothing was delivered"),
            "{message}"
        );
    }
    assert_eq!(deck.script.lock().unwrap().batch_requests.len(), 1);
}

/// Scenario (PR #2491 review): the user sends "Go ahead" to two agents while
/// acquiring the link to their deck takes 20 s, and the deck then never
/// answers the batch. The whole send still ends at the batch's one bound, so
/// the dialog is held no longer than it: each agent reads "unknown". When the
/// link is not acquired within the bound at all, each agent reads "failed",
/// nothing sent, and no batch request reaches the deck.
#[tokio::test]
async fn a_card_batch_s_bound_covers_acquiring_the_link_to_its_deck() {
    use dot_agent_deck::agent_cards::{BATCH_FAILED, CardBatchMode};
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("batch-slow-link", "a");
    deck.script.lock().unwrap().silent_batch = true;
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");
    let deadline = crate::card_batch_reply_timeout(CardBatchMode::Execute, 2);
    let slow_link = std::time::Duration::from_secs(20);
    assert!(
        slow_link < deadline,
        "precondition: the link arrives in time"
    );
    let gate = state.daemon.establishment_gate_for_test(&deck.endpoint());

    tokio::time::pause();
    let held = Arc::clone(&gate).lock_owned().await;
    let release = tokio::spawn(async move {
        tokio::time::sleep(slow_link).await;
        drop(held);
    });
    let started = tokio::time::Instant::now();
    let sent = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-slow-link-1",
        batch_entries(),
    )
    .await;
    let waited = started.elapsed();
    tokio::time::resume();
    release.await.unwrap();
    let result = sent.expect("an unanswered execution still answers each entry");
    for entry in &result.entries {
        assert_eq!(entry.outcome, crate::CARD_BATCH_UNKNOWN, "{entry:?}");
        assert_eq!(
            entry.code.as_deref(),
            Some(crate::CODE_CARD_BATCH_NO_ANSWER)
        );
    }
    assert!(
        waited >= deadline && waited < deadline + std::time::Duration::from_secs(1),
        "the time spent acquiring the link counts against the one bound: {waited:?}"
    );
    assert_eq!(deck.script.lock().unwrap().batch_requests.len(), 1);

    tokio::time::pause();
    let held = Arc::clone(&gate).lock_owned().await;
    let started = tokio::time::Instant::now();
    let sent = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Execute,
        "batch-slow-link-2",
        batch_entries(),
    )
    .await;
    let waited = started.elapsed();
    tokio::time::resume();
    drop(held);
    let result = sent.expect("an execution that was not sent still answers each entry");
    for entry in &result.entries {
        assert_eq!(entry.outcome, BATCH_FAILED, "{entry:?}");
        assert_eq!(entry.code.as_deref(), Some(crate::CODE_CARD_BATCH_NOT_SENT));
        let message = entry.message.as_deref().unwrap_or_default();
        assert!(message.ends_with("; nothing was sent"), "{message}");
    }
    assert!(
        waited >= deadline && waited < deadline + std::time::Duration::from_secs(1),
        "a link never acquired ends at the same bound: {waited:?}"
    );
    assert_eq!(
        deck.script.lock().unwrap().batch_requests.len(),
        1,
        "no batch request was written"
    );
}

/// Scenario (PR #2491 review): the user previews "Go ahead" on two agents of
/// a deck that refuses the request whole, as it refuses a batch naming more
/// cards than it allows. Each agent is skipped with the deck's own reason,
/// rather than read as a deck that did not answer, and nothing is sent.
#[tokio::test]
async fn a_card_batch_preview_the_deck_refuses_skips_each_agent_with_the_deck_s_reason() {
    use dot_agent_deck::agent_cards::{BATCH_SKIPPED, CardBatchMode};
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("batch-preview-refused", "a");
    deck.script.lock().unwrap().refuse_batch =
        Some("invoke-card-action-batch: a batch may name at most 256 cards; this one names more");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let result = crate::agent_card_invoke_batch_on(
        &state,
        &wire,
        "a",
        "go",
        CardBatchMode::Preview,
        "batch-preview-refused-1",
        batch_entries(),
    )
    .await
    .expect("a refused preview answers each entry");
    assert_eq!(result.entries.len(), 2);
    for entry in &result.entries {
        assert_eq!(entry.outcome, BATCH_SKIPPED, "{entry:?}");
        assert_eq!(
            entry.code.as_deref(),
            Some(crate::CODE_CARD_BATCH_DECK_REFUSED)
        );
        let message = entry.message.as_deref().unwrap_or_default();
        assert!(
            message.contains("at most 256 cards")
                && message.ends_with("; nothing is sent to this agent"),
            "{message}"
        );
        assert!(!message.contains("did not answer"), "{message}");
    }
    assert_eq!(deck.script.lock().unwrap().batch_requests.len(), 1);
}

/// Scenario (PR #2491 review): the user sends "Go ahead" to two agents, and
/// the deck starts the batch: it posts to the first agent and its run is cut
/// short before it can say what became of the second, or it answers a repeat
/// of a batch whose outcomes it did not keep. The first agent reads as the
/// deck answered it, and an agent the deck cannot account for reads "unknown"
/// with the deck's own code and sentence, never "failed" and never "nothing
/// was delivered".
#[tokio::test]
async fn a_card_batch_the_deck_started_and_cannot_account_for_reads_unknown_not_failed() {
    use dot_agent_deck::agent_cards::{
        BATCH_DELIVERED, BATCH_FAILED, BATCH_UNKNOWN, CODE_BATCH_ANSWER_NOT_KEPT,
        CODE_BATCH_INTERRUPTED, CardBatchMode, CardBatchOutcome, CardBatchResult,
    };
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("batch-unaccounted", "a");
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let unknown = |pane: &str, agent: &str, code: &str| {
        CardBatchOutcome {
        pane_id: pane.into(),
        agent_id: agent.into(),
        outcome: BATCH_UNKNOWN.into(),
        code: Some(code.into()),
        message: Some("it is not known whether this agent got it; check its card's delivery history before sending again".into()),
        ..Default::default()
    }
    };
    for (request_id, answer) in [
        (
            "batch-cut-short",
            vec![
                CardBatchOutcome {
                    pane_id: "pane-1".into(),
                    agent_id: "1".into(),
                    outcome: BATCH_DELIVERED.into(),
                    message_id: Some("m-1".into()),
                    state: Some("queued".into()),
                    ..Default::default()
                },
                unknown("pane-2", "2", CODE_BATCH_INTERRUPTED),
            ],
        ),
        (
            "batch-not-kept",
            vec![
                unknown("pane-1", "1", CODE_BATCH_ANSWER_NOT_KEPT),
                unknown("pane-2", "2", CODE_BATCH_ANSWER_NOT_KEPT),
            ],
        ),
    ] {
        deck.script.lock().unwrap().batch_answer = Some(CardBatchResult {
            request_id: request_id.into(),
            entries: answer.clone(),
        });
        let result = crate::agent_card_invoke_batch_on(
            &state,
            &wire,
            "a",
            "go",
            CardBatchMode::Execute,
            request_id,
            batch_entries(),
        )
        .await
        .expect("an execution the deck started answers each entry");
        assert_eq!(
            result.entries, answer,
            "{request_id}: passed on as the deck answered it"
        );
        for entry in &result.entries {
            assert_ne!(entry.outcome, BATCH_FAILED, "{request_id}: {entry:?}");
            let message = entry.message.as_deref().unwrap_or_default();
            assert!(
                !message.contains("nothing was delivered"),
                "{request_id}: {message}"
            );
        }
        assert_eq!(
            result
                .entries
                .iter()
                .filter(|e| e.outcome == crate::CARD_BATCH_UNKNOWN)
                .count(),
            answer.iter().filter(|e| e.outcome == BATCH_UNKNOWN).count(),
            "{request_id}: the deck's unknown is the bridge's"
        );
    }
    assert_eq!(
        deck.script.lock().unwrap().batch_requests.len(),
        2,
        "each sent once"
    );
}

/// Scenario (PR #2491 review): the user previews "Go ahead" on two agents of a
/// deck that answers and declines it before the request: its handshake names
/// another attach protocol, so the app classifies it incompatible, or only
/// the `Hello` on the request's own connection does. Each agent is skipped
/// with that reason, rather than read as a deck that could not be reached or
/// did not answer, and no batch request is written. Sending to them is still
/// "failed", nothing sent.
#[tokio::test]
async fn a_card_batch_preview_a_deck_declines_at_its_handshake_skips_each_agent_with_the_reason() {
    use dot_agent_deck::agent_cards::{BATCH_FAILED, BATCH_SKIPPED, CardBatchMode};
    use dot_agent_deck::daemon_protocol::PROTOCOL_VERSION;
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    for (tag, at_establishment) in [
        ("batch-declined-establishment", true),
        ("batch-declined-follow-on", false),
    ] {
        let deck = RestartingDeck::start(tag, "a");
        {
            let mut script = deck.script.lock().unwrap();
            if at_establishment {
                script.hello_protocol = Some(PROTOCOL_VERSION + 1);
            } else {
                script.follow_on_hello_protocol = Some(PROTOCOL_VERSION + 1);
            }
        }
        let _local = select(&deck);
        let state = DesktopState::default();
        let wire = crate::dto::deck_wire_id(&deck.endpoint());
        let preview = crate::agent_card_invoke_batch_on(
            &state,
            &wire,
            "a",
            "go",
            CardBatchMode::Preview,
            &format!("{tag}-preview"),
            batch_entries(),
        )
        .await
        .expect("a declined preview answers each entry");
        assert_eq!(preview.entries.len(), 2, "{tag}");
        for entry in &preview.entries {
            assert_eq!(entry.outcome, BATCH_SKIPPED, "{tag}: {entry:?}");
            assert_eq!(
                entry.code.as_deref(),
                Some(crate::CODE_CARD_BATCH_DECK_REFUSED),
                "{tag}"
            );
            let message = entry.message.as_deref().unwrap_or_default();
            assert!(
                message.contains(&format!("attach protocol v{}", PROTOCOL_VERSION + 1))
                    && message.ends_with("; nothing is sent to this agent"),
                "{tag}: {message}"
            );
            assert!(
                !message.contains("could not be reached"),
                "{tag}: {message}"
            );
        }
        let executed = crate::agent_card_invoke_batch_on(
            &state,
            &wire,
            "a",
            "go",
            CardBatchMode::Execute,
            &format!("{tag}-execute"),
            batch_entries(),
        )
        .await
        .expect("a declined execution answers each entry");
        for entry in &executed.entries {
            assert_eq!(entry.outcome, BATCH_FAILED, "{tag}: {entry:?}");
            assert_eq!(
                entry.code.as_deref(),
                Some(crate::CODE_CARD_BATCH_NOT_SENT),
                "{tag}"
            );
        }
        assert!(
            deck.script.lock().unwrap().batch_requests.is_empty(),
            "{tag}: no batch request was written"
        );
    }
}

/// The edit the card-action editor's transport tests send: add a `ship`
/// action.
fn catalog_add() -> dot_agent_deck::card_catalog::CatalogActionEdit {
    dot_agent_deck::card_catalog::CatalogActionEdit::Add {
        action: dot_agent_deck::card_catalog::CatalogActionInput {
            name: "ship".into(),
            description: "Offer when it is ready.".into(),
            prompt: "Ship it.".into(),
            icon: Some("rocket".into()),
            ..Default::default()
        },
    }
}

/// Scenario (issue #2399, PR #2491 scope extension): the user opens the Card
/// actions editor on a deck whose daemon does not advertise it, and then on
/// one that does. The first is sent no request — reading and saving both
/// answer the sentence that the daemon's release cannot be edited from the
/// app — and the second gets exactly the request the editor made, the edit
/// as given, and its catalog comes back.
#[tokio::test]
async fn a_card_catalog_edit_goes_only_to_a_deck_that_advertises_it() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("catalog-edit-gate", "a");
    deck.script.lock().unwrap().catalog_edit_capability = Some(false);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");

    for edit in [None, Some(catalog_add())] {
        let refused = crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), edit)
            .await
            .expect_err("an older daemon cannot edit its card actions");
        assert_eq!(refused, crate::CARD_CATALOG_EDIT_UNSUPPORTED);
        assert!(
            refused.contains("cannot be edited from the app"),
            "{refused}"
        );
    }
    assert!(
        deck.script.lock().unwrap().catalog_edit_requests.is_empty(),
        "nothing is sent to a daemon that does not advertise the editor"
    );

    drop(_local);

    // Control: a deck that advertises it. The capabilities a trusted link
    // holds are its `Hello`'s, so this is a deck of its own.
    let deck = RestartingDeck::start("catalog-edit-gate-on", "b");
    deck.script.lock().unwrap().catalog_edit_capability = Some(true);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    let read = crate::agent_card_catalog_edit_on(&state, &wire, Some("b"), None)
        .await
        .expect("a read is answered");
    let names: Vec<_> = read
        .actions
        .iter()
        .map(|a| a.action.name.as_str())
        .collect();
    assert_eq!(names, ["go-ahead", "merge"]);
    crate::agent_card_catalog_edit_on(&state, &wire, Some("b"), Some(catalog_add()))
        .await
        .expect("an edit is answered");
    let script = deck.script.lock().unwrap();
    assert_eq!(script.catalog_edit_requests.len(), 2);
    assert_eq!(script.catalog_edit_requests[0]["op"], "card-catalog-edit");
    assert!(
        script.catalog_edit_requests[0]
            .get("edit")
            .is_none_or(|edit| edit.is_null()),
        "a read carries no edit"
    );
    assert_eq!(
        script.catalog_edit_requests[1]["edit"],
        serde_json::to_value(catalog_add()).unwrap()
    );
}

/// Scenario (issue #2399, PR #2491 scope extension): the user saves an action
/// the deck's daemon refuses. The editor is given the daemon's own reason, as
/// the daemon worded it, and nothing else is sent.
#[tokio::test]
async fn a_refused_card_catalog_edit_comes_back_as_the_daemon_s_reason() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("catalog-edit-refused", "a");
    const REASON: &str = "catalog.actions \"Ship It\": bad name; use 1 to 40 lowercase letters, digits and '-', starting with a letter or digit";
    {
        let mut script = deck.script.lock().unwrap();
        script.catalog_edit_capability = Some(true);
        script.refuse_catalog_edit = Some(REASON);
    }
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");
    let refused = crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), Some(catalog_add()))
        .await
        .expect_err("a refused edit is an error");
    assert_eq!(refused, REASON, "the daemon's reason, as it gave it");
    assert_eq!(deck.script.lock().unwrap().catalog_edit_requests.len(), 1);
}

/// Scenario (issue #2399, PR #2491 scope extension): the user saves an action
/// on a deck that takes the request and never answers. The editor is told,
/// at the ordinary round-trip bound, that the deck did not answer and that
/// the action may have been saved; the request was sent once, not again. A
/// read the deck never answers ends at the same bound.
#[tokio::test]
async fn a_card_catalog_edit_the_deck_never_answers_is_bounded() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("catalog-edit-silent", "a");
    {
        let mut script = deck.script.lock().unwrap();
        script.catalog_edit_capability = Some(true);
        script.silent_catalog_edit = true;
    }
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");
    let bound = crate::daemon_bridge::DECK_REPLY_TIMEOUT;

    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let edited =
        crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), Some(catalog_add())).await;
    let waited = started.elapsed();
    let read = crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), None).await;
    tokio::time::resume();

    let edited = edited.expect_err("an unanswered edit is an error");
    assert!(
        edited.contains("did not answer") && edited.contains("may have been saved"),
        "{edited}"
    );
    assert!(
        waited >= bound && waited < bound + std::time::Duration::from_secs(1),
        "the wait ends at the bound: {waited:?}"
    );
    let read = read.expect_err("an unanswered read is an error");
    assert!(read.contains("did not answer"), "{read}");
    assert!(!read.contains("saved"), "a read saves nothing: {read}");
    assert_eq!(
        deck.script.lock().unwrap().catalog_edit_requests.len(),
        2,
        "each request was sent once"
    );
}

/// Scenario (PR #2491 review, Qodo 10): the user saves an action on a deck
/// whose daemon goes away, or goes quiet, before the edit is written: it closes
/// the connection on the `Hello` that would carry the edit, or never answers
/// that `Hello`. The editor is told plainly that nothing was saved, not that
/// it may have been, and no edit request reached the deck. Control: a deck
/// that reads the edit and never answers still says it may have been saved.
#[tokio::test]
async fn a_card_catalog_edit_that_fails_before_it_is_written_says_nothing_was_saved() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("catalog-edit-unsent", "a");
    deck.script.lock().unwrap().catalog_edit_capability = Some(true);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    state
        .daemon
        .trusted(&deck.endpoint())
        .await
        .expect("precondition: the deck is trusted");

    // The deck closes the connection on the `Hello` that would carry the edit.
    deck.script.lock().unwrap().drop_follow_on_hello = true;
    let dropped = crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), Some(catalog_add()))
        .await
        .expect_err("an edit that was never written is an error");
    assert!(
        !dropped.contains(crate::CARD_CATALOG_EDIT_MAY_HAVE_SAVED),
        "nothing was sent, so nothing may have been saved: {dropped}"
    );
    assert!(dropped.contains("nothing was saved"), "{dropped}");

    // The deck never answers that `Hello`: the bound runs out before the edit
    // is written.
    {
        let mut script = deck.script.lock().unwrap();
        script.drop_follow_on_hello = false;
        script.silent_hello = true;
    }
    tokio::time::pause();
    let silent =
        crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), Some(catalog_add())).await;
    tokio::time::resume();
    let silent = silent.expect_err("an edit whose `Hello` is never answered is an error");
    assert!(
        !silent.contains(crate::CARD_CATALOG_EDIT_MAY_HAVE_SAVED),
        "the edit was never written: {silent}"
    );
    assert!(silent.contains("nothing was saved"), "{silent}");
    assert!(
        deck.script.lock().unwrap().catalog_edit_requests.is_empty(),
        "no edit reached the deck"
    );

    // Control: the deck reads the edit and never answers it.
    {
        let mut script = deck.script.lock().unwrap();
        script.silent_hello = false;
        script.silent_catalog_edit = true;
    }
    tokio::time::pause();
    let unanswered =
        crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), Some(catalog_add())).await;
    tokio::time::resume();
    let unanswered = unanswered.expect_err("an unanswered edit is an error");
    assert!(
        unanswered.contains(crate::CARD_CATALOG_EDIT_MAY_HAVE_SAVED),
        "{unanswered}"
    );
    assert_eq!(deck.script.lock().unwrap().catalog_edit_requests.len(), 1);
}

/// Scenario (PR #2491 review, Qodo 4): the user reads a deck's card actions
/// from daemon process `a`, the daemon restarts as process `b`, and the user
/// saves an action from the list `a` answered. The save is bound to `a`, so
/// `b` is sent no edit and the editor is told the daemon was restarted and
/// nothing was saved; a read bound to `a` is refused the same way. Control: a
/// read and a save bound to `b`, the process now answering, reach it.
#[tokio::test]
async fn a_card_catalog_edit_bound_to_a_replaced_daemon_is_refused() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("catalog-edit-replaced", "a");
    deck.script.lock().unwrap().catalog_edit_capability = Some(true);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), None)
        .await
        .expect("a read bound to the process answering is answered");
    assert_eq!(deck.received_by("a"), ["card-catalog-edit"]);

    deck.restart_as("b");
    let refused = crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), Some(catalog_add()))
        .await
        .expect_err("an edit bound to the replaced process is refused");
    assert_eq!(
        refused,
        format!("{}; nothing was saved", crate::CARD_CATALOG_DAEMON_REPLACED)
    );
    assert!(refused.contains("restarted"), "{refused}");
    assert!(
        !refused.contains(crate::CARD_CATALOG_EDIT_MAY_HAVE_SAVED),
        "nothing was sent, so nothing may have been saved: {refused}"
    );
    let read = crate::agent_card_catalog_edit_on(&state, &wire, Some("a"), None)
        .await
        .expect_err("a read bound to the replaced process is refused");
    assert_eq!(read, crate::CARD_CATALOG_DAEMON_REPLACED);
    assert!(
        deck.received_by("b").is_empty(),
        "the replacement was sent nothing meant for the process it replaced"
    );
    assert_eq!(deck.script.lock().unwrap().catalog_edit_requests.len(), 1);

    crate::agent_card_catalog_edit_on(&state, &wire, Some("b"), None)
        .await
        .expect("a read bound to the process now answering is answered");
    crate::agent_card_catalog_edit_on(&state, &wire, Some("b"), Some(catalog_add()))
        .await
        .expect("an edit bound to the process now answering is answered");
    assert_eq!(
        deck.received_by("b"),
        ["card-catalog-edit", "card-catalog-edit"]
    );
}

/// Scenario (PR #2491 audit, A1): the app asks to read or save a deck's card
/// actions without naming the daemon process they were read from (no
/// `instance_id`, or an empty one), here after the deck's daemon restarted as
/// process `b`. Nothing is sent and no connection is opened: the read is
/// refused, and the save is refused saying nothing was saved, instead of
/// going unbound to whichever process answers now. Control: the same save
/// naming `b` reaches it.
#[tokio::test]
async fn a_card_catalog_edit_naming_no_daemon_process_is_refused_before_connecting() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let deck = RestartingDeck::start("catalog-edit-unnamed", "a");
    deck.script.lock().unwrap().catalog_edit_capability = Some(true);
    let _local = select(&deck);
    let state = DesktopState::default();
    let wire = crate::dto::deck_wire_id(&deck.endpoint());
    deck.restart_as("b");

    for instance in [None, Some("")] {
        let refused =
            crate::agent_card_catalog_edit_on(&state, &wire, instance, Some(catalog_add()))
                .await
                .expect_err("an edit naming no daemon process is refused");
        assert_eq!(
            refused,
            format!("{}; nothing was saved", crate::CARD_CATALOG_DAEMON_UNNAMED),
            "{instance:?}"
        );
        let read = crate::agent_card_catalog_edit_on(&state, &wire, instance, None)
            .await
            .expect_err("a read naming no daemon process is refused");
        assert_eq!(read, crate::CARD_CATALOG_DAEMON_UNNAMED, "{instance:?}");
    }
    assert_eq!(
        deck.hellos("a") + deck.hellos("b"),
        0,
        "no connection was opened"
    );
    assert!(
        deck.received_by("b").is_empty(),
        "the deck was sent nothing"
    );
    assert!(deck.script.lock().unwrap().catalog_edit_requests.is_empty());

    crate::agent_card_catalog_edit_on(&state, &wire, Some("b"), Some(catalog_add()))
        .await
        .expect("an edit naming the process answering reaches it");
    assert_eq!(deck.received_by("b"), ["card-catalog-edit"]);
}

/// Scenario (issue #2399 review, finding 1): the Card actions editor reads and
/// saves the deck it names. With deck A selected, a read and an edit naming A
/// reach A alone; a read or an edit naming deck B, which the app is not
/// observing, is refused and reaches neither deck. Once B is the selected deck,
/// its read and edit reach B alone, and a form still naming A — the deck it
/// was opened on — is refused rather than written to the deck now selected.
#[tokio::test]
async fn a_card_catalog_edit_reaches_only_the_deck_it_names() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    let a = RestartingDeck::start("catalog-edit-route-a", "a");
    let b = RestartingDeck::start("catalog-edit-route-b", "b");
    for deck in [&a, &b] {
        deck.script.lock().unwrap().catalog_edit_capability = Some(true);
    }
    let sent = |deck: &RestartingDeck| deck.script.lock().unwrap().catalog_edit_requests.clone();
    let a_wire = crate::dto::deck_wire_id(&a.endpoint());
    let b_wire = crate::dto::deck_wire_id(&b.endpoint());
    assert_ne!(a_wire, b_wire, "precondition: two decks");

    let selected = select(&a);
    let state = DesktopState::default();
    crate::agent_card_catalog_edit_on(&state, &a_wire, Some("a"), None)
        .await
        .expect("a read of the selected deck is answered");
    crate::agent_card_catalog_edit_on(&state, &a_wire, Some("a"), Some(catalog_add()))
        .await
        .expect("an edit of the selected deck is answered");
    assert_eq!(sent(&a).len(), 2, "A got its read and its edit");
    assert_eq!(
        sent(&a)[1]["edit"],
        serde_json::to_value(catalog_add()).unwrap()
    );
    for edit in [None, Some(catalog_add())] {
        let refused = crate::agent_card_catalog_edit_on(&state, &b_wire, Some("b"), edit)
            .await
            .expect_err("a deck the app does not observe is not edited");
        assert!(
            refused.contains("not one this app is observing"),
            "{refused}"
        );
    }
    assert_eq!(sent(&a).len(), 2, "nothing meant for B reached A");
    assert!(sent(&b).is_empty(), "nothing reached the unobserved B");
    drop(selected);

    let _selected = select(&b);
    let state = DesktopState::default();
    crate::agent_card_catalog_edit_on(&state, &b_wire, Some("b"), None)
        .await
        .expect("a read of the newly selected deck is answered");
    crate::agent_card_catalog_edit_on(&state, &b_wire, Some("b"), Some(catalog_add()))
        .await
        .expect("an edit of the newly selected deck is answered");
    let stale = crate::agent_card_catalog_edit_on(&state, &a_wire, Some("a"), Some(catalog_add()))
        .await
        .expect_err("a form opened on A is not written once A is gone");
    assert!(stale.contains("not one this app is observing"), "{stale}");
    assert_eq!(sent(&b).len(), 2, "B got its read and its edit");
    assert_eq!(sent(&a).len(), 2, "A got nothing after it left");
}

/// Scenario (issue #2399 review, finding 1): the cards snapshot the webview
/// draws says whether the deck's daemon advertised the Card actions editor,
/// which is what makes the editor editable or read-only for that deck: a deck
/// advertising it reads `catalogEdit: true`, one that does not reads `false`.
#[tokio::test]
async fn the_card_catalog_edit_capability_reaches_the_cards_snapshot() {
    let _selection = crate::dto::SELECTION_LOCK.lock().await;
    for advertise in [true, false] {
        let deck = RestartingDeck::start("catalog-edit-snapshot", "a");
        {
            let mut script = deck.script.lock().unwrap();
            script.catalog_edit_capability = Some(advertise);
            script.list_cards = true;
        }
        let _local = select(&deck);
        let state = DesktopState::default();
        state.consume_agent_cards();
        let wire = crate::dto::deck_wire_id(&deck.endpoint());
        let cards = crate::agent_cards_list_on(&state, &wire)
            .await
            .expect("the deck's cards are read")
            .expect("the deck advertises cards");
        let snapshot = serde_json::to_value(&cards).unwrap();
        assert_eq!(snapshot["deckId"], wire.as_str());
        assert_eq!(
            snapshot["catalogEdit"], advertise,
            "advertised {advertise}: {snapshot:#}"
        );
        assert!(
            deck.script.lock().unwrap().catalog_edit_requests.is_empty(),
            "reading cards sends no catalog request"
        );
    }
}
