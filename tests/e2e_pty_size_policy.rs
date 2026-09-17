#![cfg(all(feature = "e2e", unix))]

//! PRD #882 / PRD #1105 — L2 coverage for the PTY size policy with more than
//! one client attached to the same agent.
//!
//! A PTY has exactly one window size, so every client attached to an agent sees
//! the same grid. The last client to claim focus decides it from its own
//! viewers (PRD #1105 M11). When no client has claimed focus, or the focused
//! client has no measured viewer of the agent, the daemon falls back to the
//! **smallest viewport on each axis among its attached viewers**, and larger
//! clients pad the remainder. These tests drive the real spawned binary through
//! a PTY and use the deck's own attach socket for the other clients, to pin the
//! focus rule, that fallback, and their release behavior.
//!
//! Unix-gated with the rest of the L2 tier: the second client attaches over a
//! Unix domain socket.
//!
//! No LLM tokens are spent — the pane runs `/bin/sh`, which lets the test ask
//! the agent-side PTY for its real kernel window size via `stty size`.

mod common;

use common::{TuiDeck, wait_until};
use dot_agent_deck::daemon_client::{
    AttachConnection, DaemonClient, FocusReport, StartAgentOptions, generate_client_id,
};
use spec::spec;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long to let the daemon settle on a new geometry.
///
/// Generous because the path is a real round trip through a Unix socket plus a
/// `TIOCSWINSZ`, and because the deck's own re-request rides its render loop.
const SETTLE: Duration = Duration::from_secs(10);

/// The exact pane geometry the desktop's enlarged overlay proposes in the
/// owner's reproduced browser layout.
const DESKTOP_OVERLAY: (u16, u16) = (22, 153);
/// The corresponding desktop Runs tile proposal before the user enlarges it.
const DESKTOP_TILE: (u16, u16) = (13, 46);
/// The narrow, tall viewer from the owner's TUI side of the reproduction.
const TUI_TILE: (u16, u16) = (42, 58);

/// The geometry the daemon currently has applied for the deck's first agent,
/// read the way any client would — off `AgentRecord` via `list_agents`, which
/// PRD #104 plumbed the PTY dims onto.
///
/// Deliberately asks the DAEMON rather than reading the deck's rendered grid:
/// the policy's subject is the agent's PTY size, and inferring it from painted
/// cells would confuse "the agent is 40 columns wide" with "the pane happens to
/// have 40 columns of content in it right now".
fn agent_pty_size(socket: &Path) -> Option<(u16, u16)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    let socket = socket.to_path_buf();
    runtime.block_on(async move {
        let client = DaemonClient::new(socket);
        let agents = client.list_agents().await.ok()?;
        let agent = agents.first()?;
        Some((agent.rows, agent.cols))
    })
}

/// Open another client against the deck's own daemon and hold it attached to
/// the first agent. A viewport-bearing connection is the deterministic stand-in
/// for the desktop terminal attach: it uses the same daemon client call and
/// carries the same `(rows, cols)` geometry as `terminal::resize`.
///
/// Returns the connection, which must be KEPT ALIVE by the caller: the viewer
/// constraint is released when the attach ends, so dropping it is what makes
/// the agent grow back. That is the property the second test asserts, and the
/// reason this returns the connection rather than swallowing it.
fn attach_client(
    socket: &Path,
    viewport: Option<(u16, u16)>,
) -> (tokio::runtime::Runtime, AttachConnection) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime for the second client");
    let socket = socket.to_path_buf();
    let conn = runtime.block_on(async move {
        let client = DaemonClient::new(socket);
        let agents = client.list_agents().await.expect("list agents");
        let agent = agents
            .first()
            .expect("the deck spawned an agent to attach to");
        client
            .attach_as_viewer(&agent.id, viewport)
            .await
            .expect("client attaches to the agent")
    });
    (runtime, conn)
}

/// Stand in for the desktop's measured tile-to-overlay journey: attach at the
/// 46x13 tile geometry, then send the real viewer-token resize request that the
/// desktop issues after its enlarged xterm proposes 153x22.
fn attach_enlarged_desktop(socket: &Path) -> (tokio::runtime::Runtime, AttachConnection) {
    let (runtime, conn) = attach_client(socket, Some(DESKTOP_TILE));
    let viewer = conn
        .viewer()
        .expect("a viewport-bearing desktop stand-in receives a viewer token")
        .to_string();
    let socket = socket.to_path_buf();
    let applied = runtime.block_on(async move {
        let client = DaemonClient::new(socket);
        let agents = client
            .list_agents()
            .await
            .expect("list agents before resize");
        let agent = agents
            .first()
            .expect("the deck spawned an agent for the desktop to resize");
        client
            .resize_agent_as_viewer(
                &agent.id,
                DESKTOP_OVERLAY.0,
                DESKTOP_OVERLAY.1,
                Some(&viewer),
            )
            .await
            .expect("desktop stand-in resizes its attached viewer to the overlay geometry")
    });
    assert!(
        applied.is_some(),
        "the current daemon must report the geometry it applied for the desktop resize"
    );
    (runtime, conn)
}

/// Ask the shell running inside the daemon-managed PTY what window size the
/// kernel has actually applied. The marker's numeric value appears only in the
/// command's output: the echoed input contains the literal `$(stty size)`.
fn agent_visible_stty_size(
    runtime: &tokio::runtime::Runtime,
    conn: &mut AttachConnection,
    marker: &str,
) -> (u16, u16) {
    let command = format!("printf '{marker}%s\\n' \"$(stty size)\"\n");
    runtime.block_on(async {
        conn.write_input(command.as_bytes())
            .await
            .expect("write stty probe into the agent PTY");

        let deadline = tokio::time::Instant::now() + SETTLE;
        let mut output = Vec::new();
        loop {
            let rendered = String::from_utf8_lossy(&output);
            for (start, _) in rendered.match_indices(marker) {
                let after_marker = &rendered[start + marker.len()..];
                let mut fields = after_marker.split_whitespace();
                if let (Some(rows), Some(cols)) = (fields.next(), fields.next())
                    && let (Ok(rows), Ok(cols)) = (rows.parse::<u16>(), cols.parse::<u16>())
                {
                    return (rows, cols);
                }
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let next = tokio::time::timeout(remaining, conn.next_output()).await;
            match next {
                Ok(Ok(Some(bytes))) => output.extend_from_slice(&bytes),
                Ok(Ok(None)) => panic!(
                    "agent attach ended before {marker} reported `stty size`; output: {:?}",
                    String::from_utf8_lossy(&output)
                ),
                Ok(Err(error)) => panic!(
                    "reading agent output for {marker} failed: {error}; output: {:?}",
                    String::from_utf8_lossy(&output)
                ),
                Err(_) => panic!(
                    "agent did not report {marker}<rows> <cols> within {SETTLE:?}; output: {:?}",
                    String::from_utf8_lossy(&output)
                ),
            }
        }
    })
}

/// Exit only the real TUI client through its user-facing Detach choice. The
/// daemon and shell stay alive, so another attached viewer can observe the PTY
/// after the TUI's geometry constraint is released.
fn detach_tui(deck: &mut TuiDeck) {
    deck.send_keys(b"\x04");
    deck.wait_for_absence("[Command Mode Ctrl+D]");
    deck.send_keys(b"\x03");
    deck.wait_for_string("Quit dot-agent-deck?");
    deck.send_keys(b"\r");
    let exited = deck.wait_for_exit_within(Duration::from_secs(30));
    assert_eq!(
        exited,
        Some(true),
        "the real TUI must exit cleanly through Detach while its daemon-managed shell survives; \
         exit result: {exited:?}"
    );
}

/// Start one shell through the real PTY-attached binary, then detach that
/// bootstrap TUI so the test's named stand-in clients are its only viewers.
fn detached_shell(name: &str) -> (TuiDeck, PathBuf, String) {
    let mut deck = TuiDeck::builder()
        .with_pty_size(260, 50)
        .with_continue_session(name, "/bin/sh")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let socket = deck.attach_socket_path().to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build a runtime to identify the detached shell");
    let agent_id = runtime.block_on(async {
        DaemonClient::new(socket.clone())
            .list_agents()
            .await
            .expect("list the bootstrap TUI's agent")
            .first()
            .expect("the bootstrap TUI spawned a shell")
            .id
            .clone()
    });
    detach_tui(&mut deck);
    (deck, socket, agent_id)
}

/// Build one logical focus-capable client. The deterministic tests run several
/// such clients in-process, so each handle gets the distinct process identity
/// its TUI or desktop stand-in would generate in production.
fn focus_client(socket: &Path) -> DaemonClient {
    DaemonClient::new(socket.to_path_buf()).with_client_id(generate_client_id())
}

fn attach_viewer(
    runtime: &tokio::runtime::Runtime,
    client: &DaemonClient,
    agent_id: &str,
    viewport: (u16, u16),
    stand_in: &str,
) -> AttachConnection {
    runtime
        .block_on(client.attach_as_viewer(agent_id, Some(viewport)))
        .unwrap_or_else(|error| panic!("{stand_in} attaches to the shell: {error}"))
}

fn resize_viewer(
    runtime: &tokio::runtime::Runtime,
    client: &DaemonClient,
    agent_id: &str,
    connection: &AttachConnection,
    viewport: (u16, u16),
    stand_in: &str,
) {
    let viewer = connection
        .viewer()
        .unwrap_or_else(|| panic!("{stand_in} receives a viewer token"));
    let applied = runtime
        .block_on(client.resize_agent_as_viewer(agent_id, viewport.0, viewport.1, Some(viewer)))
        .unwrap_or_else(|error| panic!("{stand_in} resizes its viewer: {error}"));
    assert!(
        applied.is_some(),
        "{stand_in} resize must report the geometry the daemon applied"
    );
}

fn claim_focus(runtime: &tokio::runtime::Runtime, client: &DaemonClient, stand_in: &str) {
    let report = runtime
        .block_on(client.focus_gained())
        .unwrap_or_else(|error| panic!("{stand_in} focus claim succeeds: {error}"));
    assert_eq!(
        report,
        FocusReport::Recorded,
        "{stand_in} uses the advertised focus-gained capability rather than withholding"
    );
}

/// Scenario: First detach an oversized real TUI and prove a desktop-shaped
/// 153x22 attach can give the shell that full size by itself. Then press `[Z]`
/// in a real TUI beside a 22x40 viewer and verify the shell grows to the zoomed
/// TUI's size, and with no focus claim pair a narrow, tall TUI with the desktop
/// attach and verify the shell gets the per-axis minimum.
#[spec("resize/policy/001")]
#[test]
fn policy_001_a_smaller_second_client_shrinks_the_agent_for_everyone() {
    // Control: the desktop-sized viewer genuinely stands alone. The oversized
    // TUI is used only to create the isolated daemon + shell, then exits through
    // the product's Detach path before the desktop stand-in attaches.
    {
        let mut deck = TuiDeck::builder()
            .with_pty_size(260, 50)
            .with_continue_session("wide-only-shell", "/bin/sh")
            .launch_with_fixture("minimal");
        deck.wait_for_string("[Command Mode Ctrl+D]");
        let socket = deck.attach_socket_path().to_path_buf();
        detach_tui(&mut deck);

        let (runtime, mut desktop) = attach_enlarged_desktop(&socket);
        let seen = agent_visible_stty_size(&runtime, &mut desktop, "DAD_WIDE_ONLY=");
        assert_eq!(
            seen.0, DESKTOP_OVERLAY.0,
            "wide-viewer-only control: the shell must see the desktop overlay's 22 rows; \
             `stty size` reported {seen:?}"
        );
        assert_eq!(
            seen.1, DESKTOP_OVERLAY.1,
            "wide-viewer-only control: the shell must see all 153 desktop-overlay columns; \
             `stty size` reported {seen:?}"
        );
    }

    // Zoom: another client holds this same shell at 22x40 until the real TUI
    // receives input. That client is a #882-era stand-in with no client id, so
    // it can never claim focus.
    //
    // PRD #1105 M11 changed this case on purpose. It used to assert that a
    // visibly zoomed TUI stays capped at 22x40. The real TUI now claims focus
    // when it receives keyboard input: the first key it gets here (the Ctrl+D
    // before `[Z]`) is claimed at once, and the claim is throttled rather than
    // repeated for the keys after it. With the TUI the last-focused client, its
    // zoomed viewer decides the size and the narrow viewer clips.
    //
    // This is the one end-to-end path from a real TUI terminal event to the
    // agent's PTY: `ui.rs` hands the key to the focus reporter, the reporter
    // sends `focus-gained` under the TUI's client id, the daemon re-applies
    // sizing, and `stty` inside the shell reports the result. Break any link and
    // the shell stays at 22x40.
    {
        let deck = TuiDeck::builder()
            .with_pty_size(160, 45)
            .with_continue_session("zoom-capped-shell", "/bin/sh")
            .launch_with_fixture("minimal");
        deck.wait_for_string("[Command Mode Ctrl+D]");
        let socket = deck.attach_socket_path().to_path_buf();
        let (runtime, mut narrow_viewer) = attach_client(&socket, Some((22, 40)));

        let before_zoom = agent_visible_stty_size(&runtime, &mut narrow_viewer, "DAD_BEFORE_ZOOM=");
        assert_eq!(
            before_zoom,
            (22, 40),
            "zoom control prerequisite: the second viewer must hold the shell at 22x40; \
             `stty size` reported {before_zoom:?}"
        );

        deck.send_keys(b"\x04");
        deck.wait_for_absence("[Command Mode Ctrl+D]");
        deck.send_keys(b"\x1a"); // Ctrl+Z: the repository's current default zoom binding.
        deck.wait_for_string("[Z]");

        // The claim and the zoom resize reach the daemon asynchronously, after
        // `[Z]` is drawn, so wait for the daemon to leave 22x40 before probing.
        // A TUI that never claims leaves it there, and the assertions below
        // then fail.
        wait_until(SETTLE, || {
            agent_pty_size(&socket).is_some_and(|dims| dims.0 > 22 && dims.1 > 40)
        });
        let after_zoom = agent_visible_stty_size(&runtime, &mut narrow_viewer, "DAD_AFTER_ZOOM=");
        assert!(
            after_zoom.0 > 22,
            "a zoomed real TUI holds focus from its own input, so the shell must grow past the \
             other viewer's 22 rows; `stty size` reported {after_zoom:?}"
        );
        assert!(
            after_zoom.1 > 40,
            "a zoomed real TUI holds focus from its own input, so the shell must grow past the \
             other viewer's 40 columns; `stty size` reported {after_zoom:?}"
        );

        // What it grew to is the zoomed TUI's own geometry, read without doing
        // the layout arithmetic here. Release the narrow viewer, so the TUI is
        // the only viewer left, and probe through an observer that registers no
        // viewport. Under focus the shell must not move: the size was already
        // the TUI's.
        drop(narrow_viewer);
        let (observer_runtime, mut observer) = attach_client(&socket, None);
        let tui_alone =
            agent_visible_stty_size(&observer_runtime, &mut observer, "DAD_ZOOM_ALONE=");
        assert_eq!(
            after_zoom, tui_alone,
            "with focus, the shell's size beside the narrow viewer must be exactly the zoomed \
             TUI's own size once the narrow viewer detaches"
        );
    }

    // Owner's configuration: this is a REAL TUI viewer in a narrow/tall outer
    // PTY, not a second attach pretending to be one. The extra viewport-bearing
    // attach is the desktop terminal, using its measured 153x22 proposal.
    let deck = TuiDeck::builder()
        .with_pty_size(90, 45)
        .with_continue_session("owner-shape-shell", "/bin/sh")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");
    let socket = deck.attach_socket_path().to_path_buf();

    // Measure the TUI tile before adding the desktop. This observer passes no
    // viewport, so it does not participate in the smallest-viewer policy.
    let (observer_runtime, mut observer) = attach_client(&socket, None);
    let tui_seen = agent_visible_stty_size(&observer_runtime, &mut observer, "DAD_TUI_ONLY=");
    drop(observer);
    drop(observer_runtime);
    assert!(
        tui_seen.0 > DESKTOP_OVERLAY.0,
        "owner-shape prerequisite: the TUI tile must be taller than the desktop overlay, \
         so only the desktop can select the 22-row axis; shell saw {tui_seen:?}"
    );
    assert!(
        tui_seen.1 < DESKTOP_OVERLAY.1,
        "owner-shape prerequisite: the TUI tile must be narrower than the desktop overlay, \
         so the 153-column assertion exercises the reported symptom; shell saw {tui_seen:?}"
    );

    let (runtime, mut desktop) = attach_enlarged_desktop(&socket);
    let seen = agent_visible_stty_size(&runtime, &mut desktop, "DAD_OWNER_BOTH=");
    // Holds only while this TUI claims no focus (no input, no harness `FocusGained`); a claim gives it the TUI's rows.
    assert_eq!(
        seen.0, DESKTOP_OVERLAY.0,
        "owner configuration: the shell must see the desktop overlay's 22 rows even while \
         the real TUI tile is taller; TUI-only was {tui_seen:?}, both viewers gave {seen:?}"
    );
    assert_eq!(
        seen.1, 58,
        "owner configuration without a focus claim: the shell must fall back to the real \
         TUI tile's 58 columns rather than the desktop overlay's 153 columns; TUI-only was \
         {tui_seen:?}, both viewers gave {seen:?}"
    );
}

/// Scenario: Attach a desktop-shaped 153x22 viewer beside the owner's real
/// narrow, tall TUI and confirm the shell is initially width-capped. Detach the
/// TUI through its user-facing quit dialog; the surviving desktop viewer must
/// then make the shell itself report 22 rows and all 153 columns via `stty`.
#[spec("resize/policy/002")]
#[test]
fn policy_002_releasing_the_small_client_grows_the_agent_back() {
    let mut deck = TuiDeck::builder()
        .with_pty_size(90, 45)
        .with_continue_session("release-owner-shape-shell", "/bin/sh")
        .launch_with_fixture("minimal");
    deck.wait_for_string("[Command Mode Ctrl+D]");

    let socket = deck.attach_socket_path().to_path_buf();
    let (runtime, mut desktop) = attach_enlarged_desktop(&socket);
    let constrained = agent_visible_stty_size(&runtime, &mut desktop, "DAD_BEFORE_TUI_DETACH=");
    assert!(
        constrained.1 < DESKTOP_OVERLAY.1,
        "test prerequisite: the real narrow TUI must initially cap the shell below the \
         desktop's 153 columns; `stty size` reported {constrained:?}"
    );

    // The TUI is the smaller viewer on the column axis. Detaching it leaves the
    // desktop attach alive and gives that viewer's 153-column constraint back.
    detach_tui(&mut deck);

    assert!(
        wait_until(SETTLE, || {
            agent_pty_size(&socket).is_some_and(|dims| dims == DESKTOP_OVERLAY)
        }),
        "releasing the real narrow TUI must let the desktop viewer take over at \
         {DESKTOP_OVERLAY:?}; daemon reports {:?}",
        agent_pty_size(&socket)
    );

    let released = agent_visible_stty_size(&runtime, &mut desktop, "DAD_AFTER_TUI_DETACH=");
    assert_eq!(
        released.0, DESKTOP_OVERLAY.0,
        "after the narrow TUI releases its constraint, the shell must still see the \
         desktop overlay's 22 rows; `stty size` reported {released:?}"
    );
    assert_eq!(
        released.1, DESKTOP_OVERLAY.1,
        "after the narrow TUI releases its constraint, the shell must grow to all 153 \
         desktop-overlay columns; `stty size` reported {released:?}"
    );
}

/// Scenario: Use identity-bearing TUI and desktop stand-ins for the owner's
/// 42x58 and resized 22x153 viewers, then have the desktop claim focus. The
/// shell itself must report the desktop's 22 rows and all 153 columns.
#[spec("resize/policy/004")]
#[test]
fn policy_004_the_focused_desktop_viewer_wins_on_each_axis() {
    let (_fixture, socket, agent_id) = detached_shell("focused-desktop-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the focused desktop policy case");
    let tui = focus_client(&socket);
    let desktop = focus_client(&socket);

    // These are deterministic client-library stand-ins for the owner's real
    // TUI tile and desktop terminal. Production focus reporting lands in M11
    // steps 3 and 4; this step isolates the daemon's step-2 policy.
    let _tui_view = attach_viewer(&runtime, &tui, &agent_id, TUI_TILE, "TUI stand-in");
    let mut desktop_view = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        DESKTOP_TILE,
        "desktop stand-in",
    );
    resize_viewer(
        &runtime,
        &desktop,
        &agent_id,
        &desktop_view,
        DESKTOP_OVERLAY,
        "desktop stand-in",
    );
    claim_focus(&runtime, &desktop, "desktop stand-in");

    let seen = agent_visible_stty_size(&runtime, &mut desktop_view, "DAD_DESKTOP_FOCUSED=");
    assert_eq!(
        seen.0, DESKTOP_OVERLAY.0,
        "focused desktop: the shell must see the desktop viewer's 22 rows; \
         `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, DESKTOP_OVERLAY.1,
        "focused desktop: the shell must see all 153 desktop columns instead of the \
         unfocused TUI's 58-column minimum; `stty size` reported {seen:?}"
    );
}

/// Scenario: Attach identity-bearing TUI and desktop stand-ins at the owner's
/// two sizes, let the desktop claim focus first, then let the TUI claim it back.
/// The shell itself must return to the TUI stand-in's complete 42x58 geometry.
#[spec("resize/policy/005")]
#[test]
fn policy_005_a_later_tui_focus_claim_switches_the_agent_back() {
    let (_fixture, socket, agent_id) = detached_shell("focus-switch-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the focus-switch policy case");
    let tui = focus_client(&socket);
    let desktop = focus_client(&socket);

    // The two client-library handles stand in for the independently focused
    // TUI and desktop processes; distinct generated ids make their order real.
    let mut tui_view = attach_viewer(&runtime, &tui, &agent_id, TUI_TILE, "TUI stand-in");
    let desktop_view = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        DESKTOP_TILE,
        "desktop stand-in",
    );
    resize_viewer(
        &runtime,
        &desktop,
        &agent_id,
        &desktop_view,
        DESKTOP_OVERLAY,
        "desktop stand-in",
    );
    claim_focus(&runtime, &desktop, "desktop stand-in");
    claim_focus(&runtime, &tui, "TUI stand-in switching back");

    let seen = agent_visible_stty_size(&runtime, &mut tui_view, "DAD_TUI_REFOCUSED=");
    assert_eq!(
        seen.0, TUI_TILE.0,
        "focus switched back to the TUI: the shell must regain its 42 rows instead of \
         retaining the desktop-selected 22-row minimum; `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, TUI_TILE.1,
        "focus switched back to the TUI: the shell must see its 58 columns; \
         `stty size` reported {seen:?}"
    );
}

/// Scenario: Focus an identity-bearing desktop stand-in, then resize the
/// unfocused TUI stand-in without sending another claim. The shell itself must
/// stay at the last-focused desktop's 22x153 geometry.
#[spec("resize/policy/006")]
#[test]
fn policy_006_an_unfocused_client_resize_does_not_steal_sizing() {
    let (_fixture, socket, agent_id) = detached_shell("sticky-focus-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the sticky-focus policy case");
    let tui = focus_client(&socket);
    let desktop = focus_client(&socket);

    // The TUI and desktop are client-library stand-ins. Resizing the TUI's
    // viewer represents its layout changing while the user remains elsewhere.
    let tui_view = attach_viewer(&runtime, &tui, &agent_id, TUI_TILE, "TUI stand-in");
    let mut desktop_view = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        DESKTOP_OVERLAY,
        "desktop stand-in",
    );
    claim_focus(&runtime, &desktop, "desktop stand-in");
    resize_viewer(
        &runtime,
        &tui,
        &agent_id,
        &tui_view,
        (10, 200),
        "unfocused TUI stand-in",
    );

    let seen = agent_visible_stty_size(&runtime, &mut desktop_view, "DAD_STICKY_FOCUS=");
    assert_eq!(
        seen.0, DESKTOP_OVERLAY.0,
        "an unfocused TUI resize must not replace the desktop's last-focused 22 rows; \
         `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, DESKTOP_OVERLAY.1,
        "an unfocused TUI resize must leave the desktop's last-focused 153 columns; \
         `stty size` reported {seen:?}"
    );
}

/// Scenario: Let a desktop stand-in claim focus while viewing only a second
/// shell; two other identity-bearing stand-ins continue viewing the first. The
/// first shell must use their per-axis minimum while the second follows focus.
#[spec("resize/policy/007")]
#[test]
fn policy_007_an_agent_the_focused_client_does_not_view_uses_the_fallback() {
    let (_fixture, socket, fallback_agent_id) = detached_shell("focus-missing-viewer-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the per-agent focus fallback case");

    // These two clients stand in for a TUI and desktop still viewing the first
    // agent. Neither claims focus, so that agent must retain #882's fallback.
    let first_tui = focus_client(&socket);
    let first_desktop = focus_client(&socket);
    let _first_tui_view = attach_viewer(
        &runtime,
        &first_tui,
        &fallback_agent_id,
        TUI_TILE,
        "first-agent TUI stand-in",
    );
    let mut first_desktop_view = attach_viewer(
        &runtime,
        &first_desktop,
        &fallback_agent_id,
        DESKTOP_OVERLAY,
        "first-agent desktop stand-in",
    );

    // The focused desktop stand-in views only this other agent. Its competing
    // narrow viewer represents another client and makes focus load-bearing.
    let focused_desktop = focus_client(&socket);
    let other_client = focus_client(&socket);
    let focused_agent_id = runtime
        .block_on(focused_desktop.start_agent(StartAgentOptions {
            command: Some("/bin/sh".into()),
            display_name: Some("focused-client-only-shell".into()),
            ..Default::default()
        }))
        .expect("start the shell viewed by the focused desktop stand-in");
    let mut focused_view = attach_viewer(
        &runtime,
        &focused_desktop,
        &focused_agent_id,
        (35, 120),
        "focused desktop stand-in",
    );
    let _other_view = attach_viewer(
        &runtime,
        &other_client,
        &focused_agent_id,
        (20, 60),
        "competing client stand-in",
    );
    claim_focus(
        &runtime,
        &focused_desktop,
        "desktop stand-in on the other agent",
    );

    let fallback_seen = agent_visible_stty_size(
        &runtime,
        &mut first_desktop_view,
        "DAD_FOCUS_NO_VIEWER_FALLBACK=",
    );
    assert_eq!(
        fallback_seen.0, DESKTOP_OVERLAY.0,
        "the focused client has no viewer of this shell, so fallback must choose 22 rows; \
         `stty size` reported {fallback_seen:?}"
    );
    assert_eq!(
        fallback_seen.1, TUI_TILE.1,
        "the focused client has no viewer of this shell, so fallback must choose 58 columns; \
         `stty size` reported {fallback_seen:?}"
    );

    let focused_seen =
        agent_visible_stty_size(&runtime, &mut focused_view, "DAD_OTHER_AGENT_FOCUSED=");
    assert_eq!(
        focused_seen.0, 35,
        "the control agent that the focused desktop does view must take its 35 rows instead \
         of the competing viewer's 20-row minimum; `stty size` reported {focused_seen:?}"
    );
    assert_eq!(
        focused_seen.1, 120,
        "the control agent that the focused desktop does view must take its 120 columns; \
         `stty size` reported {focused_seen:?}"
    );
}

/// Scenario: Attach a #882-era client stand-in with no client id beside an
/// identity-bearing desktop stand-in, then have the desktop claim focus. The
/// shell observed by the older client must grow beyond its reported width.
#[spec("resize/policy/008")]
#[test]
fn policy_008_a_focused_client_overrides_an_older_identityless_viewer() {
    let (_fixture, socket, agent_id) = detached_shell("legacy-override-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the legacy-client policy case");

    // This handle deliberately omits with_client_id: it stands in for an older
    // #882-era TUI. The desktop stand-in uses the current identity and claim API.
    let legacy = DaemonClient::new(socket.clone());
    let desktop = focus_client(&socket);
    let mut legacy_view = attach_viewer(
        &runtime,
        &legacy,
        &agent_id,
        TUI_TILE,
        "older identity-less TUI stand-in",
    );
    let desktop_view = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        DESKTOP_TILE,
        "focus-capable desktop stand-in",
    );
    resize_viewer(
        &runtime,
        &desktop,
        &agent_id,
        &desktop_view,
        DESKTOP_OVERLAY,
        "focus-capable desktop stand-in",
    );
    claim_focus(&runtime, &desktop, "focus-capable desktop stand-in");

    let seen = agent_visible_stty_size(&runtime, &mut legacy_view, "DAD_LEGACY_OVERRIDDEN=");
    assert_eq!(
        seen.0, DESKTOP_OVERLAY.0,
        "the older client must receive the focused desktop's 22-row grid; \
         `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, DESKTOP_OVERLAY.1,
        "the older client's shell view must receive 153 columns — larger than the 58 it \
         reported — while the desktop holds focus; `stty size` reported {seen:?}"
    );
}

/// Scenario: Use a TUI stand-in's viewer token to resize from a small tile to
/// a large zoomed pane after that client claims focus, beside a desktop
/// stand-in. The shell itself must follow the focused TUI's zoom geometry.
#[spec("resize/policy/009")]
#[test]
fn policy_009_a_focused_tui_viewer_can_zoom_past_another_client() {
    let (_fixture, socket, agent_id) = detached_shell("focused-zoom-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the focused zoom policy case");
    let tui = focus_client(&socket);
    let desktop = focus_client(&socket);

    // The TUI stand-in models `[Z]` by resizing the same viewer token from its
    // tile to its zoomed geometry. The desktop stand-in remains attached at a
    // smaller geometry so the old minimum policy cannot accidentally pass.
    let mut tui_view = attach_viewer(&runtime, &tui, &agent_id, (22, 40), "zooming TUI stand-in");
    let _desktop_view = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        (30, 80),
        "desktop stand-in beside the zoomed TUI",
    );
    claim_focus(&runtime, &tui, "zooming TUI stand-in");
    resize_viewer(
        &runtime,
        &tui,
        &agent_id,
        &tui_view,
        (40, 140),
        "zooming TUI stand-in",
    );

    let seen = agent_visible_stty_size(&runtime, &mut tui_view, "DAD_FOCUSED_TUI_ZOOM=");
    assert_eq!(
        seen.0, 40,
        "focused TUI zoom: the shell must grow to the TUI viewer's 40 rows instead of the \
         desktop's 30-row minimum; `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, 140,
        "focused TUI zoom: the shell must grow to the TUI viewer's 140 columns; \
         `stty size` reported {seen:?}"
    );
}

/// Scenario: Give one identity-bearing desktop stand-in two viewers of the same
/// shell, one short and wide and one tall and narrow, beside a smaller viewer
/// from another client, then let the desktop claim focus. The shell itself must
/// report the desktop viewers' smallest rows and smallest columns.
#[spec("resize/policy/010")]
#[test]
fn policy_010_a_focused_client_with_two_viewers_takes_their_per_axis_minimum() {
    let (_fixture, socket, agent_id) = detached_shell("two-focused-viewers-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the several-focused-viewers policy case");
    let desktop = focus_client(&socket);
    let other = focus_client(&socket);

    // One client showing the same agent twice, as a TUI with the agent in two
    // panes would. Both views are in front of whoever focused that client, so
    // each must be able to draw the agent whole. The other client's viewer is
    // smaller on both axes, so a result that still reads it cannot pass.
    let mut short_wide = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        (30, 200),
        "focused client's short, wide view",
    );
    let _tall_narrow = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        (40, 120),
        "focused client's tall, narrow view",
    );
    let _other_view = attach_viewer(
        &runtime,
        &other,
        &agent_id,
        (10, 60),
        "unfocused client's smaller view",
    );
    claim_focus(&runtime, &desktop, "client with two views");

    let seen = agent_visible_stty_size(&runtime, &mut short_wide, "DAD_TWO_FOCUSED_VIEWS=");
    assert_eq!(
        seen.0, 30,
        "a focused client with two views: the shell must take the shorter view's 30 rows, \
         not the taller view's 40 or the unfocused client's 10; `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, 120,
        "a focused client with two views: the shell must take the narrower view's 120 columns, \
         not the wider view's 200 or the unfocused client's 60; `stty size` reported {seen:?}"
    );
}

/// Scenario: Let an identity-bearing desktop stand-in claim focus beside a TUI
/// stand-in and an older client with no id, then end the desktop's attach. The
/// shell itself must fall back to the two remaining viewers' per-axis minimum.
#[spec("resize/policy/011")]
#[test]
fn policy_011_the_focused_clients_viewer_detaching_falls_back() {
    let (_fixture, socket, agent_id) = detached_shell("focused-viewer-detach-shell");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime for the focused-viewer-detach policy case");
    let tui = focus_client(&socket);
    let legacy = DaemonClient::new(socket.clone());
    let desktop = focus_client(&socket);

    // Two viewers stay behind, and neither alone is the fallback: the TUI
    // stand-in supplies the 58 columns and the older client the 30 rows.
    let mut tui_view = attach_viewer(&runtime, &tui, &agent_id, TUI_TILE, "TUI stand-in");
    let _legacy_view = attach_viewer(
        &runtime,
        &legacy,
        &agent_id,
        (30, 100),
        "older identity-less stand-in",
    );
    let desktop_view = attach_viewer(
        &runtime,
        &desktop,
        &agent_id,
        DESKTOP_OVERLAY,
        "focused desktop stand-in",
    );
    claim_focus(&runtime, &desktop, "desktop stand-in");

    let focused = agent_visible_stty_size(&runtime, &mut tui_view, "DAD_BEFORE_FOCUSED_DETACH=");
    assert_eq!(
        focused, DESKTOP_OVERLAY,
        "test prerequisite: the focused desktop's viewer decides the shell's size; \
         `stty size` reported {focused:?}"
    );

    // Ending the attach releases the viewer on the daemon side. Focus itself
    // stays with the desktop, which now has no viewer of this shell.
    drop(desktop_view);
    let fallback = (30, TUI_TILE.1);
    wait_until(SETTLE, || {
        agent_pty_size(&socket).is_some_and(|dims| dims == fallback)
    });

    let seen = agent_visible_stty_size(&runtime, &mut tui_view, "DAD_AFTER_FOCUSED_DETACH=");
    assert_eq!(
        seen.0, 30,
        "after the focused desktop's viewer detaches, the shell must fall back to the older \
         client's 30 rows; `stty size` reported {seen:?}"
    );
    assert_eq!(
        seen.1, TUI_TILE.1,
        "after the focused desktop's viewer detaches, the shell must fall back to the TUI \
         stand-in's 58 columns; `stty size` reported {seen:?}"
    );
}
