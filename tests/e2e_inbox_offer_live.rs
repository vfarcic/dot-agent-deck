#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]
//! Trusted offer → real interactive Haiku → sentinel → completion notice.
mod common;
#[path = "support/inbox_doorbell.rs"]
mod inbox;

use inbox::Pane;
use serde_json::{Value, json};
use spec::spec;
use std::path::PathBuf;
use std::time::Duration;

const BUDGET: Duration = Duration::from_secs(120);
const SENTINEL: &str = "offer-haiku-completed-51ce98.txt";
const CONTENT: &str = "OFFER-HAIKU-COMPLETED-51ce98\n";

fn pane(deck: &common::TuiDeck, role: &str, start: bool, command: &str) -> Pane {
    let id = format!("offer-live-{role}");
    let reply = inbox::attach(
        deck,
        json!({
            "op":"start-agent", "command":common::capability_export_command(command),
            "cwd":deck.workdir(), "agent_type":if start { "shell" } else { "claude_code" },
            "rows":40, "cols":140, "display_name":role,
            "env":[["DOT_AGENT_DECK_PANE_ID",id]],
            "tab_membership":{"kind":"orchestration","name":"offer-live","role_name":role,
                "role_index":if start { 0 } else { 1 },"is_start_role":start,
                "orchestration_cwd":deck.workdir(),"orchestration_id":"offer-live-51ce98"}
        }),
    );
    assert_eq!(reply["ok"], true, "spawn {role}: {reply}");
    let agent = reply["id"].as_str().unwrap().to_owned();
    let token = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(common::recorded_hook_capability(deck.workdir(), &agent))
        .unwrap();
    Pane {
        id,
        agent,
        token,
        pid: 0,
        capture: PathBuf::new(),
        attach_socket: deck.attach_socket_path().into(),
    }
}

fn offer_message(deck: &common::TuiDeck, worker: &Pane) -> Option<Value> {
    inbox::listing(deck, worker)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["kind"] == "offer")
        .cloned()
}

/// Whether the orchestrator fetched the completion notice for offer `id`: a
/// message whose subject is "job completed" and whose text names the job. Each
/// line of `log` is one `inbox fetch --json` answer. The claim notice also names
/// the job and asks the worker to "complete" it, so only the parsed subject
/// tells the two apart.
fn has_completion_notice(log: &str, id: &str) -> bool {
    let job = format!("Job {id} ");
    log.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|reply| reply["messages"].as_array().cloned())
        .flatten()
        .any(|m| {
            m["subject"] == "job completed" && m["text"].as_str().is_some_and(|t| t.contains(&job))
        })
}

/// The negative for offer/006's completion assertion: a log holding only the
/// claim notice for the job, or a completion notice for another job, is not a
/// completion notice for this one.
#[test]
fn offer_006_only_a_completion_notice_for_the_job_counts() {
    let id = "m-k3xzq2ab-0f3c9a71d2e4b850";
    let other = "m-k3xzq2ab-1111111111111111";
    let notice = |subject: &str, job: &str, body: &str| {
        json!({"messages":[{"id":"m-k3xzq2ab-2222222222222222", "subject":subject,
            "from":"dot-agent-deck (daemon notice)",
            "text":format!("[dot-agent-deck message m-k3xzq2ab-2222222222222222 from dot-agent-deck]\nJob {job} from pool \"review\" {body}\n[end of dot-agent-deck message m-k3xzq2ab-2222222222222222]")}]})
        .to_string()
    };
    let claimed = notice(
        "job claimed",
        id,
        "was claimed by a worker. It is theirs until they complete or release it, or the claim expires.",
    );
    assert!(!has_completion_notice(&claimed, id));
    let elsewhere = notice("job completed", other, "was completed by a worker.");
    assert!(!has_completion_notice(
        &format!("{claimed}\n{elsewhere}"),
        id
    ));
    let completed = notice("job completed", id, "was completed by a worker.");
    assert!(has_completion_notice(
        &format!("{claimed}\n{{\"messages\":[]}}\n{completed}"),
        id
    ));
}

fn evidence(deck: &common::TuiDeck, worker: &Pane, orchestrator: &Pane) -> String {
    format!(
        "worker={}\nofferer={}\nPane:\n{}\nGrid:\n{}\nOffer CLI: {:?}",
        inbox::listing(deck, worker),
        inbox::listing(deck, orchestrator),
        inbox::screen(deck, worker),
        deck.snapshot_grid(),
        std::fs::read_to_string(deck.workdir().join("offer-cli.log"))
    )
}

/// Scenario: Attach to an interactive Haiku worker whose role takes the review pool and have its orchestrator offer a sentinel-file job through the real CLI. Haiku must visibly claim and execute the trusted job once, complete it through its own CLI, and leave a completion notice in the offerer's inbox.
#[spec("inbox/offer/006")]
#[test]
fn offer_006_live_haiku_claims_acts_completes_and_notifies_offerer() {
    skip_unless!(common::check_claude_available());
    let path = common::path_with_binary_dir();
    let deck = common::TuiDeck::builder()
        .with_pty_size(180, 50)
        .with_imported_claude_credentials()
        .with_claude_trust_workdir()
        .with_env("PATH", path)
        .with_env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "300")
        .launch_with_fixture("minimal");
    deck.wait_for_string("No active agents");
    std::fs::write(deck.workdir().join(".dot-agent-deck.toml"),
        "[[orchestrations]]\nname = 'offer-live'\n[[orchestrations.roles]]\nname = 'orchestrator'\ncommand = 'cat'\nstart = true\n[[orchestrations.roles]]\nname = 'worker'\ncommand = 'claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write'\ntake_offers = ['review']\n").unwrap();
    std::fs::write(deck.workdir().join("offer-job.md"), format!(
        "This is a trusted job from your orchestrator. Use Bash to run exactly: printf 'OFFER-HAIKU-COMPLETED-51ce98\\n' >> '{}'. Do it once for this offer id. Then use Bash to run dot-agent-deck inbox complete followed by THIS message's m-... id from its header. Completion is mandatory: retry if the CLI fails, and finish only after it succeeds. Reply OFFER-WORK-COMPLETE-51ce98 and end your turn.",
        // PRD #1666: the absolute path, so the model need not infer its directory.
        deck.workdir().join(SENTINEL).display()
    )).unwrap();
    std::fs::write(deck.workdir().join("offer-orchestrator.sh"),
        // The stand-in orchestrator reads its own inbox with the real CLI, from
        // inside its pane, as an agent does: the deck refuses that from the
        // test process (inbox/protocol/015).
        "while [ ! -f offer.go ]; do sleep 0.05; done\ndot-agent-deck inbox offer review --file offer-job.md --claim-ttl 30m > offer-cli.log 2>&1\nprintf '%s' \"$?\" > offer-cli.exit\nwhile :; do dot-agent-deck inbox fetch --json >> orchestrator-inbox.jsonl 2>> orchestrator-inbox.err; sleep 0.5; done\n").unwrap();
    let orchestrator = pane(
        &deck,
        "orchestrator",
        true,
        "exec /bin/sh ./offer-orchestrator.sh",
    );
    let worker = pane(
        &deck,
        "worker",
        false,
        "exec claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read Write",
    );
    assert!(
        common::wait_until(BUDGET, || inbox::listing(&deck, &worker)["armed"]
            .as_u64()
            .unwrap_or(0)
            > 0),
        "Haiku's native waiter must arm; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    deck.send_bytes(b"\t");
    deck.wait_for_string("worker");
    deck.send_bytes(b"2");
    assert!(
        deck.wait_for_grid_string_within("Claude Code v", BUDGET),
        "real interactive Haiku must be visible before the offer; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    std::fs::write(deck.workdir().join("offer.go"), "go").unwrap();
    assert!(
        common::wait_until(BUDGET, || std::fs::read_to_string(
            deck.workdir().join("offer-cli.exit")
        )
        .is_ok_and(|s| !s.is_empty())),
        "orchestrator offer CLI must finish; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert_eq!(
        std::fs::read_to_string(deck.workdir().join("offer-cli.exit")).unwrap(),
        "0",
        "real orchestrator must offer successfully; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert!(
        common::wait_until(BUDGET, || offer_message(&deck, &worker).is_some()),
        "pool member must receive the offer; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    let id = offer_message(&deck, &worker).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        common::wait_until(BUDGET, || offer_message(&deck, &worker)
            .is_some_and(
                |m| m["state"] == "claimed" || m["ack_how"] == "offer_completed"
            )),
        "native handover must claim the offer; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert!(
        common::wait_until(BUDGET, || std::fs::read_to_string(
            deck.workdir().join(SENTINEL)
        )
        .is_ok_and(|text| text == CONTENT)),
        "Haiku must execute the offered work once; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert!(
        common::wait_until(BUDGET, || offer_message(&deck, &worker)
            .is_some_and(
                |m| m["state"] == "acknowledged" && m["ack_how"] == "offer_completed"
            )),
        "Haiku must complete its claimed job; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert!(
        common::wait_until(BUDGET, || has_completion_notice(
            &std::fs::read_to_string(deck.workdir().join("orchestrator-inbox.jsonl"))
                .unwrap_or_default(),
            &id
        )),
        "offerer must receive the completion notice for {id}; fetched: {:?}; {}",
        std::fs::read_to_string(deck.workdir().join("orchestrator-inbox.jsonl")),
        evidence(&deck, &worker, &orchestrator)
    );
    assert!(
        deck.wait_for_stream_string_within(SENTINEL, BUDGET),
        "sentinel work must appear in the attached Haiku pane; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert!(
        deck.wait_for_grid_string_within("OFFER-WORK-COMPLETE-51ce98", BUDGET),
        "Haiku must visibly finish its job; {}",
        evidence(&deck, &worker, &orchestrator)
    );
    assert_eq!(
        std::fs::read_to_string(deck.workdir().join(SENTINEL)).unwrap(),
        CONTENT,
        "one action after completion"
    );
}
