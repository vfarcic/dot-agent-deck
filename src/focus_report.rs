//! PRD #1105 M11 step 3 — the TUI tells its daemon when it has focus.
//!
//! Under focus-driven sizing the **last-focused client** wins: every agent that
//! client views takes that client's viewer size. A client says it is the one
//! being looked at with a `focus-gained` claim
//! ([`DaemonClient::focus_gained`]). This module decides **when** the TUI makes
//! that claim, and it is the only place that does.
//!
//! Two signals count, per the product owner's decision recorded in the PRD's
//! 2026-09-17 Work Log entry:
//!
//! - **The terminal reports focus-in** ([`Event::FocusGained`], enabled by
//!   `EnableFocusChange` in `ui::run_tui`). Claimed at once, never throttled: a
//!   window switch is a rare event, and it is the one signal that means exactly
//!   "the person just came back to this TUI".
//! - **Input**, because not every terminal reports focus — tmux only does with
//!   `focus-events on`. Throttled to one claim per [`INPUT_CLAIM_INTERVAL`],
//!   since otherwise every keystroke would open a connection to repeat a claim
//!   that is almost always a no-op.
//!
//! **A claim that has gone out of date is dropped, not sent late.** A claim is
//! a spawned task that can wait — on its first capability handshake, and on
//! opening a connection — so a newer claim or the terminal reporting focus-out
//! can arrive while it is still unsent. Sending it then would land after the
//! event that superseded it, and against another client's claim that can take
//! focus from the window the person is now looking at. See
//! [`FocusReporter::observe`] for the rule and the window that remains.
//!
//! What is *not* here, deliberately: a focus-lost message (the contract has
//! none — losing focus changes nothing under "last focused"), and any decision
//! about sizing, which is the daemon's.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyEventKind, MouseEventKind};

use crate::daemon_client::{DaemonClient, FocusReport};

/// The shortest gap between two focus claims triggered by **input**.
///
/// One second, and the argument for it is the cost it buys on each side.
///
/// **What it saves.** A claim is a connection, a request and a reply. Typing at
/// a brisk 10 keys a second, a held key auto-repeating at ~30 a second, or a
/// wheel spin reporting dozens of notches a second, all collapse to one claim a
/// second instead of one per event. Against a remote deck each of those would
/// otherwise be a round trip through the ssh tunnel.
///
/// **What it costs.** In a terminal that does not report focus, input is the
/// only signal, so a claim suppressed by the throttle is a switch the daemon
/// does not hear about. That happens only when the person leaves the TUI,
/// another client claims, and they come back and type, all inside one second of
/// the TUI's previous claim. The first keystroke after the window ends claims,
/// so a person who keeps typing goes unclaimed for at most this long. A person
/// who presses one key and then only watches stays unclaimed until their next
/// input. A terminal that reports focus never pays this, because
/// [`FocusSignal::Gained`] bypasses the throttle.
///
/// **Why not shorter or longer.** A deliberate round trip to another window —
/// switch, look, act, switch back — takes longer than a second, so a
/// shorter window buys little and doubles the claims while typing. A longer
/// one starts to catch ordinary quick switches.
///
/// **Why no trailing claim at the end of the window.** It would bound the
/// one-key case above, but it fires *after* the input that armed it. Type in
/// the TUI and switch to the desktop within the second, and the deferred claim
/// lands after the desktop's own claim and takes focus back from the window the
/// person is now looking at. That is a worse failure than the one it removes.
pub const INPUT_CLAIM_INTERVAL: Duration = Duration::from_secs(1);

/// How long one claim may take before it is abandoned. A claim is fire and
/// forget — nothing waits on its answer — so this bounds only how long a wedged
/// daemon can hold a task open. Matches the order of the other short RPC
/// bounds in `embedded_pane`.
const CLAIM_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a terminal event may mean "this TUI has focus".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusSignal {
    /// The terminal reported focus-in. Always claimed.
    Gained,
    /// The person interacted with the TUI. Claimed at most once per
    /// [`INPUT_CLAIM_INTERVAL`].
    Input,
}

/// Classify one terminal event.
///
/// - [`Event::FocusGained`] is [`FocusSignal::Gained`].
/// - A key **press** or **repeat** and a paste are [`FocusSignal::Input`]. A
///   key **release** is not: it is the tail of a press that already counted,
///   and it is the one key event that can arrive after focus has moved on — the
///   release of the very shortcut that switched away. (On Unix the TUI asks
///   only for `DISAMBIGUATE_ESCAPE_CODES`, so releases are rare there; on
///   Windows crossterm reports one for every key.)
/// - Every mouse event **except bare motion** is input: button presses,
///   releases, drags and the wheel. Mouse input counts because the product
///   owner's decision names it, and because clicking into a terminal that does
///   not report focus would otherwise claim nothing. Motion does not count:
///   `EnableMouseCapture` turns on any-event tracking (`?1003h`), so the
///   terminal reports the pointer merely **passing over** the window, focused
///   or not. Counting that would let a pointer crossing the TUI on its way to
///   the desktop take focus from the window the person is heading for.
/// - [`Event::FocusLost`] and [`Event::Resize`] say nothing about focus.
pub fn focus_signal(event: &Event) -> Option<FocusSignal> {
    match event {
        Event::FocusGained => Some(FocusSignal::Gained),
        Event::Key(key) => (key.kind != KeyEventKind::Release).then_some(FocusSignal::Input),
        Event::Paste(_) => Some(FocusSignal::Input),
        Event::Mouse(mouse) => (mouse.kind != MouseEventKind::Moved).then_some(FocusSignal::Input),
        Event::FocusLost | Event::Resize(..) => None,
    }
}

/// The throttle, kept free of clocks and sockets so it can be tested with
/// synthetic instants.
#[derive(Debug, Default)]
pub struct ClaimThrottle {
    last_claim: Option<Instant>,
}

impl ClaimThrottle {
    /// Whether `signal` at `now` should be claimed, recording the claim if so.
    ///
    /// A [`FocusSignal::Gained`] is always admitted. A [`FocusSignal::Input`] is
    /// admitted when nothing has been claimed yet or the last claim — by either
    /// signal — is at least [`INPUT_CLAIM_INTERVAL`] old. Suppressed input is
    /// dropped, not deferred (see [`INPUT_CLAIM_INTERVAL`] for why).
    pub fn admit(&mut self, signal: FocusSignal, now: Instant) -> bool {
        let admitted = match signal {
            FocusSignal::Gained => true,
            FocusSignal::Input => self
                .last_claim
                .is_none_or(|last| now.saturating_duration_since(last) >= INPUT_CLAIM_INTERVAL),
        };
        if admitted {
            self.last_claim = Some(now);
        }
        admitted
    }
}

/// Turns terminal events into focus claims against one daemon.
///
/// Held by [`crate::embedded_pane::EmbeddedPaneController`], whose client —
/// and therefore whose `client_id` — it shares, so the identity that claims
/// focus is the identity every pane attached as.
pub struct FocusReporter {
    client: DaemonClient,
    runtime: tokio::runtime::Handle,
    throttle: Mutex<ClaimThrottle>,
    /// Advanced by every event that makes an unsent claim out of date: a newer
    /// admitted claim, and focus-out. A claim carries the value it was started
    /// at and is written only if that is still the current one.
    generation: Arc<AtomicU64>,
}

impl FocusReporter {
    pub fn new(client: DaemonClient, runtime: tokio::runtime::Handle) -> Self {
        Self {
            client,
            runtime,
            throttle: Mutex::new(ClaimThrottle::default()),
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Observe one terminal event, and claim focus if it warrants a claim.
    ///
    /// Never blocks: the claim is spawned on the runtime, because this is called
    /// from the render thread once per event and a claim is a connection. The
    /// returned handle is for tests that need to know the claim has finished;
    /// production drops it. `None` means no claim was started.
    ///
    /// Whether the daemon advertises `focus-gained` is
    /// [`DaemonClient::focus_gained`]'s check, not this one's, so an older daemon
    /// is never sent the claim — and after the first `Hello` that check is a
    /// cache read, so a throttled claim against an older daemon costs no socket.
    ///
    /// **Out-of-date claims are dropped.** Two events make a claim that has not
    /// been written yet out of date, and each drops it:
    ///
    /// - **a newer admitted claim** — the newer one is the claim to send, and the
    ///   older one reaching the daemon after it would only repeat it late;
    /// - **[`Event::FocusLost`]** — the person has left this TUI, and a claim
    ///   written now would say the opposite.
    ///
    /// Input the throttle suppresses drops nothing: the person is still here,
    /// and the claim already started says so. The check is
    /// [`DaemonClient::focus_gained_while`]'s, made after the claim's connection
    /// is open and immediately before its request is written. **What remains** is
    /// a claim whose request was already written when the newer event arrived:
    /// it lands, and if the event was a focus-out, another client's claim that
    /// the daemon accepts after it is still what decides.
    pub fn observe(&self, event: &Event, now: Instant) -> Option<tokio::task::JoinHandle<()>> {
        if matches!(event, Event::FocusLost) {
            self.generation.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        let signal = focus_signal(event)?;
        if !self
            .throttle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .admit(signal, now)
        {
            return None;
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let current = Arc::clone(&self.generation);
        let client = self.client.clone();
        Some(self.runtime.spawn(async move {
            let still_wanted = || current.load(Ordering::SeqCst) == generation;
            match tokio::time::timeout(CLAIM_TIMEOUT, client.focus_gained_while(still_wanted)).await
            {
                Ok(Ok(FocusReport::Recorded)) => tracing::trace!(?signal, "focus claimed"),
                Ok(Ok(FocusReport::Withheld)) => {
                    tracing::trace!(
                        ?signal,
                        "focus claim withheld: daemon predates focus-gained"
                    )
                }
                Ok(Ok(FocusReport::Superseded)) => {
                    tracing::trace!(?signal, "focus claim dropped: a newer event superseded it")
                }
                Ok(Err(error)) => tracing::debug!(%error, "focus claim failed"),
                Err(_) => tracing::debug!(
                    timeout_ms = CLAIM_TIMEOUT.as_millis() as u64,
                    "focus claim timed out"
                ),
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{
        KeyCode, KeyEvent, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    };

    fn key(kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent {
            code: KeyCode::Char('a'),
            modifiers: KeyModifiers::NONE,
            kind,
            state: KeyEventState::NONE,
        })
    }

    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// PRD #1105 M11 — the classification, event by event.
    #[test]
    fn terminal_events_are_classified_as_focus_signals() {
        assert_eq!(focus_signal(&Event::FocusGained), Some(FocusSignal::Gained));
        assert_eq!(focus_signal(&Event::FocusLost), None);
        assert_eq!(focus_signal(&Event::Resize(80, 24)), None);
        assert_eq!(
            focus_signal(&key(KeyEventKind::Press)),
            Some(FocusSignal::Input)
        );
        assert_eq!(
            focus_signal(&key(KeyEventKind::Repeat)),
            Some(FocusSignal::Input)
        );
        assert_eq!(
            focus_signal(&key(KeyEventKind::Release)),
            None,
            "a release is the tail of a press, and can be the release of the shortcut that \
             switched away"
        );
        assert_eq!(
            focus_signal(&Event::Paste("text".into())),
            Some(FocusSignal::Input)
        );
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
        ] {
            assert_eq!(
                focus_signal(&mouse(kind)),
                Some(FocusSignal::Input),
                "{kind:?}"
            );
        }
        assert_eq!(
            focus_signal(&mouse(MouseEventKind::Moved)),
            None,
            "any-event tracking reports a pointer merely passing over the window"
        );
    }

    /// PRD #1105 M11 — N inputs inside one window admit one claim, the first
    /// input at or after the window's end admits the next, and focus-in is never
    /// throttled but does restart the window.
    #[test]
    fn input_claims_are_throttled_and_focus_in_is_not() {
        let t0 = Instant::now();
        let mut throttle = ClaimThrottle::default();

        assert!(
            throttle.admit(FocusSignal::Input, t0),
            "the first input claims"
        );
        let inside = (1..=20)
            .map(|step| t0 + INPUT_CLAIM_INTERVAL * step / 21)
            .filter(|&now| throttle.admit(FocusSignal::Input, now))
            .count();
        assert_eq!(
            inside, 0,
            "every further input inside the window is suppressed"
        );
        assert!(
            throttle.admit(FocusSignal::Input, t0 + INPUT_CLAIM_INTERVAL),
            "the window is closed at its end, not after it"
        );

        let t1 = t0 + INPUT_CLAIM_INTERVAL;
        for step in 1..=3 {
            assert!(
                throttle.admit(FocusSignal::Gained, t1 + Duration::from_millis(step)),
                "focus-in claims every time, however recent the last claim"
            );
        }
        assert!(
            !throttle.admit(
                FocusSignal::Input,
                t1 + Duration::from_millis(3) + INPUT_CLAIM_INTERVAL / 2
            ),
            "a focus-in claim restarts the window for input"
        );
    }
}
