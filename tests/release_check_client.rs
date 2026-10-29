#![cfg(unix)]
//! SC-08: assert the shared client sends no unsupported release-check verbs.

use std::time::Duration;

use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};
use dot_agent_deck::daemon_protocol::CAP_RELEASE_CHECK;
use serde_json::{Value, json};
use spec::spec;
#[path = "fixtures/release-check/wire/scripted_daemon.rs"]
mod scripted_daemon;

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
    scripted_daemon::capture(advertised, refusal, |path| async move {
        invoke(&DaemonClient::new(path), verb).await
    })
    .await
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
            scripted_daemon::assert_no_verbs(&sent);
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
