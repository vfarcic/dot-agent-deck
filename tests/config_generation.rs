//! Shared-client admission for daemon-composed configuration requests.

use dot_agent_deck::daemon_client::{DaemonClient, GatedQuery};
use dot_agent_deck::daemon_protocol::{AttachResponse, CAP_INBOX};
use spec::spec;

/// Scenario: A daemon's Hello advertises generic inbox delivery but no config-generation capability, or carries no capability list. Asking the shared client to generate config returns Unsupported without opening another connection or sending any request.
#[spec("daemon/config-gen/004")]
#[tokio::test]
async fn config_gen_004_client_withholds_generation_from_older_daemon() {
    for capabilities in [None, Some(vec![CAP_INBOX.to_string()]), Some(vec![])] {
        let directory = tempfile::tempdir().unwrap();
        // There is deliberately no listener: a network attempt would fail,
        // rather than returning the unsupported outcome used by both clients.
        let client =
            DaemonClient::new(directory.path().join("no-daemon.sock")).with_protocol_gate();
        client.store_capabilities_from_hello(&AttachResponse {
            capabilities,
            ..AttachResponse::ok()
        });
        let result = client
            .generate_config("selected-pane", Some("selected-agent"))
            .await;
        assert!(
            matches!(result, Ok(GatedQuery::Unsupported)),
            "an older daemon must return Unsupported without I/O: {result:?}"
        );
    }
}
