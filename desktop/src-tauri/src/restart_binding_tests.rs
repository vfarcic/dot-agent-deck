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
        let dir = std::env::temp_dir().join(format!("dad-{tag}-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        let socket = dir.join("s");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind the deck");
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
                            let reply = if op == "hello" {
                                *script.lock().unwrap().hellos.entry(process).or_default() += 1;
                                follow_on = request["follow_on"] == true;
                                hello_from(process)
                            } else {
                                // A snapshot's host sample is observation, not
                                // an action on an agent, and rides beside the
                                // listing concurrently: left out of the record.
                                if op != "host-metrics" {
                                    recorded.lock().unwrap().push((process, op.clone()));
                                }
                                let live = script.lock().unwrap().live.clone();
                                with_live(serve(&op), live)
                            };
                            let encoded = serde_json::to_vec(&reply).expect("serialize the reply");
                            let _ = write_frame(&mut writer, KIND_RESP, &encoded).await;
                            if op != "hello" {
                                {
                                    let mut script = script.lock().unwrap();
                                    if script.current == process
                                        && let Some((after, next)) = script.restart_after
                                        && after == op
                                    {
                                        script.current = next;
                                        script.restart_after = None;
                                    }
                                }
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
            name: "review".into(),
            display_title: None,
            cwd: "/project".into(),
            task_prompt: "list the files".into(),
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
    let stopped = crate::stop_agent_action(&state, &wire, &card.id, &card.incarnation).await;
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
    crate::stop_agent_action(&state, &wire, &card.id, &card.incarnation)
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
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles).await;
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
    let (_, closed) = crate::stop_orchestration_action(&state, &wire, &roles).await;
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
    }
}

/// Scenario: agent `1` waits on a permission question and the voice panel answers it through the real `desktop_voice_answer_question` command, invoked over IPC on Tauri's mock runtime, after the same-build deck restarted. The answer is refused with the gone sentence and the successor is sent no answer; with no restart, the deck that listed the question receives the answer (control).
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
    for restarted in [false, true] {
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
        if restarted {
            // The command reads the question afresh, then sends the answer:
            // the deck restarts between the two.
            deck.restart_after("list-agents", "b");
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
            assert_eq!(
                deck.received_by("b"),
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
