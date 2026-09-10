#![cfg(all(feature = "e2e", feature = "e2e-live", unix))]

//! L2 lane-2 REAL-agent proof for PRD #819's launch verb: a genuine interactive
//! Claude Code coordinator reads the context **the daemon** composed and
//! published, and visibly acts on the task it found there.
//!
//! ## Why this test exists when three lane-1 tests already cover the verbs
//! `tests/e2e_project_verbs.rs` pins what `ListProjects` / `ResolveProject` /
//! `PrepareWorkflow` RETURN and where the coordinator context LANDS. Every one
//! of those assertions is satisfied by a file with the right bytes at the right
//! path — which is the plumbing, not the point. PRD #819 moves the composition
//! *and the write* off the client and onto the daemon precisely so that an agent
//! on a machine the client cannot see still gets a context to read. Nothing in
//! lane 1 proves an agent ever reads it, and CLAUDE.md rule 4 asks for exactly
//! one test per major feature that validates it as a user actually uses and sees
//! it. This is that test, and rule 5 records that it runs on a developer's
//! machine and **nowhere in CI** — so a break here surfaces when someone next
//! runs `cargo test-e2e-live`, not on any schedule.
//!
//! ## The shape, and why it is not the TUI's `Ctrl+N` flow
//! The TUI's interactive orchestration path writes the context ITSELF
//! (`src/ui.rs`'s `prepare_orchestrator_prompt(&orch_config, &dir_str, None)`),
//! so driving `Ctrl+N` here would have the client overwrite the daemon's file
//! with a task-less one and prove nothing. The client this verb exists for is
//! the desktop, and its sequence is the one modelled here, over the attach
//! socket:
//!
//!   1. `ResolveProject` — the daemon answers with ITS canonical spelling and a
//!      `config_revision`. Both are carried forward verbatim; a client never
//!      derives a path from its own environment.
//!   2. `PrepareWorkflow` — the **daemon** loads one validated config snapshot,
//!      composes the coordinator context through the shared composer and
//!      publishes it at `<project>/.dot-agent-deck/orchestrator-context.md`.
//!      This is the path under test; the test never writes that file.
//!   3. `StartAgent` — one role, spawned into the canonical path the daemon
//!      returned. `crate::event::ProjectRole` deliberately carries only `name`
//!      and `start`, so command strings never cross the wire and the client
//!      supplies the command; [`COORDINATOR_COMMAND`] is kept in lockstep with
//!      the fixture's own `command` key.
//!   4. `WriteAndSubmit` — the production guarded prompt-delivery RPC types the
//!      one-line pointer into the coordinator's PTY. That line is all the agent
//!      is ever told: it names the file and nothing else, so a coordinator that
//!      does not read the daemon's file has no task at all.
//!
//! ## What makes the assertion robust rather than lucky
//! The task the daemon writes into the context is a directive list-files
//! command (rule 4's shape, copied from `scheduler/dispatch/013`), and the
//! fixture ships a uniquely named [`SENTINEL`]. So the hard assertion is on one
//! literal filename appearing in the coordinator's pane — never on prose the
//! model composes. The sentinel is deliberately absent from the context file
//! itself (asserted below), so it can only be reported by an agent that read
//! the task AND genuinely ran the tool: spawn -> agent -> work.
//!
//! ONE role, deliberately: `build_orchestrator_context` lists only NON-start
//! roles under `## Available agents`, so a solo coordinator has an empty list
//! and nobody to delegate to. That keeps the run to a single cheap Haiku turn
//! and keeps this test's claim ("the coordinator read the daemon's file")
//! separate from the delegation chains `orchestration/route/001` and
//! `scheduler/dispatch/013` already own.
//!
//! Cost (Decision 23): one short interactive Haiku turn (read a file, run
//! `ls -a`, print the listing) — well under the <$0.05/run bound. Flaky-tolerant
//! (real LLM) per rule 4: run once, not looped.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::DOT_AGENT_DECK_PANE_ID;
use dot_agent_deck::daemon_protocol::AttachRequest;
use dot_agent_deck::event::{AgentType, SendResult};
use spec::spec;

/// The fixture's `[[orchestrations]] name`. Named explicitly so nothing about
/// the run depends on the tempdir basename the fixture is copied into.
const ORCHESTRATION: &str = "context-handoff";

/// The fixture's single role, and the `display_name` its card carries.
const COORDINATOR_ROLE: &str = "coordinator";

/// The coordinator's command, kept byte-identical to the `command` key in
/// `tests/fixtures/project-launch-real/.dot-agent-deck.toml`.
///
/// The duplication is the design, not an oversight: `ProjectRole` carries only
/// `name` and `start`, so a role's command never crosses the wire and the
/// spawning client is the party that holds it. `Read` lets the coordinator open
/// the daemon-published context without stopping at a permission prompt;
/// `Bash` lets it genuinely run `ls -a`. Interactive — no `-p`, per rule 4.
const COORDINATOR_COMMAND: &str =
    "claude --model claude-haiku-4-5-20251001 --allowedTools Bash Read";

/// The committed fixture file the coordinator must report back. Uniquely named
/// (rule 4) so the assertion survives LLM phrasing and tool variance: only this
/// literal has to come through, and `common::wait_for_pane_text_on` matches it
/// wrap-insensitively.
const SENTINEL: &str = "context_proof_9d4f2a.txt";

/// `DOT_AGENT_DECK_PANE_ID` for the coordinator pane. The test mints it (as
/// every spawning client does) because it is the handle both `WriteAndSubmit`
/// and the daemon's pane maps are keyed on.
const PANE_ID: &str = "project-launch-coordinator";

/// The task handed to `PrepareWorkflow`, which the daemon folds into the
/// published context under `## Your task`.
///
/// Directive rather than conversational, for the reason
/// `orchestration/route/001`'s `NO_STALL_CLAUSE` records: a vague brief lets a
/// real model answer with numbered options instead of doing the work. The
/// no-delegation clause is first because the composed context's `## Important`
/// section pushes hard toward delegating, and this orchestration has nobody to
/// delegate to.
const LAUNCH_TASK: &str = "Do this yourself and do not delegate - no other agents are running. \
     Use the Bash tool to run `ls -a` in the current directory and print every filename it \
     lists verbatim, one per line, with no other commentary. Do not ask what to do, offer \
     numbered choices, or wait for further instructions - this IS the task and you already \
     have everything you need.";

/// The single line delivered into the coordinator's PTY, reproduced verbatim
/// from `orchestrator_context::orchestrator_prompt_line(true)` — the private
/// helper every production launch path injects.
///
/// It is load-bearing that this line names the file and nothing else: it
/// carries no task, no sentinel and no `ls`, so everything the coordinator does
/// next it learned from the daemon's write.
const COORDINATOR_PROMPT: &str = "Read .dot-agent-deck/orchestrator-context.md for your role, the available agents, the \
     delegation protocol, and your task under `## Your task`. Then carry out that task, \
     delegating to the agents listed there.";

/// Scenario: Launch the real deck in a project whose single `context-handoff`
/// orchestration declares one interactive Haiku Claude `coordinator` role, then
/// drive the desktop's own sequence over the attach socket — `ResolveProject`
/// for the daemon's canonical path and config revision, then `PrepareWorkflow`,
/// so the DAEMON composes and publishes the coordinator context carrying a
/// directive list-files task, then `StartAgent` to bring a real coordinator up
/// in that canonical directory, then the production `WriteAndSubmit` RPC to type
/// the one-line "read `.dot-agent-deck/orchestrator-context.md` and carry out
/// your task" pointer into its pane. Its pane is opened in the attached TUI so
/// the live agent is visible while it works. HARD-ASSERT that the uniquely named
/// fixture sentinel `context_proof_9d4f2a.txt` — which appears nowhere in the
/// published context, only on disk — shows up in the coordinator's pane, which
/// it can only do by having read the daemon's file and genuinely run the tool.
/// Best-effort and logged, not gating: whether that filename also lands on the
/// deck's own vt100 stream. Reel-eligible (PTY-attached, records a
/// `full-stream.cast`); flaky-tolerant (real LLM) — run once, not looped.
#[spec("project/launch/003")]
#[test]
fn project_launch_003_a_real_coordinator_reads_the_daemon_published_context() {
    // Decision 26 runtime-skip: a missing CLI / credentials is an environmental
    // condition, not a broken test. FIRST statement of the body, because
    // `launch_with_fixture` panics rather than skips on a credential-import
    // failure (CLAUDE.md rule 5).
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        // One live pane; wide enough for a real claude TUI to render usefully
        // and small enough to stay legible in a reel clip.
        .with_pty_size(110, 32)
        // Real credentials, so the daemon-spawned interactive claude
        // authenticates.
        .with_imported_claude_credentials()
        // THE CANONICAL-PATH TRAP. The daemon canonicalises the project path and
        // that canonical spelling becomes the spawn cwd, while claude's
        // per-folder trust key is matched VERBATIM. A harness tempdir reached
        // through a symlink (`/var` -> `/private/var` on macOS, or any symlinked
        // `TMPDIR`) would therefore leave the approval prompt unanswered and the
        // agent parked until the harness timeout, with nothing saying why. This
        // builder flag trusts BOTH the raw and the canonicalised form of the work
        // dir, which is the only spelling pair the daemon can produce here.
        .with_claude_trust_workdir()
        .launch_with_fixture("project-launch-real");
    deck.wait_for_string("No active sessions");

    let socket = deck.attach_socket_path().to_path_buf();
    let events = deck.subscribe_events();
    // The spelling a user would supply — the deck's own cwd, un-canonicalised.
    let typed_path = deck.workdir().to_string_lossy().into_owned();

    // ---- 1. Resolve. The daemon answers with ITS canonical path and the
    // revision of the exact config bytes it read; both are carried forward
    // verbatim rather than recomputed here, which is what "the same directory
    // string resolves the listing and the spawn" means in practice.
    let resp = common::attach_request_on(
        &socket,
        &AttachRequest::ResolveProject {
            path: typed_path.clone(),
        },
    )
    .expect("ResolveProject over the attach socket");
    assert!(
        resp.ok,
        "the daemon must resolve the deck's own project directory ({typed_path}); it refused \
         instead: {:?}",
        resp.error
    );
    let resolved = resp
        .project
        .expect("a successful ResolveProject must carry a ResolvedProject");
    assert!(
        resolved
            .orchestrations
            .iter()
            .any(|o| o.name == ORCHESTRATION),
        "the fixture project must offer the {ORCHESTRATION:?} orchestration; got {:?}",
        resolved
            .orchestrations
            .iter()
            .map(|o| o.name.as_str())
            .collect::<Vec<_>>()
    );

    // ---- 2. Prepare. THIS is the path under test: the daemon loads one
    // validated snapshot, composes the coordinator context through the shared
    // composer, and publishes it. The test writes no context file of its own.
    let resp = common::attach_request_on(
        &socket,
        &AttachRequest::PrepareWorkflow {
            path: resolved.path.clone(),
            orchestration: ORCHESTRATION.into(),
            task: LAUNCH_TASK.into(),
            config_revision: resolved.config_revision.clone(),
        },
    )
    .expect("PrepareWorkflow over the attach socket");
    assert!(
        resp.ok,
        "PrepareWorkflow must resolve the project, compose the coordinator context and publish \
         it; it refused instead: {:?}",
        resp.error
    );
    let prepared = resp
        .workflow_prepared
        .expect("a successful PrepareWorkflow must carry a PreparedWorkflow");

    // Preconditions, not the claim — `project/launch/001` owns the publish
    // contract. They are here so a red run says WHICH half broke: the daemon
    // never wrote a usable file, or the agent never read one.
    let context_path = Path::new(&prepared.context_path).to_path_buf();
    let context = std::fs::read_to_string(&context_path).unwrap_or_else(|e| {
        panic!(
            "the coordinator context the daemon reported at {} must be readable: {e}",
            context_path.display()
        )
    });
    assert!(
        context.contains(LAUNCH_TASK),
        "the published context ({} bytes at {}) must carry the task the launch verb was given — \
         without it the coordinator has nothing to read",
        context.len(),
        context_path.display()
    );
    assert!(
        !context.contains(SENTINEL),
        "the sentinel {SENTINEL:?} must NOT appear in the published context ({}), or reporting \
         it would prove reading without proving the agent ran anything",
        context_path.display()
    );

    // ---- 3. Start the coordinator role: a REAL interactive Haiku claude, in
    // the canonical directory the daemon named.
    let resp = common::attach_request_on(
        &socket,
        &AttachRequest::StartAgent {
            command: Some(COORDINATOR_COMMAND.into()),
            cwd: Some(resolved.path.clone()),
            rows: 30,
            cols: 108,
            env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), PANE_ID.to_string())],
            display_name: Some(COORDINATOR_ROLE.to_string()),
            // A dashboard pane, not an orchestration tab: this orchestration has
            // one role and delegates to nobody, so the per-tab membership token
            // would carry no claim this test makes.
            tab_membership: None,
            agent_type: Some(AgentType::ClaudeCode),
            // Pi's native seed pull only; a claude coordinator is driven by the
            // guarded PTY delivery below.
            seed: None,
        },
    )
    .expect("StartAgent over the attach socket");
    assert!(
        resp.ok,
        "the daemon must spawn the coordinator role into {}; it refused instead: {:?}",
        resolved.path, resp.error
    );
    let agent_id = resp
        .id
        .expect("a successful StartAgent must report the registry id it created");

    // ---- 4. The real agent boots. A genuine (non-wrapper) `SessionStart` from
    // the pane is both the proof that claude actually came up and the generation
    // the guarded delivery below binds to.
    let session_id =
        events.wait_for_session_start_on_pane(PANE_ID, &agent_id, Duration::from_secs(180));

    // Open the coordinator's pane in the already-attached TUI, so the live agent
    // is on screen (and on the cast) for the whole turn — the surface a user
    // actually watches.
    deck.wait_for_absence("No active sessions");
    deck.send_keys(b"1");

    // Nothing may be injected into a claude that is still painting its UI: bytes
    // written mid-boot are dropped. Not fatal on its own — a still-busy agent may
    // still accept the prompt — but worth surfacing so a later failure is
    // diagnosable.
    if !common::wait_until_panes_settled(
        &socket,
        std::slice::from_ref(&agent_id),
        Duration::from_millis(1500),
        Duration::from_secs(8),
        Duration::from_secs(180),
    ) {
        eprintln!("warning: the coordinator pane never settled within 180s; delivering anyway");
    }

    // ---- 5. Deliver the one-line pointer through the production guarded RPC —
    // the same write-and-submit a real launch uses for the coordinator prompt.
    let resp = common::write_and_submit_with_identity_on(
        &socket,
        PANE_ID,
        COORDINATOR_PROMPT,
        &agent_id,
        Some(&session_id),
    )
    .expect("WriteAndSubmit to the coordinator pane over the attach socket");
    assert_eq!(
        resp.send_result,
        Some(SendResult::Applied),
        "the daemon refused to deliver the coordinator prompt to pane {PANE_ID}: error={:?}, \
         send_result={:?}",
        resp.error,
        resp.send_result
    );

    // ---- 6. THE claim. The coordinator was told only to read a file. If the
    // daemon's write is the one an agent actually reads, the fixture sentinel —
    // which is on disk and nowhere in the context — comes back in its pane.
    const REPORT_WAIT: Duration = Duration::from_secs(240);
    let reported = common::wait_for_pane_text_on(&socket, &agent_id, SENTINEL, REPORT_WAIT);
    assert!(
        reported,
        "the coordinator never reported the fixture sentinel {SENTINEL:?} within {}s. It was \
         told ONLY to read {} and carry out the task it found there, so this means the \
         daemon-composed context did not reach a real agent as a usable brief — the whole point \
         of moving the write daemon-side.\n=== coordinator pane (normalized, tail) ===\n{}\n\
         === deck grid ===\n{}",
        REPORT_WAIT.as_secs(),
        context_path.display(),
        tail(&common::pane_search_key_on(&socket, &agent_id)),
        deck.snapshot_grid()
    );

    // Best-effort narrative for the reel: the same filename on the deck's own
    // vt100 stream. Logged rather than asserted — a claude TUI inside a pane can
    // hard-wrap a filename across a row boundary, which the pane-level check
    // above is normalized against and a raw stream scan is not.
    let on_screen = deck.wait_for_stream_string_within(SENTINEL, Duration::from_secs(30));
    eprintln!(
        "reel narrative (soft): sentinel {SENTINEL:?} visible on the deck's own grid = {on_screen}"
    );
}

/// The last ~1500 characters of a normalized pane dump, for a failure message
/// that stays readable when the pane holds a whole claude TUI's scrollback.
fn tail(text: &str) -> &str {
    const KEEP: usize = 1500;
    if text.len() <= KEEP {
        return text;
    }
    let mut start = text.len() - KEEP;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}
