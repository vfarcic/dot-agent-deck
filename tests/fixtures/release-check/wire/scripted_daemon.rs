//! Test-owned wire capture, shared with controls that need no SC-08 production API.
#![cfg(unix)]

use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dot_agent_deck::daemon_protocol::{
    AttachResponse, KIND_REQ, PROTOCOL_VERSION, read_frame, write_resp,
};
use serde_json::Value;
use tokio::net::UnixListener;

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
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
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
                    write_resp(&mut stream, &reply).await.unwrap();
                }
            });
        }
    });
    let result = invoke(path).await;
    server.abort();
    let _ = server.await;
    let sent = requests.lock().unwrap().clone();
    (result, sent)
}
