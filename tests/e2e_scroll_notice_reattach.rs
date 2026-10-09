#![cfg(all(feature = "e2e", unix))]

//! PTY-attached L2 coverage for the cannot-scroll notice across a re-attach.
//!
//! The deck's own copy of a pane is rebuilt from what the daemon replays when a
//! client attaches. A full-screen agent enters the alternate screen ONCE, at
//! start-up, and never says so again — so these tests drive a real external
//! daemon, a first deck that sees the agent start, and a second deck that only
//! ever sees the replay.

mod common;

use std::path::Path;
use std::time::Duration;

use common::TuiDeck;
use spec::spec;

/// What the cannot-scroll notice begins with in every render tier.
const NOTICE: &str = "Nothing to scroll";
/// The stand-in's first painted row carries this prefix and its frame number.
const FRAME_PREFIX: &str = "FS_STANDIN_FRAME=";

/// A stand-in for an agent running a full-screen TUI — modelled on what
/// `claude` with `"tui": "fullscreen"` was measured to emit: `ESC[?1049h` and
/// mouse tracking once at start-up, then whole-screen repaints in place by
/// cursor address. On SIGWINCH it re-sends mouse tracking, clears and repaints,
/// and does NOT re-send `ESC[?1049h` — exactly the measured claude response.
const FULLSCREEN_STANDIN: &str = r#"
paint() {
  read -r rows cols < <(stty size)
  frame=$((frame + 1))
  printf '\033[1;1H%-*.*s' "$cols" "$cols" "FS_STANDIN_FRAME=$frame"
  for ((r = 2; r <= rows; r++)); do
    printf '\033[%d;1H%-*.*s' "$r" "$cols" "$cols" "fullscreen stand-in row $r frame $frame"
  done
}
frame=0
printf '\033[?1049h\033[?1000h\033[?1002h\033[?1003h\033[?1006h'
trap 'printf "\033[?1000h\033[?1002h\033[?1003h\033[?1006h\033[2J"; paint' WINCH
while :; do paint; sleep 0.1; done
"#;

fn write_standin(dir: &Path) -> String {
    let path = dir.join("fullscreen-standin.sh");
    std::fs::write(&path, FULLSCREEN_STANDIN).expect("write full-screen stand-in");
    format!("bash {}", path.display())
}

/// The highest frame number the stand-in has painted onto the visible grid.
fn visible_frame(grid: &str) -> Option<u64> {
    grid.lines()
        .filter_map(|line| {
            let at = line.find(FRAME_PREFIX)? + FRAME_PREFIX.len();
            let digits: String = line[at..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            digits.parse().ok()
        })
        .max()
}

/// Wait until the stand-in has painted `frames` more whole screens onto
/// `deck`'s grid than it had when this was called. Each frame rewrites every
/// row, so twenty of them is well past the notice's eight-screenful maturity
/// bar — which is what makes "no notice" below a claim about the pane rather
/// than about a pane too young to qualify.
fn wait_for_more_frames(deck: &TuiDeck, frames: u64, what: &str) {
    let start = visible_frame(&deck.snapshot_grid()).unwrap_or(0);
    deck.wait_until_grid(what, |grid| {
        visible_frame(grid).is_some_and(|f| f >= start + frames)
    });
}

fn launch_deck_against(
    daemon: &common::DaemonProc,
    cols: u16,
    rows: u16,
    command: Option<&str>,
) -> TuiDeck {
    let mut builder = TuiDeck::builder()
        .with_pty_size(cols, rows)
        .with_env(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            daemon.attach_socket.to_string_lossy().to_string(),
        )
        .with_env(
            "DOT_AGENT_DECK_SOCKET",
            daemon.hook_socket.to_string_lossy().to_string(),
        );
    if let Some(command) = command {
        builder = builder.with_continue_session("fullscreen-standin", command);
    }
    builder.launch_with_fixture("minimal")
}

/// In command mode the focused pane is the one drawn with a heavy border; an
/// unfocused pane's border is light. PageUp in command mode scrolls only the focused pane, so a
/// "no notice" that was not preceded by this would prove nothing.
fn pane_is_focused(grid: &str) -> bool {
    grid.contains("┏fullscreen-standin")
}

/// Press PageUp in command mode and give the notice the time it would need to
/// appear; true when it did.
fn page_up_shows_notice(deck: &TuiDeck) -> bool {
    deck.send_keys(b"\x1b[5~");
    deck.wait_for_grid_string_within(NOTICE, Duration::from_secs(2))
}

/// Scenario: Start an external daemon and a deck whose pane runs a stand-in for a full-screen agent (it enters the alternate screen once at start-up and repaints in place). Resize that deck so the agent is resized, press PageUp in command mode — no notice, the control — then detach and attach a second deck to the same daemon. Once the agent has repainted many more screens, PageUp in the second deck must not claim `Nothing to scroll` either, since the agent is still a full-screen program.
#[spec("mode/scroll/008")]
#[test]
fn mode_scroll_008_reattached_fullscreen_pane_does_not_claim_nothing_to_scroll() {
    let daemon = common::spawn_daemon_serve(None, "0");
    let scratch = common::race_safe_tempdir();
    let command = write_standin(scratch.path());

    let mut first = launch_deck_against(&daemon, 160, 45, Some(&command));
    first.wait_until_grid("stand-in painting in the first deck", |grid| {
        visible_frame(grid).is_some()
    });

    // Resize the deck, which resizes the agent. That SIGWINCH is where the
    // stand-in — like claude — repaints without re-entering the alternate
    // screen, and the resize is also what clears the daemon's replay ring.
    first.resize(140, 40);
    wait_for_more_frames(&first, 20, "stand-in repainted after the resize");

    // Control: the deck that watched the agent start knows it is full-screen,
    // so a scroll that cannot move says nothing — the documented behaviour.
    first.send_keys(b"\x04"); // Ctrl+D -> command mode
    first.wait_until_grid("first deck in command mode on the focused pane", |grid| {
        pane_is_focused(grid) && grid.lines().any(|line| line.starts_with(" COMMAND "))
    });
    assert!(
        !page_up_shows_notice(&first),
        "control: the deck that saw the full-screen agent start must not claim there is \
         nothing to scroll.\nGrid:\n{}",
        first.snapshot_grid()
    );

    // Detach-quit: the daemon and the agent survive.
    first.send_bytes(b"\x03"); // Ctrl+C -> quit-confirm modal
    first.wait_for_string("Quit dot-agent-deck?");
    first.send_bytes(b"\r"); // Enter -> Detach (default)
    assert_eq!(
        first.wait_for_exit_within(Duration::from_secs(30)),
        Some(true),
        "the first deck did not detach cleanly.\nGrid:\n{}",
        first.snapshot_grid()
    );
    let records = daemon.wait_for_agent_count(1, Duration::from_secs(10));
    assert_eq!(records.len(), 1, "the agent must survive the detach");

    // A second deck, at yet another size, sees the agent only through the
    // daemon's replay.
    let second = launch_deck_against(&daemon, 150, 42, None);
    second.wait_until_grid("stand-in painting in the second deck", |grid| {
        visible_frame(grid).is_some()
    });
    // The re-attached deck shows the pane but does not focus it, and a scroll
    // with no focused pane reaches no pane at all. Focus it the way a user does
    // — Enter on its card enters typing mode — then go back to command mode,
    // where PageUp scrolls the deck's own copy of the pane.
    second.send_keys(b"\r"); // Enter -> focus the pane (typing mode)
    second.wait_until_grid("second deck focused the pane in typing mode", |grid| {
        grid.lines().any(|line| line.starts_with(" TYPING "))
    });
    second.send_keys(b"\x04"); // Ctrl+D -> command mode
    second.wait_until_grid("second deck in command mode on the focused pane", |grid| {
        pane_is_focused(grid) && grid.lines().any(|line| line.starts_with(" COMMAND "))
    });
    wait_for_more_frames(&second, 20, "stand-in matured in the second deck");
    assert!(
        !page_up_shows_notice(&second),
        "the re-attached deck claimed `{NOTICE}` for a pane whose agent is still a \
         full-screen program — the same agent the first deck (control) correctly said \
         nothing about.\nGrid:\n{}",
        second.snapshot_grid()
    );
}
