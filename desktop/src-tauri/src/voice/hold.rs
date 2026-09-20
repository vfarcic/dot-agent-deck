//! PRD #802 — everything voice holds on the machine, in one place.
//!
//! # What this type is for
//!
//! Voice control holds two resources that the user cannot see: the **audio
//! device**, and — since the sleep work — a **sleep inhibit** that stops the
//! operating system suspending a machine whose owner is talking to it rather
//! than typing at it. They are the same class of thing. Both are held by the
//! process, neither shows up anywhere the user would look, and a leaked one
//! costs something real: a microphone that stays open, or a laptop that never
//! sleeps again with nothing on screen to explain why.
//!
//! So they are released together, and the release is the only way out.
//!
//! # The property, and why it is the COMPILER that holds it rather than review
//!
//! *Every path that releases the microphone also releases the inhibit.*
//!
//! The tempting way to arrange that is a convention — call the pairing helper,
//! not the session — and conventions are exactly what
//! [`crate::selection_capture`]'s module docs describe decaying. This type
//! arranges it structurally instead: **[`VoiceHold::session`] is private to
//! this module**, so `lib.rs` cannot reach
//! [`CaptureSession::cancel`][crate::voice::CaptureSession::cancel] at all. The
//! only cancel it can spell is [`VoiceHold::release`], and that one releases
//! both. A teardown trigger that forgot the inhibit does not fail review; it
//! fails to compile.
//!
//! [`tests::the_only_cancel_is_the_one_that_also_releases_the_machine`] is the
//! second half, and it guards the one thing privacy cannot: somebody adding a
//! `pub fn cancel` passthrough here later.
//!
//! # The two device releases that deliberately KEEP the inhibit
//!
//! [`VoiceHold::stop`] and [`VoiceHold::cap_reached`] close the device without
//! touching the wake lock, and that is the design rather than an oversight.
//!
//! Voice control is a **cycle**, not one long recording: start → speak → stop →
//! transcribe → resolve → start, for as long as the button says on. The
//! microphone is shut for the whole transcribe-and-resolve leg, which PRD #802
//! measured at over a second per utterance. An inhibit that tracked the device
//! would therefore lapse in every gap between sentences — and an idle timer
//! that comes due in one of those gaps sleeps the machine in the middle of a
//! voice session, which is the entire defect this was built to fix. So the
//! inhibit spans the cycle: taken when the device first opens, kept across the
//! stops, and given back by the one release that ends the session.
//!
//! The bound is still tight, because the inhibit's lifetime is contained by the
//! session's: [`VoiceHold::start`] is the only thing that takes one, so it
//! cannot be held by a process that never opened a microphone, and
//! [`VoiceHold::release`] is what every teardown trigger and the cancel command
//! call.
//!
//! There is one residual case and it is worth naming rather than discovering: a
//! `start` that the session refuses while a recording is already running leaves
//! the inhibit held with the panel showing `Voice off`. That is the **same**
//! state the device is in, by construction — which is the point of pairing them
//! — and the surface already has a name and a remedy for it (`unreleased`, and
//! the mount reconcile in `VoiceControlPanel.tsx`). Both end at the next
//! release.

use std::sync::Arc;

use super::capture::{
    AudioSource, CaptureError, CaptureSession, CaptureStatus, CaptureTicket, Pcm16,
};
use super::wake::WakeLock;

/// The microphone and the sleep inhibit, held and released as one.
///
/// Cheap to clone — both halves are `Arc`s — which is what lets a command move
/// one into `spawn_blocking` without the state it came from. Every platform
/// call underneath is blocking, so that is where all of these belong.
#[derive(Clone)]
pub struct VoiceHold {
    /// **Private on purpose.** See the module docs: this is what stops anything
    /// outside this file cancelling the session without releasing the machine.
    session: Arc<CaptureSession>,
    awake: Arc<WakeLock>,
}

impl VoiceHold {
    /// The production pairing: this platform's audio device and this platform's
    /// sleep inhibit.
    pub fn new(source: Arc<dyn AudioSource>) -> Self {
        Self::with_parts(
            Arc::new(CaptureSession::new(source)),
            Arc::new(WakeLock::platform()),
        )
    }

    /// The seam both halves come through, so a test can stub either.
    pub fn with_parts(session: Arc<CaptureSession>, awake: Arc<WakeLock>) -> Self {
        Self { session, awake }
    }

    /// What the session is doing right now.
    pub fn status(&self) -> CaptureStatus {
        self.session.status()
    }

    /// Open the microphone, and ask the machine to stay awake while it is.
    ///
    /// **The inhibit is taken only after the device opens**, so a start the
    /// platform refused leaves nothing behind — there is no unwind path here
    /// that could hold the machine awake for a microphone that never opened.
    ///
    /// **A refused inhibit is not a refused start.** [`WakeLock::hold`] never
    /// fails upwards: a headless box, a container with no D-Bus or a policy
    /// that says no gets voice control working exactly as before, and the
    /// machine may sleep. That ordering is the whole of the failure policy — the
    /// feature degrades to what it was, rather than reporting an error the user
    /// cannot act on.
    pub fn start(&self) -> Result<(CaptureStatus, CaptureTicket), CaptureError> {
        let started = self.session.start()?;
        self.awake.hold();
        Ok(started)
    }

    /// Close the device and take the audio, leaving the session mid-cycle.
    ///
    /// **Keeps the inhibit.** This is the end of an utterance, not the end of
    /// voice: the transcribe and resolve that follow run with the microphone
    /// shut, and [`VoiceHold::start`] opens it again straight after. See the
    /// module docs for why an inhibit that lapsed in that gap would reintroduce
    /// the defect.
    pub fn stop(&self) -> Result<Pcm16, CaptureError> {
        self.session.stop()
    }

    /// Record what the transcription made of the utterance.
    pub fn settle(&self, heard: bool) -> CaptureStatus {
        self.session.settle(heard)
    }

    /// The length cap elapsed: release the device, keep the audio.
    ///
    /// **Keeps the inhibit**, for [`VoiceHold::stop`]'s reason and one more of
    /// its own: the session stays `Recording` here, so voice is not merely
    /// still on — the utterance is still the user's to send or discard.
    pub fn cap_reached(&self, ticket: CaptureTicket) -> CaptureStatus {
        self.session.cap_reached(ticket)
    }

    /// End the voice session: close the device and let the machine sleep.
    ///
    /// **The only cancel this crate can reach**, and the module docs say why.
    /// Idempotent and never refused, because every call site is unconditional —
    /// a closing window, a replaced document, a lost web-content process, the
    /// app exiting, and the user pressing Voice a second time.
    ///
    /// The machine is released **first**. Closing the device joins its thread
    /// and is as slow as the platform's own teardown, and a release that
    /// deadlocked or panicked behind it would leave a laptop awake for good;
    /// letting go of the inhibit costs nothing and cannot be the slow half.
    pub fn release(&self) -> CaptureStatus {
        self.awake.release();
        self.session.cancel()
    }

    /// Whether the machine is being held awake right now.
    ///
    /// `false` both when voice is off and when the platform refused the
    /// inhibit — see [`WakeLock::is_held`], which is deliberately the answer to
    /// *is it being held* rather than to *did we ask*.
    pub fn awake_held(&self) -> bool {
        self.awake.is_held()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::capture::{AudioFormat, CaptureState, StubSource, TARGET_SAMPLE_RATE};
    use crate::voice::wake::{StubInhibitor, WakeCounts};
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A hold whose device is a stub tone and whose machine is a stub inhibitor.
    ///
    /// The three extras are what the assertions need: the stub stream's own
    /// `Drop` flag, which is the only way to tell the DEVICE was released from
    /// the state machine merely saying idle; and the wake counters, which
    /// outlive every hold they made.
    fn stub_hold(
        seconds: f64,
        inhibitor: StubInhibitor,
    ) -> (VoiceHold, Arc<AtomicBool>, Arc<WakeCounts>) {
        let source = StubSource::tone(AudioFormat::new(TARGET_SAMPLE_RATE, 1), seconds);
        let stopped = source.stopped();
        let counts = inhibitor.counts();
        let hold = VoiceHold::with_parts(
            Arc::new(CaptureSession::new(Arc::new(source))),
            Arc::new(WakeLock::new(Arc::new(inhibitor))),
        );
        (hold, stopped, counts)
    }

    /// Voice on acquires; voice off releases — over the stub, the way capture
    /// itself is tested.
    #[test]
    fn opening_the_microphone_holds_the_machine_awake_and_releasing_gives_it_back() {
        let (hold, stopped, counts) = stub_hold(1.0, StubInhibitor::new());
        assert!(!hold.awake_held(), "nothing is held before voice is on");

        let (opened, _ticket) = hold.start().expect("the stub device opens");
        assert_eq!(opened.state, CaptureState::Recording);
        assert!(hold.awake_held(), "voice on keeps the machine awake");
        assert_eq!(counts.acquired(), 1);

        let after = hold.release();

        assert_eq!(after.state, CaptureState::Idle);
        assert!(stopped.load(Ordering::Relaxed), "the device is closed");
        assert!(!hold.awake_held(), "and the machine may sleep again");
        assert_eq!(counts.outstanding(), 0);
    }

    /// The cycle: one inhibit spans every utterance, rather than one per
    /// sentence. See the module docs — an inhibit that lapsed between
    /// utterances would let the idle timer fire mid-session.
    #[test]
    fn a_whole_voice_session_takes_exactly_one_inhibit() {
        let (hold, _stopped, counts) = stub_hold(0.2, StubInhibitor::new());

        for _ in 0..3 {
            hold.start().expect("the stub device opens");
            // The end of an utterance: the device closes and the machine does
            // NOT get released, because voice is still on.
            let _ = hold.stop();
            assert!(
                hold.awake_held(),
                "the gap between utterances is not voice off"
            );
            hold.settle(true);
        }

        assert_eq!(counts.acquired(), 1, "one voice session, one inhibit");
        hold.release();
        assert_eq!(counts.outstanding(), 0);
    }

    /// The capped utterance releases the device and keeps the audio — and keeps
    /// the machine awake with it, because the session is still `Recording` and
    /// the utterance is still the user's to send or discard.
    #[test]
    fn the_length_cap_keeps_the_machine_awake() {
        let (hold, _stopped, counts) = stub_hold(2.0, StubInhibitor::new());
        let (_, ticket) = hold.start().expect("the stub device opens");

        let capped = hold.cap_reached(ticket);

        assert!(capped.capped, "the cap released the device");
        assert_eq!(capped.state, CaptureState::Recording);
        assert!(hold.awake_held(), "the cap is not voice off");
        assert_eq!(counts.outstanding(), 1);

        hold.release();
        assert_eq!(counts.outstanding(), 0);
    }

    /// A refused acquisition leaves voice **fully** working: the device opens,
    /// the audio is captured, the stop hands it back. All that is lost is the
    /// inhibit, and `awake_held` says so rather than pretending.
    #[test]
    fn a_machine_that_refuses_the_inhibit_still_records() {
        let (hold, _stopped, counts) = stub_hold(0.5, StubInhibitor::refusing());

        let (opened, _ticket) = hold
            .start()
            .expect("a refused inhibit must not refuse the device");

        assert_eq!(opened.state, CaptureState::Recording);
        assert!(
            hold.status().captured_ms > 0,
            "the stub delivered its tone, so voice is genuinely capturing"
        );
        assert!(!hold.awake_held(), "and the app does not claim otherwise");
        assert_eq!(counts.acquired(), 0);

        let audio = hold.stop().expect("the utterance is still there to take");
        assert!(!audio.samples().is_empty());

        // The ordinary teardown still runs, on a machine that granted nothing.
        hold.release();
        assert!(!hold.awake_held());
    }

    /// A start the device refuses must leave nothing held — the inhibit is
    /// taken after the open, so there is no path here that keeps a machine
    /// awake for a microphone that never opened.
    #[test]
    fn a_device_that_will_not_open_holds_nothing() {
        let source = StubSource::failing(CaptureError::Device(
            "no audio device on this machine".to_string(),
        ));
        let inhibitor = StubInhibitor::new();
        let counts = inhibitor.counts();
        let hold = VoiceHold::with_parts(
            Arc::new(CaptureSession::new(Arc::new(source))),
            Arc::new(WakeLock::new(Arc::new(inhibitor))),
        );

        assert!(hold.start().is_err(), "the stub source refuses to open");

        assert!(!hold.awake_held());
        assert_eq!(counts.acquired(), 0, "a failed start asks for nothing");
    }

    /// Releasing twice is what a closing window and an app exit do between
    /// them. It must be a no-op the second time rather than a double release.
    #[test]
    fn releasing_twice_is_idempotent_on_both_halves() {
        let (hold, stopped, counts) = stub_hold(0.5, StubInhibitor::new());
        hold.start().expect("the stub device opens");

        hold.release();
        hold.release();

        assert!(stopped.load(Ordering::Relaxed));
        assert!(!hold.awake_held());
        assert_eq!(counts.released(), 1, "one hold, released once");
    }

    /// The structural guard that privacy alone cannot make.
    ///
    /// The module docs' property — *every path that releases the microphone
    /// also releases the inhibit* — is held by `session` being private, which
    /// makes [`CaptureSession::cancel`] unreachable from anywhere but this
    /// file. What privacy cannot stop is somebody adding a `pub fn cancel`
    /// here that forwards to it, which would reopen the hole from the inside
    /// with every call site still compiling.
    ///
    /// So: exactly one `self.session.cancel()` in this module's production
    /// half, and it sits in the body of `release`, next to the wake lock's own.
    /// **If this fails, do not raise a count** — move the cancel back into
    /// `release`, or read the module docs and change them deliberately.
    ///
    /// Scanned the way [`crate::selection_capture`] scans: `include_str!` so a
    /// moved file fails the build, CRLF normalised so a Windows checkout finds
    /// the same text, and the test module cut off so its own fixtures do not
    /// read as production call sites.
    #[test]
    fn the_only_cancel_is_the_one_that_also_releases_the_machine() {
        const TEST_MODULE_MARKER: &str = "\n#[cfg(test)]\nmod tests {";
        let source = include_str!("hold.rs").replace("\r\n", "\n");
        let production = source
            .split(TEST_MODULE_MARKER)
            .next()
            .expect("split always yields a first part");
        assert!(
            production.len() < source.len(),
            "the test module marker must be found, or this guard scans nothing"
        );

        assert_eq!(
            production.matches("self.session.cancel()").count(),
            1,
            "exactly one cancel, and it is `release`'s"
        );

        let release = production
            .split_once("pub fn release(&self)")
            .expect("`release` is this module's only cancel")
            .1;
        let body = release
            .split_once("\n    /// ")
            .map_or(release, |(first, _)| first);
        assert!(
            body.contains("self.awake.release()"),
            "`release` must let the machine sleep"
        );
        assert!(
            body.contains("self.session.cancel()"),
            "`release` must close the device"
        );
    }
}
