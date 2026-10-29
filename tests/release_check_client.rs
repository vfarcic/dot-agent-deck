#![cfg(unix)]
//! SC-08: assert the shared client sends no unsupported release-check verbs.

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};
use dot_agent_deck::daemon_protocol::{
    AttachResponse, CAP_RELEASE_CHECK, KIND_REQ, PROTOCOL_VERSION, read_frame, write_resp,
};
use serde_json::{Value, json};
use spec::spec;
use tokio::net::UnixListener;

#[path = "../src/test_temp.rs"]
mod test_temp;

#[derive(Clone, Copy, Debug)]
enum Verb {
    Run,
    Cancel,
    Forget,
}

impl Verb {
    fn request(self) -> Value {
        match self {
            Self::Run => json!({"op":"release-check-run","agent":"claude"}),
            Self::Cancel => json!({"op":"release-check-cancel"}),
            Self::Forget => {
                json!({"op":"release-check-forget","agent":"claude","release":"2.1.313"})
            }
        }
    }
}

async fn invoke(client: &DaemonClient, verb: Verb) -> Result<GatedQuery<()>, String> {
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        match verb {
            Verb::Run => client.release_check_run("claude").await,
            Verb::Cancel => client.release_check_cancel().await,
            Verb::Forget => client.release_check_forget("claude", "2.1.313").await,
        }
    })
    .await
    .expect("shared client request must have a bounded completion");
    result.map_err(|error| error.to_string())
}

async fn case(
    advertised: Option<Vec<String>>,
    refusal: bool,
    verb: Verb,
) -> (Result<GatedQuery<()>, String>, Vec<Value>) {
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
    let result = invoke(&DaemonClient::new(path), verb).await;
    server.abort();
    let _ = server.await;
    let sent = requests.lock().unwrap().clone();
    (result, sent)
}

/// Scenario: Request run, cancel and forget from isolated scripted daemons whose Hello omits, empties or advertises unrelated capabilities. Each call is unsupported and sends no verb, whereas a daemon advertising release_check receives each exact request once and its refusal is surfaced.
#[spec("inbox/release-check/002")]
#[tokio::test]
async fn release_check_002_every_shared_client_verb_is_capability_gated() {
    for verb in [Verb::Run, Verb::Cancel, Verb::Forget] {
        for advertised in [
            None,
            Some(vec![]),
            Some(vec!["focus-gained".into(), "release_check_future".into()]),
        ] {
            let (result, sent) = case(advertised.clone(), false, verb).await;
            assert_eq!(
                result,
                Ok(GatedQuery::Unsupported),
                "{verb:?}, {advertised:?}"
            );
            assert!(sent.is_empty(), "unsupported {verb:?} sent {sent:?}");
        }
        let (result, sent) = case(Some(vec![CAP_RELEASE_CHECK.into()]), false, verb).await;
        assert_eq!(result, Ok(GatedQuery::Answered(())), "{verb:?}");
        assert_eq!(sent, [verb.request()], "positive control for {verb:?}");
        let (result, sent) = case(Some(vec![CAP_RELEASE_CHECK.into()]), true, verb).await;
        assert!(
            result.is_err_and(|error| error.contains("release-check test refusal")),
            "refused {verb:?} must not look successful"
        );
        assert_eq!(sent, [verb.request()]);
    }
}
