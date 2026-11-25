//! PRD #1790 stage 2: the desktop's interrupted work commands against a
//! scripted daemon — an older one that advertises no `interrupted-work`,
//! another daemon process than the one named, and a supporting one.

use super::*;
use dot_agent_deck::daemon_protocol::{
    AttachRequest, AttachResponse, CAP_HELLO_FOLLOW_ON, CAP_INTERRUPTED_WORK, CAP_RESTORE_AGENTS,
    KIND_REQ, KIND_RESP, PROTOCOL_VERSION, read_frame, write_frame,
};
use dot_agent_deck::interrupted_work::{
    DismissStopReason, DismissStopped, RECLAIM_WOULD_REMOVE, ReclaimVerdict, RefusalReason,
    RestoreRefusal, RestoreRefused, RestoredAgent,
};

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

// ---------------------------------------------------------------------------
// Dismiss all in batches (final review, reviewer 1 / auditor 2)
// ---------------------------------------------------------------------------

/// A daemon serving one connection per request, as the shared client opens
/// them: connection `n` answers the gate's `Hello` as process
/// `processes[n]` (the last one from then on), and the request after it with
/// `answer(n, request)`, or closes the connection unanswered on `None`.
/// Stops once no client connects for a while. Returns every request sent
/// after a `Hello`.
async fn daemon_per_request(
    listener: tokio::net::UnixListener,
    processes: Vec<&'static str>,
    mut answer: impl FnMut(usize, &AttachRequest) -> Option<AttachResponse>,
) -> Vec<AttachRequest> {
    let mut seen = Vec::new();
    let mut n = 0usize;
    while let Ok(Ok((stream, _))) =
        tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept()).await
    {
        let process = processes[n.min(processes.len() - 1)];
        let (mut rd, mut wr) = stream.into_split();
        while let Ok(Some((kind, payload))) = read_frame(&mut rd).await {
            assert_eq!(kind, KIND_REQ);
            let request: AttachRequest = serde_json::from_slice(&payload).expect("a request");
            let response = match &request {
                AttachRequest::Hello { .. } => {
                    let mut hello = AttachResponse::hello(PROTOCOL_VERSION);
                    hello.capabilities = Some(supported().iter().map(|c| c.to_string()).collect());
                    hello.instance_id = Some(process.to_string());
                    Some(hello)
                }
                other => {
                    let response = answer(n, other);
                    seen.push(request.clone());
                    response
                }
            };
            let Some(response) = response else { break };
            let encoded = serde_json::to_vec(&response).expect("encode");
            write_frame(&mut wr, KIND_RESP, &encoded)
                .await
                .expect("answer");
        }
        n += 1;
    }
    seen
}

fn many_ids(count: usize) -> Vec<String> {
    (0..count)
        .map(|n| format!("w-abcdefgh-{:016}", 1000 + n))
        .collect()
}

/// A daemon's answer dismissing every id the request named.
fn dismissing(request: &AttachRequest) -> AttachResponse {
    let AttachRequest::DismissInterrupted { ids, .. } = request else {
        panic!("only dismissals expected, got {request:?}");
    };
    let mut response = AttachResponse::ok();
    let mut outcome = DismissOutcome::default();
    for id in ids {
        outcome.push_dismissed(dot_agent_deck::interrupted_work::DismissedItem {
            id: id.clone(),
            name: None,
        });
    }
    response.interrupted_dismiss = Some(outcome);
    response
}

fn dismissed_ids(seen: &[AttachRequest]) -> Vec<(Vec<String>, bool, bool)> {
    seen.iter()
        .filter_map(|request| match request {
            AttachRequest::DismissInterrupted { ids, all, caller } => {
                Some((ids.clone(), *all, caller.is_some()))
            }
            _ => None,
        })
        .collect()
}

/// 257 confirmed ids, one more than the daemon admits in one request, go in
/// two bounded batches pinned to the one process, together naming exactly
/// those ids, never `all` and with no pane claim; the answer adds them up.
#[tokio::test]
async fn dismiss_sends_more_ids_than_one_request_admits_in_bounded_batches() {
    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon_per_request(
        listener,
        vec![INSTANCE],
        |_, request| Some(dismissing(request)),
    ));
    let client = DaemonClient::new(socket.clone());
    let ids = many_ids(257);
    let reply = dismiss(&client, INSTANCE, ids.clone()).await;
    drop(client);
    let seen = server.await.unwrap();
    let batches = dismissed_ids(&seen);
    assert_eq!(
        batches
            .iter()
            .map(|(ids, ..)| ids.len())
            .collect::<Vec<_>>(),
        vec![256, 1],
        "{batches:?}"
    );
    assert!(batches.iter().all(|(_, all, claim)| !all && !claim));
    let named: Vec<String> = batches.into_iter().flat_map(|(ids, ..)| ids).collect();
    assert_eq!(named, ids);
    let Ok(InterruptedWorkReply::Answered { instance, answer }) = reply else {
        panic!("expected an answer, got {reply:?}");
    };
    assert_eq!(instance, INSTANCE);
    assert_eq!(answer.dismissed_count(), 257, "{answer:?}");
    assert_eq!(answer.stopped, None);
}

/// A later batch refused whole, or one that gets no answer, stops the rest:
/// no further batch is sent, and the answer carries what the earlier batches
/// dismissed and why the rest were not.
#[tokio::test]
async fn a_later_batch_that_fails_stops_the_rest_and_says_why() {
    for refused in [true, false] {
        let (_dir, socket) = scratch();
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(daemon_per_request(
            listener,
            vec![INSTANCE],
            move |n, request| match n {
                0 => Some(dismissing(request)),
                _ if refused => {
                    let mut response = AttachResponse::ok();
                    response.interrupted_dismiss = Some(DismissOutcome {
                        refusal: Some(dot_agent_deck::interrupted_work::Refusal::new(
                            RefusalReason::Unreadable,
                        )),
                        ..DismissOutcome::default()
                    });
                    Some(response)
                }
                _ => None,
            },
        ));
        let client = DaemonClient::new(socket.clone());
        let reply = dismiss(&client, INSTANCE, many_ids(513)).await;
        drop(client);
        let seen = server.await.unwrap();
        assert_eq!(dismissed_ids(&seen).len(), 2, "refused={refused}: {seen:?}");
        let Ok(InterruptedWorkReply::Answered { answer, .. }) = reply else {
            panic!("refused={refused}: expected an answer, got {reply:?}");
        };
        assert_eq!(answer.dismissed_count(), 256);
        assert_eq!(
            answer.stopped,
            Some(DismissStopped {
                reason: if refused {
                    DismissStopReason::Refused
                } else {
                    DismissStopReason::Failed
                },
                refusal: refused.then_some(RefusalReason::Unreadable),
                not_sent: 257,
            }),
            "refused={refused}"
        );
    }
}

/// The daemon process is replaced between batches: the next batch is
/// withheld from the new process, and the answer says the rest were not
/// dismissed because the daemon was replaced.
#[tokio::test]
async fn a_daemon_replaced_between_batches_is_sent_no_further_batch() {
    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon_per_request(
        listener,
        vec![INSTANCE, "daemon-process-b"],
        |n, request| {
            assert_eq!(n, 0, "a batch reached the replacement: {request:?}");
            Some(dismissing(request))
        },
    ));
    let client = DaemonClient::new(socket.clone());
    let reply = dismiss(&client, INSTANCE, many_ids(257)).await;
    drop(client);
    let seen = server.await.unwrap();
    assert_eq!(dismissed_ids(&seen).len(), 1, "{seen:?}");
    let Ok(InterruptedWorkReply::Answered { answer, .. }) = reply else {
        panic!("expected an answer, got {reply:?}");
    };
    assert_eq!(answer.dismissed_count(), 256);
    assert_eq!(
        answer.stopped,
        Some(DismissStopped {
            reason: DismissStopReason::Replaced,
            refusal: None,
            not_sent: 1,
        })
    );
}

// ---------------------------------------------------------------------------
// PRD #1790 stage 3: restore and start over.
// ---------------------------------------------------------------------------

fn restoring() -> Vec<&'static str> {
    vec![
        CAP_INTERRUPTED_WORK,
        CAP_HELLO_FOLLOW_ON,
        CAP_RESTORE_AGENTS,
    ]
}

/// One agent a restore started, as the daemon sends it.
fn restored_agent() -> RestoredAgent {
    serde_json::from_value(serde_json::json!({
        "attempt": "run/7",
        "agent_id": "41",
        "pane_id": "pane-41",
        "name": "fix-login",
        "first_message_posted": true,
        "recorded": true,
    }))
    .expect("a restored agent")
}

/// A daemon that serves interrupted work but advertises no `restore-agents`
/// — one from before stage 3 — is sent neither a restore nor a start-over:
/// both answer `unsupported`, and nothing but the gate's `Hello` reaches it.
#[tokio::test]
async fn a_daemon_without_restore_agents_is_sent_no_restore() {
    for verb in 0..2 {
        let (_dir, socket) = scratch();
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(daemon(listener, supported(), INSTANCE, |_| {
            panic!("a restore reached a daemon that does not restore agents")
        }));
        let client = DaemonClient::new(socket.clone());
        let unsupported = match verb {
            0 => restore(
                &client,
                INSTANCE,
                "w-1",
                vec!["run/7".into()],
                RestoreMode::Only,
                false,
            )
            .await
            .map(|r| r == InterruptedWorkReply::Unsupported),
            _ => start_over(&client, INSTANCE, "w-1", "run/7", false)
                .await
                .map(|r| r == InterruptedWorkReply::Unsupported),
        };
        assert_eq!(unsupported, Ok(true), "verb {verb}");
        drop(client);
        assert!(
            sent_after_hello(&server.await.unwrap()).is_empty(),
            "verb {verb}"
        );
    }
}

/// A restore names the item, the attempt and the mode the person chose,
/// with no pane claim, to the daemon process the webview named; its final
/// answer comes back tagged with that process.
#[tokio::test]
async fn a_restore_is_sent_as_chosen_with_no_pane_claim() {
    for mode in [RestoreMode::Continue, RestoreMode::Only] {
        let (_dir, socket) = scratch();
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
        let server = tokio::spawn(daemon(listener, restoring(), INSTANCE, |_| {
            let mut response = AttachResponse::ok();
            response.interrupted_restore = Some(RestoreOutcome {
                restored: vec![restored_agent()],
                ..RestoreOutcome::default()
            });
            response
        }));
        let client = DaemonClient::new(socket.clone());
        let answer = restore(&client, INSTANCE, "w-1", vec!["run/7".into()], mode, false)
            .await
            .expect("answered");
        let InterruptedWorkReply::Answered { instance, answer } = answer else {
            panic!("expected an answer, got {answer:?}");
        };
        assert_eq!(instance, INSTANCE);
        assert_eq!(answer.restored, vec![restored_agent()]);
        drop(client);
        let seen = server.await.unwrap();
        assert!(
            matches!(
                sent_after_hello(&seen)[..],
                [AttachRequest::RestoreInterrupted { id, agents, mode: sent, dry_run: false, caller: None }]
                    if id == "w-1" && agents.len() == 1 && agents[0] == "run/7" && *sent == mode
            ),
            "{seen:?}"
        );
    }
}

/// A start-over names the one attempt, with no pane claim, and is withheld
/// from another daemon process than the one named; a refusal comes back as
/// the daemon worded it.
#[tokio::test]
async fn a_start_over_is_sent_to_the_named_process_only() {
    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon(listener, restoring(), "daemon-process-b", |_| {
        panic!("a start-over reached another daemon process")
    }));
    let client = DaemonClient::new(socket.clone());
    assert_eq!(
        start_over(&client, INSTANCE, "w-1", "run/7", false).await,
        Ok(InterruptedWorkReply::Unsupported)
    );
    drop(client);
    assert!(sent_after_hello(&server.await.unwrap()).is_empty());

    let (_dir, socket) = scratch();
    let listener = tokio::net::UnixListener::bind(&socket).expect("bind");
    let server = tokio::spawn(daemon(listener, restoring(), INSTANCE, |_| {
        let mut response = AttachResponse::ok();
        response.interrupted_start_over = Some(StartOverOutcome {
            refused: Some(RestoreRefused::new(
                Some(restored_agent().attempt),
                RestoreRefusal::StillRunning,
            )),
            ..StartOverOutcome::default()
        });
        response
    }));
    let client = DaemonClient::new(socket.clone());
    let answer = start_over(&client, INSTANCE, "w-1", "run/7", false)
        .await
        .expect("answered");
    let InterruptedWorkReply::Answered { answer, .. } = answer else {
        panic!("expected an answer, got {answer:?}");
    };
    assert_eq!(
        answer.refused.map(|r| r.reason),
        Some(RestoreRefusal::StillRunning)
    );
    drop(client);
    let seen = server.await.unwrap();
    assert!(
        matches!(
            sent_after_hello(&seen)[..],
            [AttachRequest::StartOverInterrupted { id, agent, dry_run: false, caller: None }]
                if id == "w-1" && agent == "run/7"
        ),
        "{seen:?}"
    );
}
