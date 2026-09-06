#![cfg(feature = "e2e")]

//! Issue #868: PTY-attached e2e coverage for what M3's daemon-hosted `pane
//! spawn <role>` verb does to an ALREADY-ATTACHED TUI.
//!
//! M3 (`tests/pane_spawn.rs`) added the daemon verb itself and proved it
//! end-to-end at the handler level: a role declared in
//! `.dot-agent-deck.toml` but never spawned into a running orchestration
//! instance can be spawned on demand, and afterward is reachable via
//! `delegate_targets`. What M3 deliberately leaves unhandled — see
//! `handle_spawn_role_with_state`'s own doc comment in `src/state.rs`
//! ("a live TUI merging this into an already-open orchestration tab is
//! issue #868's job, not this one's") — is whether an ALREADY-ATTACHED TUI
//! correctly absorbs the resulting `BroadcastMsg::OrchestrationSurface`
//! broadcast into the orchestration tab it already has open, rather than
//! building a second, confusing tab for the same orchestration. That is
//! fundamentally a rendered-UI behavior no handler-level test can exercise —
//! exactly why this file exists as a genuine L2, PTY-attached test, unlike
//! M1-M3's handler-level suites.
//!
//! The bug, precisely: `surface_one_orchestration`'s idempotency guard
//! (`src/ui.rs`) only checks whether the pane ids IN THIS SURFACE already
//! belong to some tab. A `pane spawn` broadcast carries only the ONE
//! newly-spawned role's (novel) pane id, so the guard is always `false` for
//! it, and without `TabManager::orchestration_tab_index_for`'s growth check
//! the function would fall straight through to
//! `open_orchestration_tab_with_existing_role_panes`, which unconditionally
//! pushes a brand-new `Tab::Orchestration` — producing a SECOND tab for the
//! same orchestration instead of the new role's card joining the tab already
//! open for it.
//!
//! `pane/spawn/005` reuses the `pane/spawn` catalog area M3 established
//! (`tests/pane_spawn.rs`, ids `001`-`004`/`007`/`008`/`011`, all
//! L1/handler-level) rather than opening a new one — this is the SAME `pane
//! spawn` verb's TUI-visible consequence, not a different feature.
//!
//! `pane/drift/001` pins the companion M5 question: does the session
//! snapshot writer read a grown tab's role list live, or from a copy frozen
//! at tab-open time?

mod common;

use std::time::Duration;

use common::{TuiDeck, agent_records_on, role_pane_border_title, wait_for_file_substr_count};
use dot_agent_deck::agent_pty::TabMembership;
use spec::spec;

/// Drive the new-pane dialog to open the (single) orchestration in the
/// `pane-spawn-live` fixture. With no `[[modes]]` defined the Mode chip row
/// is `[No mode] [Orch: spawn-orch] [schedule]`, so ONE Right selects the
/// orchestration; selecting an orchestration HIDES the Command field, so a
/// second Enter submits the form. Mirrors `e2e_dashboard_selection.rs`'s own
/// `open_orchestration` helper.
fn open_orchestration(deck: &TuiDeck) {
    deck.send_keys(b"\x0e"); // Ctrl+n -> directory picker
    deck.send_keys(b" "); // Space -> confirm current dir -> new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_keys(b"\x1b[C"); // Right -> [Orch: spawn-orch]
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // submit (Command hidden for an orchestration)
}

/// Drive the new-pane dialog to create a plain (non-orchestration) dashboard
/// pane running `sleep 600`, exactly like `e2e_session_save.rs`'s own
/// `spawn_plain_pane` helper. Used here purely as a session-dirtying
/// trigger: `surface_one_orchestration`'s M4 growth branch calls
/// `ui.mark_session_dirty()` only conditionally, on the branch where the
/// start role's `pane_metadata` snapshot already exists — so growing an
/// orchestration tab alone is not a reliable way to flush a fresh
/// `session.toml`, and a plain new pane is a proven, independent way to
/// force the next coalesced write and observe what it actually contains.
fn spawn_plain_pane(deck: &TuiDeck, command: &str) {
    deck.send_keys(b"\x04"); // Ctrl+D toggles between PaneInput and Normal
    deck.send_keys(b"\x0e"); // Ctrl+N -> directory picker
    deck.wait_for_string("Select Directory");
    deck.send_keys(b" "); // Space -> confirm current dir -> new-pane form
    deck.wait_for_string("No mode"); // form up, Mode field focused at "No mode"
    deck.send_keys(b"\r"); // Mode -> Name
    deck.send_keys(b"\r"); // Name (default) -> Command
    deck.send_keys(command.as_bytes());
    deck.send_keys(b"\r"); // submit
    deck.wait_for_string("[Command Mode Ctrl+D]"); // pane spawned & auto-focused
}

/// The orchestrator role's full daemon registry record — read back rather
/// than reconstructed by hand, so it is exactly what `handle_spawn_role_with_state`
/// and `surface_one_orchestration` will themselves resolve.
fn orchestrator_pane_id_and_cwd(deck: &TuiDeck) -> (String, String) {
    let records = agent_records_on(deck.attach_socket_path());
    let orchestrator_record = records
        .iter()
        .find(|r| {
            matches!(
                &r.tab_membership,
                Some(TabMembership::Orchestration { role_name, is_start_role: true, .. })
                    if role_name == "orchestrator"
            )
        })
        .expect("the orchestrator role must be registered with its tab membership");
    let pane_id = orchestrator_record
        .pane_id_env
        .clone()
        .expect("the orchestrator role must carry a DOT_AGENT_DECK_PANE_ID");
    let cwd = orchestrator_record
        .cwd
        .clone()
        .expect("the orchestrator role must carry its cwd");
    (pane_id, cwd)
}

/// Overwrite the running orchestration's own `.dot-agent-deck.toml` (at
/// `cwd`) to add a THIRD role, `reviewer`, that was never part of the config
/// the tab was opened from — the "operator added a role mid-session"
/// scenario `tests/pane_spawn.rs` (M3) already exercises at the handler
/// level.
fn add_reviewer_role(cwd: &str) {
    let config_path = std::path::Path::new(cwd).join(".dot-agent-deck.toml");
    std::fs::write(
        &config_path,
        "[[orchestrations]]\n\
         name = \"spawn-orch\"\n\
         \n\
         [[orchestrations.roles]]\n\
         name = \"orchestrator\"\n\
         command = \"cat\"\n\
         start = true\n\
         \n\
         [[orchestrations.roles]]\n\
         name = \"coder\"\n\
         command = \"cat\"\n\
         \n\
         [[orchestrations.roles]]\n\
         name = \"reviewer\"\n\
         command = \"cat\"\n",
    )
    .expect("add the reviewer role to the running orchestration's own config");
}

/// Invoke the REAL `pane spawn reviewer` CLI subcommand exactly as a real
/// orchestrator agent would from inside its own pane (`src/main.rs`'s
/// `PaneCmd::Spawn` arm): a plain subprocess carrying only
/// `DOT_AGENT_DECK_PANE_ID` and the hook socket path over the same
/// unversioned `DaemonMessage` channel `delegate`/`pane restart` already
/// use. The daemon resolves the caller from those two alone — never from
/// this subprocess's own filesystem cwd, which stays the test's tempdir.
fn run_pane_spawn_reviewer(deck: &TuiDeck, orchestrator_pane_id: &str) {
    let bin = env!("CARGO_BIN_EXE_dot-agent-deck");
    let output = std::process::Command::new(bin)
        .args(["pane", "spawn", "reviewer"])
        .env("DOT_AGENT_DECK_PANE_ID", orchestrator_pane_id)
        .env("DOT_AGENT_DECK_SOCKET", deck.hook_socket_path())
        .output()
        .expect("run `dot-agent-deck pane spawn reviewer`");
    assert!(
        output.status.success(),
        "`pane spawn reviewer` must succeed at the daemon level (M3's already-GREEN \
         verb, not this test's own pin) -- stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Scenario: Open a real Orchestration tab (`pane-spawn-live` fixture: one
/// orchestration, roles `orchestrator` [start] + `coder`, both spawned the
/// instant the tab opens) and confirm both role cards are visible together.
/// Then mutate the RUNNING orchestration's own `.dot-agent-deck.toml` (read
/// back from the daemon's registry) to add a THIRD role, `reviewer`, that
/// was never part of the config this tab was opened from. Invoke the REAL
/// `dot-agent-deck pane spawn reviewer` CLI subcommand and assert reviewer's
/// card joins the SAME orchestration tab that is still active (this test
/// never switches tabs) and that the tab bar still shows exactly one
/// Dashboard tab + one orchestration tab — not two orchestration tabs for
/// the same orchestration.
#[spec("pane/spawn/005")]
#[test]
fn spawn_005_pane_spawn_joins_the_already_open_orchestration_tab() {
    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .launch_with_fixture("pane-spawn-live");
    deck.wait_for_string("No active sessions");

    open_orchestration(&deck);
    deck.wait_for_absence("New Agent"); // form closed -> tab up, orchestrator focused
    deck.wait_for_string("[Command Mode Ctrl+D]"); // live PTY, PaneInput mode, orchestrator focused

    // Precondition: both configured roles spawned into ONE tab at open time.
    assert!(
        deck.wait_for_grid_string_within("coder", Duration::from_secs(10)),
        "the coder role card must be visible on the freshly-opened orchestration \
         tab before this test touches anything else.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Precondition: exactly Dashboard + this ONE orchestration tab. Tab
    // labels are joined by a bare `│` divider with no other occurrence on the
    // one-row tab-bar strip (`render_tab_strip`, `src/ui.rs`), so counting
    // dividers on that row is an exact tab count, independent of what either
    // tab happens to be titled.
    let tab_bar_row_before = deck
        .snapshot_grid()
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let tabs_before = tab_bar_row_before.matches('│').count() + 1;
    assert_eq!(
        tabs_before, 2,
        "precondition: exactly one Dashboard tab + one orchestration tab must be \
         open before the spawn.\nTab bar row: {tab_bar_row_before:?}"
    );

    let (orchestrator_pane_id, orchestration_cwd) = orchestrator_pane_id_and_cwd(&deck);
    add_reviewer_role(&orchestration_cwd);
    run_pane_spawn_reviewer(&deck, &orchestrator_pane_id);

    // The pin, part 1: reviewer's card must join the SAME orchestration tab
    // that is still active -- this test never sends a tab-switch chord, so if
    // reviewer is visible at all it can only be on the tab already open.
    assert!(
        deck.wait_for_grid_string_within("reviewer", Duration::from_secs(10)),
        "spawning `reviewer` into an orchestration that already has an ATTACHED \
         tab open (orchestrator + coder) must make reviewer's card join that SAME \
         tab -- it never appeared on screen at all, meaning it was built into a \
         tab this test never switched to.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // The pin, part 2: still only ONE orchestration tab (+ Dashboard) exists
    // -- the spawn must not have built a SECOND tab for the same
    // orchestration alongside the one already open.
    let grid_after = deck.snapshot_grid();
    let tab_bar_row_after = grid_after.lines().next().unwrap_or_default();
    let tabs_after = tab_bar_row_after.matches('│').count() + 1;
    assert_eq!(
        tabs_after, 2,
        "spawning a configured-but-unspawned role into an ALREADY-OPEN \
         orchestration tab must not create a second tab for the same \
         orchestration -- expected the tab bar to still show exactly 2 tabs \
         (Dashboard + the one orchestration tab), got {tabs_after} from row \
         {tab_bar_row_after:?}.\nFull grid:\n{grid_after}"
    );

    // And reviewer's card specifically lives inside the role-pane box
    // structure of that tab (its own bordered box titled "reviewer"), not
    // merely as stray text somewhere on screen.
    assert_eq!(
        role_pane_border_title(&grid_after, "reviewer").as_deref(),
        Some("reviewer"),
        "reviewer's role pane must render as its own bordered box titled \
         'reviewer' inside the currently-visible orchestration tab.\nGrid:\n{grid_after}"
    );
}

/// Scenario: Open a real orchestration tab (`pane-spawn-live` fixture:
/// `orchestrator` [start] + `coder`) with `DOT_AGENT_DECK_SESSION` redirected
/// to a test-owned path, confirm the leading-edge snapshot write already
/// captures both roles in `[panes.orchestration]`, then grow the SAME
/// already-open tab with a third role (`reviewer`) via the real `pane spawn`
/// CLI exactly as `pane/spawn/005` does. Force one more snapshot flush
/// (spawning an unrelated plain dashboard pane, since the growth branch
/// itself never marks the session dirty) and assert the re-flushed
/// `[panes.orchestration]` block's role list includes `reviewer`. This is
/// the issue #868 save/restore config-drift question: does the snapshot
/// writer read the role list from the tab's own live, M4-grown
/// `config.roles`, or from a stale copy captured once at tab-open time? The
/// only place `ui.pane_metadata`'s `OrchestrationSnapshot.roles` is written
/// is `open_orchestration_tab`'s one-time capture at tab-open —
/// `surface_one_orchestration`'s M4 growth branch
/// (`add_role_to_existing_orchestration`) extends the live
/// `Tab::Orchestration` but must ALSO push onto that snapshot, or every
/// later snapshot flush keeps re-serializing the ORIGINAL two-role list. A
/// restored session would then see `resolve_orchestration_for_restore`'s
/// drift guard false-positive (`current_roles` re-read from the now-3-role
/// `.dot-agent-deck.toml` vs. `saved_roles` frozen at 2) and fall back to a
/// plain pane, discarding the whole orchestration tab reconstruction even
/// though nothing on disk ever actually diverged.
#[spec("pane/drift/001")]
#[test]
fn drift_001_role_grown_via_pane_spawn_survives_session_capture() {
    let session_dir = common::race_safe_tempdir();
    let session_file = session_dir.path().join("session.toml");

    let deck = TuiDeck::builder()
        .with_pty_size(120, 40)
        .with_env(
            "DOT_AGENT_DECK_SESSION",
            session_file.to_str().expect("session path is UTF-8"),
        )
        .launch_with_fixture("pane-spawn-live");
    deck.wait_for_string("No active sessions");

    open_orchestration(&deck);
    deck.wait_for_absence("New Agent");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    assert!(
        deck.wait_for_grid_string_within("coder", Duration::from_secs(10)),
        "precondition: both configured roles must be visible before this test \
         touches anything else.\nGrid:\n{}",
        deck.snapshot_grid()
    );

    // Precondition: the leading-edge snapshot write (triggered by
    // `open_orchestration_tab`'s own `mark_session_dirty`) has already
    // captured the two roles the tab opened with.
    assert!(
        wait_for_file_substr_count(
            &session_file,
            "[panes.orchestration]",
            1,
            Duration::from_secs(10)
        ),
        "opening the orchestration tab must flush a snapshot carrying \
         [panes.orchestration] before this test grows the tab.\nFile contents: {:?}",
        std::fs::read_to_string(&session_file).ok()
    );
    let toml_before = std::fs::read_to_string(&session_file).unwrap_or_default();
    assert!(
        toml_before.contains("orchestrator") && toml_before.contains("coder"),
        "precondition: the initial capture must list both fixture roles.\n\
         File contents:\n{toml_before}"
    );

    // Grow the already-open tab with `reviewer`, mirroring `pane/spawn/005`.
    let (orchestrator_pane_id, orchestration_cwd) = orchestrator_pane_id_and_cwd(&deck);
    add_reviewer_role(&orchestration_cwd);
    run_pane_spawn_reviewer(&deck, &orchestrator_pane_id);

    assert!(
        deck.wait_for_grid_string_within("reviewer", Duration::from_secs(10)),
        "setup precondition: reviewer's card must join the already-open tab \
         (pane/spawn/005's own pin) before this test checks session capture.\n\
         Grid:\n{}",
        deck.snapshot_grid()
    );

    // Force one more coalesced flush: the growth branch itself never calls
    // `ui.mark_session_dirty()` on the "nothing grew" path, so nothing
    // re-serializes `ui.pane_metadata` on its own after the spawn above.
    spawn_plain_pane(&deck, "sleep 600");

    // The pin: the re-flushed orchestration snapshot must include `reviewer`,
    // proving the snapshot writer reads the tab's live (M4-grown) role list
    // rather than a copy frozen at tab-open time.
    let captured =
        wait_for_file_substr_count(&session_file, "reviewer", 1, Duration::from_secs(10));
    let toml_after = std::fs::read_to_string(&session_file).unwrap_or_default();
    assert!(
        captured && toml_after.contains("reviewer"),
        "a role added to an already-open orchestration tab via `pane spawn` \
         (issue #868) must be reflected in the NEXT session snapshot flush \
         -- the captured [panes.orchestration] role list must include \
         'reviewer', not just the two roles the tab opened with. A restored \
         session built from a snapshot missing the grown role would false-\
         positive `resolve_orchestration_for_restore`'s drift guard (saved \
         roles != the now-3-role .dot-agent-deck.toml) and fall back to a \
         plain pane.\nFile contents:\n{toml_after}"
    );
}
