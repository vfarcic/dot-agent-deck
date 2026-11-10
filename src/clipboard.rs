//! Putting text the user selected in a pane on their clipboard.
//!
//! A copy goes out two ways, because neither reaches every setup on its own:
//!
//! - the **native** system clipboard (`arboard`: X11 or Wayland on Linux, the
//!   pasteboard on macOS), which lands whatever the terminal is, but only on
//!   the machine the deck runs on and only when that machine has a clipboard
//!   to open — a headless box or an ssh session without a forwarded display
//!   has none;
//! - an **OSC 52** escape sequence written to the user's terminal, which
//!   crosses ssh, but only lands when the terminal honours it (several common
//!   terminals ignore it or keep it behind a preference).
//!
//! The native write is attempted first and OSC 52 is sent every time, and
//! [`CopyOutcome`] records which of the two went out, so the status line can
//! say what happened instead of claiming a copy that may not have landed
//! ([`status_message`]).
//!
//! [`ClipboardBackend`] is the seam: the deck uses [`SystemClipboard`], and
//! tests inject a fake so that none of them writes the clipboard of the
//! machine running them.

use std::cell::{Cell, RefCell};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::Duration;

/// Environment knob that turns the native clipboard write off, leaving OSC 52
/// as the only delivery. `0`, `false`, `off` or `no` (any case) turn it off;
/// anything else, or unset, leaves it on.
///
/// The e2e harness sets it to `0` for every deck it spawns, because on macOS
/// the pasteboard needs no `DISPLAY` and a PTY test would otherwise write the
/// developer's real clipboard. See `docs/develop/clipboard.md`.
pub const NATIVE_CLIPBOARD_ENV: &str = "DOT_AGENT_DECK_NATIVE_CLIPBOARD";

/// How long a copy waits for the native write to report back before the
/// status line stops waiting and reports only what it knows was sent.
///
/// The write runs on its own thread (see [`NativeClipboard`]), so this bounds
/// how long one copy can hold the UI thread, including the first copy, which
/// also opens the clipboard.
pub const NATIVE_WRITE_WAIT: Duration = Duration::from_millis(250);

/// How long an ordinary exit waits for the native clipboard's worker to
/// finish, so that dropping the clipboard (arboard's handoff to an X11
/// clipboard manager) is not cut off by the process exiting, while an X server
/// that stopped answering cannot hang the exit. See
/// [`NativeClipboard::shutdown`].
pub const NATIVE_SHUTDOWN_WAIT: Duration = Duration::from_millis(300);

/// The native clipboard worker thread's name.
pub const NATIVE_THREAD_NAME: &str = "native-clipboard";

/// What the native clipboard write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeWrite {
    /// The system clipboard accepted the text.
    Written,
    /// There is no native clipboard to write: it could not be opened (no
    /// display, a headless session) or [`NATIVE_CLIPBOARD_ENV`] turned it off.
    Unavailable,
    /// The clipboard was open but refused the write.
    Failed,
    /// The write did not report back within [`NATIVE_WRITE_WAIT`], or an
    /// earlier write was still running. It may still land; it is not claimed.
    NotConfirmed,
}

/// What one copy did, on both routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyOutcome {
    pub native: NativeWrite,
    /// The OSC 52 sequence was written to the user's terminal. That says it
    /// was sent, not that the terminal put it on a clipboard.
    pub osc52_sent: bool,
}

/// The two delivery routes, as one seam.
pub trait ClipboardBackend {
    fn write_native(&mut self, text: &str) -> NativeWrite;
    /// Write the OSC 52 sequence carrying `text` to the user's terminal;
    /// `true` when the bytes were written.
    fn send_osc52(&mut self, text: &str) -> bool;
}

/// Copy `text` by both routes: the native write first, then OSC 52.
///
/// Returns `None` for empty text, which sends nothing, so that there is no
/// copy to report.
pub fn deliver(backend: &mut dyn ClipboardBackend, text: &str) -> Option<CopyOutcome> {
    if text.is_empty() {
        return None;
    }
    let native = backend.write_native(text);
    let osc52_sent = backend.send_osc52(text);
    Some(CopyOutcome { native, osc52_sent })
}

/// Status line after a confirmed native write.
pub const COPIED_MESSAGE: &str = "Copied to clipboard";
/// Status line when the native write was unavailable or refused and the OSC 52
/// sequence went out.
pub const OSC52_ONLY_MESSAGE: &str =
    "Sent to terminal clipboard (OSC 52); if paste is empty, see Troubleshooting";
/// Status line when the OSC 52 sequence went out and the native write had not
/// answered yet: it may still land, so it is neither claimed nor called failed.
pub const OSC52_PENDING_MESSAGE: &str =
    "Sent to terminal clipboard (OSC 52); clipboard write still pending";
/// Status line when the native write was unavailable or refused and no OSC 52
/// sequence went out.
pub const NOT_COPIED_MESSAGE: &str = "Not copied: no clipboard was reachable";
/// Status line when no OSC 52 sequence went out and the native write had not
/// answered yet: it may still land, so it is not called a failure.
pub const NOT_CONFIRMED_MESSAGE: &str = "Clipboard write not confirmed yet";

/// The status line for a copy, derived only from what [`deliver`] reports.
///
/// It claims a copy only when the native write confirmed one, and then says
/// nothing about OSC 52. Without a confirmed native write, the message names
/// OSC 52 when that sequence went out; with the native write unavailable or
/// refused it also points at the troubleshooting page, because whether that
/// lands depends on the user's terminal. A native write that had not answered yet may still
/// land, so it is reported as pending, never as a copy and never as a
/// failure. `None` (nothing to copy) yields no message at all.
pub fn status_message(outcome: Option<CopyOutcome>) -> Option<&'static str> {
    let outcome = outcome?;
    Some(match outcome {
        CopyOutcome {
            native: NativeWrite::Written,
            ..
        } => COPIED_MESSAGE,
        CopyOutcome {
            native: NativeWrite::NotConfirmed,
            osc52_sent: true,
        } => OSC52_PENDING_MESSAGE,
        CopyOutcome {
            native: NativeWrite::NotConfirmed,
            osc52_sent: false,
        } => NOT_CONFIRMED_MESSAGE,
        CopyOutcome {
            osc52_sent: true, ..
        } => OSC52_ONLY_MESSAGE,
        CopyOutcome { .. } => NOT_COPIED_MESSAGE,
    })
}

/// A native clipboard the worker thread can write to.
pub trait NativeSink {
    fn write_text(&mut self, text: &str) -> Result<(), String>;
}

impl NativeSink for arboard::Clipboard {
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        self.set_text(text.to_owned()).map_err(|e| e.to_string())
    }
}

/// Opens the native clipboard, on the worker thread, once.
pub type NativeOpener = Box<dyn FnOnce() -> Option<Box<dyn NativeSink>> + Send>;

fn open_system_clipboard() -> Option<Box<dyn NativeSink>> {
    match arboard::Clipboard::new() {
        Ok(clipboard) => Some(Box::new(clipboard)),
        Err(error) => {
            tracing::info!(%error, "no native clipboard; copies go out as OSC 52 only");
            None
        }
    }
}

/// One copy handed to the worker thread.
struct Request {
    /// The copy's place in the order copies were made; see [`Slot::latest`].
    generation: u64,
    text: String,
    reply: mpsc::SyncSender<NativeWrite>,
}

/// What the UI thread and the worker thread share, under one lock.
#[derive(Default)]
struct Slot {
    /// The newest copy the worker has not started yet. A newer copy replaces
    /// it, so a copy superseded before it started is never written.
    pending: Option<Request>,
    /// The generation of the newest copy handed in. The worker writes a copy
    /// only while it is still the newest, so a stale one is dropped rather
    /// than written over a newer selection.
    latest: u64,
    /// The worker is opening the clipboard or inside a write.
    busy: bool,
    /// The deck is shutting the clipboard down.
    closing: bool,
    /// The worker has stopped: the clipboard failed to open, it panicked, or
    /// it was shut down.
    gone: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Slot> {
        // Nothing panics while holding the lock, but a poisoned lock must not
        // take the copy path down with it.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }
}

enum NativeState {
    /// Not opened yet; the opener runs on the first write.
    NotStarted(NativeOpener),
    Running {
        shared: Arc<Shared>,
        /// Disconnects when the worker thread has finished, after it has
        /// dropped the clipboard (which is when arboard's X11 backend hands
        /// its contents to a clipboard manager).
        finished: mpsc::Receiver<()>,
    },
    /// Turned off, failed to open, or the worker is gone. Settled for the
    /// rest of the deck's life, so a headless session does not try to open a
    /// clipboard again on every copy.
    Unavailable,
}

thread_local! {
    /// Set for the whole life of the native clipboard's worker thread. The
    /// `run_tui` panic hook reads it through [`in_clipboard_worker`], so a
    /// panic on that thread, which the worker catches and settles as
    /// [`NativeWrite::Unavailable`], does not tear down the terminal the TUI
    /// is still drawing on.
    static IN_CLIPBOARD_WORKER: Cell<bool> = const { Cell::new(false) };
}

thread_local! {
    /// Where the clipboard worker's panic happened, as the `run_tui` panic
    /// hook noted it through [`note_clipboard_worker_panic`] before leaving
    /// the terminal alone. The worker takes it when it catches the panic, to
    /// log it; it stays empty under a hook that does not note it.
    static WORKER_PANIC_LOCATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Note where a panic on the clipboard worker happened, for the warning the
/// worker logs when it catches that panic. Called by the `run_tui` panic hook,
/// on the panicking thread, in place of the terminal teardown.
#[cfg_attr(panic = "abort", allow(dead_code))]
pub(crate) fn note_clipboard_worker_panic(info: &std::panic::PanicHookInfo<'_>) {
    if let Some(location) = info.location() {
        WORKER_PANIC_LOCATION.with(|noted| *noted.borrow_mut() = Some(location.to_string()));
    }
}

/// The warning logged when the clipboard worker catches a panic: what the
/// panic said, where it happened when that was noted, and that native writes
/// are off for the rest of the session. The message is quoted with its
/// escapes, so a payload carrying line breaks or terminal controls cannot
/// forge a log line.
fn worker_panic_warning(payload: &(dyn std::any::Any + Send), location: Option<&str>) -> String {
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<a panic payload that is not text>");
    let at = location.map(|l| format!(" at {l}")).unwrap_or_default();
    format!(
        "the native clipboard thread panicked{at}: {message:?}; native clipboard writes are \
         disabled for the rest of this session, and copies go out as OSC 52 only"
    )
}

/// The warning for a panic the clipboard worker caught, on the worker
/// thread: [`worker_panic_warning`] with the location the panic hook noted,
/// which it takes so a later panic is not reported at this one's location.
fn caught_worker_panic(payload: &(dyn std::any::Any + Send)) -> String {
    let location = WORKER_PANIC_LOCATION.with(|noted| noted.borrow_mut().take());
    worker_panic_warning(payload, location.as_deref())
}

/// True on the native clipboard's worker thread. See [`IN_CLIPBOARD_WORKER`].
///
/// The `run_tui` hook reads it only under `panic = "unwind"`, for the reason
/// it reads `embedded_pane::in_guarded_parser_feed`; hence the conditional
/// `dead_code` allowance.
#[cfg_attr(panic = "abort", allow(dead_code))]
pub(crate) fn in_clipboard_worker() -> bool {
    IN_CLIPBOARD_WORKER.with(Cell::get)
}

/// The native half of a copy, kept off the UI thread.
///
/// The clipboard is opened lazily, on a dedicated thread, at the first copy,
/// and that thread keeps it until the deck shuts it down: on X11 and Wayland
/// the copied text is served by the process that owns the selection, so a
/// clipboard object dropped right after the write would take the copy with
/// it. Opening and writing can block (an X server that is slow or
/// unreachable), so the UI thread waits at most [`NATIVE_WRITE_WAIT`] for an
/// answer and does not wait at all while an earlier write is still running.
///
/// While a write is stuck, later copies do not queue up behind it: one slot
/// holds the newest copy not yet started, and a newer copy replaces it, so
/// when the clipboard answers again the newest selection is the one written
/// and the superseded ones are dropped. A write already in progress cannot be
/// stopped; it finishes when the backend returns, before the newest copy is
/// written.
pub struct NativeClipboard {
    state: NativeState,
    wait: Duration,
}

impl NativeClipboard {
    /// The deck's native clipboard, honouring [`NATIVE_CLIPBOARD_ENV`].
    pub fn system() -> Self {
        let enabled = native_enabled(std::env::var(NATIVE_CLIPBOARD_ENV).ok().as_deref());
        if enabled {
            Self::with_opener(Box::new(open_system_clipboard), NATIVE_WRITE_WAIT)
        } else {
            Self::off()
        }
    }

    /// A native clipboard that is never opened.
    pub fn off() -> Self {
        Self {
            state: NativeState::Unavailable,
            wait: NATIVE_WRITE_WAIT,
        }
    }

    /// A native clipboard opened by `opener` on its first write. The seam
    /// tests use to stand in a fake sink.
    pub fn with_opener(opener: NativeOpener, wait: Duration) -> Self {
        Self {
            state: NativeState::NotStarted(opener),
            wait,
        }
    }

    fn start(&mut self) {
        if !matches!(self.state, NativeState::NotStarted(_)) {
            return;
        }
        let NativeState::NotStarted(opener) =
            std::mem::replace(&mut self.state, NativeState::Unavailable)
        else {
            unreachable!("checked just above");
        };
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
        });
        let (finished_tx, finished) = mpsc::channel::<()>();
        let worker_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name(NATIVE_THREAD_NAME.into())
            .spawn(move || {
                IN_CLIPBOARD_WORKER.with(|flag| flag.set(true));
                let served =
                    std::panic::catch_unwind(AssertUnwindSafe(|| serve(&worker_shared, opener)));
                if let Err(payload) = served {
                    tracing::warn!("{}", caught_worker_panic(&*payload));
                }
                let mut slot = worker_shared.lock();
                slot.gone = true;
                // Its reply drops with it, which tells a waiting copy.
                slot.pending = None;
                drop(slot);
                // The clipboard was dropped inside `serve` (or by the
                // unwind), so this says the exit handoff is over.
                drop(finished_tx);
            });
        if spawned.is_ok() {
            self.state = NativeState::Running { shared, finished };
        }
    }

    pub fn write(&mut self, text: &str) -> NativeWrite {
        self.start();
        let NativeState::Running { shared, .. } = &self.state else {
            return NativeWrite::Unavailable;
        };
        let (reply, answer) = mpsc::sync_channel(1);
        let backlog = {
            let mut slot = shared.lock();
            if slot.gone {
                drop(slot);
                self.state = NativeState::Unavailable;
                return NativeWrite::Unavailable;
            }
            slot.latest += 1;
            let backlog = slot.busy || slot.pending.is_some();
            // Replaces (and so drops) any older copy that had not started.
            slot.pending = Some(Request {
                generation: slot.latest,
                text: text.to_owned(),
                reply,
            });
            backlog
        };
        shared.wake.notify_all();
        if backlog {
            // An earlier write is still running: this copy waits in the slot
            // for it, and the UI does not.
            return NativeWrite::NotConfirmed;
        }
        match answer.recv_timeout(self.wait) {
            Ok(NativeWrite::Unavailable) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Failed to open, or the worker died: settle it.
                self.state = NativeState::Unavailable;
                NativeWrite::Unavailable
            }
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => NativeWrite::NotConfirmed,
        }
    }

    /// Stop the worker and wait up to `deadline` for it to finish, which
    /// includes dropping the clipboard. On X11, arboard's drop is when a
    /// clipboard manager is asked to take over what the deck copied, so a copy
    /// can outlive the deck; waiting here keeps the process from exiting in
    /// the middle of that. A copy not yet started is dropped, not written.
    ///
    /// Returns `true` when the worker finished (or never ran), `false` when
    /// the deadline passed first, as with an X server that stopped answering:
    /// the deck then exits without the handoff. Idempotent.
    pub fn shutdown(&mut self, deadline: Duration) -> bool {
        let state = std::mem::replace(&mut self.state, NativeState::Unavailable);
        let NativeState::Running { shared, finished } = state else {
            return true;
        };
        {
            let mut slot = shared.lock();
            slot.closing = true;
            slot.pending = None;
        }
        shared.wake.notify_all();
        !matches!(
            finished.recv_timeout(deadline),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    }
}

impl Drop for NativeClipboard {
    /// The backstop for a way out of the TUI that skips its explicit
    /// shutdown; bounded like it.
    fn drop(&mut self) {
        self.shutdown(NATIVE_SHUTDOWN_WAIT);
    }
}

/// Whether the value of [`NATIVE_CLIPBOARD_ENV`] leaves the native write on.
fn native_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// Wait for the next copy to write, marking the worker busy; `None` once the
/// deck shuts the clipboard down.
fn next_request(shared: &Shared) -> Option<Request> {
    let mut slot = shared.lock();
    slot.busy = false;
    loop {
        if slot.closing {
            return None;
        }
        if let Some(request) = slot.pending.take() {
            slot.busy = true;
            return Some(request);
        }
        slot = shared.wake.wait(slot).unwrap_or_else(|e| e.into_inner());
    }
}

/// Whether `request` is still the newest copy, checked just before it is
/// written.
fn still_newest(shared: &Shared, request: &Request) -> bool {
    let slot = shared.lock();
    !slot.closing && slot.latest == request.generation
}

/// The worker thread: open the clipboard at the first copy, then write copies
/// until the deck shuts it down. On a failed open it answers that copy
/// `Unavailable` and returns, which the UI side records.
fn serve(shared: &Shared, opener: NativeOpener) {
    let Some(first) = next_request(shared) else {
        return;
    };
    let Some(mut sink) = opener() else {
        let _ = first.reply.try_send(NativeWrite::Unavailable);
        return;
    };
    let mut next = Some(first);
    while let Some(request) = next.take().or_else(|| next_request(shared)) {
        // Superseded while the clipboard was opening or the last write ran:
        // dropping it closes its reply, and the newer copy is in the slot.
        if !still_newest(shared, &request) {
            continue;
        }
        let result = match sink.write_text(&request.text) {
            Ok(()) => NativeWrite::Written,
            Err(error) => {
                tracing::debug!(%error, "native clipboard refused a write");
                NativeWrite::Failed
            }
        };
        // Not busy any more BEFORE the reply goes out: a copy the UI makes as
        // soon as this one is answered must find the worker idle and wait for
        // its own answer, not be taken for a backlog and reported unconfirmed.
        shared.lock().busy = false;
        // The UI side may have stopped waiting; a closed reply is fine.
        let _ = request.reply.try_send(result);
    }
    // Dropping the clipboard here is arboard's exit handoff on X11.
    drop(sink);
}

/// The deck's clipboard: [`NativeClipboard::system`] plus an OSC 52 sender.
pub struct SystemClipboard {
    native: NativeClipboard,
    osc52: fn(&str) -> bool,
}

impl SystemClipboard {
    /// `osc52` writes the sequence to the user's terminal and reports
    /// whether it was written.
    pub fn new(osc52: fn(&str) -> bool) -> Self {
        Self {
            native: NativeClipboard::system(),
            osc52,
        }
    }
}

impl SystemClipboard {
    /// Shut the native clipboard down on an ordinary exit, waiting at most
    /// [`NATIVE_SHUTDOWN_WAIT`]; see [`NativeClipboard::shutdown`]. Keeping
    /// the copy past the exit is best effort.
    pub fn shutdown(&mut self) -> bool {
        self.native.shutdown(NATIVE_SHUTDOWN_WAIT)
    }
}

impl ClipboardBackend for SystemClipboard {
    fn write_native(&mut self, text: &str) -> NativeWrite {
        self.native.write(text)
    }

    fn send_osc52(&mut self, text: &str) -> bool {
        (self.osc52)(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spec::spec;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// A backend that records what it was asked to send and answers as told.
    struct FakeBackend {
        native: NativeWrite,
        osc52: bool,
        native_writes: Vec<String>,
        osc52_writes: Vec<String>,
    }

    impl FakeBackend {
        fn new(native: NativeWrite, osc52: bool) -> Self {
            Self {
                native,
                osc52,
                native_writes: Vec::new(),
                osc52_writes: Vec::new(),
            }
        }
    }

    impl ClipboardBackend for FakeBackend {
        fn write_native(&mut self, text: &str) -> NativeWrite {
            self.native_writes.push(text.to_owned());
            self.native
        }

        fn send_osc52(&mut self, text: &str) -> bool {
            self.osc52_writes.push(text.to_owned());
            self.osc52
        }
    }

    /// Scenario: A selection is copied while the native clipboard accepts the write. Both the native write and the OSC 52 sequence carry the text, and the status line says it was copied to the clipboard.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_a_confirmed_native_write_says_copied() {
        let mut backend = FakeBackend::new(NativeWrite::Written, true);
        let outcome = deliver(&mut backend, "hello");
        assert_eq!(backend.native_writes, ["hello"]);
        assert_eq!(backend.osc52_writes, ["hello"], "OSC 52 is sent every time");
        assert_eq!(status_message(outcome), Some(COPIED_MESSAGE));
    }

    /// Scenario: A selection is copied where no native clipboard exists, the headless or ssh case, or where the native write failed. Only the OSC 52 sequence went out, so the status line says it was sent to the terminal and points at Troubleshooting instead of claiming a copy.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_osc52_only_says_sent_to_the_terminal() {
        for native in [NativeWrite::Unavailable, NativeWrite::Failed] {
            let mut backend = FakeBackend::new(native, true);
            let message = status_message(deliver(&mut backend, "hello"));
            assert_eq!(message, Some(OSC52_ONLY_MESSAGE), "native {native:?}");
            let message = message.unwrap();
            assert!(!message.contains("Copied"), "{message}");
            assert!(message.contains("Troubleshooting"), "{message}");
        }
    }

    /// Scenario: A selection is copied while the native clipboard write is still running when the status line is chosen, and the OSC 52 sequence went out. The status line says it was sent to the terminal and that the clipboard write is still pending, neither claiming the copy nor saying it failed, because the native write may still land.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_an_unconfirmed_native_write_with_osc52_says_pending() {
        let mut backend = FakeBackend::new(NativeWrite::NotConfirmed, true);
        let message = status_message(deliver(&mut backend, "hello"));
        assert_eq!(message, Some(OSC52_PENDING_MESSAGE));
        let message = message.unwrap();
        assert!(message.contains("Sent to terminal clipboard"), "{message}");
        assert!(message.contains("pending"), "{message}");
        assert!(!message.contains("Copied"), "{message}");
        assert!(!message.contains("Not copied"), "{message}");
    }

    /// Scenario: A copy finds neither a native clipboard that accepted it nor a terminal to write OSC 52 to. The status line says nothing was copied rather than claiming a copy.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_nothing_sent_claims_no_copy() {
        for native in [NativeWrite::Unavailable, NativeWrite::Failed] {
            let mut backend = FakeBackend::new(native, false);
            let message = status_message(deliver(&mut backend, "hello"));
            assert_eq!(message, Some(NOT_COPIED_MESSAGE), "native {native:?}");
        }
    }

    /// Scenario: A copy finds no terminal to write OSC 52 to while the native clipboard write is still running. The status line says the clipboard write is not confirmed yet, not that nothing was copied, because the native write may still land.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_an_unconfirmed_native_write_alone_is_not_called_a_failure() {
        let mut backend = FakeBackend::new(NativeWrite::NotConfirmed, false);
        let message = status_message(deliver(&mut backend, "hello"));
        assert_eq!(message, Some(NOT_CONFIRMED_MESSAGE));
        let message = message.unwrap();
        assert!(!message.contains("Copied"), "{message}");
        assert!(!message.contains("Not copied"), "{message}");
    }

    /// Scenario: An empty selection is released. Nothing is written by either route and no status message appears.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_an_empty_selection_sends_nothing_and_says_nothing() {
        let mut backend = FakeBackend::new(NativeWrite::Written, true);
        let outcome = deliver(&mut backend, "");
        assert_eq!(outcome, None);
        assert!(backend.native_writes.is_empty());
        assert!(backend.osc52_writes.is_empty());
        assert_eq!(status_message(outcome), None);
    }

    /// A sink that records writes, and optionally blocks each one until
    /// released.
    struct FakeSink {
        written: Arc<Mutex<Vec<String>>>,
        gate: Option<mpsc::Receiver<()>>,
        fail: bool,
        /// Told the text of each write as the write starts.
        entered: Option<mpsc::Sender<String>>,
    }

    impl NativeSink for FakeSink {
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            if let Some(entered) = &self.entered {
                let _ = entered.send(text.to_owned());
            }
            if let Some(gate) = &self.gate {
                let _ = gate.recv();
            }
            if self.fail {
                return Err("refused".into());
            }
            self.written.lock().unwrap().push(text.to_owned());
            Ok(())
        }
    }

    fn counting_opener(opens: &Arc<AtomicUsize>, sink: Option<FakeSink>) -> NativeOpener {
        let opens = Arc::clone(opens);
        Box::new(move || {
            opens.fetch_add(1, Ordering::SeqCst);
            sink.map(|s| Box::new(s) as Box<dyn NativeSink>)
        })
    }

    const LONG: Duration = Duration::from_secs(10);

    /// Scenario: The native clipboard opens on the first copy and is kept for later ones. Two copies are both written to the one clipboard object, which is opened exactly once.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_the_clipboard_is_opened_once_and_kept() {
        let opens = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(Mutex::new(Vec::new()));
        let sink = FakeSink {
            written: Arc::clone(&written),
            gate: None,
            fail: false,
            entered: None,
        };
        let mut native = NativeClipboard::with_opener(counting_opener(&opens, Some(sink)), LONG);
        assert_eq!(opens.load(Ordering::SeqCst), 0, "opened lazily");
        assert_eq!(native.write("one"), NativeWrite::Written);
        assert_eq!(native.write("two"), NativeWrite::Written);
        assert_eq!(opens.load(Ordering::SeqCst), 1);
        assert_eq!(*written.lock().unwrap(), ["one", "two"]);
    }

    /// Scenario: The native clipboard cannot be opened, as in a headless session. The first copy reports it unavailable, and later copies report the same without trying to open it again.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_a_failed_open_is_settled_not_retried() {
        let opens = Arc::new(AtomicUsize::new(0));
        let mut native = NativeClipboard::with_opener(counting_opener(&opens, None), LONG);
        for _ in 0..3 {
            assert_eq!(native.write("text"), NativeWrite::Unavailable);
        }
        assert_eq!(opens.load(Ordering::SeqCst), 1);
    }

    /// Scenario: The native clipboard refuses a write. The copy reports the write failed, so the status line falls back to the OSC 52 wording.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_a_refused_write_reports_failed() {
        let opens = Arc::new(AtomicUsize::new(0));
        let sink = FakeSink {
            written: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            fail: true,
            entered: None,
        };
        let mut native = NativeClipboard::with_opener(counting_opener(&opens, Some(sink)), LONG);
        assert_eq!(native.write("text"), NativeWrite::Failed);
    }

    /// Scenario: The native write blocks, as an unresponsive X server can. The copy stops waiting after its bound and reports the write unconfirmed, a further copy behind the stuck one is not queued indefinitely, and once the clipboard answers again writes are confirmed.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_a_blocked_write_does_not_hold_the_ui() {
        let opens = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = mpsc::channel();
        let sink = FakeSink {
            written: Arc::clone(&written),
            gate: Some(gate),
            fail: false,
            entered: None,
        };
        let wait = Duration::from_millis(20);
        let mut native = NativeClipboard::with_opener(counting_opener(&opens, Some(sink)), wait);
        // The worker blocks on the first write it takes; the copies behind it
        // either wait in the one queue slot or find it full. Every one of them
        // comes back unconfirmed while the gate is shut, and none is held
        // until the gate opens (which would deadlock this test).
        for text in ["a", "b", "c"] {
            assert_eq!(native.write(text), NativeWrite::NotConfirmed, "{text}");
        }
        assert!(written.lock().unwrap().is_empty());
        // Open the gate for good: dropping the sender makes every later
        // `recv` return at once. A later write is then confirmed.
        drop(release);
        native.wait = LONG;
        let deadline = std::time::Instant::now() + LONG;
        loop {
            match native.write("d") {
                NativeWrite::Written => break,
                // The queue slot can still hold "b" for a moment.
                NativeWrite::NotConfirmed if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                other => panic!("expected the write to land, got {other:?}"),
            }
        }
        let written = written.lock().unwrap();
        assert_eq!(written.first().map(String::as_str), Some("a"));
        assert_eq!(written.last().map(String::as_str), Some("d"));
    }

    /// Scenario: The native-clipboard environment knob is read. `0`, `false`, `off` and `no` turn the native write off; unset or any other value leaves it on.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_the_env_knob_turns_the_native_write_off() {
        for off in ["0", "false", "OFF", " no "] {
            assert!(!native_enabled(Some(off)), "{off:?}");
        }
        for on in [None, Some("1"), Some("true"), Some("")] {
            assert!(native_enabled(on), "{on:?}");
        }
        let mut native = NativeClipboard::off();
        assert_eq!(native.write("text"), NativeWrite::Unavailable);
    }

    /// Scenario: The native write blocks on a first copy, A, and two more copies, B and then C, are made while it is stuck. Once the clipboard answers again, A finishes, B (superseded before it started) is never written, and C, the newest copy, is what the clipboard ends up holding.
    #[spec("clipboard/native/001")]
    #[test]
    fn native_001_the_newest_copy_wins_after_a_stuck_write() {
        let opens = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(Mutex::new(Vec::new()));
        let (release, gate) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        let sink = FakeSink {
            written: Arc::clone(&written),
            gate: Some(gate),
            fail: false,
            entered: Some(entered_tx),
        };
        let wait = Duration::from_millis(20);
        let mut native = NativeClipboard::with_opener(counting_opener(&opens, Some(sink)), wait);
        assert_eq!(native.write("A"), NativeWrite::NotConfirmed);
        // A is in flight: the worker is inside the sink's write.
        assert_eq!(entered.recv_timeout(LONG).as_deref(), Ok("A"));
        assert_eq!(native.write("B"), NativeWrite::NotConfirmed);
        assert_eq!(native.write("C"), NativeWrite::NotConfirmed);
        // The clipboard recovers.
        drop(release);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !written.lock().unwrap().iter().any(|t| t == "C")
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        // Let any stale write that was still going to happen happen.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            *written.lock().unwrap(),
            ["A", "C"],
            "the in-flight write finishes, the superseded one is dropped, the newest lands last"
        );
    }

    /// A sink whose `Drop` stands in for arboard's X11 clipboard-manager
    /// handoff, which runs when the clipboard is dropped: it waits for
    /// `gate` (or `delay`) and then records that it finished.
    struct HandoffSink {
        finished: Arc<std::sync::atomic::AtomicBool>,
        delay: Duration,
        gate: Option<mpsc::Receiver<()>>,
    }

    impl NativeSink for HandoffSink {
        fn write_text(&mut self, _text: &str) -> Result<(), String> {
            Ok(())
        }
    }

    impl Drop for HandoffSink {
        fn drop(&mut self) {
            std::thread::sleep(self.delay);
            if let Some(gate) = &self.gate {
                let _ = gate.recv();
            }
            self.finished.store(true, Ordering::SeqCst);
        }
    }

    /// Scenario: The deck exits after a copy and the clipboard's exit handoff takes a moment to finish. Shutting the native clipboard down waits for that handoff, so it has completed by the time the shutdown returns.
    #[spec("clipboard/native/002")]
    #[test]
    fn native_002_exit_waits_for_the_clipboard_handoff() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sink = HandoffSink {
            finished: Arc::clone(&finished),
            delay: Duration::from_millis(50),
            gate: None,
        };
        let mut native = NativeClipboard::with_opener(
            Box::new(move || Some(Box::new(sink) as Box<dyn NativeSink>)),
            LONG,
        );
        assert_eq!(native.write("text"), NativeWrite::Written);
        assert!(
            native.shutdown(Duration::from_secs(5)),
            "the handoff completed"
        );
        assert!(finished.load(Ordering::SeqCst));
        assert!(
            NATIVE_SHUTDOWN_WAIT <= Duration::from_millis(300),
            "exit waits at most a short, bounded time"
        );
    }

    /// Scenario: The deck exits while the clipboard's exit handoff never completes, as with an X server that stopped answering. The shutdown gives up after its deadline and returns, so the exit does not hang.
    #[spec("clipboard/native/002")]
    #[test]
    fn native_002_exit_does_not_hang_on_a_stuck_handoff() {
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, gate) = mpsc::channel::<()>();
        let sink = HandoffSink {
            finished: Arc::clone(&finished),
            delay: Duration::ZERO,
            gate: Some(gate),
        };
        let mut native = NativeClipboard::with_opener(
            Box::new(move || Some(Box::new(sink) as Box<dyn NativeSink>)),
            LONG,
        );
        assert_eq!(native.write("text"), NativeWrite::Written);
        let started = std::time::Instant::now();
        assert!(
            !native.shutdown(Duration::from_millis(100)),
            "a handoff that never finishes is reported as not finished"
        );
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
        assert!(!finished.load(Ordering::SeqCst));
        drop(release);
    }

    struct PanickingSink;

    impl NativeSink for PanickingSink {
        fn write_text(&mut self, _text: &str) -> Result<(), String> {
            panic!("deliberate panic in a fake clipboard write");
        }
    }

    /// Scenario: Opening the native clipboard panics on its worker thread, as arboard's X11 constructor can when it cannot start its own thread, and so does a write. The deck's panic hook leaves the terminal alone for those panics, the warning the worker logs for such a panic names its message, where it happened and that native clipboard writes are off for the session, and the copies report the native clipboard unavailable while a panic on any other thread still gets the teardown.
    #[spec("clipboard/native/003")]
    #[test]
    fn native_003_a_clipboard_thread_panic_leaves_the_terminal_alone() {
        let torn_down = Arc::new(Mutex::new(Vec::<String>::new()));
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(crate::ui::tui_panic_hook({
            let torn_down = Arc::clone(&torn_down);
            move |_info| {
                let name = std::thread::current().name().unwrap_or_default().to_owned();
                torn_down
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(name);
            }
        })));

        // Control: a panic on an ordinary thread still reaches the teardown.
        let control = std::thread::Builder::new()
            .name("not-the-clipboard".into())
            .spawn(|| panic!("deliberate control panic"))
            .unwrap()
            .join();
        assert!(control.is_err());

        let mut opener_panics = NativeClipboard::with_opener(
            Box::new(|| panic!("deliberate panic in a fake clipboard open")),
            LONG,
        );
        assert_eq!(opener_panics.write("text"), NativeWrite::Unavailable);
        assert_eq!(opener_panics.write("again"), NativeWrite::Unavailable);

        let mut write_panics = NativeClipboard::with_opener(
            Box::new(|| Some(Box::new(PanickingSink) as Box<dyn NativeSink>)),
            LONG,
        );
        assert_eq!(write_panics.write("text"), NativeWrite::Unavailable);
        assert_eq!(write_panics.write("again"), NativeWrite::Unavailable);
        // The warning the worker logs for a panic it caught, built as the
        // worker builds it, on a thread marked as the worker, under this hook.
        let warning = std::thread::Builder::new()
            .name(NATIVE_THREAD_NAME.into())
            .spawn(|| {
                IN_CLIPBOARD_WORKER.with(|flag| flag.set(true));
                let line = line!() + 2;
                let payload = std::panic::catch_unwind(|| {
                    panic!("deliberate panic in a fake clipboard open");
                })
                .expect_err("it panicked");
                (line, caught_worker_panic(&*payload))
            })
            .unwrap()
            .join()
            .unwrap();

        std::panic::set_hook(previous);
        let torn_down = torn_down.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            torn_down.iter().any(|n| n == "not-the-clipboard"),
            "control: {torn_down:?}"
        );
        assert!(
            !torn_down.iter().any(|n| n == NATIVE_THREAD_NAME),
            "the clipboard thread's panics tore the terminal down: {torn_down:?}"
        );

        let (line, warning) = warning;
        assert!(
            warning.contains(&format!("panicked at {}:{line}:", file!()))
                && warning.contains("\"deliberate panic in a fake clipboard open\"")
                && warning.contains("native clipboard writes are disabled"),
            "the warning names the panic, where it happened and that native writes are off: \
             {warning}"
        );
    }

    #[test]
    fn a_worker_panic_warning_quotes_any_text_payload() {
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("line\nWARN forged"));
        let warning = worker_panic_warning(&*owned, Some("src/x.rs:1:2"));
        assert!(
            warning.contains("panicked at src/x.rs:1:2: \"line\\nWARN forged\";"),
            "{warning}"
        );
        assert!(!warning.contains('\n'), "{warning}");
        let other: Box<dyn std::any::Any + Send> = Box::new(7_u8);
        let warning = worker_panic_warning(&*other, None);
        assert!(
            warning.starts_with("the native clipboard thread panicked: \"<a panic payload"),
            "{warning}"
        );
    }
}
