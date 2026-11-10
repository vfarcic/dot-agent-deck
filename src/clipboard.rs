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

use std::sync::mpsc;
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
/// Status line when only the OSC 52 sequence is known to have gone out.
pub const OSC52_ONLY_MESSAGE: &str =
    "Sent to terminal clipboard (OSC 52); if paste is empty, see Troubleshooting";
/// Status line when neither route sent anything.
pub const NOT_COPIED_MESSAGE: &str = "Not copied: no clipboard was reachable";

/// The status line for a copy, derived only from what [`deliver`] reports.
///
/// It claims a copy only when the native write confirmed one. When only the
/// OSC 52 sequence went out, it says so and points at the troubleshooting
/// page, because whether that lands depends on the user's terminal. `None`
/// (nothing to copy) yields no message at all.
pub fn status_message(outcome: Option<CopyOutcome>) -> Option<&'static str> {
    let outcome = outcome?;
    Some(match outcome {
        CopyOutcome {
            native: NativeWrite::Written,
            ..
        } => COPIED_MESSAGE,
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

struct Request {
    text: String,
    reply: mpsc::SyncSender<NativeWrite>,
}

enum NativeState {
    /// Not opened yet; the opener runs on the first write.
    NotStarted(NativeOpener),
    Running(mpsc::SyncSender<Request>),
    /// Turned off, failed to open, or the worker is gone. Settled for the
    /// rest of the deck's life, so a headless session does not try to open a
    /// clipboard again on every copy.
    Unavailable,
}

/// The native half of a copy, kept off the UI thread.
///
/// The clipboard is opened lazily, on a dedicated thread, at the first copy,
/// and that thread keeps it for the deck's lifetime: on X11 and Wayland the
/// copied text is served by the process that owns the selection, so a
/// clipboard object dropped right after the write would take the copy with
/// it. Opening and writing can block (an X server that is slow or
/// unreachable), so the UI thread waits at most [`NATIVE_WRITE_WAIT`] for an
/// answer and never queues more than one write behind a stuck one.
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

    pub fn write(&mut self, text: &str) -> NativeWrite {
        if matches!(self.state, NativeState::NotStarted(_)) {
            let NativeState::NotStarted(opener) =
                std::mem::replace(&mut self.state, NativeState::Unavailable)
            else {
                unreachable!("checked just above");
            };
            // Capacity 1: one write may wait behind the one being served.
            let (tx, rx) = mpsc::sync_channel::<Request>(1);
            let spawned = std::thread::Builder::new()
                .name("native-clipboard".into())
                .spawn(move || serve(rx, opener));
            if spawned.is_ok() {
                self.state = NativeState::Running(tx);
            }
        }
        let NativeState::Running(tx) = &self.state else {
            return NativeWrite::Unavailable;
        };
        let (reply, answer) = mpsc::sync_channel(1);
        let request = Request {
            text: text.to_owned(),
            reply,
        };
        match tx.try_send(request) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => return NativeWrite::NotConfirmed,
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.state = NativeState::Unavailable;
                return NativeWrite::Unavailable;
            }
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
}

/// Whether the value of [`NATIVE_CLIPBOARD_ENV`] leaves the native write on.
fn native_enabled(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

/// The worker thread: open the clipboard at the first request, then serve
/// writes until the deck drops its [`NativeClipboard`]. On a failed open it
/// answers that request `Unavailable` and exits, which the UI side records.
fn serve(requests: mpsc::Receiver<Request>, opener: NativeOpener) {
    let Ok(first) = requests.recv() else {
        return;
    };
    let Some(mut sink) = opener() else {
        let _ = first.reply.try_send(NativeWrite::Unavailable);
        return;
    };
    for request in std::iter::once(first).chain(requests.iter()) {
        let result = match sink.write_text(&request.text) {
            Ok(()) => NativeWrite::Written,
            Err(error) => {
                tracing::debug!(%error, "native clipboard refused a write");
                NativeWrite::Failed
            }
        };
        // The UI side may have stopped waiting; a closed reply is fine.
        let _ = request.reply.try_send(result);
    }
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

    /// Scenario: A selection is copied where no native clipboard exists, the headless or ssh case, or where the native write failed or did not answer in time. Only the OSC 52 sequence went out, so the status line says it was sent to the terminal and points at Troubleshooting instead of claiming a copy.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_osc52_only_says_sent_to_the_terminal() {
        for native in [
            NativeWrite::Unavailable,
            NativeWrite::Failed,
            NativeWrite::NotConfirmed,
        ] {
            let mut backend = FakeBackend::new(native, true);
            let message = status_message(deliver(&mut backend, "hello"));
            assert_eq!(message, Some(OSC52_ONLY_MESSAGE), "native {native:?}");
            let message = message.unwrap();
            assert!(!message.contains("Copied"), "{message}");
            assert!(message.contains("Troubleshooting"), "{message}");
        }
    }

    /// Scenario: A copy finds neither a native clipboard nor a terminal to write OSC 52 to. The status line says nothing was copied rather than claiming a copy.
    #[spec("clipboard/deliver/001")]
    #[test]
    fn deliver_001_nothing_sent_claims_no_copy() {
        let mut backend = FakeBackend::new(NativeWrite::Unavailable, false);
        let message = status_message(deliver(&mut backend, "hello"));
        assert_eq!(message, Some(NOT_COPIED_MESSAGE));
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
    }

    impl NativeSink for FakeSink {
        fn write_text(&mut self, text: &str) -> Result<(), String> {
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
}
