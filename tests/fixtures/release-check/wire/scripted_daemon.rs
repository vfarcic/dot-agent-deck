//! Test-owned wire capture, shared with controls that need no SC-08 production API.
#![cfg(unix)]

use std::future::Future;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dot_agent_deck::daemon_protocol::{
    AttachResponse, KIND_REQ, PROTOCOL_VERSION, read_frame, write_resp,
};
use serde_json::Value;
use tokio::net::{UnixListener, UnixStream};

#[path = "../../../../src/test_temp.rs"]
mod test_temp;

pub fn assert_no_verbs(sent: &[Value]) {
    assert!(sent.is_empty(), "unsupported call sent {sent:?}");
}

pub async fn capture<T, F: Future<Output = T>>(
    advertised: Option<Vec<String>>,
    refusal: bool,
    invoke: impl FnOnce(PathBuf) -> F,
) -> (T, Vec<Value>) {
    let dir = test_temp::tempdir().unwrap();
    let path = dir.path().join("scripted.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let (finished, mut stop) = tokio::sync::oneshot::channel();
    let mut server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        let spawn_reader = |mut stream: UnixStream, connections: &mut tokio::task::JoinSet<()>| {
            let captured = captured.clone();
            let advertised = advertised.clone();
            connections.spawn(async move {
                while let Some((kind, payload)) = read_frame(&mut stream).await.unwrap() {
                    assert_eq!(kind, KIND_REQ);
                    let request: Value = serde_json::from_slice(&payload).unwrap();
                    let reply = if request["op"] == "hello" {
                        AttachResponse {
                            capabilities: advertised.clone(),
                            ..AttachResponse::hello(PROTOCOL_VERSION)
                        }
                    } else {
                        captured.lock().unwrap().push(request);
                        if refusal {
                            AttachResponse::err("release-check test refusal")
                        } else {
                            AttachResponse::ok()
                        }
                    };
                    if let Err(error) = write_resp(&mut stream, &reply).await {
                        // A write-then-Unsupported caller may close before our reply.
                        // Keep draining its input through EOF even after BrokenPipe.
                        assert_eq!(error.kind(), ErrorKind::BrokenPipe, "reply: {error}");
                    }
                }
            });
        };
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    spawn_reader(accepted.unwrap().0, &mut connections);
                }
                _ = &mut stop => break,
            }
        }
        // Invocation and all client-owned sockets have ended. A stop notification
        // may win select while established peers remain in the accept backlog.
        // A nonblocking OS accept drains it without depending on Tokio readiness.
        let listener = listener.into_std().unwrap();
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(true).unwrap();
                    spawn_reader(UnixStream::from_std(stream).unwrap(), &mut connections);
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => panic!("drain accept backlog: {error}"),
            }
        }
        drop(listener);
        while let Some(result) = connections.join_next().await {
            result.expect("wire capture reader must reach clean EOF without panicking");
        }
    });
    let result = invoke(path).await;
    let _ = finished.send(());
    match tokio::time::timeout(Duration::from_secs(3), &mut server).await {
        Ok(result) => result.expect("wire capture server must drain every reader"),
        Err(_) => {
            server.abort();
            let _ = server.await;
            panic!("wire capture did not reach EOF on every connection within 3 seconds");
        }
    }
    let sent = requests.lock().unwrap().clone();
    (result, sent)
}
