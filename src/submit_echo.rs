//! Issue #1243: hold a submit's CR until the text it submits is on the agent's
//! screen.
//!
//! The guarded submit writes the payload, waits
//! [`crate::pane_input::SUBMIT_DELAY`], then writes `\r`. The delay exists
//! because an agent TUI that receives the CR together with the text treats the
//! burst as a paste and the CR as a newline in it. A fixed delay only keeps the
//! two apart while the agent keeps up. Measured against a real interactive
//! Claude Code under 48 busy-loops on 16 CPUs, writing the delegate pointer
//! 1000 ms after `SessionStart` (the respawn path's shape): **3 of 30** pointers
//! were left in the composer with a trailing empty line and no turn, against
//! 0 of 10 on an idle box. The CR was taken into the paste.
//!
//! Waiting for the agent to READ the payload is not enough. Polling the PTY
//! slave's unread-input count and writing the CR 150 ms after it reached zero
//! still lost **1 of 30** under the same load: Claude Code had read the text
//! and was still inside its paste window, which runs on a timer that a starved
//! event loop stretches. What does separate them is the agent PAINTING the
//! text: it only does that once it has committed the input to the composer.
//! Writing the CR after the pointer's tail appeared on screen submitted
//! **20 of 20** under the same load, with that paint taking up to 1.06 s.
//!
//! So [`EchoWatch`] follows the agent's output from just before the payload is
//! written and reports when the payload's last word is on screen, bounded by
//! [`SUBMIT_ECHO_BOUND`]. At the bound the CR goes anyway, which is exactly the
//! pre-#1243 behaviour, only later. The `SUBMIT_DELAY` floor still applies, so
//! on a box that keeps up the CR lands when it always did.
//!
//! **Losing the thread is not a reason to go early.** A watch that falls
//! behind the output bus (a busy agent outruns its receiver's queue) or whose
//! parser fails can no longer tell whether the payload rendered, and that
//! happens under exactly the load #1243 is about, so it holds the CR to the
//! bound rather than dropping to the floor. That includes a screen that could
//! not be parsed before the payload was written: the watch still exists, it has
//! only lost the thread from the start. Only a closed bus — the agent is gone,
//! and nothing will ever paint — returns at once.
//!
//! **Opt-in, not the default for every guarded submit.** A pane that does not
//! echo its input (a raw-mode stand-in, a program that hides what is typed)
//! pays the whole bound on every write, and the writer is held for it. The
//! delegate pointer takes it, because it is the write #1243 lost and a
//! delegation is one write. Other automatic writes keep the fixed delay.
//!
//! **A heuristic, not an attestation.** The watch counts the token anywhere
//! on the screen, not in the input box, so output that shows it before the
//! agent paints the payload — a worker or a process in its repository printing
//! the delivery id, which its task file names — releases the CR early. Such a
//! spoof is same-user, reaches only that worker's own submit, and at worst
//! makes it as early as the fixed `SUBMIT_DELAY` the gate replaced; the
//! delegate's in-place retry (issue #1383) recovers a pointer left unsubmitted
//! that way. `docs/develop/hook-provenance.md` records it beside the other
//! same-user signals the retry trusts.
//!
//! **Bounded work, not just a bounded wait.** The parser is as large as the
//! pane and the screen is rescanned after every batch of output, both inside
//! the pane's writer; a pane over [`MAX_ECHO_WATCH_CELLS`] gets no gate, and
//! the deadline is checked between chunks as well as while waiting for one, so
//! sustained output cannot hold the writer past [`SUBMIT_ECHO_BOUND`] by more
//! than one chunk's parse.
//!
//! **Eligible payloads** are single-line printable text up to
//! [`MAX_ECHO_GATED_PAYLOAD`] bytes whose last word has at least
//! [`MIN_TOKEN_CHARS`] matchable characters. A multi-line payload is
//! bracketed paste, and agents render it as a placeholder rather than as the
//! text, so there is nothing to match; a long single line is shown that way
//! too by Claude Code. Neither gets a gate.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

/// The longest a gated submit holds its CR waiting for the payload to render.
/// Twice the slowest paint measured under 48 busy-loops on 16 CPUs (1.06 s).
pub const SUBMIT_ECHO_BOUND: Duration = Duration::from_secs(2);

/// The longest payload that is gated. Claude Code shows a longer single-line
/// paste as a placeholder rather than the text, so its tail would never appear.
pub const MAX_ECHO_GATED_PAYLOAD: usize = 512;

/// The fewest matchable characters the payload's last word may have. Shorter
/// tokens are too likely to be on screen already, or to be formed by chance.
pub const MIN_TOKEN_CHARS: usize = 6;

/// The largest pane, in character cells, that gets a gate. Above it the
/// submit keeps the fixed `SUBMIT_DELAY`.
///
/// PTY axes are accepted up to 4096 each (`agent_pty::PTY_RESIZE_DIM_MAX`), so
/// without a cap one attach client's geometry sizes a 16.7-million-cell parser
/// that is rescanned after every output chunk while the pane's writer is held.
/// 500,000 covers any terminal a person can look at: an 8K display (7680 x
/// 4320 px) filled by a 6 x 12 px font is 1280 x 360 = 460,800 cells.
pub const MAX_ECHO_WATCH_CELLS: u32 = 500_000;

/// How a gated wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EchoOutcome {
    /// The payload's last word appeared on screen one more time than before the
    /// write.
    Rendered,
    /// The bound passed without it.
    TimedOut,
    /// The output could no longer be followed — this watcher lagged behind the
    /// agent's output bus, or the screen could not be parsed — so the wait was
    /// held to the bound: the payload may still be inside a paste window.
    Lost,
    /// The agent's output bus closed. Nothing will render, so the wait ended at
    /// once and the caller's `SUBMIT_DELAY` floor applies.
    Closed,
}

/// Only ASCII letters, digits and `-`, which is what survives a line wrap and
/// a composer's border glyphs intact.
fn squeeze(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect()
}

/// The token a gated submit waits for, or `None` when `payload` is not
/// eligible (see the module docs).
pub fn echo_token(payload: &[u8]) -> Option<String> {
    if payload.len() > MAX_ECHO_GATED_PAYLOAD {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    if text.chars().any(char::is_control) {
        return None;
    }
    let token = squeeze(text.split_whitespace().last()?);
    (token.len() >= MIN_TOKEN_CHARS).then_some(token)
}

#[cfg(test)]
thread_local! {
    /// Test seam: make every [`EchoWatch::feed`] on this thread panic inside
    /// the parser's `catch_unwind`, as a vt100 failure would. vt100 has no
    /// known panicking input at a geometry the watch accepts.
    static FAIL_FEED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Test seam: make [`EchoWatch`]'s parser fail on this thread while `fail`.
#[cfg(test)]
pub(crate) fn fail_parse_for_test(fail: bool) {
    FAIL_FEED.with(|cell| cell.set(fail));
}

/// Follows one agent's output and reports when a payload's last word renders.
pub struct EchoWatch {
    parser: vt100::Parser,
    rx: broadcast::Receiver<Arc<Vec<u8>>>,
    token: String,
    baseline: usize,
    /// The snapshot the watch started from could not be parsed, so its screen
    /// is missing output and a count over it means nothing. [`Self::wait`]
    /// holds to the bound.
    lost: bool,
}

impl std::fmt::Debug for EchoWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoWatch")
            .field("token", &self.token)
            .field("baseline", &self.baseline)
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

impl EchoWatch {
    /// Start watching for `payload`. `snapshot` and `rx` must come from one
    /// atomic subscription taken BEFORE the payload is written, so no byte of
    /// the echo is missed; `rows`/`cols` are the geometry that output is drawn
    /// at. `None` when the payload is not eligible or the pane's geometry gets
    /// no gate; the caller then keeps the fixed `SUBMIT_DELAY`. A snapshot that
    /// cannot be parsed is neither: the payload is still one the gate is for, so
    /// the watch is returned lost and holds the CR to the bound (Qodo round 7,
    /// PR #1414), as a parse failure later in [`Self::wait`] does.
    pub fn new(
        snapshot: &[u8],
        rx: broadcast::Receiver<Arc<Vec<u8>>>,
        rows: u16,
        cols: u16,
        payload: &[u8],
    ) -> Option<Self> {
        let token = echo_token(payload)?;
        // vt100 0.16.2 underflows in `col_wrap` when text wraps in a one-column
        // or one-row screen (see `pane_screen_text::visible_tail_lines`).
        if rows < 2 || cols < 2 {
            return None;
        }
        if u32::from(rows) * u32::from(cols) > MAX_ECHO_WATCH_CELLS {
            return None;
        }
        let mut watch = Self {
            parser: vt100::Parser::new(rows, cols, 0),
            rx,
            token,
            baseline: 0,
            lost: false,
        };
        if watch.feed(snapshot) {
            watch.baseline = watch.occurrences();
        } else {
            watch.lost = true;
        }
        Some(watch)
    }

    /// Process output; `false` if the parser panicked.
    fn feed(&mut self, bytes: &[u8]) -> bool {
        let parser = &mut self.parser;
        std::panic::catch_unwind(AssertUnwindSafe(|| {
            #[cfg(test)]
            if FAIL_FEED.with(std::cell::Cell::get) {
                panic!("injected vt100 parse failure");
            }
            parser.process(bytes)
        }))
        .is_ok()
    }

    fn occurrences(&self) -> usize {
        squeeze(&self.parser.screen().contents())
            .matches(self.token.as_str())
            .count()
    }

    /// Wait up to `bound` for the payload to render. Returns before the bound
    /// only when the payload rendered or the output bus closed; see
    /// [`EchoOutcome`].
    pub async fn wait(mut self, bound: Duration) -> EchoOutcome {
        let deadline = tokio::time::Instant::now() + bound;
        if self.lost {
            return Self::hold_to(deadline).await;
        }
        loop {
            if self.occurrences() > self.baseline {
                return EchoOutcome::Rendered;
            }
            // `timeout_at` polls the receive before the deadline, so with
            // output queued it would never time out on its own.
            if tokio::time::Instant::now() >= deadline {
                return EchoOutcome::TimedOut;
            }
            let chunk = match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                Err(_) => return EchoOutcome::TimedOut,
                Ok(Ok(chunk)) => chunk,
                Ok(Err(broadcast::error::RecvError::Closed)) => return EchoOutcome::Closed,
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {
                    return Self::hold_to(deadline).await;
                }
            };
            if !self.feed(&chunk) {
                return Self::hold_to(deadline).await;
            }
            match self.drain(deadline) {
                Ok(()) => {}
                Err(EchoOutcome::Lost) => return Self::hold_to(deadline).await,
                Err(outcome) => return outcome,
            }
        }
    }

    /// The watch lost the thread: the screen it holds is missing output, so a
    /// count over it could go either way. Nothing it reads can release the CR
    /// early any more, so it waits out the bound.
    async fn hold_to(deadline: tokio::time::Instant) -> EchoOutcome {
        tokio::time::sleep_until(deadline).await;
        EchoOutcome::Lost
    }

    /// Take whatever else is already queued before the screen is re-read, up
    /// to `deadline`: output that keeps arriving must not hold the writer past
    /// it. `Err` is [`EchoOutcome::Lost`] or [`EchoOutcome::Closed`].
    fn drain(&mut self, deadline: tokio::time::Instant) -> Result<(), EchoOutcome> {
        while tokio::time::Instant::now() < deadline {
            match self.rx.try_recv() {
                Ok(chunk) => {
                    if !self.feed(&chunk) {
                        return Err(EchoOutcome::Lost);
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) => return Ok(()),
                Err(broadcast::error::TryRecvError::Lagged(_)) => return Err(EchoOutcome::Lost),
                Err(broadcast::error::TryRecvError::Closed) => return Err(EchoOutcome::Closed),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POINTER: &[u8] =
        b"Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-7f3a9c21]";

    type Output = Arc<Vec<u8>>;

    fn channel() -> (broadcast::Sender<Output>, broadcast::Receiver<Output>) {
        broadcast::channel(64)
    }

    #[test]
    fn echo_token_is_the_squeezed_last_word() {
        assert_eq!(echo_token(POINTER).as_deref(), Some("d-7f3a9c21"));
    }

    #[test]
    fn echo_token_refuses_what_would_not_render_as_typed() {
        // Multi-line: written as bracketed paste, shown as a placeholder.
        assert_eq!(
            echo_token(b"\x1b[200~line one\nline two-long\x1b[201~"),
            None
        );
        assert_eq!(echo_token(b"first line\nsecond-line"), None);
        // Too long: shown as a placeholder by Claude Code.
        let long = format!("{} tail-token", "x".repeat(MAX_ECHO_GATED_PAYLOAD));
        assert_eq!(echo_token(long.as_bytes()), None);
        // A last word too short to be distinctive.
        assert_eq!(echo_token(b"please say ok"), None);
        assert_eq!(echo_token(b"[!!]"), None);
        assert_eq!(echo_token(b""), None);
        assert_eq!(echo_token(&[0xff, 0xfe, b'a']), None);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_returns_once_the_tail_is_painted() {
        let (tx, rx) = channel();
        let watch = EchoWatch::new(b"\x1b[2J\x1b[H> ", rx, 24, 80, POINTER).expect("eligible");
        let task = tokio::spawn(watch.wait(SUBMIT_ECHO_BOUND));
        tokio::time::sleep(Duration::from_millis(300)).await;
        // The head of the pointer is not enough.
        tx.send(Arc::new(
            b"Read .dot-agent-deck/worker-task-coder.md".to_vec(),
        ))
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !task.is_finished(),
            "rendered before the tail was on screen"
        );
        tx.send(Arc::new(b" for your task. [delivery d-7f3a9c21]".to_vec()))
            .unwrap();
        assert_eq!(task.await.unwrap(), EchoOutcome::Rendered);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_finds_a_tail_split_by_a_wrap_and_border_glyphs() {
        let (tx, rx) = channel();
        let watch = EchoWatch::new(b"", rx, 10, 20, POINTER).expect("eligible");
        let task = tokio::spawn(watch.wait(SUBMIT_ECHO_BOUND));
        tx.send(Arc::new(
            "\x1b[1;1H│ [delivery d-7f3a │\x1b[2;1H│ 9c21]            │"
                .as_bytes()
                .to_vec(),
        ))
        .unwrap();
        assert_eq!(task.await.unwrap(), EchoOutcome::Rendered);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_needs_a_new_copy_when_the_token_is_already_on_screen() {
        // A previous delivery of the same pointer, still in the transcript.
        let (tx, rx) = channel();
        let watch = EchoWatch::new(b"\x1b[1;1H> [delivery d-7f3a9c21]", rx, 24, 80, POINTER)
            .expect("eligible");
        let task = tokio::spawn(watch.wait(SUBMIT_ECHO_BOUND));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!task.is_finished(), "the old copy counted as the echo");
        tx.send(Arc::new(b"\x1b[5;1H> [delivery d-7f3a9c21]".to_vec()))
            .unwrap();
        assert_eq!(task.await.unwrap(), EchoOutcome::Rendered);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out_at_the_bound_on_a_pane_that_does_not_echo() {
        let (_tx, rx) = channel();
        let watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        let started = tokio::time::Instant::now();
        assert_eq!(watch.wait(SUBMIT_ECHO_BOUND).await, EchoOutcome::TimedOut);
        assert_eq!(started.elapsed(), SUBMIT_ECHO_BOUND);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_gives_up_at_once_when_the_output_bus_closes() {
        let (tx, rx) = channel();
        let watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        drop(tx);
        let started = tokio::time::Instant::now();
        assert_eq!(watch.wait(SUBMIT_ECHO_BOUND).await, EchoOutcome::Closed);
        assert_eq!(started.elapsed(), Duration::ZERO);
    }

    /// Qodo round 4: a watch that falls behind a busy agent's output can no
    /// longer see the payload render, and that is when a CR at the floor lands
    /// in the paste window (#1243). It holds to the bound instead — on the
    /// blocking receive and on the drain behind it alike.
    #[tokio::test(start_paused = true)]
    async fn wait_holds_to_the_bound_when_it_lags_behind_the_output() {
        // On the receive: more output than the queue holds before the wait.
        let (tx, rx) = broadcast::channel(2);
        let watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        for _ in 0..4 {
            tx.send(Arc::new(b"chatter ".to_vec())).unwrap();
        }
        let started = tokio::time::Instant::now();
        assert_eq!(watch.wait(SUBMIT_ECHO_BOUND).await, EchoOutcome::Lost);
        assert_eq!(started.elapsed(), SUBMIT_ECHO_BOUND);

        // On the drain: a lag found there is reported as lost, which `wait`
        // holds to the bound exactly as above; a closed bus as closed.
        let (tx, rx) = broadcast::channel(2);
        let mut watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        for _ in 0..4 {
            tx.send(Arc::new(b"chatter ".to_vec())).unwrap();
        }
        let later = tokio::time::Instant::now() + SUBMIT_ECHO_BOUND;
        assert_eq!(watch.drain(later), Err(EchoOutcome::Lost));
        let (tx, rx) = channel();
        let mut watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        drop(tx);
        assert_eq!(watch.drain(later), Err(EchoOutcome::Closed));
    }

    /// Auditor M5: a pane over the cell cap gets no gate (the caller then
    /// keeps the fixed delay); one at the cap still does.
    #[test]
    fn new_refuses_a_pane_over_the_cell_cap() {
        let (_tx, rx) = channel();
        assert!(EchoWatch::new(b"", rx, 4096, 4096, POINTER).is_none());
        let (_tx, rx) = channel();
        assert!(EchoWatch::new(b"", rx, 1000, 501, POINTER).is_none());
        let (_tx, rx) = channel();
        assert!(
            EchoWatch::new(b"", rx, 1000, 500, POINTER).is_some(),
            "exactly {MAX_ECHO_WATCH_CELLS} cells is within the cap"
        );
    }

    /// Auditor M5: queued output does not keep a wait going past its bound —
    /// neither the receive (which returns queued chunks before it looks at the
    /// deadline) nor the drain behind it.
    #[tokio::test(start_paused = true)]
    async fn wait_stops_at_the_bound_with_output_still_queued() {
        let (tx, rx) = channel();
        let watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        for _ in 0..8 {
            tx.send(Arc::new(b"chatter ".to_vec())).unwrap();
        }
        tx.send(Arc::new(b"[delivery d-7f3a9c21]".to_vec()))
            .unwrap();
        assert_eq!(
            watch.wait(Duration::ZERO).await,
            EchoOutcome::TimedOut,
            "a queued chunk was read past the deadline"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn drain_reads_nothing_once_the_deadline_has_passed() {
        let (tx, rx) = channel();
        let mut watch = EchoWatch::new(b"", rx, 24, 80, POINTER).expect("eligible");
        tx.send(Arc::new(b"[delivery d-7f3a9c21]".to_vec()))
            .unwrap();
        let passed = tokio::time::Instant::now();
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(watch.drain(passed), Ok(()));
        assert_eq!(
            watch.occurrences(),
            watch.baseline,
            "the drain fed a chunk after its deadline"
        );
        // Before the deadline it drains as it always did.
        assert_eq!(
            watch.drain(tokio::time::Instant::now() + Duration::from_secs(1)),
            Ok(())
        );
        assert_eq!(watch.occurrences(), watch.baseline + 1);
    }

    /// Qodo round 7 on PR #1414: an eligible payload over a snapshot the parser
    /// fails on still gets a watch, and that watch holds to the bound rather
    /// than letting the caller fall back to the `SUBMIT_DELAY` floor — the
    /// same as a parse failure partway through the wait.
    #[tokio::test(start_paused = true)]
    async fn new_over_an_unparseable_snapshot_holds_to_the_bound() {
        let (tx, rx) = channel();
        fail_parse_for_test(true);
        let watch = EchoWatch::new(b"> ", rx, 24, 80, POINTER);
        fail_parse_for_test(false);
        let watch = watch.expect("an eligible payload gets a watch even when the parse fails");
        let started = tokio::time::Instant::now();
        let task = tokio::spawn(watch.wait(SUBMIT_ECHO_BOUND));
        // Even the payload painting cannot release it: the screen it would be
        // counted on is missing output.
        tx.send(Arc::new(b"[delivery d-7f3a9c21]".to_vec()))
            .unwrap();
        assert_eq!(task.await.unwrap(), EchoOutcome::Lost);
        assert_eq!(started.elapsed(), SUBMIT_ECHO_BOUND);
    }

    #[test]
    fn new_refuses_an_ineligible_payload_and_a_degenerate_screen() {
        let (_tx, rx) = channel();
        assert!(EchoWatch::new(b"", rx, 24, 80, b"say ok").is_none());
        let (_tx, rx) = channel();
        assert!(EchoWatch::new(b"", rx, 1, 80, POINTER).is_none());
        let (_tx, rx) = channel();
        assert!(EchoWatch::new(b"", rx, 24, 1, POINTER).is_none());
    }
}

/// Issue #1243, driven through a real registry and a real PTY against a
/// stand-in with Claude Code's measured shape: input that arrives within a
/// paste window after text joins the paste, a CR in it becomes a newline, and
/// the text is painted only once the window closes. Real time, because the
/// guarded write sleeps on the Tokio clock and the PTY reader is a thread.
#[cfg(all(test, unix))]
mod registry_tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::agent_pty::{
        AgentPtyRegistry, DOT_AGENT_DECK_PANE_ID, GuardedSend, GuardedSendDetail, SpawnOptions,
    };

    const POINTER: &str =
        "Read .dot-agent-deck/worker-task-coder.md for your task. [delivery d-1243cafe]";

    /// Raw mode; after a chunk of text, wait out a 400 ms paste window,
    /// folding anything that arrives into the paste with CRs as newlines, then
    /// paint the composer. A lone CR over a composer holding text submits it.
    const PASTE_WINDOW_AGENT: &str = r#"import os, select, time, tty
fd = 0
tty.setraw(fd)
os.write(1, b'READY\r\n')
composer = b''
while True:
    chunk = os.read(fd, 4096)
    if chunk == b'\r' and composer:
        os.write(1, b'\r\nSUBMITTED ' + composer.replace(b'\n', b'<NL>') + b'\r\n')
        composer = b''
        continue
    deadline = time.monotonic() + 0.4
    while (left := deadline - time.monotonic()) > 0:
        if select.select([fd], [], [], left)[0]:
            chunk += os.read(fd, 4096)
    composer += chunk.replace(b'\r', b'\n')
    os.write(1, b'\r\n> ' + composer.replace(b'\n', b'\r\n') + b'\r\n')
"#;

    /// Through Tokio's process API, so the check does not block a runtime
    /// worker.
    async fn python_available() -> bool {
        tokio::process::Command::new("python3")
            .arg("--version")
            .output()
            .await
            .is_ok_and(|out| out.status.success())
    }

    async fn start(pane: &str, dir: &std::path::Path) -> (Arc<AgentPtyRegistry>, String) {
        let script = dir.join("agent.py");
        tokio::fs::write(&script, PASTE_WINDOW_AGENT).await.unwrap();
        let command = format!("exec python3 -u '{}'", script.display());
        let registry = Arc::new(AgentPtyRegistry::new());
        let agent = registry
            .spawn_agent(SpawnOptions {
                command: Some(&command),
                env: vec![(DOT_AGENT_DECK_PANE_ID.to_string(), pane.to_string())],
                ..SpawnOptions::default()
            })
            .expect("spawn paste-window stand-in");
        assert!(
            wait_for(&registry, &agent, "READY", Duration::from_secs(10)).await,
            "stand-in never came up"
        );
        (registry, agent)
    }

    async fn wait_for(
        registry: &AgentPtyRegistry,
        agent: &str,
        needle: &str,
        within: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let screen =
                String::from_utf8_lossy(&registry.snapshot(agent).unwrap_or_default()).into_owned();
            if screen.contains(needle) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Scenario: type the delegate pointer into an agent still inside its
    /// paste window. The fixed-delay submit's CR lands in the paste and the
    /// pointer stays unsubmitted; the echo-gated submit presses Enter only
    /// once the pointer is painted, and the agent submits it.
    #[tokio::test]
    async fn echo_gated_submit_lands_after_the_paste_window_the_fixed_delay_falls_into() {
        if !python_available().await {
            eprintln!("SKIP: python3 is not available");
            return;
        }
        let dir = tempfile::tempdir().unwrap();

        let (fixed, agent) = start("echo-gate-fixed", dir.path()).await;
        let outcome = fixed
            .write_and_submit_guarded_detailed("echo-gate-fixed", POINTER, &agent, || async {
                true
            })
            .await
            .expect("fixed-delay write");
        assert_eq!(outcome, GuardedSendDetail::Outcome(GuardedSend::Applied));
        assert!(
            wait_for(&fixed, &agent, "d-1243cafe", Duration::from_secs(5)).await,
            "the pointer never painted"
        );
        assert!(
            !wait_for(&fixed, &agent, "SUBMITTED", Duration::from_secs(1)).await,
            "precondition: a CR {:?} after the text must fall into the paste window",
            crate::pane_input::SUBMIT_DELAY
        );
        fixed.shutdown_all();

        let (gated, agent) = start("echo-gate-gated", dir.path()).await;
        let outcome = gated
            .write_and_submit_guarded_after_echo("echo-gate-gated", POINTER, &agent, || async {
                true
            })
            .await
            .expect("echo-gated write");
        assert_eq!(outcome, GuardedSendDetail::Outcome(GuardedSend::Applied));
        assert!(
            wait_for(
                &gated,
                &agent,
                &format!("SUBMITTED {POINTER}\r\n"),
                Duration::from_secs(5)
            )
            .await,
            "the echo-gated submit did not submit the pointer alone; screen: {:?}",
            String::from_utf8_lossy(&gated.snapshot(&agent).unwrap_or_default())
        );
        gated.shutdown_all();
    }
}
