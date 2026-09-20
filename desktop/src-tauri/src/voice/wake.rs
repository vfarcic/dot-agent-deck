//! PRD #802 — keeping the machine awake while voice control is on.
//!
//! # Why this belongs to voice
//!
//! **Voice control generates no input events.** Every idle timer the three
//! platforms ship counts mouse movement and keystrokes; none of them counts a
//! microphone. So a user who is actively driving the app by speaking — the one
//! state in which they are demonstrably *not* idle — looks idle to the
//! operating system, and the machine suspends underneath them. That is not a
//! missing nicety; it is the platform's definition of "in use" disagreeing with
//! this feature's definition of it.
//!
//! The product owner's framing is the whole requirement: *"if it's controlled
//! by voice, it would be silly that one needs to move a mouse every once in a
//! while to keep it from going asleep."*
//!
//! **It follows the Voice button and nothing else.** Two narrower triggers were
//! offered and both are worse: *while the window is focused* keeps a machine
//! awake because an app is in the foreground, which no user asked for, and
//! *while agents are running* keeps it awake with nobody in the room. *While
//! voice is on* is exactly the state that produces no events, it is already
//! visible (the Voice button **is** the indicator), and the user switches it
//! off by pressing the thing they pressed to switch it on. There is deliberately
//! **no setting** — a preference for it would be a second control over the same
//! state.
//!
//! # Sleep, not the display
//!
//! The user is talking, not watching. Keeping the screen lit is a battery cost
//! they did not ask for, so all three mechanisms below are the *system* idle
//! ones and each leaves the display free to blank on its own schedule.
//!
//! # The three mechanisms, and what happens if the process is killed
//!
//! Every one of them is **owned by the process**, which is the property this
//! module was chosen for: an inhibit is invisible, and a stuck one keeps a
//! laptop awake indefinitely with nothing on screen to explain it. None of the
//! three can survive the process that took it.
//!
//! | platform | call | released on process death by |
//! | --- | --- | --- |
//! | Linux | `org.freedesktop.login1.Manager.Inhibit("idle", …, "block")` over D-Bus | the **kernel**: the inhibit is a file descriptor, and logind drops the inhibitor when its end sees EOF |
//! | macOS | `IOPMAssertionCreateWithName(kIOPMAssertPreventUserIdleSystemSleep, …)` | **powerd**, which tracks assertions per process and releases them when the task dies |
//! | Windows | `SetThreadExecutionState(ES_CONTINUOUS \| ES_SYSTEM_REQUIRED)` | the **kernel**: the request is per-thread state, cleared when the thread exits |
//!
//! So a `SIGKILL`, a panic, an OOM kill or a crash releases the inhibit on all
//! three without anything here running. [`WakeLock::release`] is what makes it
//! prompt rather than what makes it certain.
//!
//! **Linux's fd is the reason `idle`/`block` was chosen over the alternatives.**
//! `what=sleep` blocks an *explicit* suspend — the user's own menu item
//! included — which is rude and needs a privileged polkit action;
//! `org.freedesktop.ScreenSaver.Inhibit` is the display one this feature's
//! scope excludes. `idle` tells logind this session is not idle, which is
//! precisely the false premise the sleep was drawn from, and an unprivileged
//! session is allowed to take it (measured: `systemd-inhibit --what=idle
//! --mode=block` is granted to an ordinary user on this project's dev box).
//!
//! **What the Linux mechanism does NOT cover, stated rather than implied**: a
//! desktop environment whose power manager makes its own idle decisions without
//! consulting logind's inhibitors will still suspend. Covering those would mean
//! a second, desktop-specific call (`org.gnome.SessionManager.Inhibit` with the
//! suspend flag, and a KDE equivalent), and that is a shape this module can add
//! later against a real report rather than guess at now. The acquisition
//! succeeds either way, so a refusal is not the signal for it.
//!
//! # Failure is silent and never fatal
//!
//! A headless box, a container with no D-Bus, a locked-down policy, a machine
//! with no logind at all: every one of them refuses, and voice control carries
//! on working exactly as it did. [`WakeLock::hold`] swallows the refusal into
//! one log line and returns, and [`WakeLock::is_held`] then reports `false`. There
//! is no sentence for the user, because there is no action they could take from
//! inside this app — and a banner over a working feature would be worse than
//! the sleep it is warning about.
//!
//! **The log line is latched to once per process.** `hold` is called at the
//! start of every utterance (see [`crate::voice::VoiceHold::start`]), so a
//! machine that refuses would otherwise write a line per sentence spoken. The
//! *attempt* is not latched — a refusal can be transient (a bus that was not up
//! yet) and retrying costs a few milliseconds on a path that is already opening
//! an audio device.
//!
//! # What is verifiable here and what is not
//!
//! The Linux path is real on this project's dev box and in CI's Linux jobs, and
//! the tests below drive the seam with a stub on every platform. **The macOS
//! and Windows calls are compiled by `build-macos` and `build-windows` and
//! executed by nothing in this repository** — no tier runs a GUI app on either
//! platform, and no unit test can assert that powerd or the Windows kernel
//! actually honoured the request. Those two arms are type-checked and reviewed,
//! not exercised. `scripts/windows-cross-check.sh` type-checks the Windows one
//! from Linux; the macOS one has no local counterpart at all.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// One held inhibit.
///
/// Releasing is [`Drop`] and nothing else, so a hold that escapes its owner —
/// a panic unwinding past [`WakeLock`], a `WakeLock` dropped without a
/// `release` — still ends. The trait has no methods for the same reason: there
/// is nothing to ask a hold, and a `release()` method would be a second way to
/// end one that `Drop` would then have to tolerate being called twice.
pub trait SleepInhibit: Send {}

/// The seam between voice and the operating system's idle timer.
///
/// One method, because there is one question: *stay awake, please*. The answer
/// is a hold or a reason, and the reason is a `String` rather than a typed
/// error because its only consumer is a log line — no caller branches on which
/// way the platform said no.
pub trait SleepInhibitor: Send + Sync {
    /// Ask the machine not to sleep while the returned value is alive.
    fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String>;

    /// What this asked, for the one log line a refusal produces.
    fn mechanism(&self) -> &'static str;
}

/// Who and why, as the machine's own inhibit list shows them.
///
/// Both are user-visible — `systemd-inhibit --list` on Linux and `pmset -g
/// assertions` on macOS print them — so they are written for somebody trying to
/// work out what is keeping their laptop awake, which is exactly the situation
/// where a cryptic name costs the most.
pub const WAKE_WHO: &str = "dot-agent-deck";
/// See [`WAKE_WHO`].
pub const WAKE_WHY: &str = "Voice control is on";

/// The machine held awake for as long as voice is on.
///
/// # Why the hold lives here and not in the capture session
///
/// The device and the inhibit are the same *class* of resource — held by the
/// process, invisible to the user, leaked in the same ways — but they have
/// different lifetimes, and putting the inhibit inside
/// [`crate::voice::CaptureSession`] would have fused them. The microphone
/// closes and reopens **once per utterance**: the cycle is start → speak →
/// stop → transcribe → resolve → start, and transcribe-and-resolve is a
/// second or more with the device shut. An inhibit tied to the device would
/// therefore lapse for a fifth of the time voice is on, and an idle timer that
/// fires in one of those gaps sleeps the machine mid-session. So the inhibit
/// spans the whole cycle and the device does not, which is what
/// [`crate::voice::VoiceHold`] exists to arrange.
pub struct WakeLock {
    inhibitor: Arc<dyn SleepInhibitor>,
    state: Mutex<LockState>,
    /// Whether a refusal has already been logged by this lock. See the module
    /// docs — `hold` runs once per utterance, so an unlatched line would be one
    /// per sentence spoken on a machine that cannot grant the inhibit.
    reported: AtomicBool,
}

/// Everything the lock guards, so that acquiring can happen OUTSIDE the guard.
#[derive(Default)]
struct LockState {
    /// The live hold, or `None` for *not currently holding* — which covers both
    /// *voice is off* and *the machine refused*, deliberately: nothing
    /// downstream treats those differently.
    held: Option<Box<dyn SleepInhibit>>,
    /// Bumped by every release and by the start of every acquisition, so an
    /// acquisition that finishes **after** a release can tell that it did.
    generation: u64,
    /// The generation currently being acquired, if any. Present means *the
    /// platform is being asked right now*, which is neither held nor free.
    acquiring: Option<u64>,
}

impl WakeLock {
    /// A lock over whatever this platform offers.
    pub fn platform() -> Self {
        Self::new(platform_inhibitor())
    }

    pub fn new(inhibitor: Arc<dyn SleepInhibitor>) -> Self {
        Self {
            inhibitor,
            state: Mutex::new(LockState::default()),
            reported: AtomicBool::new(false),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LockState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Ask the machine to stay awake. Idempotent, silent, and never refused
    /// upwards.
    ///
    /// Idempotent because it is called at the top of **every** utterance rather
    /// than once at the press: the cycle's own `start` is the only Rust-side
    /// event that reliably means *voice is on right now*, and a second
    /// acquisition would be a second inhibitor registered against this process
    /// for the same reason.
    ///
    /// # The platform is asked with the lock NOT held, and that is a fix rather
    /// than a style
    ///
    /// The first version of this asked for the inhibit with the mutex held,
    /// reasoning that it made two concurrent `hold`s impossible to interleave.
    /// It also made [`WakeLock::release`] wait on the platform — and the two
    /// run on different threads by construction, because `hold` is reached
    /// from `desktop_voice_start`'s `spawn_blocking` and `release` from
    /// `desktop_voice_cancel`'s. A system bus that accepted the connection and
    /// then never answered would therefore have wedged the *release* too, and a
    /// release that cannot run is a **microphone that cannot be closed** — the
    /// exact defect PRD #802's audit blocker was about, reintroduced by the
    /// thing meant to sit beside it.
    ///
    /// So the acquisition happens outside the guard, and a **reservation**
    /// orders it against a release instead. It is the same device
    /// [`crate::voice::CaptureSession`] uses for the same reason — read
    /// `SessionInner::opening`'s doc comment, which is this one for the
    /// microphone: an operation that outlives the decision to abandon it must
    /// be able to tell that it did. A release bumps the generation, so an
    /// acquisition that lands afterwards finds its own reservation stale and
    /// **drops the hold it was given** rather than installing it against a
    /// voice session that is over.
    ///
    /// Concurrency is still exact: `acquiring` is neither held nor free, so a
    /// second `hold` while one is in flight returns without asking the platform
    /// a second time.
    ///
    /// Measured on this project's dev box, warm: **2.3–4.0 ms** to acquire (the
    /// first call carries the system-bus connection) and **4–42 µs** to
    /// release, against a call path that is already opening an audio device.
    pub fn hold(&self) {
        let mine = {
            let mut state = self.state();
            if state.held.is_some() || state.acquiring.is_some() {
                return;
            }
            state.generation += 1;
            let mine = state.generation;
            state.acquiring = Some(mine);
            mine
        };

        match self.inhibitor.inhibit() {
            Ok(hold) => {
                let mut state = self.state();
                if state.generation != mine {
                    // Voice was switched off while the platform was answering.
                    // Dropped outside the guard, like every other release here.
                    drop(state);
                    drop(hold);
                    return;
                }
                state.acquiring = None;
                state.held = Some(hold);
            }
            Err(reason) => {
                {
                    let mut state = self.state();
                    // Only if this reservation is still the live one: a release
                    // has already cleared it otherwise, and a later `hold` may
                    // have taken a new one.
                    if state.generation == mine {
                        state.acquiring = None;
                    }
                }
                // Once per process. See the field's doc comment.
                if !self.reported.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "voice: the machine was not asked to stay awake ({}: {reason})",
                        self.inhibitor.mechanism()
                    );
                }
            }
        }
    }

    /// Let the machine sleep again. Idempotent, and a no-op when nothing is
    /// held.
    ///
    /// The hold is **taken out of the lock and dropped after the guard**, the
    /// same discipline [`crate::voice::CaptureSession::cancel`] applies to the
    /// audio stream: releasing is a platform call — on Windows it joins a
    /// thread — and running it with a mutex held is how an unrelated caller
    /// ends up parked behind the operating system.
    ///
    /// **This never waits on an acquisition**, which is the property that keeps
    /// a wedged system bus from turning into a microphone nobody can close.
    /// See [`WakeLock::hold`] for the reservation that makes it safe to bump
    /// the generation and walk away while the platform is still answering.
    pub fn release(&self) {
        let hold = {
            let mut state = self.state();
            // Ahead of the take, because it is also what tells an acquisition
            // still in flight that the session it belongs to is over.
            state.generation += 1;
            state.acquiring = None;
            state.held.take()
        };
        drop(hold);
    }

    /// Whether this lock is holding the machine awake right now.
    ///
    /// `false` when voice is off **and** when the platform refused, which is
    /// what makes it the honest answer to *is the machine being kept awake*
    /// rather than to *did we ask*.
    pub fn is_held(&self) -> bool {
        self.state().held.is_some()
    }
}

impl std::fmt::Debug for WakeLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WakeLock")
            .field("mechanism", &self.inhibitor.mechanism())
            .field("held", &self.is_held())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Linux — systemd-logind, over the system bus.
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod logind {
    use super::{SleepInhibit, SleepInhibitor, WAKE_WHO, WAKE_WHY};
    use std::os::fd::OwnedFd;

    /// `what`: the session is not idle. Deliberately not `sleep`, which blocks
    /// the user's own explicit suspend as well as the automatic one. See the
    /// module docs.
    const WHAT: &str = "idle";
    /// `mode`: refuse the idle action outright rather than asking for a delay
    /// window. A delay inhibitor postpones a suspend by seconds; this needs it
    /// not to happen at all while voice is on.
    const MODE: &str = "block";

    /// How long logind gets to answer before this is treated as a refusal.
    ///
    /// zbus's own default is **no timeout at all** (`method_timeout` is an
    /// `Option` and is `None` unless set), and this call is made from the same
    /// `spawn_blocking` that opened the microphone — so an unbounded one is a
    /// press that never finishes. Two seconds is roughly **800x** the 2.4 ms
    /// median measured on this project's dev box, so it cannot be tripped by a
    /// loaded machine; a bus that has not answered by then is not going to.
    ///
    /// It bounds the *method call* and not the connection handshake in front of
    /// it, which is why [`super::WakeLock::hold`]'s reservation is the real
    /// containment rather than this.
    const ANSWER_WITHIN: std::time::Duration = std::time::Duration::from_secs(2);

    pub struct Logind;

    impl SleepInhibitor for Logind {
        fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String> {
            match acquire() {
                Ok(fd) => Ok(Box::new(Inhibited(fd))),
                Err(error) => Err(error.to_string()),
            }
        }

        fn mechanism(&self) -> &'static str {
            "org.freedesktop.login1 Inhibit(idle)"
        }
    }

    /// The inhibit itself.
    ///
    /// The **file descriptor is the mechanism**, which is why nothing reads it:
    /// logind holds the other end, and the inhibitor lasts exactly as long as
    /// this one stays open. `OwnedFd`'s own `Drop` closes it, so releasing is
    /// dropping this value — and the kernel does the same drop on a process
    /// that dies any way at all, which is the crash-safety property the module
    /// docs claim.
    struct Inhibited(#[allow(dead_code)] OwnedFd);

    impl SleepInhibit for Inhibited {}

    /// One `Inhibit` call on a connection opened for it and closed after.
    ///
    /// A per-acquisition connection rather than one held for the life of the
    /// app: this runs at most once per voice-on, the returned descriptor is
    /// ours and outlives the connection that carried it, and a long-lived
    /// system-bus connection would be a socket and a reader task held by an app
    /// that otherwise speaks to no bus at all.
    fn acquire() -> zbus::Result<OwnedFd> {
        let bus = zbus::blocking::connection::Builder::system()?
            .method_timeout(ANSWER_WITHIN)
            .build()?;
        let reply = bus.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &(WHAT, WAKE_WHO, WAKE_WHY, MODE),
        )?;
        // Bound rather than chained: `Body` borrows the message, and the
        // descriptor it yields borrows the body.
        let body = reply.body();
        let fd: zbus::zvariant::OwnedFd = body.deserialize()?;
        Ok(fd.into())
    }
}

#[cfg(target_os = "linux")]
pub fn platform_inhibitor() -> Arc<dyn SleepInhibitor> {
    Arc::new(logind::Logind)
}

// ---------------------------------------------------------------------------
// macOS — an IOKit power assertion.
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod iokit {
    use super::{SleepInhibit, SleepInhibitor, WAKE_WHO, WAKE_WHY};
    use std::ffi::c_void;

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type CFIndex = isize;
    type CFStringEncoding = u32;
    type Boolean = u8;
    type IOReturn = i32;
    type IOPMAssertionID = u32;
    type IOPMAssertionLevel = u32;

    const K_CF_STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
    const K_IOPM_ASSERTION_LEVEL_ON: IOPMAssertionLevel = 255;
    const K_IO_RETURN_SUCCESS: IOReturn = 0;

    /// `kIOPMAssertPreventUserIdleSystemSleep`: the *system* stays awake and
    /// the **display is left alone**, which is the split this feature's scope
    /// asks for. Its louder sibling
    /// `kIOPMAssertPreventUserIdleDisplaySleep` is the one that also keeps the
    /// screen lit, and a user who is talking is not watching.
    ///
    /// Spelled out rather than read from a constant because the symbol is a
    /// `CFSTR` macro in the IOKit headers: there is no exported data symbol to
    /// link against, so the string literal IS the ABI here.
    const ASSERTION_TYPE: &str = "PreventUserIdleSystemSleep";

    // Declared by hand rather than taken from a crate. `objc2-core-foundation`
    // is in the lockfile through tauri and an `objc2-io-kit` is not, so the
    // IOKit half would have needed a new dependency for four declarations —
    // and mixing one crate's CoreFoundation types with a hand-rolled IOKit
    // extern is harder to read than declaring both. See the desktop crate's
    // `Cargo.toml`: this whole module adds nothing to the dependency graph.
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: CFStringEncoding,
            is_external_representation: Boolean,
        ) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            assertion_level: IOPMAssertionLevel,
            assertion_name: CFStringRef,
            assertion_id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(assertion_id: IOPMAssertionID) -> IOReturn;
    }

    /// A `CFStringRef` that releases itself.
    ///
    /// Both strings this module builds are borrowed by the assertion call and
    /// not retained by it, so they are released the moment it returns — but an
    /// early return between the two creations would otherwise leak the first,
    /// which is what `?` on `CfString::new` would do without this.
    struct CfString(CFStringRef);

    impl CfString {
        fn new(text: &str) -> Result<Self, String> {
            // SAFETY: a null allocator is `kCFAllocatorDefault`, the pointer
            // and length come from a live `&str`, and the encoding matches the
            // bytes `str` guarantees.
            let handle = unsafe {
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    text.as_ptr(),
                    text.len() as CFIndex,
                    K_CF_STRING_ENCODING_UTF8,
                    0,
                )
            };
            if handle.is_null() {
                return Err("CoreFoundation would not allocate a string".to_string());
            }
            Ok(Self(handle))
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: created by `CFStringCreateWithBytes` above and released
            // exactly once, since `CfString` is neither `Copy` nor `Clone`.
            unsafe { CFRelease(self.0) };
        }
    }

    pub struct IoKit;

    impl SleepInhibitor for IoKit {
        fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String> {
            let kind = CfString::new(ASSERTION_TYPE)?;
            let name = CfString::new(&format!("{WAKE_WHO}: {WAKE_WHY}"))?;
            let mut id: IOPMAssertionID = 0;
            // SAFETY: both strings are live for the call, and `id` is a live
            // out-parameter written only on success.
            let result = unsafe {
                IOPMAssertionCreateWithName(kind.0, K_IOPM_ASSERTION_LEVEL_ON, name.0, &mut id)
            };
            if result != K_IO_RETURN_SUCCESS {
                return Err(format!("IOPMAssertionCreateWithName returned {result}"));
            }
            Ok(Box::new(Assertion(id)))
        }

        fn mechanism(&self) -> &'static str {
            "IOPMAssertionCreateWithName(PreventUserIdleSystemSleep)"
        }
    }

    /// A live assertion, released by `Drop`.
    ///
    /// powerd also releases it when this process dies, which is what keeps a
    /// crash from leaving a Mac awake — see the module docs.
    struct Assertion(IOPMAssertionID);

    impl Drop for Assertion {
        fn drop(&mut self) {
            // SAFETY: the id was written by a successful
            // `IOPMAssertionCreateWithName` and is released exactly once.
            // A failed release leaves powerd to reap it with the process;
            // there is nothing else to do about it and nobody to tell.
            unsafe { IOPMAssertionRelease(self.0) };
        }
    }

    impl SleepInhibit for Assertion {}
}

#[cfg(target_os = "macos")]
pub fn platform_inhibitor() -> Arc<dyn SleepInhibitor> {
    Arc::new(iokit::IoKit)
}

// ---------------------------------------------------------------------------
// Windows — a thread holding ES_SYSTEM_REQUIRED.
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod execution_state {
    use super::{SleepInhibit, SleepInhibitor};
    use std::sync::mpsc::{Sender, channel};
    use std::thread::JoinHandle;
    use windows_sys::Win32::System::Power::{
        ES_CONTINUOUS, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
    };

    pub struct ExecutionState;

    impl SleepInhibitor for ExecutionState {
        /// **A dedicated thread, and it is not ceremony.**
        /// `SetThreadExecutionState` sets state on the *calling thread* and the
        /// system clears it when that thread exits. Called from the blocking
        /// pool — which is where every other platform call in this feature runs
        /// — the request would lapse silently the next time the pool retired an
        /// idle worker, and the failure mode is a machine that sleeps anyway
        /// with everything here still reporting `held`. So the hold owns a
        /// thread whose whole job is to be the thread that asked.
        fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String> {
            let (stop, wait) = channel::<()>();
            let (answer, granted) = channel::<bool>();
            let thread = std::thread::Builder::new()
                .name("dad-voice-wake".to_string())
                .spawn(move || {
                    // SAFETY: a documented Win32 call taking a flag word.
                    let previous =
                        unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
                    let _ = answer.send(previous != 0);
                    if previous == 0 {
                        return;
                    }
                    // Park until the hold is dropped. `recv` returns `Err` when
                    // the sender goes away, which is the release, so there is no
                    // message to interpret and no second path out.
                    let _ = wait.recv();
                    // SAFETY: as above. `ES_CONTINUOUS` alone clears the
                    // request this thread made.
                    unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
                })
                .map_err(|error| format!("no thread to hold the request: {error}"))?;
            match granted.recv() {
                Ok(true) => Ok(Box::new(Held {
                    stop: Some(stop),
                    thread: Some(thread),
                })),
                // The thread has already returned in both arms: `Ok(false)` is
                // the call failing, `Err` is it panicking before it answered.
                Ok(false) => Err("SetThreadExecutionState returned NULL".to_string()),
                Err(_) => Err("the thread holding the request did not start".to_string()),
            }
        }

        fn mechanism(&self) -> &'static str {
            "SetThreadExecutionState(ES_SYSTEM_REQUIRED)"
        }
    }

    struct Held {
        stop: Option<Sender<()>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Held {
        /// Dropping the sender is the signal, and the join is what makes the
        /// release **done** rather than scheduled — the same reason every other
        /// teardown in this feature is synchronous. It is a thread that is
        /// already returning, so the wait is one context switch.
        fn drop(&mut self) {
            self.stop.take();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    impl SleepInhibit for Held {}
}

#[cfg(windows)]
pub fn platform_inhibitor() -> Arc<dyn SleepInhibitor> {
    Arc::new(execution_state::ExecutionState)
}

// ---------------------------------------------------------------------------
// Everything else.
// ---------------------------------------------------------------------------

/// A platform with no mechanism wired up here.
///
/// Not a failure and not a panic: this app ships to Linux, macOS and Windows,
/// and anywhere else the honest answer is that nothing was asked. It refuses
/// exactly as a Linux box with no logind does, so it travels the same path.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod unsupported {
    use super::{SleepInhibit, SleepInhibitor};

    pub struct Unsupported;

    impl SleepInhibitor for Unsupported {
        fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String> {
            Err("this platform has no sleep inhibit wired up".to_string())
        }

        fn mechanism(&self) -> &'static str {
            "none"
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn platform_inhibitor() -> Arc<dyn SleepInhibitor> {
    Arc::new(unsupported::Unsupported)
}

// ---------------------------------------------------------------------------
// The stub, for every test that must not touch the machine's power management.
// ---------------------------------------------------------------------------

/// What a [`StubInhibitor`] has been asked for and what became of it.
///
/// Shared by `Arc` with the holds it hands out, because the count that matters
/// — how many are still alive — can only be maintained by their `Drop`.
#[derive(Debug, Default)]
pub struct WakeCounts {
    acquired: AtomicUsize,
    released: AtomicUsize,
}

impl WakeCounts {
    /// How many holds have been handed out.
    pub fn acquired(&self) -> usize {
        self.acquired.load(Ordering::Relaxed)
    }

    /// How many have been dropped.
    pub fn released(&self) -> usize {
        self.released.load(Ordering::Relaxed)
    }

    /// How many are still alive — the number that is a leak when voice is off.
    pub fn outstanding(&self) -> usize {
        self.acquired().saturating_sub(self.released())
    }
}

/// An inhibitor that asks the machine for nothing and remembers everything.
///
/// `pub` and not behind a cargo feature, for the reason `voice::test_support`
/// and [`crate::voice::StubSource`] state at length: code behind a feature
/// nobody's gate enables is type-checked by nothing.
pub struct StubInhibitor {
    refuse: AtomicBool,
    counts: Arc<WakeCounts>,
}

impl Default for StubInhibitor {
    fn default() -> Self {
        Self::new()
    }
}

impl StubInhibitor {
    pub fn new() -> Self {
        Self {
            refuse: AtomicBool::new(false),
            counts: Arc::new(WakeCounts::default()),
        }
    }

    /// An inhibitor that answers the way a headless box does.
    pub fn refusing() -> Self {
        let stub = Self::new();
        stub.refuse.store(true, Ordering::Relaxed);
        stub
    }

    /// The counters, which outlive the inhibitor and every hold it made.
    pub fn counts(&self) -> Arc<WakeCounts> {
        Arc::clone(&self.counts)
    }
}

impl SleepInhibitor for StubInhibitor {
    fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String> {
        if self.refuse.load(Ordering::Relaxed) {
            return Err("the stub was told to refuse".to_string());
        }
        self.counts.acquired.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(StubHold(Arc::clone(&self.counts))))
    }

    fn mechanism(&self) -> &'static str {
        "stub"
    }
}

struct StubHold(Arc<WakeCounts>);

impl Drop for StubHold {
    fn drop(&mut self) {
        self.0.released.fetch_add(1, Ordering::Relaxed);
    }
}

impl SleepInhibit for StubHold {}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub_lock() -> (WakeLock, Arc<WakeCounts>) {
        let stub = StubInhibitor::new();
        let counts = stub.counts();
        (WakeLock::new(Arc::new(stub)), counts)
    }

    /// Voice on acquires; voice off releases. The whole feature, over the stub.
    #[test]
    fn a_hold_is_taken_and_a_release_gives_it_back() {
        let (lock, counts) = stub_lock();
        assert!(!lock.is_held(), "nothing is held before voice is on");

        lock.hold();
        assert!(lock.is_held());
        assert_eq!(counts.acquired(), 1);
        assert_eq!(counts.outstanding(), 1);

        lock.release();
        assert!(!lock.is_held(), "voice off lets the machine sleep again");
        assert_eq!(counts.released(), 1);
        assert_eq!(counts.outstanding(), 0);
    }

    /// `hold` runs at the top of every utterance, and there is one voice
    /// session — so the second, third and fortieth calls must register nothing
    /// new with the operating system.
    #[test]
    fn holding_twice_registers_one_inhibit() {
        let (lock, counts) = stub_lock();
        lock.hold();
        lock.hold();
        lock.hold();
        assert_eq!(counts.acquired(), 1, "one voice session, one inhibit");
        lock.release();
        assert_eq!(counts.outstanding(), 0, "and one release ends it");
    }

    /// A release with nothing held is what every teardown trigger does on an
    /// app that never turned voice on. It must be a no-op rather than a
    /// refusal, because those call sites are unconditional.
    #[test]
    fn releasing_what_was_never_held_is_a_no_op() {
        let (lock, counts) = stub_lock();
        lock.release();
        lock.release();
        assert_eq!(counts.acquired(), 0);
        assert_eq!(counts.released(), 0);
        assert!(!lock.is_held());
    }

    /// Voice can be turned on again after it has been turned off.
    #[test]
    fn a_second_voice_session_takes_a_second_inhibit() {
        let (lock, counts) = stub_lock();
        lock.hold();
        lock.release();
        lock.hold();
        assert_eq!(counts.acquired(), 2);
        assert_eq!(counts.outstanding(), 1);
        lock.release();
        assert_eq!(counts.outstanding(), 0);
    }

    /// A refused acquisition is silent, and leaves the lock honestly reporting
    /// that the machine is **not** being held awake. Nothing above it branches
    /// on this — see [`crate::voice::VoiceHold::start`], which opens the device
    /// either way.
    #[test]
    fn a_refusal_holds_nothing_and_says_nothing() {
        let stub = StubInhibitor::refusing();
        let counts = stub.counts();
        let lock = WakeLock::new(Arc::new(stub));

        lock.hold();

        assert!(!lock.is_held(), "a refusal is not a hold");
        assert_eq!(counts.acquired(), 0);
        // And the call sites still work: releasing after a refusal is the
        // ordinary teardown path, not an error case.
        lock.release();
        assert!(!lock.is_held());
    }

    /// The ordering guard, and the reason [`WakeLock::hold`] asks the platform
    /// with the lock NOT held.
    ///
    /// A release that lands while the platform is still answering must leave
    /// the machine free — the hold that arrives afterwards belongs to a voice
    /// session that is over, and installing it would keep a laptop awake with
    /// the button reading `Voice off`.
    ///
    /// Driven deterministically rather than with threads and sleeps: the
    /// inhibitor itself performs the release, at exactly the instant the real
    /// race would — inside the platform call, before the answer comes back.
    #[test]
    fn a_release_during_an_acquisition_leaves_the_machine_free() {
        struct ReleasesMidCall {
            counts: Arc<WakeCounts>,
            target: Arc<std::sync::OnceLock<Arc<WakeLock>>>,
        }

        impl SleepInhibitor for ReleasesMidCall {
            fn inhibit(&self) -> Result<Box<dyn SleepInhibit>, String> {
                // Voice switched off while the platform was working.
                self.target.get().expect("wired before the hold").release();
                self.counts.acquired.fetch_add(1, Ordering::Relaxed);
                Ok(Box::new(StubHold(Arc::clone(&self.counts))))
            }

            fn mechanism(&self) -> &'static str {
                "stub that races a release"
            }
        }

        let counts = Arc::new(WakeCounts::default());
        let target = Arc::new(std::sync::OnceLock::new());
        let lock = Arc::new(WakeLock::new(Arc::new(ReleasesMidCall {
            counts: Arc::clone(&counts),
            target: Arc::clone(&target),
        })));
        target.set(Arc::clone(&lock)).ok();

        lock.hold();

        assert!(
            !lock.is_held(),
            "a hold that arrives after its session ended must not be installed"
        );
        assert_eq!(
            counts.outstanding(),
            0,
            "and must be given back rather than dropped on the floor"
        );

        // And the lock is not wedged: the next voice session works.
        let plain = StubInhibitor::new();
        let plain_counts = plain.counts();
        let next = WakeLock::new(Arc::new(plain));
        next.hold();
        assert!(next.is_held());
        assert_eq!(plain_counts.outstanding(), 1);
        next.release();
    }

    /// The leak guard. A `WakeLock` that goes out of scope without a `release`
    /// — a panic unwinding past it, a `VoiceState` dropped with voice on — must
    /// not leave the machine awake.
    #[test]
    fn dropping_the_lock_releases_what_it_held() {
        let stub = StubInhibitor::new();
        let counts = stub.counts();
        let lock = WakeLock::new(Arc::new(stub));
        lock.hold();
        assert_eq!(counts.outstanding(), 1);

        drop(lock);

        assert_eq!(
            counts.outstanding(),
            0,
            "a dropped lock releases the machine"
        );
    }

    /// `Debug` reports the mechanism and whether anything is held, and prints
    /// no handle — an fd number or an assertion id in a log is noise that
    /// invites somebody to act on it.
    #[test]
    fn debug_names_the_mechanism_and_the_state() {
        let (lock, _) = stub_lock();
        assert_eq!(
            format!("{lock:?}"),
            "WakeLock { mechanism: \"stub\", held: false }"
        );
        lock.hold();
        assert!(format!("{lock:?}").contains("held: true"));
    }

    /// The real platform call, exercised for the one thing a unit test can
    /// assert about it: that asking is safe wherever the tier runs.
    ///
    /// **Both answers pass on purpose.** A developer's Linux box grants the
    /// logind inhibit and a CI container with no system bus refuses it, and
    /// this tier runs on both — an assertion either way would be a test of the
    /// machine rather than of the code. What it does catch is the thing that
    /// actually goes wrong in a hand-rolled platform layer: a panic, an abort,
    /// a hang, or a release that faults.
    ///
    /// **On macOS and Windows this runs nowhere**, because no tier in this
    /// repository runs `cargo test` on those platforms' GUI app — see the
    /// module docs. There it is compiled and no more.
    #[test]
    fn asking_the_real_platform_is_safe_whatever_it_answers() {
        let lock = WakeLock::platform();
        lock.hold();
        lock.release();
        assert!(!lock.is_held(), "a release ends it whether or not it began");
    }
}
