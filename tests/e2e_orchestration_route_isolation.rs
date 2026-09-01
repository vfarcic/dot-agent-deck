#![cfg(all(feature = "e2e", feature = "e2e-live"))]

//! L2 PTY-attached, REAL-agent proof that two tabs of the SAME orchestration in
//! the SAME directory do NOT cross-deliver delegate / work-done (PRD #140 M5.1,
//! CLAUDE.md rule 4's hard gate).
//!
//! ## Why this test exists when `src/state.rs` already unit-tests the routing
//! The mutation-checked unit tests around `delegate_targets` /
//! `orchestrator_for_worker` pin the routing DECISION against a hand-built
//! `AppState`. They cannot pin the thing that actually broke in issue #140: that
//! the per-tab `orchestration_id` is really MINTED once per `Ctrl+N`
//! orchestration tab, really survives the `StartAgent` → daemon-registry →
//! `ListAgents` round trip, and is really what the live daemon keys
//! `pane_orchestration_map` on when a real orchestrator's `dot-agent-deck
//! delegate` arrives over the hook socket. Every one of those seams is exercised
//! here through the REAL binary: the deck is driven by keystrokes in a PTY, both
//! tabs are opened through the production new-pane flow against one directory,
//! and the signals are issued by real agents rather than by the test.
//!
//! ## Why THREE roles, and why both chains run at once
//! `.dot-agent-deck/worker-task-{role}.md` and `work-done-{role}.md` are keyed by
//! ROLE within a cwd — deliberately, PRD #140 keeps that layer out of scope — so
//! two same-cwd tabs sharing a role name also share those files. That makes a
//! file artifact useless as a "which tab did this?" discriminator, and an agent
//! TUI's redrawing scrollback makes occurrence-COUNTING unreliable. The fixture
//! therefore carries three roles and each tab is driven through a DIFFERENT one:
//!
//! - tab A: `orchestrator` → delegate to `coder`
//! - tab B: `orchestrator` → delegate to `reviewer`
//!
//! so every no-cross-delivery check is a presence/absence question about a pane
//! that would otherwise have received NOTHING (tab B's `coder`, tab A's
//! `reviewer`), and the two work-done feedback strings the daemon writes back
//! carry the role name ("Worker coder …" vs "Worker reviewer …") and so stay
//! distinguishable inside a single orchestrator pane. The split roles also mean
//! the two coordination-file sets never collide, so both chains can run
//! CONCURRENTLY — which is the issue's literal repro ("start a task in each")
//! and the state in which the pre-#140 `HashSet`-ordered work-done lookup was
//! most non-deterministic.
//!
//! The load-bearing observables are DAEMON-authored, not agent-authored: the
//! delegate pointer and the work-done feedback are bytes `handle_delegate` /
//! `handle_work_done` write into a specific pane's PTY. Which pane they land in
//! IS the routing decision. The real agents supply the ends of the chain — a
//! real orchestrator deciding to shell `dot-agent-deck delegate`, a real worker
//! reading its task file, doing the work, and shelling `dot-agent-deck
//! work-done` — plus the per-tab, uniquely-named sentinel files that prove the
//! work genuinely happened (rule 4).
//!
//! Cost (Decision 23): four short interactive Haiku turns (two orchestrators
//! delegate, two workers create one file each) — well under the <$0.05/run
//! bound. Local-only (Decision 8 / rule 5): gated on the `e2e` feature so CI's
//! `cargo test-fast` never compiles it; flaky-tolerant (real LLM) per rule 4 —
//! run once, not looped.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use dot_agent_deck::agent_pty::TabMembership;
use dot_agent_deck::event::SendResult;
use spec::spec;

/// The fixture's `[[orchestrations]] name` — the label BOTH tabs render with in
/// the tab strip (that they are indistinguishable by name is the whole point).
const ORCH_NAME: &str = "route-iso";

/// Per-tab sentinel filenames. Uniquely named (rule 4) so the assertion survives
/// LLM phrasing/tool variance: only the literal filename has to come through.
const SENTINEL_A: &str = "route_alpha_5f3c.txt";
const SENTINEL_B: &str = "route_beta_9d21.txt";

/// Appended to both delegated tasks to close the conversational half of the
/// stall this test hit on a full-parallel e2e gate.
///
/// Tab B's reviewer explored its directory, read the orchestrator context and
/// the OTHER role's task file, and replied "What would you like me to do?
/// Should I 1. Act as the orchestrator and delegate the coder task 2. Wait for
/// a reviewer task to be defined 3. Something else" instead of doing the work.
/// The ROOT CAUSE was a product bug — `delegate` pointed the worker at a task
/// file it had failed to write, so the worker genuinely had nothing to act on
/// (fixed in `state::resolve_delegate_task_body`, which now inlines the body
/// instead). This clause is belt-and-braces for the half that is a model
/// choice, in the spirit of `bf517c4` for the pi worker.
///
/// It names the OBSERVED framings rather than forbidding questions in general:
/// `bf517c4` recorded that a vague clause ("do not offer to perform any action")
/// passed once and then failed with the same behaviour rephrased, so the wording
/// has to reject the specific moves. The last sentence covers the empty-file
/// case directly, so even a worker that somehow still reads an empty pointer
/// target does the work rather than asking about it.
const NO_STALL_CLAUSE: &str = " Do not ask what to do, offer numbered choices, \
     or wait for a task to be defined - this message IS the task and you have \
     everything you need. If a file you were pointed at looks empty or missing, \
     create the file described above anyway instead of asking about it.";

/// The single-line pointer `handle_delegate` writes into the TARGET worker's
/// PTY. Role-qualified, so "did this pane receive tab A's delegate?" and "…tab
/// B's?" are different needles.
const POINTER_CODER: &str = "worker-task-coder.md";
const POINTER_REVIEWER: &str = "worker-task-reviewer.md";

/// The feedback `handle_work_done` writes into the OWNING orchestrator's PTY.
/// Role-qualified for the same reason.
const FEEDBACK_CODER: &str = "Worker coder has completed their task";
const FEEDBACK_REVIEWER: &str = "Worker reviewer has completed their task";

/// Ctrl+PageDown / Ctrl+PageUp — the NON-configurable global tab-cycling chords.
/// On an orchestration tab a role pane holds keyboard focus and swallows plain
/// keys as text (the configurable `l`/`h` aliases end up typed into the agent's
/// input box), so tab navigation here must use the global chords.
const TAB_NEXT: &[u8] = b"\x1b[6;5~";
const TAB_PREV: &[u8] = b"\x1b[5;5~";

/// PATH for the spawned deck (→ daemon → agents) with the freshly-built
/// `dot-agent-deck` binary's dir prepended to the host PATH: the orchestrator
/// panes shell `dot-agent-deck delegate` and the worker panes shell
/// `dot-agent-deck work-done`, so the binary under test must resolve. The rest
/// of the host PATH is preserved so `claude` still resolves.
fn path_with_binary_dir() -> String {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let bindir = Path::new(bin)
        .parent()
        .expect("binary path has a parent dir");
    format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// One role pane of one orchestration tab, as the daemon reports it.
#[derive(Clone, Debug)]
struct RolePane {
    /// Daemon registry id — the handle `AttachRequest::Snapshot` takes.
    agent_id: String,
    /// `DOT_AGENT_DECK_PANE_ID` — the handle `AttachRequest::WriteAndSubmit`
    /// takes, and the key the daemon's orchestration maps are built on.
    pane_id: String,
}

/// One orchestration TAB, grouped by the PRD #140 per-tab instance token.
#[derive(Clone, Debug)]
struct OrchTab {
    orchestration_id: String,
    roles: HashMap<String, RolePane>,
}

impl OrchTab {
    fn role(&self, name: &str) -> &RolePane {
        self.roles.get(name).unwrap_or_else(|| {
            panic!(
                "orchestration tab {} has no `{name}` role pane (roles: {:?})",
                self.orchestration_id,
                self.roles.keys().collect::<Vec<_>>()
            )
        })
    }
}

/// Group the daemon's live orchestration panes by their per-tab
/// `orchestration_id`. Panes whose membership carries no token are skipped —
/// their absence is itself a failure of M1.2 and is asserted by the caller's
/// tab/role count expectations.
fn orchestration_tabs(socket: &Path) -> Vec<OrchTab> {
    let mut by_id: BTreeMap<String, HashMap<String, RolePane>> = BTreeMap::new();
    for record in common::agent_records_on(socket) {
        let Some(TabMembership::Orchestration {
            role_name,
            orchestration_id: Some(orchestration_id),
            ..
        }) = record.tab_membership.clone()
        else {
            continue;
        };
        let Some(pane_id) = record.pane_id_env.clone() else {
            continue;
        };
        by_id.entry(orchestration_id).or_default().insert(
            role_name,
            RolePane {
                agent_id: record.id.clone(),
                pane_id,
            },
        );
    }
    by_id
        .into_iter()
        .map(|(orchestration_id, roles)| OrchTab {
            orchestration_id,
            roles,
        })
        .collect()
}

/// Poll `ListAgents` until exactly `tabs` distinct orchestration tabs are live,
/// each with `roles_per_tab` role panes.
fn wait_for_orchestration_tabs(
    socket: &Path,
    tabs: usize,
    roles_per_tab: usize,
    timeout: Duration,
) -> Vec<OrchTab> {
    let complete = |found: &[OrchTab]| {
        found.len() == tabs && found.iter().all(|t| t.roles.len() == roles_per_tab)
    };
    common::wait_until(timeout, || complete(&orchestration_tabs(socket)));
    let found = orchestration_tabs(socket);
    assert!(
        complete(&found),
        "expected {tabs} orchestration tab(s) of {roles_per_tab} role panes within {timeout:?}, \
         got {found:#?}"
    );
    found
}

/// Hard-assert that `needle` has NEVER been written into `label`'s pane. Called
/// immediately after the matching positive assertion, so the two observations
/// are as close in time as the harness can make them.
fn assert_pane_never_saw(socket: &Path, pane: &RolePane, label: &str, needle: &str) {
    let key = common::search_key(needle);
    let hay = common::pane_search_key_on(socket, &pane.agent_id);
    assert!(
        !hay.contains(&key),
        "CROSS-DELIVERY: {label} (pane {}) received {needle:?}, which belongs to the OTHER \
         tab's orchestration. Two tabs of the same orchestration in one directory must be \
         separate routing groups (PRD #140).\n=== {label} pane (normalized, tail) ===\n{}",
        pane.pane_id,
        tail(&hay)
    );
}

/// Drive the production new-pane flow to open the fixture's single orchestration
/// against the deck's CURRENT directory. With no `[[modes]]` in the fixture the
/// Mode chip row is `[No mode] [Orch: route-iso] [schedule]`, so ONE Right
/// selects the orchestration; selecting an orchestration HIDES the Command
/// field, so the second Enter submits. Mirrors `e2e_session_save`'s
/// `open_orchestration`.
///
/// `expect_same_cwd_warning` asserts PRD #140 M4.0's non-blocking guard on the
/// SECOND open — the only difference between the two opens, and the surface a
/// user actually sees when they do this.
fn open_orchestration_tab(deck: &TuiDeck, expect_same_cwd_warning: bool) {
    deck.send_keys(b"\x0e"); // Ctrl+n → directory picker
    deck.send_keys(b" "); // Space → confirm the current dir → new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused
    deck.send_keys(b"\x1b[C"); // Right → [Orch: route-iso]
    deck.wait_for_string(ORCH_NAME);
    if expect_same_cwd_warning {
        deck.wait_until_grid("the same-cwd orchestration warning", |grid| {
            grid.contains("already runs an orchestration")
        });
        let grid = deck.snapshot_grid();
        assert!(
            grid.contains("worktree-prd"),
            "the same-cwd warning must point at the worktree alternative (PRD #140 M4.0).\n\
             Grid:\n{grid}"
        );
    }
    deck.send_keys(b"\r"); // Mode → Name
    deck.send_keys(b"\r"); // submit (Command is hidden for an orchestration)
}

/// Ask a real orchestrator pane to delegate `task` to `role`, by writing the
/// directive into its PTY through the daemon's production prompt-delivery RPC —
/// the same guarded write-and-submit the deck uses for a seed or orchestrator
/// prompt. What the orchestrator does with it (shell `dot-agent-deck delegate`)
/// is the agent's own doing.
fn ask_orchestrator_to_delegate(
    deck: &TuiDeck,
    orchestrator: &RolePane,
    session_id: &str,
    role: &str,
    task: &str,
) {
    let directive = format!(
        "Use the Bash tool to run exactly this one command, then stop and say nothing else: \
         dot-agent-deck delegate --to {role} --task \"{task}\""
    );
    let resp = common::write_and_submit_with_identity_on(
        deck.attach_socket_path(),
        &orchestrator.pane_id,
        &directive,
        &orchestrator.agent_id,
        Some(session_id),
    )
    .expect("WriteAndSubmit to the orchestrator pane over the attach socket");
    assert_eq!(
        resp.send_result,
        Some(SendResult::Applied),
        "the daemon refused to deliver the directive to orchestrator pane {}: error={:?}, \
         send_result={:?}",
        orchestrator.pane_id,
        resp.error,
        resp.send_result
    );
}

/// Scenario: Launch the real deck against a fixture whose `route-iso`
/// orchestration has three REAL interactive Claude Haiku roles, then open that
/// SAME orchestration TWICE in the SAME directory through the production
/// `Ctrl+N` flow, asserting the second open shows the non-blocking same-cwd
/// warning. Ask tab A's real orchestrator to delegate a uniquely-named sentinel
/// task to its `coder` and tab B's to its `reviewer`, concurrently. Assert each
/// delegate pointer, sentinel file and work-done feedback stayed inside the
/// delegating tab and never reached the other tab's identically-named panes.
#[spec("orchestration/route/001")]
#[test]
fn route_001_two_tabs_same_cwd_do_not_cross_deliver() {
    // Decision 26 runtime-skip: a missing CLI / credentials is an environmental
    // condition, not a broken test.
    skip_unless!(common::check_claude_available());

    let deck = TuiDeck::builder()
        // Six live agent panes across two tabs; a roomy deck keeps each role
        // card's PTY wide enough for a real claude TUI to render usefully.
        .with_pty_size(200, 55)
        .with_imported_claude_credentials()
        // Both orchestrators shell `dot-agent-deck delegate` and both workers
        // shell `dot-agent-deck work-done`, so the freshly-built binary's dir
        // must be on the PATH the deck → daemon → agents inherit (this override
        // wins over the harness's env scrub).
        .with_env("PATH", path_with_binary_dir())
        .launch_with_fixture("orchestration-route");
    deck.wait_for_string("No active sessions");

    let socket = deck.attach_socket_path().to_path_buf();
    let cwd = deck.workdir().to_path_buf();
    let events = deck.subscribe_events();

    // Pre-trust the orchestration cwd for the six interactive `claude` panes so
    // their first-run onboarding + per-folder trust dialogs never appear and the
    // daemon-injected prompts aren't swallowed answering them. Unlike
    // `with_claude_project_trust`, this cwd is the per-test TEMPDIR — it does not
    // exist until launch — so the seeding happens here instead, which is still
    // well before the first `claude` is spawned by the Ctrl+N below. Both the
    // literal and canonical forms are seeded because the deck picks its cwd up
    // from the process, not from this path value.
    let mut trust_paths = vec![cwd.to_string_lossy().into_owned()];
    if let Ok(canon) = cwd.canonicalize() {
        let canon = canon.to_string_lossy().into_owned();
        if !trust_paths.contains(&canon) {
            trust_paths.push(canon);
        }
    }
    common::seed_claude_trust_in_home(deck.home_dir(), &trust_paths)
        .expect("seed claude project trust into the per-test HOME");

    // ---- Tab A: the first orchestration in this directory (no warning yet) ----
    open_orchestration_tab(&deck, false);
    let first = wait_for_orchestration_tabs(&socket, 1, 3, Duration::from_secs(90));
    let tab_a_id = first[0].orchestration_id.clone();

    // ---- Tab B: the SAME orchestration, the SAME directory ----
    // Ctrl+N is a global chord, so it opens the new-pane flow straight from the
    // orchestration tab (where a role pane holds keyboard focus) — no need to
    // walk back to the Dashboard first.
    open_orchestration_tab(&deck, true);
    let tabs = wait_for_orchestration_tabs(&socket, 2, 3, Duration::from_secs(90));

    let tab_a = tabs
        .iter()
        .find(|t| t.orchestration_id == tab_a_id)
        .expect("the first tab's orchestration_id must survive the second open")
        .clone();
    let tab_b = tabs
        .iter()
        .find(|t| t.orchestration_id != tab_a_id)
        .expect("the second tab must mint its OWN orchestration_id (PRD #140 M1.2)")
        .clone();
    assert_ne!(
        tab_a.orchestration_id, tab_b.orchestration_id,
        "two tabs of the same orchestration in one directory must be two routing groups"
    );

    let orch_a = tab_a.role("orchestrator").clone();
    let coder_a = tab_a.role("coder").clone();
    let reviewer_a = tab_a.role("reviewer").clone();
    let orch_b = tab_b.role("orchestrator").clone();
    let coder_b = tab_b.role("coder").clone();
    let reviewer_b = tab_b.role("reviewer").clone();

    // Every pane must be input-ready before anything is injected into it: with
    // `clear = false` the daemon writes a delegate pointer straight into the
    // running child's PTY rather than respawning it and waiting for a fresh
    // `SessionStart`, so bytes injected mid-boot would be dropped.
    let all_agent_ids: Vec<String> = [
        &orch_a,
        &coder_a,
        &reviewer_a,
        &orch_b,
        &coder_b,
        &reviewer_b,
    ]
    .iter()
    .map(|p| p.agent_id.clone())
    .collect();
    if !common::wait_until_panes_settled(
        &socket,
        &all_agent_ids,
        Duration::from_millis(1500),
        Duration::from_secs(8),
        Duration::from_secs(180),
    ) {
        // Not fatal on its own — a still-busy agent may still accept the
        // injected prompt — but worth surfacing so a later failure is
        // diagnosable.
        eprintln!("warning: not every role pane settled within 180s; proceeding anyway");
    }
    let orch_a_session_id = events.wait_for_session_start_on_pane(
        &orch_a.pane_id,
        &orch_a.agent_id,
        Duration::from_secs(10),
    );
    let orch_b_session_id = events.wait_for_session_start_on_pane(
        &orch_b.pane_id,
        &orch_b.agent_id,
        Duration::from_secs(10),
    );

    // ===== Both tabs are given a task, CONCURRENTLY — the reported repro ======
    // Issue 140's report is "open the same orchestration in two tabs, start a
    // task in each": both chains run at the same time, which is also when the
    // pre-#140 `HashSet`-order work-done lookup was at its most non-deterministic.
    // Concurrency is safe here precisely because the two tabs are driven through
    // DIFFERENT roles, so their `.dot-agent-deck/*-{role}.md` coordination files
    // (which PRD #140 deliberately does not partition) never collide.
    //
    ask_orchestrator_to_delegate(
        &deck,
        &orch_a,
        &orch_a_session_id,
        "coder",
        &format!(
            "Create a file named {SENTINEL_A} in the current directory with the exact \
             contents ALPHA_OK. That is the entire task - do nothing else.{NO_STALL_CLAUSE}"
        ),
    );
    ask_orchestrator_to_delegate(
        &deck,
        &orch_b,
        &orch_b_session_id,
        "reviewer",
        &format!(
            "Create a file named {SENTINEL_B} in the current directory with the exact \
             contents BETA_OK. That is the entire task - do nothing else.{NO_STALL_CLAUSE}"
        ),
    );

    // ---- Direction 1: each delegate reached ONLY its own tab's worker ----
    // Watch tab A's chain first. Ctrl+PageUp is the NON-configurable global
    // prev-tab chord, so it switches tabs even though a role pane holds keyboard
    // focus — unlike the `h`/`l` aliases, which such a pane swallows as text.
    deck.send_keys(TAB_PREV);
    let delivered = common::wait_for_pane_text_on(
        &socket,
        &coder_a.agent_id,
        POINTER_CODER,
        Duration::from_secs(150),
    );
    assert!(
        delivered,
        "tab A's orchestrator delegated to `coder`, but the daemon never wrote the \
         {POINTER_CODER:?} pointer into tab A's coder pane ({}) within 150s. Either the \
         orchestrator never ran `dot-agent-deck delegate` or the delegate did not route.\n\
         === tab A orchestrator pane (normalized, tail) ===\n{}",
        coder_a.pane_id,
        tail(&common::pane_search_key_on(&socket, &orch_a.agent_id)),
    );
    // The routing decision, stated as an absence: the OTHER tab's identically
    // named `coder` pane must never have seen tab A's delegate.
    assert_pane_never_saw(&socket, &coder_b, "tab B's coder", POINTER_CODER);

    deck.send_keys(TAB_NEXT); // over to tab B for the second half of the cast
    let delivered = common::wait_for_pane_text_on(
        &socket,
        &reviewer_b.agent_id,
        POINTER_REVIEWER,
        Duration::from_secs(150),
    );
    assert!(
        delivered,
        "tab B's orchestrator delegated to `reviewer`, but the daemon never wrote the \
         {POINTER_REVIEWER:?} pointer into tab B's reviewer pane ({}) within 150s.\n\
         === tab B orchestrator pane (normalized, tail) ===\n{}",
        reviewer_b.pane_id,
        tail(&common::pane_search_key_on(&socket, &orch_b.agent_id)),
    );
    assert_pane_never_saw(&socket, &reviewer_a, "tab A's reviewer", POINTER_REVIEWER);

    // ---- Each worker really did ITS OWN task (rule 4's real-work evidence) ----
    assert!(
        common::wait_for_path(&cwd.join(SENTINEL_A), Duration::from_secs(150)),
        "tab A's coder never produced its sentinel {SENTINEL_A}.\n\
         === tab A coder pane (normalized, tail) ===\n{}",
        tail(&common::pane_search_key_on(&socket, &coder_a.agent_id)),
    );
    assert!(
        common::wait_for_path(&cwd.join(SENTINEL_B), Duration::from_secs(150)),
        "tab B's reviewer never produced its sentinel {SENTINEL_B}.\n\
         === tab B reviewer pane (normalized, tail) ===\n{}",
        tail(&common::pane_search_key_on(&socket, &reviewer_b.agent_id)),
    );
    assert!(
        common::wait_for_path(
            &cwd.join(".dot-agent-deck").join("work-done-coder.md"),
            Duration::from_secs(150)
        ),
        "tab A's coder never signalled work-done (no .dot-agent-deck/work-done-coder.md).\n\
         === tab A coder pane (normalized, tail) ===\n{}",
        tail(&common::pane_search_key_on(&socket, &coder_a.agent_id)),
    );
    assert!(
        common::wait_for_path(
            &cwd.join(".dot-agent-deck").join("work-done-reviewer.md"),
            Duration::from_secs(150)
        ),
        "tab B's reviewer never signalled work-done (no \
         .dot-agent-deck/work-done-reviewer.md).\n\
         === tab B reviewer pane (normalized, tail) ===\n{}",
        tail(&common::pane_search_key_on(&socket, &reviewer_b.agent_id)),
    );

    // ---- Direction 2: each work-done reached ONLY its own orchestrator ----
    let fed_back = common::wait_for_pane_text_on(
        &socket,
        &orch_a.agent_id,
        FEEDBACK_CODER,
        Duration::from_secs(120),
    );
    assert!(
        fed_back,
        "tab A's coder signalled work-done but its orchestrator pane ({}) never received \
         {FEEDBACK_CODER:?}.\n=== tab A orchestrator pane (normalized, tail) ===\n{}",
        orch_a.pane_id,
        tail(&common::pane_search_key_on(&socket, &orch_a.agent_id)),
    );
    let fed_back = common::wait_for_pane_text_on(
        &socket,
        &orch_b.agent_id,
        FEEDBACK_REVIEWER,
        Duration::from_secs(120),
    );
    assert!(
        fed_back,
        "tab B's reviewer signalled work-done but its orchestrator pane ({}) never received \
         {FEEDBACK_REVIEWER:?}.\n=== tab B orchestrator pane (normalized, tail) ===\n{}",
        orch_b.pane_id,
        tail(&common::pane_search_key_on(&socket, &orch_b.agent_id)),
    );

    // ---- Final sweep: nothing leaked in either direction over the whole run ----
    assert_pane_never_saw(&socket, &coder_b, "tab B's coder", POINTER_CODER);
    assert_pane_never_saw(&socket, &reviewer_a, "tab A's reviewer", POINTER_REVIEWER);
    assert_pane_never_saw(&socket, &orch_b, "tab B's orchestrator", FEEDBACK_CODER);
    assert_pane_never_saw(&socket, &orch_a, "tab A's orchestrator", FEEDBACK_REVIEWER);
}

/// Last ~2 000 characters of a normalized pane key — enough context for a
/// failure message without dumping a megabyte of scrollback.
fn tail(text: &str) -> &str {
    &text[text.len().saturating_sub(2000)..]
}
