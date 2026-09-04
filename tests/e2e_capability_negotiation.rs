#![cfg(all(feature = "e2e", unix))]

//! L2 lane-1 coverage for PRD #819 M5's capability negotiation on `Hello`:
//! what a project-aware client does against a daemon that does not advertise
//! the project verbs.
//!
//! The subject under test is the real client helper —
//! `DaemonClient::require_capability` over `DaemonCapabilities`
//! (`src/daemon_client.rs`) — driven across a REAL Unix socket. The fast-tier
//! unit tests beside that type already pin its pure reading of a reply. What
//! they cannot pin is the property this file exists for: that a withheld verb
//! is one the **wire never carries**. The peer here records the `op` of every
//! request it receives, so "withheld" is asserted as an absence from the
//! daemon's own request log rather than as an error the client happened to
//! return. A client that sent `list-projects` and then handled the refusal
//! would fail this test too — but it would be branching on serde's
//! `unknown variant …` text, which the PRD forbids, and the request log is
//! what tells the two apart.
//!
//! **Why a scripted peer rather than a real `daemon serve`.** The production
//! `Hello` handler calls `AttachResponse::with_capabilities()` unconditionally
//! and that builder takes no argument, so no real daemon can be asked to omit
//! or narrow the field. The alternative — a production env knob on the
//! `DOT_AGENT_DECK_TEST_OMIT_RUNNING_AGENTS` model — would fake the very thing
//! being measured, on the path this PRD is trying to make trustworthy
//! (CLAUDE.md rule 12 warns off that variable by name). A protocol-faithful
//! fake peer is the honest double here: the client is the production one, and
//! only the daemon is synthetic.
//!
//! [`ScriptedDaemon`] below is a trimmed COPY of the one in
//! `tests/e2e_tab_close_regressions.rs`, not a lift into
//! `tests/common/mod.rs`. `common` is linked by every `tests/e2e_*.rs` file, so
//! editing it makes "the tests covering what you touched" the whole tier
//! (CLAUDE.md rule 5) — too much for a ~150-line convenience with two call
//! sites.
//!
//! Lane 1: no credential, no real agent, no spawned binary — the only child
//! this file creates is a thread. `unix` on the file gate because it binds and
//! chmods a Unix-domain socket, the same pattern as
//! `tests/e2e_project_verbs.rs` and `tests/e2e_tab_close_regressions.rs`. No
//! body here sleeps or polls: every wait is a blocking read on the socket
//! (linkage-check Decision 21).

mod common;

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use dot_agent_deck::daemon_client::{ClientError, DaemonClient};
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, CAP_LIST_PROJECTS, DAEMON_CAPABILITIES, KIND_REQ,
    PROTOCOL_VERSION, read_frame, write_resp,
};
use dot_agent_deck::event::{KnownProject, ProjectListing};
use spec::spec;
use tempfile::TempDir;
use tokio::net::{UnixListener, UnixStream};
use tokio::runtime::Runtime;

/// The `op` strings the three project verbs serialize to. Read from the
/// capability constants rather than re-spelled, because PRD #819 makes the
/// capability string and the `op` name deliberately the same token — a test
/// with its own copy could pass while the two drifted.
const PROJECT_VERB_OPS: &[&str] = DAEMON_CAPABILITIES;

/// The single project this scripted daemon knows, when it knows any. Named so
/// the positive half can tell a real reply from a default-constructed one.
const ADVERTISED_PROJECT_NAME: &str = "capability-negotiation-fixture";

/// Whether the scripted daemon's `Hello` reply carries `capabilities`.
#[derive(Clone, Copy)]
enum CapabilityScript {
    /// A daemon at this build: `Hello` advertises [`DAEMON_CAPABILITIES`], and
    /// the project verbs are answered.
    Advertising,
    /// A daemon that predates PRD #819: `Hello` omits the field entirely, and a
    /// project verb would fail its frame decode — which is what the real
    /// serve loop turns into `malformed request: unknown variant …`
    /// (`src/daemon_protocol.rs:1782-1789`), the text nothing may branch on.
    Silent,
}

/// A protocol-faithful synthetic daemon on its own attach socket, which records
/// the `op` of every request it is asked to answer.
struct ScriptedDaemon {
    socket_path: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    _dir: TempDir,
}

impl ScriptedDaemon {
    fn spawn(script: CapabilityScript) -> Self {
        let dir = common::harness_tempdir().expect("scripted daemon tempdir");
        let socket_path = dir.path().join("daemon.sock");
        let listener = StdUnixListener::bind(&socket_path).expect("bind scripted daemon socket");
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            .expect("make scripted daemon socket owner-only");
        listener
            .set_nonblocking(true)
            .expect("set scripted daemon listener nonblocking");

        let requests = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests_for_thread = Arc::clone(&requests);
        let shutdown_for_thread = Arc::clone(&shutdown);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("scripted daemon runtime");
            runtime.block_on(async move {
                let listener =
                    UnixListener::from_std(listener).expect("convert scripted listener to tokio");
                loop {
                    let (stream, _) = listener
                        .accept()
                        .await
                        .expect("accept scripted daemon client");
                    if shutdown_for_thread.load(Ordering::SeqCst) {
                        break;
                    }
                    let requests = Arc::clone(&requests_for_thread);
                    tokio::spawn(async move {
                        handle_connection(stream, requests, script).await;
                    });
                }
            });
        });

        Self {
            socket_path,
            requests,
            shutdown,
            thread: Some(thread),
            _dir: dir,
        }
    }

    fn path(&self) -> &Path {
        &self.socket_path
    }

    /// Every `op` this daemon has been asked to answer, in arrival order. The
    /// ordering is deterministic without any synchronisation of its own: each
    /// request is recorded before its reply is written, and the client only
    /// issues the next one after reading that reply.
    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = StdUnixStream::connect(&self.socket_path);
        if let Some(thread) = self.thread.take() {
            thread.join().expect("join scripted daemon thread");
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    requests: Arc<Mutex<Vec<String>>>,
    script: CapabilityScript,
) {
    let Some((kind, payload)) = read_frame(&mut stream)
        .await
        .expect("read scripted daemon request")
    else {
        // The shutdown poke connects and deliberately sends nothing.
        return;
    };
    assert_eq!(kind, KIND_REQ, "scripted daemon expects request frames");

    // Decoded as a `serde_json::Value`, not as an `AttachRequest`: this build's
    // request enum knows the project verbs, and a daemon that predates them is
    // exactly what the `Silent` script has to be able to impersonate.
    let request: serde_json::Value =
        serde_json::from_slice(&payload).expect("decode scripted daemon request");
    let op = request
        .get("op")
        .and_then(|op| op.as_str())
        .expect("every AttachRequest carries an `op` tag")
        .to_string();
    requests.lock().unwrap().push(op.clone());

    let response = match (op.as_str(), script) {
        ("hello", CapabilityScript::Advertising) => {
            AttachResponse::hello(PROTOCOL_VERSION).with_capabilities()
        }
        // The pre-PRD-#819 shape: a well-formed handshake with no
        // `capabilities` key at all.
        ("hello", CapabilityScript::Silent) => AttachResponse::hello(PROTOCOL_VERSION),
        (op, CapabilityScript::Advertising) if op == CAP_LIST_PROJECTS => {
            let mut resp = AttachResponse::ok();
            resp.projects = Some(ProjectListing {
                projects: vec![KnownProject {
                    path: "/scripted/capability-negotiation".to_string(),
                    name: ADVERTISED_PROJECT_NAME.to_string(),
                }],
                primary: None,
            });
            resp
        }
        (op, CapabilityScript::Silent) if PROJECT_VERB_OPS.contains(&op) => {
            // What the real serve loop replies when the frame does not decode.
            // Reached only by a client that sent the verb anyway — which is the
            // failure this test is here to catch.
            AttachResponse::err(format!(
                "malformed request: unknown variant `{op}`, expected one of `list-agents`, \
                 `start-agent`, `hello`"
            ))
        }
        (op, _) => AttachResponse::err(format!("scripted daemon has no arm for `{op}`")),
    };
    write_resp(&mut stream, &response)
        .await
        .expect("reply to scripted daemon request");
}

/// A project-aware call site, in the shape PRD #819 M5 prescribes: consult the
/// capability captured at the handshake FIRST, and open the socket only if the
/// verb was advertised.
///
/// It lives here rather than in `src/` because the PRD's *Capability
/// negotiation* section puts the TUI's five `ui.rs` sites out of scope, so this
/// build has no production caller of `require_capability` yet to point the test
/// at. Both halves below drive this one function, which is what makes the
/// positive half load-bearing: a helper that withheld unconditionally would
/// fail it.
fn guarded_list_projects(
    runtime: &Runtime,
    client: &DaemonClient,
    socket: &Path,
) -> Result<AttachResponse, ClientError> {
    runtime.block_on(client.require_capability(CAP_LIST_PROJECTS))?;
    common::attach_request_on(socket, &AttachRequest::ListProjects {}).map_err(ClientError::Io)
}

/// Scenario: Stand up a synthetic daemon on a real attach socket whose `Hello`
/// omits `capabilities` — an older daemon, exactly — and point a real
/// `DaemonClient` at it through a call site that asks before it sends; every
/// project verb must be withheld, and the daemon's own request log must show
/// nothing but the one handshake, proving the verb never reached the wire and
/// its refusal text was never read. Then repeat against a second synthetic
/// daemon that DOES advertise: the same call site proceeds, `list-projects`
/// appears in that daemon's log, and the reply comes back.
#[spec("lifecycle/handshake/008")]
#[test]
fn handshake_008_absent_capabilities_withhold_project_verbs_before_the_wire() {
    common::init_test_env();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build capability-negotiation runtime");

    // ---- Half 1: a daemon that advertises nothing. -----------------------
    let silent = ScriptedDaemon::spawn(CapabilityScript::Silent);
    let client = DaemonClient::new(silent.path().to_path_buf());

    let captured = runtime
        .block_on(client.capabilities())
        .expect("the handshake itself succeeds — it is the ADVERTISEMENT that is absent");
    assert!(
        !captured.is_advertised(),
        "a `Hello` with no `capabilities` key must read as unadvertised, not as an empty set"
    );

    for capability in DAEMON_CAPABILITIES {
        assert!(
            !captured.supports(capability),
            "`{capability}` must not be treated as supported by a daemon that never named it"
        );
        let err = runtime
            .block_on(client.require_capability(capability))
            .expect_err("an unadvertised verb must be withheld");
        assert!(
            matches!(err, ClientError::Server(_)),
            "the withhold is the client's own decision, not a transport failure: {err:?}"
        );
        assert!(
            err.to_string().contains(capability),
            "the decline must name the capability that was checked: {err}"
        );
    }

    let withheld = guarded_list_projects(&runtime, &client, silent.path())
        .expect_err("the call site must not reach the socket");
    assert!(
        withheld.to_string().contains(CAP_LIST_PROJECTS),
        "the call site propagates the capability decline verbatim: {withheld}"
    );

    // THE ASSERTION THIS FILE EXISTS FOR. Not "the verb was refused" — "the
    // verb was never sent". The daemon logged every op it was asked to answer,
    // and one handshake is the whole of it: three capability questions cost a
    // single `Hello` (the set is captured once), and no project verb followed.
    let silent_log = silent.requests();
    assert_eq!(
        silent_log,
        vec!["hello".to_string()],
        "an unadvertising daemon must see the handshake and nothing else"
    );
    for verb in PROJECT_VERB_OPS {
        assert!(
            !silent_log.iter().any(|op| op == verb),
            "`{verb}` reached the wire; the client read a refusal instead of withholding, \
             which is the serde-error-text branch PRD #819 forbids (log: {silent_log:?})"
        );
    }

    // ---- Half 2: the same call site against a daemon that DOES advertise. -
    // Without this, a client that withheld unconditionally would pass.
    let advertising = ScriptedDaemon::spawn(CapabilityScript::Advertising);
    let client = DaemonClient::new(advertising.path().to_path_buf());

    for capability in DAEMON_CAPABILITIES {
        runtime
            .block_on(client.require_capability(capability))
            .unwrap_or_else(|e| panic!("`{capability}` was advertised and must be permitted: {e}"));
    }

    let reply = guarded_list_projects(&runtime, &client, advertising.path())
        .expect("an advertised verb must proceed to the socket");
    assert!(reply.ok, "the scripted daemon answered the verb: {reply:?}");
    let listing = reply
        .projects
        .expect("an advertised `list-projects` comes back with a listing");
    assert_eq!(
        listing
            .projects
            .iter()
            .map(|project| project.name.as_str())
            .collect::<Vec<_>>(),
        vec![ADVERTISED_PROJECT_NAME],
        "the reply is the scripted daemon's, not a default-constructed one"
    );
    assert_eq!(
        advertising.requests(),
        vec!["hello".to_string(), CAP_LIST_PROJECTS.to_string()],
        "the advertised verb reached the wire exactly once, after one handshake"
    );
}
