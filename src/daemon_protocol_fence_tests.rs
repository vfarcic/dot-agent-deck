//! PRD #1666 part 3 (Q35-15): the attach loop's forced short and failed
//! writes with no fence, through the private writer seam. These are
//! `inbox/probe/015`'s rows its lane-1 harness can't force (Q88), kept here
//! as `inbox/probe/011`. With them, the attach loop's side of a closed fence
//! (the Q35-15 fix): a connection that ends while its frame waits for room,
//! and a held frame's human-input clock.

use super::*;
use crate::agent_pty::{Accepted, UserFrame};
use crate::composer_probe::gate::FENCE_CAP;
use crate::ingress_ledger::Settlement;
use spec::spec;

/// A PTY writer that takes `budget` bytes in all, then fails with `kind`.
struct Failing {
    taken: Arc<std::sync::Mutex<Vec<u8>>>,
    budget: usize,
    kind: io::ErrorKind,
}

impl std::io::Write for Failing {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.budget == 0 {
            return Err(io::Error::from(self.kind));
        }
        let n = self.budget.min(buf.len());
        self.budget -= n;
        self.taken.lock().unwrap().extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Attach to a `cat` pane whose PTY writer takes `budget` bytes and then
/// fails with `kind`, send `frames`, and return what the writer took, how
/// each frame's ticket settled, and whether the stream ended.
async fn attach_and_write(
    budget: usize,
    kind: io::ErrorKind,
    frames: &[&[u8]],
    expect_end: bool,
) -> (Vec<u8>, Vec<Settlement>, bool) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("attach.sock");
    let registry = Arc::new(AgentPtyRegistry::new());
    let id = registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(dir.path().to_str().unwrap()),
            env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                DOT_AGENT_DECK_PANE_ID.to_string(),
                "probe-011-forced".to_string(),
            )]),
            ..SpawnOptions::default()
        })
        .expect("spawn the stand-in");
    let taken = Arc::new(std::sync::Mutex::new(Vec::new()));
    let _displaced = registry
        .replace_agent_writer_for_test(
            &id,
            Box::new(Failing {
                taken: Arc::clone(&taken),
                budget,
                kind,
            }),
        )
        .await;
    let ledger = registry
        .ingress_ledger(&id)
        .expect("the generation's ledger");
    let listener = bind_attach_listener(&sock).expect("bind the attach socket");
    let server = tokio::spawn(serve_attach(
        listener,
        Arc::clone(&registry),
        broadcast::channel(16).0,
    ));
    let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
    let request = serde_json::json!({"op": "attach-stream", "id": id});
    write_frame(
        &mut stream,
        KIND_REQ,
        &serde_json::to_vec(&request).unwrap(),
    )
    .await
    .unwrap();
    let (kind, body) = read_frame(&mut stream).await.unwrap().unwrap();
    assert_eq!(kind, KIND_RESP);
    let response: AttachResponse = serde_json::from_slice(&body).unwrap();
    assert!(response.ok, "{response:?}");
    for frame in frames {
        if write_frame(&mut stream, KIND_STREAM_IN, frame)
            .await
            .is_err()
        {
            break;
        }
    }
    let ended = expect_end
        && tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                match read_frame(&mut stream).await {
                    Ok(Some((KIND_STREAM_END, _))) | Ok(None) | Err(_) => return,
                    Ok(Some(_)) => {}
                }
            }
        })
        .await
        .is_ok();
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while ledger.any_outstanding() || (!expect_end && ledger.settled().len() < frames.len()) {
        assert!(
            std::time::Instant::now() < deadline,
            "a ticket never settled"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let settled = ledger.settled().into_iter().map(|(_, how)| how).collect();
    server.abort();
    registry.shutdown_all();
    let taken = taken.lock().unwrap().clone();
    (taken, settled, ended)
}

/// Scenario: Attach with no fence to a pane whose PTY takes three bytes of a six-byte frame and then fails, and to one whose PTY fails at once, each followed by a second frame. As before the gate existed, the attach loop ends the connection at the failed write: the first pane's PTY holds the prefix and its frame settles partial, the second's holds nothing and its frame settles failed, and neither second frame is written or admitted.
#[cfg(unix)]
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_attach_ends_at_a_short_or_failed_write_as_before() {
    let (taken, settled, ended) =
        attach_and_write(3, io::ErrorKind::BrokenPipe, &[b"abcdef", b"after"], true).await;
    assert_eq!(taken, b"abc");
    assert_eq!(settled, [Settlement::Partial { accepted: 3 }]);
    assert!(ended, "the connection ends at the failed write");

    let (taken, settled, ended) =
        attach_and_write(0, io::ErrorKind::Other, &[b"xyz", b"after"], true).await;
    assert_eq!(taken, b"");
    assert_eq!(settled, [Settlement::Failed(io::ErrorKind::Other)]);
    assert!(ended);

    let (taken, settled, _) = attach_and_write(
        usize::MAX,
        io::ErrorKind::Other,
        &[b"one", b"", b"two"],
        false,
    )
    .await;
    assert_eq!(taken, b"onetwo", "the control: nothing fails, all in order");
    assert_eq!(
        settled,
        [
            Settlement::Written { bytes: 3 },
            Settlement::Written { bytes: 0 },
            Settlement::Written { bytes: 3 },
        ]
    );
}

/// A `cat` pane on a live attach server whose PTY writer only records what
/// it takes, for the fenced attach tests.
struct FencedPane {
    _dir: tempfile::TempDir,
    sock: std::path::PathBuf,
    registry: Arc<AgentPtyRegistry>,
    id: String,
    pane: &'static str,
    taken: Arc<std::sync::Mutex<Vec<u8>>>,
    ledger: Arc<crate::ingress_ledger::IngressLedger>,
    server: tokio::task::JoinHandle<io::Result<()>>,
    /// The PTY's own writer, kept: dropping it would end `cat`'s input.
    _displaced: Box<dyn std::io::Write + Send>,
}

impl FencedPane {
    async fn new(pane: &'static str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("attach.sock");
        let registry = Arc::new(AgentPtyRegistry::new());
        let id = registry
            .spawn_agent(SpawnOptions {
                command: Some("cat"),
                cwd: Some(dir.path().to_str().unwrap()),
                env: crate::test_isolation::pin_unreachable_endpoints(vec![(
                    DOT_AGENT_DECK_PANE_ID.to_string(),
                    pane.to_string(),
                )]),
                ..SpawnOptions::default()
            })
            .expect("spawn the stand-in");
        let taken = Arc::new(std::sync::Mutex::new(Vec::new()));
        let displaced = registry
            .replace_agent_writer_for_test(
                &id,
                Box::new(Failing {
                    taken: Arc::clone(&taken),
                    budget: usize::MAX,
                    kind: io::ErrorKind::Other,
                }),
            )
            .await;
        let ledger = registry
            .ingress_ledger(&id)
            .expect("the generation's ledger");
        let listener = bind_attach_listener(&sock).expect("bind the attach socket");
        let server = tokio::spawn(serve_attach(
            listener,
            Arc::clone(&registry),
            broadcast::channel(16).0,
        ));
        Self {
            _dir: dir,
            sock,
            registry,
            id,
            pane,
            taken,
            ledger,
            server,
            _displaced: displaced,
        }
    }

    /// An attached connection that declared a viewport, so it registers a
    /// viewer.
    async fn attach(&self) -> tokio::net::UnixStream {
        let mut stream = tokio::net::UnixStream::connect(&self.sock).await.unwrap();
        let request =
            serde_json::json!({"op": "attach-stream", "id": self.id, "rows": 24, "cols": 80});
        write_frame(
            &mut stream,
            KIND_REQ,
            &serde_json::to_vec(&request).unwrap(),
        )
        .await
        .unwrap();
        let (kind, body) = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(kind, KIND_RESP);
        let response: AttachResponse = serde_json::from_slice(&body).unwrap();
        assert!(response.ok, "{response:?}");
        stream
    }

    fn gate(&self) -> Arc<crate::agent_pty::PaneGate> {
        self.registry.composer_fence(&self.id).expect("the gate").0
    }

    fn writer(&self) -> Arc<tokio::sync::Mutex<crate::agent_pty::PaneWriter>> {
        self.registry
            .composer_fence(&self.id)
            .expect("the writer")
            .1
    }

    fn taken(&self) -> Vec<u8> {
        self.taken.lock().unwrap().clone()
    }

    async fn until(&self, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "never: {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn stop(self) {
        self.server.abort();
        self.registry.shutdown_all();
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Scenario: Close a pane's fence and fill it, then from an attached connection send a frame, which waits for room, and one more behind it; close that connection without releasing anything. Its waiting frame gives up its place: the fence's queue of waiters is empty again, its ticket settles cancelled once, and its viewer is released; once the fence is released, the PTY takes what the fence held and neither of that connection's frames (R-U2-01, core §8.2).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_connection_that_ends_while_full_gives_up_its_place() {
    let pane = FencedPane::new("probe-011-full-disconnect").await;
    assert!(pane.gate().fence());
    let filler = pane.ledger.admit();
    let accepted = pane
        .writer()
        .lock()
        .await
        .accept(
            UserFrame::new(vec![b'h'; FENCE_CAP], Some(filler), None),
            None,
        )
        .await;
    assert!(matches!(accepted, Accepted::Held));
    let mut client = pane.attach().await;
    assert_eq!(pane.registry.viewers_of(&pane.id).len(), 1);
    write_frame(&mut client, KIND_STREAM_IN, b"parked")
        .await
        .unwrap();
    pane.until("the frame waits for room", || {
        pane.gate().status().full_waiters == 1
    })
    .await;
    write_frame(&mut client, KIND_STREAM_IN, b"behind")
        .await
        .unwrap();
    drop(client);
    pane.until("the waiter leaves", || {
        pane.gate().status().full_waiters == 0
    })
    .await;
    pane.until("the viewer is released", || {
        pane.registry.viewers_of(&pane.id).is_empty()
    })
    .await;
    let cancelled = || {
        pane.ledger
            .settled()
            .into_iter()
            .filter(|(_, how)| *how == Settlement::Cancelled)
            .count()
    };
    pane.until("the waiting frame settles", || cancelled() == 1)
        .await;
    assert_eq!(pane.gate().status().held_ops, 1, "nothing released yet");
    assert_eq!(pane.writer().lock().await.release_fence(Ok(())), 1);
    pane.until("the fence's frame is written", || {
        pane.taken().len() >= FENCE_CAP
    })
    .await;
    // Anything still parked would retry at the release and be written now.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let taken = pane.taken();
    assert_eq!(taken.len(), FENCE_CAP);
    assert!(!contains(&taken, b"parked") && !contains(&taken, b"behind"));
    assert_eq!(cancelled(), 1, "settled once");
    pane.until("the fence's frame settles", || {
        !pane.ledger.any_outstanding()
    })
    .await;
    pane.stop();
}

/// Scenario: Close a pane's fence and fill it, then from an attached connection send a frame, which waits for room, and one more behind it; shut the connection for writing while still reading from it. A shut write half is the client leaving: the waiting frame gives up its place and settles cancelled once, the connection's stream ends for its reader, and after the release neither frame is written (Q3515FIX-A-U3-02).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_connection_shut_for_writing_while_full_gives_up_its_place() {
    use tokio::io::AsyncWriteExt;
    let pane = FencedPane::new("probe-011-full-half-close").await;
    assert!(pane.gate().fence());
    let filler = pane.ledger.admit();
    let accepted = pane
        .writer()
        .lock()
        .await
        .accept(
            UserFrame::new(vec![b'h'; FENCE_CAP], Some(filler), None),
            None,
        )
        .await;
    assert!(matches!(accepted, Accepted::Held));
    let mut client = pane.attach().await;
    write_frame(&mut client, KIND_STREAM_IN, b"parked")
        .await
        .unwrap();
    pane.until("the frame waits for room", || {
        pane.gate().status().full_waiters == 1
    })
    .await;
    write_frame(&mut client, KIND_STREAM_IN, b"behind")
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    pane.until("the waiter leaves", || {
        pane.gate().status().full_waiters == 0
    })
    .await;
    let cancelled = || {
        pane.ledger
            .settled()
            .into_iter()
            .filter(|(_, how)| *how == Settlement::Cancelled)
            .count()
    };
    pane.until("the waiting frame settles", || cancelled() == 1)
        .await;
    // Still reading: the daemon ends the stream rather than leaving it open.
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match read_frame(&mut client).await {
                Ok(Some((KIND_STREAM_END, _))) | Ok(None) | Err(_) => break,
                Ok(Some(_)) => {}
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the reader sees the stream end");
    assert_eq!(pane.writer().lock().await.release_fence(Ok(())), 1);
    pane.until("the fence's frame is written", || {
        pane.taken().len() >= FENCE_CAP
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let taken = pane.taken();
    assert_eq!(taken.len(), FENCE_CAP);
    assert!(!contains(&taken, b"parked") && !contains(&taken, b"behind"));
    assert_eq!(cancelled(), 1, "settled once");
    pane.stop();
}

/// Scenario: Close a pane's fence, note the human-input counter, and from an attached connection send an empty frame; note it again and send a key. Each frame, though nothing reaches the PTY until the fence is released, counts as the person's input as soon as it is accepted: the counter has moved past each mark while the key is still held, so an answer typed for a question asked before either is refused (R-U3-01).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_held_frame_is_human_input_at_acceptance() {
    let pane = FencedPane::new("probe-011-held-clock").await;
    assert!(pane.gate().fence());
    let mut client = pane.attach().await;
    let typed_since = |mark: u64| {
        pane.registry
            .user_typed_since_question(pane.pane, "q", None, Some(mark))
    };
    let mark = pane.registry.human_input_counter();
    write_frame(&mut client, KIND_STREAM_IN, b"").await.unwrap();
    pane.until("the empty frame counts", || typed_since(mark))
        .await;
    assert_eq!(pane.gate().status().held_ops, 0, "an empty frame settles");
    let mark = pane.registry.human_input_counter();
    write_frame(&mut client, KIND_STREAM_IN, b"k")
        .await
        .unwrap();
    pane.until("the held key counts", || typed_since(mark))
        .await;
    assert_eq!(pane.gate().status().held_ops, 1, "the key is still held");
    assert_eq!(pane.taken(), b"", "nothing reached the PTY");
    assert_eq!(pane.writer().lock().await.release_fence(Ok(())), 1);
    pane.until("the key is written", || pane.taken() == b"k")
        .await;
    drop(client);
    pane.stop();
}

/// Scenario: Close a pane's fence and fill it, then from an attached connection send a frame, which waits for room, and one more behind it, and release the fence. The connection stays: the waiting frame and the one queued behind it in the socket both reach the PTY after the fence's own bytes, in order, so watching for the connection's end while the frame waited left nothing unread (R-U2-01).
#[spec("inbox/probe/011")]
#[tokio::test(flavor = "multi_thread")]
async fn probe_011_a_frame_behind_a_full_one_is_read_after_the_release() {
    let pane = FencedPane::new("probe-011-full-then-read").await;
    assert!(pane.gate().fence());
    let filler = pane.ledger.admit();
    let accepted = pane
        .writer()
        .lock()
        .await
        .accept(
            UserFrame::new(vec![b'h'; FENCE_CAP], Some(filler), None),
            None,
        )
        .await;
    assert!(matches!(accepted, Accepted::Held));
    let mut client = pane.attach().await;
    write_frame(&mut client, KIND_STREAM_IN, b"waiting|")
        .await
        .unwrap();
    pane.until("the frame waits for room", || {
        pane.gate().status().full_waiters == 1
    })
    .await;
    write_frame(&mut client, KIND_STREAM_IN, b"after|")
        .await
        .unwrap();
    // Let the queued frame's readiness reach the parked wait first.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(pane.writer().lock().await.release_fence(Ok(())), 1);
    let mut expected = vec![b'h'; FENCE_CAP];
    expected.extend_from_slice(b"waiting|after|");
    pane.until("both frames are written after the fence's", || {
        pane.taken() == expected
    })
    .await;
    drop(client);
    pane.stop();
}
