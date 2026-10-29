//! PRD #1666 part 3 (Q35-15): the attach loop's forced short and failed
//! writes with no fence, through the private writer seam. These are
//! `inbox/probe/015`'s rows its lane-1 harness can't force (Q88), kept here
//! as `inbox/probe/011`.

use super::*;
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
