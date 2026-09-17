#![cfg(all(feature = "e2e", unix))]

//! PRD #882 — L2 coverage for the PTY size policy with more than one client
//! attached to the same agent.
//!
//! A PTY has exactly one window size, so every client attached to an agent sees
//! the same grid. When no client has claimed focus, the daemon falls back to
//! the **smallest viewport on each axis among its attached viewers**, and larger
//! clients pad the remainder. These tests drive the real spawned binary through
//! a PTY and use the deck's own attach socket as the SECOND client to pin that
//! fallback and its release behavior.
//!
//! Unix-gated with the rest of the L2 tier: the second client attaches over a
//! Unix domain socket.
//!
//! No LLM tokens are spent — the pane runs `/bin/sh`, which lets the test ask
//! the agent-side PTY for its real kernel window size via `stty size`.

mod common;

use common::{TuiDeck, wait_until};
use dot_agent_deck::daemon_client::{AttachConnection, DaemonClient};
use spec::spec;
use std::path::Path;
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

/// Scenario: First detach an oversized real TUI and prove a desktop-shaped
/// 153x22 attach can give the shell that full size by itself. With no focus
/// claim, pair a narrow, tall TUI with that desktop attach and verify the shell
/// gets the per-axis minimum, while a zoom control pins today's same fallback.
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

    // Zoom control: another client holds this same shell at 40 columns. The
    // real TUI visibly enters zoom (`[Z]`), but its full-width resize request is
    // still arbitrated against that attached viewer before reaching the PTY.
    // Focus-driven sizing is expected to change this case: keyboard input will
    // make the zoomed TUI the last-focused client, so its viewer size should win.
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

        let after_zoom = agent_visible_stty_size(&runtime, &mut narrow_viewer, "DAD_AFTER_ZOOM=");
        assert_eq!(
            after_zoom.0, 22,
            "a visibly zoomed real TUI remains capped by the other viewer's 22 rows; \
             `stty size` reported {after_zoom:?}"
        );
        assert_eq!(
            after_zoom.1, 40,
            "a visibly zoomed real TUI remains capped by the other viewer's 40 columns; \
             `stty size` reported {after_zoom:?}"
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
