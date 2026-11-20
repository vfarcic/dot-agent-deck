//! PRD #1790 stage 2: the desktop's interrupted work commands against a
//! scripted daemon — an older one that advertises no `interrupted-work`,
//! another daemon process than the one named, and a supporting one.

use super::*;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, CAP_HELLO_FOLLOW_ON, CAP_INTERRUPTED_WORK, KIND_REQ, KIND_RESP,
    PROTOCOL_VERSION, read_frame, write_frame,
};
use dot_agent_deck::interrupted_work::{RECLAIM_WOULD_REMOVE, ReclaimVerdict};

const INSTANCE: &str = "daemon-process-a";

/// A socket in a fresh directory, short enough to bind.
fn scratch() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("dad-iw-")
        .tempdir()
        .expect("a scratch directory");
    let socket = dir.path().join("s");
    (dir, socket)
}

/// A one-connection daemon: answers the gate's `Hello` advertising
/// `capabilities` as process `instance`, then, when the client sends one,
/// the request after it with `answer`. Returns every request it was sent.
async fn daemon(
    listener: tokio::net::UnixListener,
    capabilities: Vec<&'static str>,
    instance: &'static str,
    answer: impl FnOnce(&AttachRequest) -> AttachResponse,
) -> Vec<AttachRequest> {
    let (stream, _) = listener.accept().await.expect("accept the client");
    let (mut rd, mut wr) = stream.into_split();
    let mut seen = Vec::new();
    let mut answer = Some(answer);
    while let Ok(Some((kind, payload))) = read_frame(&mut rd).await {
        assert_eq!(kind, KIND_REQ);
        let request: AttachRequest = serde_json::from_slice(&payload).expect("a request");
        let response = match &request {
            AttachRequest::Hello { .. } => {
                let mut hello = AttachResponse::hello(PROTOCOL_VERSION);
                hello.capabilities = Some(capabilities.iter().map(|c| c.to_string()).collect());
                hello.instance_id = Some(instance.to_string());
                hello
            }
            other => (answer.take().expect("one request after the Hello"))(other),
        };
        seen.push(request);
        let encoded = serde_json::to_vec(&response).expect("encode");
        write_frame(&mut wr, KIND_RESP, &encoded)
            .await
            .expect("answer");
    }
    seen
}

fn supported() -> Vec<&'static str> {
    vec![CAP_INTERRUPTED_WORK, CAP_HELLO_FOLLOW_ON]
}

fn sent_after_hello(seen: &[AttachRequest]) -> Vec<&AttachRequest> {
    seen.iter()
        .filter(|request| !matches!(request, AttachRequest::Hello { .. }))
        .collect()
}

/// An older daemon, which advertises no `interrupted-work`: every verb
/// answers `Unsupported`, and nothing but the gate's `Hello` reaches it.
#[tokio::test]
async fn an_old_daemon_is_unsupported_and_sent_nothing() {
    for verb in 0..4 {
        let (_dir, socket) = scratch();
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(daemon(
            listener,
            vec![CAP_HELLO_FOLLOW_ON],
            INSTANCE,
            |_| panic!("an unsupported verb was sent"),
        ));
        let client = DaemonClient::new(socket.clone());
        let outcome = match verb {
            0 => list(&client, INSTANCE, true)
                .await
                .map(|r| r == InterruptedWorkReply::Unsupported),
            1 => show(&client, INSTANCE, "w-1", None, Some(3))
                .await
                .map(|r| r == InterruptedWorkReply::Unsupported),
            2 => reclaim(&client, INSTANCE, "w-1", false)
                .await
                .map(|r| r == InterruptedWorkReply::Unsupported),
            _ => dismiss(&client, INSTANCE, vec!["w-1".into()])
                .await
                .map(|r| r == InterruptedWorkReply::Unsupported),
        };
        assert_eq!(outcome, Ok(true), "verb {verb}");
        drop(client);
        let seen = server.await.expect("the daemon finished");
        assert!(sent_after_hello(&seen).is_empty(), "verb {verb}: {seen:?}");
    }
}

/// A daemon that is another process than the one the webview named — a
/// restart in between — is sent no reclaim: an item's id belongs to one
/// daemon's state directory.
#[tokio::test]
async fn another_daemon_process_is_sent_no_reclaim() {
    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon(listener, supported(), "daemon-process-b", |_| {
        panic!("a reclaim reached another daemon process")
    }));
    let client = DaemonClient::new(socket.clone());
    assert_eq!(
        reclaim(&client, INSTANCE, "w-1", false).await,
        Ok(InterruptedWorkReply::Unsupported)
    );
    drop(client);
    assert!(sent_after_hello(&server.await.unwrap()).is_empty());
}

/// A supporting daemon is sent exactly the verb asked for: a dry run with
/// no pane claim, and its answer comes back tagged with the process.
#[tokio::test]
async fn a_supporting_daemon_answers_a_dry_run_sent_with_no_pane_claim() {
    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon(listener, supported(), INSTANCE, |_| {
        let mut response = AttachResponse::ok();
        response.interrupted_reclaim = Some(ReclaimOutcome {
            id: Some("w-1".into()),
            dry_run: true,
            verdict: ReclaimVerdict::WouldRemove,
            message: RECLAIM_WOULD_REMOVE.into(),
            refusal: None,
            worktree: Some("/w".into()),
            branch: Some("b".into()),
            unpushed_commits: Some(2),
            nested_worktrees: Vec::new(),
            recorded: false,
        });
        response
    }));
    let client = DaemonClient::new(socket.clone());
    let answer = reclaim(&client, INSTANCE, "w-1", true)
        .await
        .expect("answered");
    let InterruptedWorkReply::Answered { instance, answer } = answer else {
        panic!("expected an answer, got {answer:?}");
    };
    assert_eq!(instance, INSTANCE);
    assert_eq!(answer.verdict, ReclaimVerdict::WouldRemove);
    drop(client);
    let seen = server.await.unwrap();
    assert!(
        matches!(
            sent_after_hello(&seen)[..],
            [AttachRequest::ReclaimInterrupted { id, dry_run: true, caller: None }] if id == "w-1"
        ),
        "{seen:?}"
    );
}

/// Dismiss sends exactly the ids it was given, never `all`, and no pane
/// claim.
#[tokio::test]
async fn dismiss_sends_exactly_the_ids_never_all() {
    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon(listener, supported(), INSTANCE, |_| {
        let mut response = AttachResponse::ok();
        response.interrupted_dismiss = Some(DismissOutcome::default());
        response
    }));
    let client = DaemonClient::new(socket.clone());
    let ids = vec!["w-1".to_string(), "w-2".to_string()];
    assert!(matches!(
        dismiss(&client, INSTANCE, ids.clone()).await,
        Ok(InterruptedWorkReply::Answered { .. })
    ));
    drop(client);
    let seen = server.await.unwrap();
    assert!(
        matches!(
            sent_after_hello(&seen)[..],
            [AttachRequest::DismissInterrupted { ids: sent, all: false, caller: None }] if *sent == ids
        ),
        "{seen:?}"
    );
}

/// The webview reads `{ outcome: "unsupported" }` and `{ outcome:
/// "answered", instance, answer }`.
#[test]
fn the_reply_serialises_as_the_webview_reads_it() {
    let unsupported: InterruptedWorkReply<u8> = InterruptedWorkReply::Unsupported;
    assert_eq!(
        serde_json::to_value(unsupported).unwrap(),
        serde_json::json!({ "outcome": "unsupported" })
    );
    let answered = InterruptedWorkReply::Answered {
        instance: INSTANCE.to_string(),
        answer: 7u8,
    };
    assert_eq!(
        serde_json::to_value(answered).unwrap(),
        serde_json::json!({ "outcome": "answered", "instance": INSTANCE, "answer": 7 })
    );
}
