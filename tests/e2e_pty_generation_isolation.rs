#![cfg(all(feature = "e2e", target_os = "linux"))]

mod common;
#[path = "support/pty_frontier.rs"]
mod fixture;

use dot_agent_deck::agent_pty::{
    AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, PaneRecreateIdentity, SpawnOptions,
};
use fixture::{Capture, Peer, Registry, SENTINEL, contains, listener, quote, script};
use spec::spec;
use std::sync::Arc;

async fn isolation(recreate: bool) {
    common::init_test_env();
    let root = common::race_safe_tempdir();
    let before = listener(root.path(), "before");
    let writer = listener(root.path(), "writer");
    let after = listener(root.path(), "after");
    let script = script(root.path());
    let command = |role: &str| format!("python3 {} {} {role}", quote(&script), quote(root.path()));
    let registry = Registry(Arc::new(AgentPtyRegistry::new()));
    let cwd = root.path().to_str().unwrap();
    let id = registry
        .0
        .spawn_agent(SpawnOptions {
            command: Some(&command("before")),
            cwd: Some(cwd),
            env: vec![
                (DOT_AGENT_DECK_PANE_ID.into(), "isolation-pane".into()),
                ("HOME".into(), cwd.into()),
            ],
            ..SpawnOptions::default()
        })
        .unwrap();
    let mut parent = Peer::accept(&before, false);
    let mut predecessor = Capture::new(&registry.0, &id);
    let spawned = parent.command("retain", b"");
    let mut writer = Peer::accept(&writer, true);
    assert_eq!(writer.ready["pid"], spawned["writer"]);
    let initial = writer.command("write", SENTINEL);
    assert_eq!(initial["errno"], serde_json::Value::Null);
    assert_eq!(initial["count"], SENTINEL.len());
    predecessor.through(SENTINEL).await;
    drop(predecessor); // Do not retain an extra master handle across teardown.

    let successor = if recreate {
        registry.0.close_agent(&id).unwrap();
        registry
            .0
            .respawn_or_recreate_agent_for_pane(
                "isolation-pane",
                &command("after"),
                &PaneRecreateIdentity {
                    cwd: Some(cwd.into()),
                    env: vec![
                        (DOT_AGENT_DECK_PANE_ID.into(), "isolation-pane".into()),
                        ("HOME".into(), cwd.into()),
                    ],
                    ..PaneRecreateIdentity::default()
                },
            )
            .await
            .map(|result| {
                assert!(
                    result.recreated,
                    "exercise the missing-record recreation route"
                );
                result.agent_id
            })
            .unwrap()
    } else {
        registry
            .0
            .respawn_agent_for_pane("isolation-pane", &command("after"))
            .await
            .unwrap()
    };
    let mut next = Peer::accept(&after, false);
    let mut capture = Capture::new(&registry.0, &successor);
    for attempt in 1..=3 {
        let ack = writer.command("write", SENTINEL);
        assert_eq!(
            ack["attempted"], true,
            "post-replacement write {attempt} ran"
        );
        let error = ack["errno"].as_i64();
        assert!(
            error.is_none()
                || [libc::EIO, libc::ENXIO, libc::EBADF]
                    .iter()
                    .any(|value| error == Some(i64::from(*value))),
            "retained slave either wrote or reported an old-terminal errno: {ack}"
        );
        let barrier = format!("SUCCESSOR-CAPTURE-{attempt}-COMPLETE");
        let emitted = next.command("write", barrier.as_bytes());
        assert_eq!(emitted["count"], barrier.len());
        assert_eq!(emitted["errno"], serde_json::Value::Null);
        capture.through(barrier.as_bytes()).await;
        assert!(
            !contains(&capture.bytes, SENTINEL),
            "predecessor sentinel reached successor master after write {attempt}: {:?}",
            String::from_utf8_lossy(&capture.bytes)
        );
    }
}

/// Scenario: Start a synthetic pane and a detached writer retaining its slave, and capture that writer's return-marker sentinel before respawning the pane. The writer acknowledges three actual post-respawn write attempts while successor-owned capture barriers advance, and none of its sentinel appears on the successor master stream.
#[spec("inbox/probe/063")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_063_respawn_isolates_a_surviving_predecessor_slave_writer() {
    isolation(false).await;
}

/// Scenario: Close a synthetic pane, leaving a detached writer with its predecessor slave, and recreate the pane through the missing-record path. After a positive predecessor capture, all three acknowledged old-slave write attempts are excluded from the successor master stream as successor-owned capture barriers advance.
#[spec("inbox/probe/063")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_063_recreate_isolates_a_surviving_predecessor_slave_writer() {
    isolation(true).await;
}
