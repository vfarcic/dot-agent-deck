// Unix-only at the source level, matching the two sibling fast-tier suites that
// cover this same daemon path (`idle_worker_detector.rs`,
// `delegate_prompt_injection.rs`): the harness spawns real PTYs running `cat`
// stubs so a pane has a live registry agent, and `cat` plus the PTY primitives
// are POSIX. CI's Windows job compiles the fast tier, so without this gate the
// file would build there and then fail on a command that does not exist.
// `#![cfg(unix)]` makes the crate empty on Windows; on Linux and macOS every
// test runs, matching the `Platform coverage: mac+linux` recorded for every
// `orchestration/delegate/*` entry in `tests/CATALOG.md`.
#![cfg(unix)]
//! Issues #309 / #330: what the daemon tells the CALLER about a delegate or a
//! work-done.
//!
//! Both verbs used to be fire-and-forget. Every outcome — routed to three
//! workers, refused for coming from a worker pane, naming a role no pane answers
//! to — reached the caller identically: exit 0, no output. #330 is what that
//! costs. Unable to tell a delivered delegation from a dropped one, an
//! orchestrator re-ran the identical command to check; that superseded the
//! delegation it had just armed, restarted the worker under the default
//! `clear = true`, and left a record that fired a false idle-worker report two
//! hours later.
//!
//! These tests pin the daemon half of the fix: `handle_delegate` and
//! `handle_work_done` return a classified response instead of a bare `()` behind
//! a `warn!`. They assert on that response — the CLI's own exit code, stdout and
//! stderr are a separate layer and get their own coverage once the CLI reads the
//! reply.
//!
//! Deliberately behavioural: each test asserts the OUTCOME (`ok`, which roles
//! were acked, that a warning names the superseded role) and, where the wording
//! is the whole point, that the reason mentions the remedy. None of them pin
//! full message strings, so the prose can be improved without a test edit.

use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::broadcast;

use dot_agent_deck::agent_pty::{AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, SpawnOptions};
use dot_agent_deck::event::{BroadcastMsg, DelegateSignal, WorkDoneSignal};
use dot_agent_deck::state::{AppState, OrchestrationIdentity};

use spec::spec;

const ORCH_PANE: &str = "feedback-orchestrator-pane";
const ORCH_ROLE: &str = "orchestrator";
const CODER_PANE: &str = "feedback-coder-pane";
const CODER_ROLE: &str = "coder";
const ORCHESTRATION: &str = "feedback-test-orchestration";

/// A daemon-side orchestration with a live orchestrator pane and zero or more
/// worker panes, wired exactly as `StartAgent` wires them.
///
/// The panes are real `cat` PTYs rather than fakes because the arming path reads
/// `AgentPtyRegistry::pane_current_agent_id` for the orchestrator — a pane with
/// no live agent legitimately arms nothing, so a stubbed registry would quietly
/// skip the very book-keeping the supersede warning is derived from.
struct Deck {
    cwd: TempDir,
    registry: Arc<AgentPtyRegistry>,
    state: AppState,
    event_tx: broadcast::Sender<BroadcastMsg>,
}

impl Deck {
    /// Build a deck whose orchestrator is live and which has one worker pane per
    /// entry in `worker_roles`.
    fn new(worker_roles: &[&str]) -> Self {
        let cwd = TempDir::new().expect("create orchestration cwd");
        let cwd_str = cwd.path().to_string_lossy().into_owned();
        let registry = Arc::new(AgentPtyRegistry::new());
        let orchestration = OrchestrationIdentity::Instance {
            id: format!("{ORCHESTRATION}-instance-1"),
            name: ORCHESTRATION.to_string(),
        };

        let mut state = AppState::default();
        spawn_stub(&registry, &cwd_str, ORCH_PANE);
        state
            .pane_role_map
            .insert(ORCH_PANE.to_string(), ORCH_ROLE.to_string());
        state.orchestrator_pane_ids.insert(ORCH_PANE.to_string());
        state
            .pane_orchestration_map
            .insert(ORCH_PANE.to_string(), orchestration.clone());
        state
            .pane_cwd_map
            .insert(ORCH_PANE.to_string(), cwd_str.clone());

        for role in worker_roles {
            let pane_id = worker_pane(role);
            spawn_stub(&registry, &cwd_str, &pane_id);
            state
                .pane_role_map
                .insert(pane_id.clone(), (*role).to_string());
            state
                .pane_orchestration_map
                .insert(pane_id.clone(), orchestration.clone());
            state.pane_cwd_map.insert(pane_id, cwd_str.clone());
        }

        let (event_tx, _event_rx) = broadcast::channel(64);
        Self {
            cwd,
            registry,
            state,
            event_tx,
        }
    }

    fn cwd_str(&self) -> String {
        self.cwd.path().to_string_lossy().into_owned()
    }

    /// Write a project config so `clear` and the role command resolve — the read
    /// behind the supersede warning's "was the worker restarted?" clause.
    fn write_config(&self, coder_clears: bool) {
        std::fs::write(
            self.cwd.path().join(".dot-agent-deck.toml"),
            format!(
                "[[orchestrations]]\nname = \"{ORCHESTRATION}\"\n\n\
                 [[orchestrations.roles]]\nname = \"{ORCH_ROLE}\"\ncommand = \"true\"\n\
                 start = true\n\n\
                 [[orchestrations.roles]]\nname = \"{CODER_ROLE}\"\ncommand = \"cat\"\n\
                 clear = {coder_clears}\n"
            ),
        )
        .expect("write orchestration config");
    }

    async fn delegate_from(
        &self,
        pane_id: &str,
        roles: &[&str],
    ) -> dot_agent_deck::event::DelegateResponse {
        self.state
            .handle_delegate(
                DelegateSignal {
                    pane_id: pane_id.to_string(),
                    task: "Perform the delegated test task.".to_string(),
                    to: roles.iter().map(|role| (*role).to_string()).collect(),
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
                &self.event_tx,
            )
            .await
    }

    async fn delegate(&self, roles: &[&str]) -> dot_agent_deck::event::DelegateResponse {
        self.delegate_from(ORCH_PANE, roles).await
    }

    async fn work_done_from(
        &self,
        pane_id: &str,
        done: bool,
    ) -> dot_agent_deck::event::WorkDoneResponse {
        self.state
            .handle_work_done(
                WorkDoneSignal {
                    pane_id: pane_id.to_string(),
                    task: "The delegated test task is complete.".to_string(),
                    done,
                    timestamp: chrono::Utc::now(),
                },
                &self.registry,
            )
            .await
    }
}

/// Drive an async body from a sync `#[test]`.
///
/// The spec'd tests below are deliberately NOT `#[tokio::test]`: `cargo xtask
/// linkage-check` links a `#[spec(...)]` annotation to the next *plain* `fn`, so
/// an `async fn` leaves the annotation dangling and fails rule 4. Same shape as
/// `orchestration/delegate/005` and `session/live/007`.
fn run<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build test runtime")
        .block_on(fut)
}

fn worker_pane(role: &str) -> String {
    if role == CODER_ROLE {
        CODER_PANE.to_string()
    } else {
        format!("feedback-{role}-pane")
    }
}

/// Spawn a `cat`-backed pane so the registry has a live agent under `pane_id`.
fn spawn_stub(registry: &AgentPtyRegistry, cwd: &str, pane_id: &str) -> String {
    registry
        .spawn_agent(SpawnOptions {
            command: Some("cat"),
            cwd: Some(cwd),
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane_id.to_string())],
            ..SpawnOptions::default()
        })
        .unwrap_or_else(|error| panic!("spawn stub for {pane_id}: {error}"))
}

/// The single joined string of a response's refusal reason plus every warning —
/// what a caller ends up reading, whichever channel carried each part.
fn caller_text(error: &Option<String>, warnings: &[String]) -> String {
    let mut text = error.clone().unwrap_or_default();
    for warning in warnings {
        text.push(' ');
        text.push_str(warning);
    }
    text.to_lowercase()
}

/// Scenario: A pane the daemon has never heard of runs `delegate --to coder`.
/// The daemon must refuse it and say why — that the pane has no role on record —
/// instead of the pre-fix silent `return` that let the caller believe the task
/// shipped.
#[spec("orchestration/delegate/016")]
#[test]
fn delegate_016_unknown_sender_pane_is_refused_with_a_reason() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck
            .delegate_from("a-pane-nobody-registered", &[CODER_ROLE])
            .await;

        assert!(
            !resp.ok,
            "a delegate from an unregistered pane routes nothing, so it must not report success: {resp:?}"
        );
        assert!(
            resp.delegated.is_empty(),
            "nothing was routed, so nothing may be acked: {resp:?}"
        );
        let reason = resp.error.clone().unwrap_or_default();
        assert!(
            !reason.trim().is_empty(),
            "a refusal must carry a reason the caller can act on: {resp:?}"
        );
        assert!(
            reason.contains("DOT_AGENT_DECK_PANE_ID"),
            "the reason must name the usual cause so the caller can fix it: {reason:?}"
        );
    });
}

/// Scenario: A worker pane (registered, but not the `start = true` role) runs
/// `delegate --to coder`. The daemon's anti-spoofing guard must refuse it, name
/// the role the caller actually holds, and point at `work-done` as the thing a
/// worker should call instead — a rejection nobody can observe teaches nobody.
#[spec("orchestration/delegate/017")]
#[test]
fn delegate_017_non_orchestrator_sender_is_refused_and_told_what_to_use() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.delegate_from(CODER_PANE, &[CODER_ROLE]).await;

        assert!(
            !resp.ok,
            "only the orchestrator may delegate, so a worker's delegate must not report success: {resp:?}"
        );
        let reason = resp.error.clone().unwrap_or_default();
        assert!(
            reason.contains(CODER_ROLE),
            "the reason must name the role the caller actually holds: {reason:?}"
        );
        assert!(
            reason.contains("work-done"),
            "the reason must point a worker at the verb it should be using: {reason:?}"
        );
    });
}

/// Scenario: The orchestrator delegates to a `coder` worker that exists. The
/// daemon must report success AND name what it routed — the role and the pane —
/// because a confirmation an orchestrator can see is the whole reason it stops
/// re-running the command to find out whether the first one worked (#330).
#[spec("orchestration/delegate/018")]
#[test]
fn delegate_018_success_acks_the_role_and_pane_it_routed_to() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.delegate(&[CODER_ROLE]).await;

        assert!(resp.ok, "a routed delegate must report success: {resp:?}");
        assert!(resp.error.is_none(), "a success carries no error: {resp:?}");
        assert_eq!(
            resp.delegated.len(),
            1,
            "exactly one worker pane answers to this role: {resp:?}"
        );
        assert_eq!(resp.delegated[0].role, CODER_ROLE);
        assert_eq!(
            resp.delegated[0].pane_id, CODER_PANE,
            "the ack must name the pane the task actually went to, not just the role: {resp:?}"
        );
        assert!(
            resp.warnings.is_empty(),
            "a clean first delegate has nothing to warn about: {resp:?}"
        );
    });
}

/// Scenario: The orchestrator delegates to a role no pane in the orchestration
/// answers to — a typo, a renamed role, a closed worker. Nothing is armed and
/// nothing is dispatched, so this must be reported as a refusal rather than the
/// pre-fix exit-0-with-no-output, which was the widest silent-success path of
/// all and the one neither #309 nor #330 names.
#[spec("orchestration/delegate/019")]
#[test]
fn delegate_019_role_matching_no_pane_is_refused_not_silently_dropped() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.delegate(&["reviewer"]).await;

        assert!(
            !resp.ok,
            "nothing was armed and nothing dispatched, so this is not a success: {resp:?}"
        );
        assert!(
            resp.delegated.is_empty(),
            "no pane answered, so nothing may be acked: {resp:?}"
        );
        let reason = resp.error.clone().unwrap_or_default();
        assert!(
            reason.contains("reviewer"),
            "the reason must name the role that resolved to nothing: {reason:?}"
        );
        assert!(
            reason.contains(".dot-agent-deck.toml"),
            "the reason must point at where the role name is defined: {reason:?}"
        );
    });
}

/// Scenario: One delegate names two roles, only one of which has a pane. The
/// real work must still be handed out — so this is a success, not a refusal —
/// but the caller must be told the other role received nothing, or it will sit
/// waiting on a worker that was never given anything.
#[spec("orchestration/delegate/020")]
#[test]
fn delegate_020_partial_fan_out_succeeds_but_names_the_role_that_missed() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.delegate(&[CODER_ROLE, "reviewer"]).await;

        assert!(
            resp.ok,
            "a delegate that routed real work is a success even when another role missed: {resp:?}"
        );
        assert_eq!(
            resp.delegated.len(),
            1,
            "only the coder pane exists, so only it is acked: {resp:?}"
        );
        assert_eq!(resp.delegated[0].role, CODER_ROLE);
        let warnings = resp.warnings.join(" ");
        assert!(
            warnings.contains("reviewer"),
            "the caller must be told which role received nothing: {:?}",
            resp.warnings
        );
    });
}

/// Scenario: The orchestrator delegates to `coder`, then delegates to `coder`
/// again while the first is still outstanding — the exact shape of #330's
/// confirmatory re-run. The second delegate must still succeed (interrupt and
/// redirect is legitimate) but must warn that it displaced live work, and,
/// because the role config sets `clear = true`, that the worker was restarted
/// and its in-progress session discarded.
#[spec("orchestration/delegate/021")]
#[test]
fn delegate_021_superseding_a_busy_worker_warns_that_live_work_was_displaced() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);
        deck.write_config(true);

        let first = deck.delegate(&[CODER_ROLE]).await;
        assert!(first.ok, "the first delegate routes normally: {first:?}");
        assert!(
            first.warnings.is_empty(),
            "the first delegate displaced nothing: {first:?}"
        );

        let second = deck.delegate(&[CODER_ROLE]).await;

        assert!(
            second.ok,
            "re-delegating is legal — the daemon warns rather than refuses, because \
             interrupt-and-redirect is a documented recovery path: {second:?}"
        );
        let warnings = second.warnings.join(" ");
        assert!(
            warnings.contains(CODER_ROLE),
            "the warning must name the worker whose outstanding delegation was displaced: {:?}",
            second.warnings
        );
        assert!(
            warnings.to_lowercase().contains("supersede"),
            "the warning must say the previous delegation was superseded: {:?}",
            second.warnings
        );
        assert!(
            warnings.to_lowercase().contains("restart"),
            "with clear = true the worker was restarted mid-task, which is the destructive half \
             the caller most needs to know about: {:?}",
            second.warnings
        );
    });
}

/// Scenario: A worker signals `work-done` in an orchestration whose orchestrator
/// pane is gone. The summary file is still written, but the feedback reaches
/// nobody — the failure `docs/orchestration.md` documents as silent. The worker
/// must be told its report landed nowhere, and still be told where the summary
/// was saved so the content is not simply lost.
#[spec("orchestration/delegate/022")]
#[test]
fn delegate_022_work_done_with_no_live_orchestrator_reports_reaching_nobody() {
    run(async {
        let mut deck = Deck::new(&[CODER_ROLE]);
        // Retire the orchestrator exactly as closing its pane would: the routing
        // identity goes with it, so `orchestrator_for_worker` finds nothing.
        deck.state.unregister_pane(ORCH_PANE);

        let resp = deck.work_done_from(CODER_PANE, false).await;

        assert!(
            !resp.ok,
            "a completion nobody receives is not a success: {resp:?}"
        );
        assert!(
            resp.reported_to.is_none(),
            "there was no orchestrator pane to report into: {resp:?}"
        );
        assert!(
            resp.summary_path.is_some(),
            "the summary is still written, and saying so is what stops the report being lost: {resp:?}"
        );
        let text = caller_text(&resp.error, &resp.warnings);
        assert!(
            text.contains("orchestrator"),
            "the reason must say what was missing: {resp:?}"
        );
    });
}

/// Scenario: A worker signals `work-done` while its orchestrator is live. The
/// daemon writes the summary and the feedback line into the orchestrator's pane,
/// and — because this handler awaits that write rather than spawning it — the
/// reply can honestly report delivery, naming both the pane it reached and the
/// file it wrote.
#[spec("orchestration/delegate/023")]
#[test]
fn delegate_023_work_done_reports_the_orchestrator_it_reached() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.work_done_from(CODER_PANE, false).await;

        assert!(resp.ok, "the completion was delivered: {resp:?}");
        assert_eq!(
            resp.reported_to.as_deref(),
            Some(ORCH_PANE),
            "the reply must name the orchestrator pane the feedback reached: {resp:?}"
        );
        let summary = resp.summary_path.clone().unwrap_or_default();
        assert!(
            summary.contains(&format!("work-done-{CODER_ROLE}.md")),
            "the reply must name the summary file it wrote, which is also the path a worker must \
             avoid passing as its own --task-file input: {resp:?}"
        );
        assert!(
            std::path::Path::new(&summary).exists(),
            "the named summary file must actually be on disk: {summary:?}"
        );
        // The harness's cwd must outlive the assertions above.
        drop(deck);
    });
}

/// Scenario: The orchestrator itself signals `work-done --done`, which completes
/// the whole orchestration. There is nobody to report to by design, so this must
/// be reported as a success rather than borrowing the "reached nobody" failure
/// that a worker in the same position would get.
#[spec("orchestration/delegate/024")]
#[test]
fn delegate_024_orchestrator_done_is_a_success_with_nobody_to_report_to() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.work_done_from(ORCH_PANE, true).await;

        assert!(
            resp.ok,
            "the orchestration completing is a success, not a delivery failure: {resp:?}"
        );
        assert!(
            resp.reported_to.is_none(),
            "the orchestrator does not report its own completion to itself: {resp:?}"
        );
        assert!(resp.error.is_none(), "nothing failed: {resp:?}");
    });
}

/// Scenario: The orchestrator names the same role twice in one `delegate`. The
/// repeat is already de-duplicated (PRD #126 M1 audit finding 3), so the daemon
/// must ack the role exactly once and tell the caller the repeat did nothing —
/// otherwise a caller reading two acks would believe it had doubled the work.
#[spec("orchestration/delegate/025")]
#[test]
fn delegate_025_repeated_role_is_acked_once_and_reported_as_ignored() {
    run(async {
        let deck = Deck::new(&[CODER_ROLE]);

        let resp = deck.delegate(&[CODER_ROLE, CODER_ROLE]).await;

        assert!(resp.ok, "the delegate still routes: {resp:?}");
        assert_eq!(
            resp.delegated.len(),
            1,
            "the repeat is de-duplicated, so it is acked once: {resp:?}"
        );
        let warnings = resp.warnings.join(" ");
        assert!(
            warnings.contains(CODER_ROLE),
            "the caller must learn the repeat was ignored rather than doubled: {:?}",
            resp.warnings
        );
    });
}

/// A live worker pane is required for the acks above to mean anything, so prove
/// the harness itself is honest: the stub really is running and really is the
/// pane the registry resolves for the orchestrator identity the arming path
/// reads. Without this, every assertion above could pass against a dead registry.
#[tokio::test]
async fn harness_panes_are_live_in_the_registry() {
    let deck = Deck::new(&[CODER_ROLE]);

    assert!(
        deck.registry.pane_current_agent_id(ORCH_PANE).is_some(),
        "the orchestrator pane must have a live agent, or the idle record — and with it the \
         supersede warning — is never armed at all"
    );
    assert!(
        deck.registry.pane_current_agent_id(CODER_PANE).is_some(),
        "the worker pane must be live for the dispatch to have somewhere to go"
    );
    // The cwd the state maps point at is the one the config read resolves from.
    assert_eq!(
        deck.state.pane_cwd_map.get(CODER_PANE).cloned(),
        Some(deck.cwd_str())
    );
    // Give the spawned dispatch tasks a moment to settle so the temp dir is not
    // yanked out from under a config read still in flight.
    tokio::time::sleep(Duration::from_millis(50)).await;
}
