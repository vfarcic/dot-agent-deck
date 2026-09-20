//! PRD #802 M7: the microphone, Rust-side.
//!
//! The pipeline is capture → transcribe → resolve → validate → execute →
//! report, and this module owns the first stage: open the default input
//! device, accumulate one bounded utterance, and hand
//! [`super::transcribe::Transcriber`] 16 kHz mono PCM. It resolves nothing and
//! renders no sentence.
//!
//! # Why the audio is captured here and not in the webview
//!
//! PRD #802 measured `wry` 0.55.1, which is what the build resolves, and the
//! three webviews do not agree. WKWebView implements
//! `webView:requestMediaCapturePermissionForOrigin:…` and grants
//! unconditionally; WebView2 gets a `PermissionRequested` handler only when
//! `attributes.clipboard` is set, and that handler allows only the clipboard
//! kind; and the WebKitGTK backend connects **no** `permission-request` signal
//! and never sets `enable-media-stream`, so a webview-side `getUserMedia` on
//! Linux has no path to being granted at all. A `getUserMedia` capture path
//! would therefore ship a macOS-only feature — the same shape of mistake PRD
//! #802 already rejects when it argues for a portable transcriber over
//! `SFSpeechRecognizer`.
//!
//! Capturing here removes the AudioWorklet, the CSP question and the
//! three-engine matrix in one move, and the audio has to reach this process
//! anyway: `tauri.conf.json`'s `connect-src` names `ipc:` and
//! `http://ipc.localhost` and nothing else, so every network hop is Rust-side.
//!
//! The residual platform obligation is real and is not this module's to
//! discharge: a bundled macOS `.app` recording audio needs
//! `NSMicrophoneUsageDescription`, which lives in the bundle overlay.
//!
//! # No audio is written to disk, and no log line carries a sample
//!
//! PRD #802's Open Question 5 is answered the same way for audio as for text:
//! nothing persists. Audio is the one part of the pipeline where that claim
//! needs no qualification at all — a buffer never leaves this process except as
//! an upload to the transcription endpoint, and no child process is ever handed
//! one. ([`super::Transcript`] carries the qualified version, which the
//! agent-CLI intent backend's child needs.) [`Pcm16`]'s [`fmt::Debug`] is
//! written by hand and prints
//! a duration and no samples, so a derived `{:?}` on a type that *holds* one
//! prints none either — the same closure [`super::Transcript`] applies to the
//! text. That covers the careless route; the deliberate one is a rule, and the
//! rule is that a buffer goes from the device to the transcriber and is
//! dropped.
//!
//! # The device is behind a trait
//!
//! [`AudioSource`] exists because `cpal`'s own types are not stubbable and
//! every test in this file has to pass on a machine with **no microphone and no
//! audio server** — which is what a CI runner is. [`CpalSource`] is the real
//! one and is never constructed by a test; [`StubSource`] is what the state
//! machine, the cap and the resampler are all driven through.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The sample rate every [`super::transcribe::Transcriber`] in this build is
/// handed.
///
/// 16 kHz mono is what speech models want, whisper included, and it is the rate
/// this module resamples to whatever the device offers. Higher rates buy
/// nothing a speech model can use and cost bytes on the wire.
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// The longest single utterance this build records.
///
/// **A bound, not a preference.** Without one, a recording is an unbounded
/// allocation on a buffer the user has no way to see — a microphone left open
/// by a webview reload or a forgotten toggle would grow until the process died.
/// Thirty seconds is whisper's own window, so it is also the point past which
/// the model would segment anyway, and at [`TARGET_SAMPLE_RATE`] mono `i16` it
/// caps one utterance at 960 KB.
///
/// It is enforced twice, deliberately. [`PcmSink`] refuses to grow past it, so
/// the allocation is bounded by the data path itself; and [`CaptureSession`]
/// drops the stream when it elapses, so the device is genuinely released rather
/// than left open feeding a sink that discards. Losing either one leaves a real
/// defect: the first alone leaks a live microphone, the second alone trusts a
/// timer.
pub const MAX_UTTERANCE: Duration = Duration::from_secs(30);

/// The format a capture device delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels,
        }
    }
}

/// Why capture could not happen.
///
/// Split three ways because the remedies are three different things: change a
/// setting, press the button in a different order, or plug a microphone in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// No microphone path is offered — `[voice] transcription` is `off`.
    ///
    /// Not a failure. PRD #802 makes `off` a product statement, and the sentence
    /// is a settings instruction rather than an error: the Voice button still
    /// renders, and pressing it names Settings → Voice rather than turning on.
    NotConfigured(String),
    /// The state machine refused the transition — a double start, a stop when
    /// nothing is recording. A caller bug, reported rather than panicked on.
    Refused(String),
    /// The device could not be opened, or failed while open.
    Device(String),
}

impl CaptureError {
    pub fn detail(&self) -> &str {
        match self {
            CaptureError::NotConfigured(detail)
            | CaptureError::Refused(detail)
            | CaptureError::Device(detail) => detail,
        }
    }
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.detail())
    }
}

impl std::error::Error for CaptureError {}

/// One utterance, at [`TARGET_SAMPLE_RATE`], mono, 16-bit.
///
/// [`fmt::Debug`] prints the duration and **no samples**, for
/// [`super::Transcript`]'s reason: a derived `{:?}` on a container is how audio
/// would reach a log line by accident.
#[derive(Clone, PartialEq, Eq)]
pub struct Pcm16 {
    samples: Vec<i16>,
}

impl Pcm16 {
    pub fn new(samples: Vec<i16>) -> Self {
        Self { samples }
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// How long the utterance is.
    pub fn duration(&self) -> Duration {
        Duration::from_secs_f64(self.samples.len() as f64 / f64::from(TARGET_SAMPLE_RATE))
    }

    /// Whether every sample is below the floor an all-zero buffer would sit at.
    ///
    /// Detects **one** thing: a device that opened and delivered nothing, or one
    /// that was muted. It is **not** voice-activity detection and must not be
    /// read as one — it looks at the whole buffer after the fact and decides
    /// nothing about when an utterance ended.
    ///
    /// **It is also not the question "is this worth transcribing", and reading
    /// it as that one shipped a defect.** `all()` means a *single* sample over
    /// the floor makes the whole buffer not-silent, so one keyboard tap, breath
    /// or chair creak anywhere in thirty seconds cleared it — and a speech model
    /// handed thirty seconds of room tone does not answer "nothing". Whisper
    /// models are trained on captioned video and emit their training artefacts
    /// ("Don't forget to subscribe", "Thanks for watching") when there is no
    /// speech to transcribe, so the report told the product owner it had heard
    /// sentences nobody had said. [`Pcm16::has_speech`] is the question that was
    /// actually being asked.
    pub fn is_silent(&self) -> bool {
        self.samples
            .iter()
            .all(|s| s.unsigned_abs() < SILENCE_FLOOR)
    }

    /// The longest **unbroken** stretch of speech-level audio in the buffer.
    ///
    /// Measured exactly the way [`Vad`] measures the live signal — mean square
    /// over a [`VAD_FRAME`] against [`SPEECH_FLOOR`] squared — so the two cannot
    /// disagree about what counts as somebody speaking. A trailing partial frame
    /// is not judged, for the same reason it is not judged there.
    ///
    /// **Unbroken rather than accumulated, and that is the whole discriminating
    /// power of it.** What this has to separate is a spoken word from a train of
    /// impulses: a 20 ms RMS window dilutes a keyboard tap to one frame, maybe
    /// two, while the voiced part of even a one-syllable command is an unbroken
    /// run several times that. A *total* would let ten keystrokes inside one
    /// silence hold sum to the same number a word reaches on its own, which is
    /// precisely the audio that was reaching the backend.
    pub fn speech_run(&self) -> Duration {
        let floor = f64::from(SPEECH_FLOOR) * f64::from(SPEECH_FLOOR);
        let mut longest = 0usize;
        let mut run = 0usize;
        for frame in self.samples.chunks_exact(VAD_FRAME) {
            let energy: f64 = frame
                .iter()
                .map(|&sample| {
                    let value = f64::from(sample);
                    value * value
                })
                .sum();
            // Mean square against the squared floor: the same comparison as RMS
            // against the floor, without the square root. See [`Vad::push`].
            if energy / VAD_FRAME as f64 >= floor {
                run += VAD_FRAME;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        Duration::from_secs_f64(longest as f64 / f64::from(TARGET_SAMPLE_RATE))
    }

    /// Whether the buffer holds enough speech to be worth transcribing at all.
    ///
    /// The eligibility test in front of every transcription call
    /// ([`super::handle_audio`]), and a strictly stronger one than
    /// [`Pcm16::is_silent`]: it subsumes both an empty buffer and an all-silent
    /// one, because neither contains a [`MIN_SPEECH`] run.
    pub fn has_speech(&self) -> bool {
        self.speech_run() >= MIN_SPEECH
    }

    /// The buffer as a 16-bit mono WAV, which is what a transcription API
    /// accepts and the shape whisper reads natively.
    ///
    /// Written here rather than taken from a crate: a canonical PCM WAV header
    /// is 44 fixed bytes with four numbers in it, and a dependency for that
    /// would be a bigger cost than the code. There is no seeking, no chunk
    /// discovery and no format matrix — the input is always
    /// [`TARGET_SAMPLE_RATE`] mono `i16`.
    pub fn to_wav(&self) -> Vec<u8> {
        let data_len = (self.samples.len() * 2) as u32;
        let mut wav = Vec::with_capacity(44 + data_len as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // format: PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
        wav.extend_from_slice(&TARGET_SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&(TARGET_SAMPLE_RATE * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        for sample in &self.samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }
        wav
    }
}

impl fmt::Debug for Pcm16 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Pcm16(<{} samples / {} ms, not printed>)",
            self.samples.len(),
            self.duration().as_millis()
        )
    }
}

/// The amplitude below which a sample counts as silence, out of `i16::MAX`.
///
/// About -60 dBFS. Room tone and a converter's own noise floor sit under it;
/// anything a microphone actually heard sits over it.
const SILENCE_FLOOR: u16 = 32;

/// The **root-mean-square** amplitude, out of `i16::MAX`, at or above which one
/// [`VAD_FRAME`] counts as speech rather than as the room.
///
/// About -35 dBFS. Near-field speech sits between -30 and -20 dBFS RMS —
/// roughly 1 000 to 3 300 in these units — and room tone, a fan and a
/// converter's own noise floor sit below -45, under 200. This is in the middle
/// of a wide gap rather than at the edge of a narrow one.
///
/// **Erring high is deliberate, because the two directions do not cost the
/// same.** A floor set too low never accumulates a silence run in a room with
/// any noise in it, so no utterance ever ends, every command runs into
/// [`MAX_UTTERANCE`], and the user loses all of them. A floor set too high can
/// only end an utterance early — and only after [`SILENCE_HOLD`] of
/// below-threshold audio, which is a pause somebody took rather than the gap
/// between two words.
///
/// It is an order of magnitude above [`SILENCE_FLOOR`] and they answer
/// different questions: that one asks whether a buffer is worth sending at all,
/// this one asks whether somebody is speaking *now*.
const SPEECH_FLOOR: u16 = 600;

/// How long an unbroken run of speech-level frames has to be before a buffer is
/// worth sending to a transcription backend — [`Pcm16::has_speech`]'s threshold.
///
/// 120 ms, which is six [`VAD_FRAME`]s. The two things it has to tell apart sit
/// on either side of it by a wide margin rather than a narrow one:
///
/// * an **impulse** — a keyboard tap, a click, a chair creak — is loud for a few
///   milliseconds, so RMS over a 20 ms window puts it over [`SPEECH_FLOOR`] for
///   one frame and occasionally two. Six is out of reach for anything that is
///   not sustained.
/// * a **spoken word** carries its energy in a voiced nucleus, and the shortest
///   command this table takes — a bare "back" on the overview — still holds one
///   for well over 120 ms at a normal speaking level, where [`SPEECH_FLOOR`]
///   sits some 15 dB below near-field speech.
///
/// **Erring low is deliberate here, and it is the opposite direction from
/// [`SPEECH_FLOOR`]'s**, because these two constants fail differently. Too high
/// a threshold refuses a command somebody really said, and the user has no way
/// to tell that from the feature being broken. Too low a one lets a report say
/// it heard something nobody said — which is what PRD #802's product owner met,
/// and is recoverable in one glance because the sentence beside it is the audio
/// it came from. So this is set where a *word* clears it comfortably rather than
/// where a *tap* only just fails it.
///
/// It is a threshold on the AUDIO and not on the transcript: nothing here reads
/// what a model returned, and no list of known hallucinated phrases exists
/// anywhere in this module. A blocklist of training artefacts would be endless,
/// locale-specific and wrong the first time somebody said one of them.
pub const MIN_SPEECH: Duration = Duration::from_millis(120);

/// How long the quiet has to run before the utterance is over.
///
/// 800 ms — longer than the pauses inside a spoken phrase, where a comma is
/// 200-400 ms, and short enough to stay a minority of what the user waits for
/// rather than the term that dominates it: PRD #802 measured a 0.653 s median
/// for the default speech container and 0.62-1.03 s for commands, so the hold
/// is one term of a wait a little over a second. **It used to be a far smaller
/// fraction of that wait**, when the default intent backend was the agent CLI
/// at 4.3-6.3 s; that backend went and this number did not, which is worth
/// knowing before anyone quotes the hold as negligible.
pub const SILENCE_HOLD: Duration = Duration::from_millis(800);

/// The window one RMS is measured over, in output samples: 20 ms at
/// [`TARGET_SAMPLE_RATE`].
///
/// The frame length every speech VAD uses, and the granularity of
/// [`SILENCE_HOLD`] — a hold is counted in whole frames, so it is accurate to
/// 20 ms, which is two orders of magnitude inside the thing it is measuring.
const VAD_FRAME: usize = TARGET_SAMPLE_RATE as usize / 50;

/// Voice-activity detection: an RMS threshold, and a quiet that has run long
/// enough to be the end of what somebody said.
///
/// This is what [`Pcm16::is_silent`] explicitly is not, and the difference is
/// the whole reason both exist. That one looks at a finished buffer and decides
/// whether a backend call is worth making. This one runs on the data path,
/// frame by frame, and answers a question with a **time** in it — *has the
/// speaking stopped?* — which is what turns one open microphone into a sequence
/// of separate utterances.
///
/// **It is not a speech/noise classifier and does not try to be.** A steady
/// loud noise reads as speech here and holds the utterance open; what bounds
/// that is [`MAX_UTTERANCE`], and PRD #802's surface discards a capped segment
/// rather than paying to transcribe it. A more discriminating detector is a
/// model, with a model's size, licence and failure modes, and nothing in a
/// navigation vocabulary of one-to-four-word commands needs one.
///
/// Silence before the first speech is ignored, so a microphone switched on in a
/// quiet room does not immediately "end" an utterance nobody started. That is
/// also why an open microphone nobody speaks into ends at the cap rather than
/// here.
pub struct Vad {
    /// [`SPEECH_FLOOR`] squared, so a frame costs no square root.
    floor: f64,
    /// [`SILENCE_HOLD`] in output samples.
    hold: usize,
    /// Sum of squares of the frame being filled.
    energy: f64,
    /// How much of that frame has arrived.
    filled: usize,
    /// Whether any frame has been over the floor yet.
    heard: bool,
    /// Output samples of below-floor audio since the last one that was not.
    quiet: usize,
    /// Latched: an utterance that has ended does not un-end.
    ended: bool,
}

impl Default for Vad {
    fn default() -> Self {
        Self::new(SPEECH_FLOOR, SILENCE_HOLD)
    }
}

impl Vad {
    pub fn new(speech_floor: u16, hold: Duration) -> Self {
        Self {
            floor: f64::from(speech_floor) * f64::from(speech_floor),
            hold: (hold.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)) as usize,
            energy: 0.0,
            filled: 0,
            heard: false,
            quiet: 0,
            ended: false,
        }
    }

    /// Accept output samples, in the order they were produced.
    ///
    /// Called with whatever the resampler just emitted — a few hundred samples
    /// per device callback — rather than a fixed block, so it carries a partial
    /// frame across calls. A trailing partial frame is not judged: at most 20 ms
    /// of the hold is therefore uncounted, which is why the hold is counted in
    /// samples and not in calls.
    pub fn push(&mut self, samples: &[i16]) {
        if self.ended {
            return;
        }
        for &sample in samples {
            let value = f64::from(sample);
            self.energy += value * value;
            self.filled += 1;
            if self.filled < VAD_FRAME {
                continue;
            }
            let mean_square = self.energy / VAD_FRAME as f64;
            self.energy = 0.0;
            self.filled = 0;
            // Compared as mean square against the squared floor: the same
            // comparison as RMS against the floor, without the square root.
            if mean_square >= self.floor {
                self.heard = true;
                self.quiet = 0;
            } else if self.heard {
                self.quiet += VAD_FRAME;
                if self.quiet >= self.hold {
                    self.ended = true;
                    return;
                }
            }
        }
    }

    /// Whether the utterance is over: speech was heard, and the quiet after it
    /// has run for [`SILENCE_HOLD`].
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Whether anything over the floor has arrived at all.
    pub fn heard_speech(&self) -> bool {
        self.heard
    }
}

/// Where a device's callback thread puts its samples.
///
/// Owns the whole conversion — channel downmix, resample to
/// [`TARGET_SAMPLE_RATE`], `f32` to `i16` — because the alternative is doing it
/// per device backend and per sample format. It is also where the length cap is
/// enforced on the data path, so the allocation is bounded whether or not the
/// timer that releases the device ever fires.
pub struct PcmSink {
    format: AudioFormat,
    /// The ceiling, in output samples.
    cap: usize,
    state: Mutex<SinkState>,
    /// Read by the status command while the device thread holds the lock, which
    /// is why it is not simply a field of [`SinkState`].
    full: AtomicBool,
    /// Output samples accumulated, mirrored out of the lock for the same
    /// reason. Monotonic until [`PcmSink::finish`].
    written: AtomicU64,
    /// Whether [`Vad`] has ended the utterance, mirrored for the same reason
    /// and latched for [`Vad::ended`]'s.
    ended: AtomicBool,
}

#[derive(Default)]
struct SinkState {
    out: Vec<i16>,
    /// Fed every output sample as it is produced, which is what makes the
    /// detection incremental rather than a pass over the finished buffer.
    vad: Vad,
    /// Where the next output sample sits, as an absolute position in **input
    /// frames**. Fractional because the rates rarely divide.
    next: f64,
    /// Absolute index of the first input frame the next callback will carry.
    base: u64,
    /// The last input frame of the previous callback.
    ///
    /// This is the whole of what makes the resampler incremental rather than
    /// per-chunk: an output sample whose position falls between the last frame
    /// of one callback and the first frame of the next interpolates across that
    /// seam. Resampling each callback independently instead would put a
    /// discontinuity at every buffer boundary — a few hundred per second, which
    /// is audible as a buzz and is exactly the kind of damage a speech model
    /// has no reason to survive.
    prev: Option<f32>,
}

/// One sample as a device delivered it, normalised to `-1.0..=1.0`.
///
/// A trait of this module's own rather than `cpal`'s conversion traits, so
/// [`PcmSink`] names no `cpal` type at all and most of the tests below drive
/// it with plain slices. The ranges and origins are the ones
/// `cpal::SampleFormat` documents: the signed formats sit at zero and the
/// unsigned ones at the midpoint, which is why 128 in a `u8` stream is silence
/// and not full negative scale.
///
/// **The two 24-bit impls are the exception, and they have to be.** There is
/// no Rust primitive for a 24-bit sample, so `cpal` carries its own
/// `I24`/`U24` newtypes over `i32`, and a device delivering one hands the
/// callback a slice of them. Converting through a temporary `Vec` to keep
/// `cpal` out of this trait would allocate per callback, which is the one
/// thing [`PcmSink::push`] is written not to do.
///
/// Full scale in a signed format is `-MIN` rather than `MAX`, since two's
/// complement is asymmetric — `i8::MAX` is 127/128 of full scale, and that is
/// the honest value rather than one nudged to 1.0.
pub trait DeviceSample: Copy {
    fn to_unit(self) -> f32;
}

impl DeviceSample for f32 {
    #[inline]
    fn to_unit(self) -> f32 {
        self
    }
}

impl DeviceSample for f64 {
    #[inline]
    fn to_unit(self) -> f32 {
        self as f32
    }
}

impl DeviceSample for i8 {
    #[inline]
    fn to_unit(self) -> f32 {
        f32::from(self) / -f32::from(i8::MIN)
    }
}

impl DeviceSample for i16 {
    #[inline]
    fn to_unit(self) -> f32 {
        f32::from(self) / -f32::from(i16::MIN)
    }
}

impl DeviceSample for i32 {
    #[inline]
    fn to_unit(self) -> f32 {
        (f64::from(self) / -f64::from(i32::MIN)) as f32
    }
}

impl DeviceSample for u8 {
    #[inline]
    fn to_unit(self) -> f32 {
        (f32::from(self) - 128.0) / 128.0
    }
}

impl DeviceSample for u16 {
    #[inline]
    fn to_unit(self) -> f32 {
        (f32::from(self) - 32_768.0) / 32_768.0
    }
}

impl DeviceSample for i64 {
    #[inline]
    fn to_unit(self) -> f32 {
        (self as f64 / -(i64::MIN as f64)) as f32
    }
}

impl DeviceSample for u32 {
    #[inline]
    fn to_unit(self) -> f32 {
        ((f64::from(self) - 2_147_483_648.0) / 2_147_483_648.0) as f32
    }
}

impl DeviceSample for u64 {
    #[inline]
    fn to_unit(self) -> f32 {
        ((self as f64 - U64_ORIGIN) / U64_ORIGIN) as f32
    }
}

/// `1 << 63`, the origin of a `u64` sample stream.
const U64_ORIGIN: f64 = 9_223_372_036_854_775_808.0;

/// Full scale for a 24-bit sample, `1 << 23`.
///
/// Both 24-bit formats share it: it is `-I24::MIN` and it is `U24`'s origin,
/// which is the same relationship every other signed/unsigned pair here has.
const SCALE_24: f32 = 8_388_608.0;

impl DeviceSample for cpal::I24 {
    #[inline]
    fn to_unit(self) -> f32 {
        self.inner() as f32 / SCALE_24
    }
}

impl DeviceSample for cpal::U24 {
    #[inline]
    fn to_unit(self) -> f32 {
        // `U24` is a 0..=(1 << 24) - 1 value carried in an `i32`, so the
        // subtraction is what re-centres it; there is no wrapping to guard.
        (self.inner() as f32 - SCALE_24) / SCALE_24
    }
}

impl PcmSink {
    pub fn new(format: AudioFormat, cap: Duration) -> Self {
        let cap = (cap.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)) as usize;
        Self {
            format,
            cap,
            state: Mutex::new(SinkState::default()),
            full: AtomicBool::new(false),
            written: AtomicU64::new(0),
            ended: AtomicBool::new(false),
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Whether the length cap has been reached.
    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Relaxed)
    }

    /// Whether [`Vad`] has decided the speaking stopped.
    ///
    /// Read by [`CaptureSession::status`] while the device thread may be
    /// holding the sink lock, which is why it is an atomic rather than a look
    /// inside [`SinkState`]: a status poll must never queue behind a callback.
    pub fn utterance_ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    /// How much audio has been accumulated.
    ///
    /// What has been **converted**, which lags what the device has delivered by
    /// at most one input frame: the resampler holds the newest frame until it
    /// has a successor to interpolate towards, and [`PcmSink::finish`] is what
    /// flushes it. That is sub-millisecond at every rate a device offers, and it
    /// is named here because it makes this value one sample short of the
    /// arithmetic a reader would do from the sample rate.
    pub fn captured(&self) -> Duration {
        Duration::from_secs_f64(
            self.written.load(Ordering::Relaxed) as f64 / f64::from(TARGET_SAMPLE_RATE),
        )
    }

    /// Accept one callback's worth of interleaved samples.
    ///
    /// Called from the device's own callback thread, which is why it takes
    /// `&self` and why nothing here allocates per sample: the channel downmix
    /// reads the slice in place rather than building a mono copy first.
    pub fn push<S: DeviceSample>(&self, interleaved: &[S]) {
        let channels = self.format.channels.max(1) as usize;
        let frames = interleaved.len() / channels;
        if frames == 0 {
            return;
        }
        let ratio = f64::from(self.format.sample_rate.max(1)) / f64::from(TARGET_SAMPLE_RATE);

        let mut state = match self.state.lock() {
            Ok(state) => state,
            // A poisoned sink means a previous callback panicked. Dropping the
            // samples is the right answer: the recording is already wrong, and
            // the alternative is a second panic on the device thread.
            Err(poisoned) => poisoned.into_inner(),
        };

        let base = state.base;
        let prev = state.prev.unwrap_or(0.0);
        let at = |index: u64| -> f32 {
            if index < base {
                prev
            } else {
                let start = ((index - base) as usize) * channels;
                let frame = &interleaved[start..start + channels];
                frame.iter().map(|s| s.to_unit()).sum::<f32>() / channels as f32
            }
        };
        // The highest input index this callback can answer for.
        let highest = base + frames as u64 - 1;
        // Where this callback's own output starts, so the detector below is fed
        // exactly what was produced here and nothing twice.
        let produced = state.out.len();

        while state.out.len() < self.cap {
            let floor = state.next.floor();
            let index = floor as u64;
            // Both ends of the interpolation have to be readable.
            if index + 1 > highest {
                break;
            }
            let fraction = (state.next - floor) as f32;
            let left = at(index);
            let right = at(index + 1);
            state.out.push(to_i16(left + (right - left) * fraction));
            state.next += ratio;
        }

        state.prev = Some(at(highest));
        state.base = base + frames as u64;
        state.observe(produced);
        self.finished_pushing(&state);
    }

    /// Take the utterance, flushing whatever the resampler was still holding.
    ///
    /// The last input frame of the stream has no successor to interpolate
    /// towards, so the tail is emitted by holding it. Without this the output
    /// is short by up to one input frame's worth — inaudible, but it makes
    /// "1 second in, 16000 samples out" false, which is the property the
    /// resampling tests assert.
    pub fn finish(&self) -> Pcm16 {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        let ratio = f64::from(self.format.sample_rate.max(1)) / f64::from(TARGET_SAMPLE_RATE);
        let held = to_i16(state.prev.unwrap_or(0.0));
        let total = state.base as f64;
        let produced = state.out.len();
        while state.out.len() < self.cap && state.next < total {
            state.out.push(held);
            state.next += ratio;
        }
        state.observe(produced);
        self.finished_pushing(&state);
        Pcm16::new(std::mem::take(&mut state.out))
    }

    fn finished_pushing(&self, state: &SinkState) {
        self.written
            .store(state.out.len() as u64, Ordering::Relaxed);
        if state.out.len() >= self.cap {
            self.full.store(true, Ordering::Relaxed);
        }
        if state.vad.ended() {
            self.ended.store(true, Ordering::Relaxed);
        }
    }
}

impl SinkState {
    /// Hand the detector the output samples from `produced` onward.
    ///
    /// A method rather than two lines at each call site because the two fields
    /// have to be borrowed disjointly — `self.vad` mutably and `self.out`
    /// immutably — which is available through `&mut self` here and not through
    /// the `MutexGuard` the callers hold.
    fn observe(&mut self, produced: usize) {
        let SinkState { out, vad, .. } = self;
        vad.push(&out[produced..]);
    }
}

#[inline]
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16
}

/// An open capture stream.
///
/// **Dropping it stops the device.** That is the whole contract, which is why
/// the trait declares no method: every implementation's teardown belongs in its
/// own `Drop`, and a `stop()` that a caller could forget would be a microphone
/// left open.
pub trait AudioStream: Send {}

/// A device that was opened, and the sink it is filling.
pub struct Capture {
    pub stream: Box<dyn AudioStream>,
    pub sink: Arc<PcmSink>,
}

/// A microphone.
///
/// The source builds the sink because only it knows the device's format, and
/// asking twice could answer twice: `cpal`'s `default_input_config` is a query
/// against the OS, not a constant, and a sink built from a stale answer
/// resamples from the wrong rate.
pub trait AudioSource: Send + Sync {
    /// Open the device and start delivering into a sink capped at `cap`.
    ///
    /// Blocking — opening an audio device is a round trip to the OS and can
    /// prompt — so callers put it on a blocking thread, the way a keychain call
    /// already is.
    fn start(&self, cap: Duration) -> Result<Capture, CaptureError>;
}

// -- the real device -------------------------------------------------------

/// The default input device, through `cpal`.
///
/// # It runs on a thread of its own, and that is not an optimisation
///
/// `cpal::Stream` is `!Send` on some hosts — CoreAudio's is — so it cannot be
/// held in a `tauri::State` at all. A dedicated thread owns the stream for its
/// whole life and is told to let go through a channel, which makes the handle
/// this module hands back `Send` on every platform rather than on two of them.
pub struct CpalSource;

impl CpalSource {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CpalSource {
    fn default() -> Self {
        Self::new()
    }
}

/// The handle to a running [`CpalSource`] thread.
struct CpalStream {
    /// Hanging this up is what tells the device thread to drop the stream.
    stop: Option<std::sync::mpsc::Sender<()>>,
    joiner: Option<std::thread::JoinHandle<()>>,
}

impl AudioStream for CpalStream {}

impl Drop for CpalStream {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(joiner) = self.joiner.take() {
            // Joined rather than detached: the sink outlives this handle by one
            // `finish()` call, and a callback still running while that reads
            // would race it.
            let _ = joiner.join();
        }
    }
}

impl AudioSource for CpalSource {
    fn start(&self, cap: Duration) -> Result<Capture, CaptureError> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Arc<PcmSink>, CaptureError>>();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let joiner = std::thread::Builder::new()
            .name("dad-voice-capture".to_string())
            .spawn(move || device_thread(cap, &ready_tx, &stop_rx))
            .map_err(|error| {
                CaptureError::Device(format!("the microphone thread would not start ({error})"))
            })?;
        match ready_rx.recv() {
            Ok(Ok(sink)) => Ok(Capture {
                stream: Box::new(CpalStream {
                    stop: Some(stop_tx),
                    joiner: Some(joiner),
                }),
                sink,
            }),
            Ok(Err(error)) => {
                let _ = joiner.join();
                Err(error)
            }
            // The thread ended without answering, which is a panic inside it.
            Err(_) => {
                let _ = joiner.join();
                Err(CaptureError::Device(
                    "the microphone thread stopped before it opened a device".to_string(),
                ))
            }
        }
    }
}

/// Owns one `cpal::Stream` for its whole life.
///
/// Never reached by a test: it needs a device and an audio server, and CI has
/// neither. Everything above it is driven through [`StubSource`] instead, which
/// is the trait's reason for existing.
fn device_thread(
    cap: Duration,
    ready: &std::sync::mpsc::Sender<Result<Arc<PcmSink>, CaptureError>>,
    stop: &std::sync::mpsc::Receiver<()>,
) {
    let opened = open_device(cap);
    match opened {
        Err(error) => {
            let _ = ready.send(Err(error));
        }
        Ok((stream, sink)) => {
            if ready.send(Ok(sink)).is_err() {
                return;
            }
            // Returns on a stop, and on the handle being dropped.
            let _ = stop.recv();
            drop(stream);
        }
    }
}

fn open_device(cap: Duration) -> Result<(cpal::Stream, Arc<PcmSink>), CaptureError> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host.default_input_device().ok_or_else(|| {
        CaptureError::Device("no microphone was found on this machine".to_string())
    })?;
    let supported = device.default_input_config().map_err(|error| {
        CaptureError::Device(format!(
            "the microphone would not report a format ({error})"
        ))
    })?;
    let format = AudioFormat::new(supported.sample_rate(), supported.channels());
    let sample_format = supported.sample_format();
    // Refused BEFORE the device is opened, which is the difference between a
    // sentence a user can act on and a recording that heard nothing.
    if let Some(refusal) = unsupported_format(sample_format) {
        return Err(CaptureError::Device(refusal));
    }
    let sink = Arc::new(PcmSink::new(format, cap));

    let filling = Arc::clone(&sink);
    let stream = device
        .build_input_stream_raw(
            supported.config(),
            sample_format,
            move |data, _| feed(&filling, data),
            // Nothing is logged here. A device error names hardware and not the
            // utterance, but the rule this feature adopts is that the capture
            // path writes no log line at all, and a callback that starts
            // printing is one edit away from printing what it was handed. The
            // user-visible consequence is an empty or short recording, which
            // the surface already has a sentence for.
            |_error| {},
            None,
        )
        .map_err(|error| {
            CaptureError::Device(format!("the microphone would not open ({error})"))
        })?;
    stream.play().map_err(|error| {
        CaptureError::Device(format!("the microphone would not start ({error})"))
    })?;

    Ok((stream, sink))
}

/// `as_slice` answers `None` when the format does not match the type, which
/// [`conversion_for`] has already decided — so this cannot silently drop a
/// buffer the routing claimed to handle.
fn take<S: cpal::SizedSample + DeviceSample>(sink: &PcmSink, data: &cpal::Data) {
    if let Some(samples) = data.as_slice::<S>() {
        sink.push(samples);
    }
}

/// How one device format reaches the sink, or `None` where it cannot.
///
/// **One match with two consumers, and that is the whole point of it being a
/// function.** [`open_device`] asks it before opening a device and refuses one
/// it cannot read; [`feed`] asks it per callback and routes the buffer. Two
/// lists would drift, and the drift is silent in precisely the direction that
/// matters — a format accepted at open and unhandled in the callback is a
/// microphone that records nothing and says nothing, which is the defect this
/// shape replaced: `I24`, `U24`, `I64` and `U64` fell through an empty arm, so
/// a device whose default input config named one of them recorded an empty
/// buffer and the user was told the microphone heard nothing.
///
/// `cpal::SampleFormat` is `#[non_exhaustive]`, so the fall-through is
/// load-bearing rather than tidy: a variant a later `cpal` adds is refused at
/// open rather than dropped per callback.
///
/// **The DSD trio is the deliberate `None`.** It is a 1-bit sigma-delta stream
/// and not PCM at all — turning it into samples needs a decimation filter,
/// which is a signal-processing project rather than a [`DeviceSample`] impl.
/// A user with such a device gets a sentence naming their format instead of a
/// recording that heard nothing.
fn conversion_for(format: cpal::SampleFormat) -> Option<fn(&PcmSink, &cpal::Data)> {
    use cpal::SampleFormat as Format;

    Some(match format {
        Format::F32 => take::<f32>,
        Format::F64 => take::<f64>,
        Format::I8 => take::<i8>,
        Format::I16 => take::<i16>,
        Format::I24 => take::<cpal::I24>,
        Format::I32 => take::<i32>,
        Format::I64 => take::<i64>,
        Format::U8 => take::<u8>,
        Format::U16 => take::<u16>,
        Format::U24 => take::<cpal::U24>,
        Format::U32 => take::<u32>,
        Format::U64 => take::<u64>,
        _ => return None,
    })
}

/// The sentence a device this build cannot read is refused with, or `None`
/// when it can be read.
///
/// Actionable rather than apologetic: it names the format, because that is the
/// one thing the user can take to their system's audio settings. The
/// alternative it replaced was a device that opened, recorded, and yielded
/// nothing.
fn unsupported_format(format: cpal::SampleFormat) -> Option<String> {
    if conversion_for(format).is_some() {
        return None;
    }
    Some(format!(
        "this microphone's sample format ({format}) is not one this build can \
         record from — choose a different input device, or change its format \
         in the system's audio settings"
    ))
}

/// Convert one raw callback buffer into the sink.
///
/// A buffer whose format has no conversion contributes nothing, which
/// [`open_device`] has already made unreachable by refusing such a device —
/// this is the second line of defence rather than the first, and it is silence
/// rather than a panic because this runs on a real-time audio thread.
fn feed(sink: &PcmSink, data: &cpal::Data) {
    if let Some(convert) = conversion_for(data.sample_format()) {
        convert(sink, data);
    }
}

// -- the stub --------------------------------------------------------------

/// A device that is not one.
///
/// Every test of the state machine, the cap and the resampler drives this:
/// the whole module has to work on a machine with no microphone and no audio
/// server, which is what a CI runner is. It is `pub` for [`super::StubResolver`]'s
/// reason — the credentialed lane PRD #802 M9 describes wants a capture path it
/// can drive without a room to speak into.
pub struct StubSource {
    format: AudioFormat,
    /// Pushed into the sink as soon as the stream opens.
    samples: Vec<f32>,
    failure: Option<CaptureError>,
    /// Set when the stream this source handed out was dropped, which is how a
    /// test asserts the device was genuinely released.
    stopped: Arc<AtomicBool>,
}

impl StubSource {
    /// A source that delivers `samples` at `format` the moment it is opened.
    pub fn new(format: AudioFormat, samples: Vec<f32>) -> Self {
        Self {
            format,
            samples,
            failure: None,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A source that delivers a steady tone for `seconds`, for driving the
    /// length cap without writing out a million literals.
    pub fn tone(format: AudioFormat, seconds: f64) -> Self {
        let frames = (f64::from(format.sample_rate) * seconds) as usize;
        let channels = format.channels.max(1) as usize;
        let samples = (0..frames * channels)
            .map(|i| {
                let frame = (i / channels) as f64;
                (frame * 2.0 * std::f64::consts::PI * 440.0 / f64::from(format.sample_rate)).sin()
                    as f32
            })
            .collect();
        Self::new(format, samples)
    }

    /// A source that refuses to open.
    pub fn failing(error: CaptureError) -> Self {
        Self {
            format: AudioFormat::new(TARGET_SAMPLE_RATE, 1),
            samples: Vec::new(),
            failure: Some(error),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Whether the stream handed out has been dropped.
    pub fn stopped(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stopped)
    }
}

struct StubStream(Arc<AtomicBool>);

impl AudioStream for StubStream {}

impl Drop for StubStream {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl AudioSource for StubSource {
    fn start(&self, cap: Duration) -> Result<Capture, CaptureError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let sink = Arc::new(PcmSink::new(self.format, cap));
        // Delivered in callback-sized chunks rather than one slab, because the
        // seam between two callbacks is where an incremental resampler is
        // wrong if it is wrong at all.
        let channels = self.format.channels.max(1) as usize;
        for chunk in self.samples.chunks(1024 * channels) {
            sink.push(chunk);
        }
        Ok(Capture {
            stream: Box::new(StubStream(Arc::clone(&self.stopped))),
            sink,
        })
    }
}

// -- the state machine -----------------------------------------------------

/// What a capture session is doing.
///
/// PRD #802 M7's shape exactly: idle → recording → transcribing → done or
/// failed. Every transition not drawn here is **refused** with a sentence, not
/// panicked on — a double press on the microphone button is a thing users do,
/// and the frontend's idea of the state can legitimately lag this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureState {
    /// Nothing is open. The only state [`CaptureSession::start`] accepts, along
    /// with the two terminal ones.
    Idle,
    /// The device is open, or was and hit the cap.
    Recording,
    /// The device is closed and the transcriber has the buffer.
    Transcribing,
    /// The last utterance produced a transcript.
    Done,
    /// The last utterance did not.
    Failed,
}

impl CaptureState {
    /// Whether a new recording may begin from here.
    fn accepts_start(self) -> bool {
        matches!(self, Self::Idle | Self::Done | Self::Failed)
    }
}

/// What the webview is told about the session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatus {
    pub state: CaptureState,
    /// Milliseconds of audio captured so far.
    pub captured_ms: u32,
    /// [`MAX_UTTERANCE`] in milliseconds, so the surface renders a countdown
    /// against the real bound rather than against a constant of its own that
    /// could drift from this one.
    pub max_ms: u32,
    /// Whether the cap ended the recording rather than the user.
    ///
    /// The surface needs this: from the user's side the microphone simply
    /// stopped, and a panel that did not know why would go on rendering
    /// *listening…* over a closed device.
    pub capped: bool,
}

/// One microphone, one utterance at a time.
///
/// Holds no settings and reads none: which transcriber runs and whether capture
/// is offered at all are decided by the caller, because the caller is where the
/// settings document already is.
pub struct CaptureSession {
    source: Arc<dyn AudioSource>,
    inner: Mutex<SessionInner>,
}

struct SessionInner {
    state: CaptureState,
    /// Present exactly while `state` is [`CaptureState::Recording`].
    live: Option<Live>,
    /// Bumped by every start and every cancel, so a cap timer that fires after
    /// its own recording has already been stopped ends nothing — and so a
    /// device that finishes opening after a cancel can tell that it did.
    generation: u64,
    /// The generation an in-flight [`CaptureSession::start`] reserved before it
    /// released the lock to open the device.
    ///
    /// **This is what makes cancellation observable across the open**, which is
    /// the whole of PRD #802's audit blocker: `AudioSource::start` blocks on the
    /// OS — a permission dialog, a slow audio server — and it cannot be called
    /// under the lock without parking every status poll behind it. So the lock
    /// is released, and a `cancel` arriving in that window used to set the
    /// session idle and be *overwritten* when the open completed, because idle
    /// is a state a start is allowed to begin from. The user saw a cancel
    /// succeed and the microphone recorded on with the panel closed.
    ///
    /// A reservation turns that back into something the returning `start` can
    /// check: it takes the next generation before releasing the lock, and
    /// installs the stream only if that exact generation is still reserved when
    /// it comes back. [`CaptureSession::cancel`] clears the reservation, so
    /// cancellation wins the race by construction rather than by timing.
    ///
    /// Deliberately NOT an `Opening` variant of [`CaptureState`]: that enum is
    /// serialised to the webview, and a state token the panel has never heard of
    /// would arrive on any status poll made while the device is opening.
    opening: Option<u64>,
}

struct Live {
    stream: Box<dyn AudioStream>,
    sink: Arc<PcmSink>,
}

/// A cap timer's claim on one recording.
///
/// Carries the generation so the timer for utterance *n* cannot close utterance
/// *n+1* — the sequence that produces it is ordinary: record, stop, start
/// again, all inside [`MAX_UTTERANCE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureTicket(u64);

impl CaptureSession {
    pub fn new(source: Arc<dyn AudioSource>) -> Self {
        Self {
            source,
            inner: Mutex::new(SessionInner {
                state: CaptureState::Idle,
                live: None,
                generation: 0,
                opening: None,
            }),
        }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, SessionInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// What the session is doing right now.
    pub fn status(&self) -> CaptureStatus {
        let inner = self.inner();
        let (captured, capped, ended) = match &inner.live {
            Some(live) => (
                live.sink.captured(),
                live.sink.is_full(),
                live.sink.utterance_ended(),
            ),
            None => (Duration::ZERO, false, false),
        };
        CaptureStatus {
            // [`Vad`] has heard the speaking stop: the recording is still open
            // and the audio is still here, but nothing more is coming, so the
            // surface is told the utterance is DONE and takes it with a stop.
            //
            // The reported state moves while `inner.state` does not, which is
            // deliberate: `stop` is the transition, and it still refuses
            // anything that is not `Recording`. What this reports is the
            // utterance's state, not the session's.
            //
            // `Done` is the same token [`CaptureSession::settle`] leaves behind
            // after a transcript, and on the wire the two are indistinguishable.
            // In Rust they are not — that one has no `live` — and the surface
            // cannot confuse them either, because it only asks between a start
            // and a stop. The token is honest for both: the utterance is over.
            state: if ended && inner.state == CaptureState::Recording {
                CaptureState::Done
            } else {
                inner.state
            },
            captured_ms: millis(captured),
            max_ms: millis(MAX_UTTERANCE),
            capped,
        }
    }

    /// Open the device. Idle, Done or Failed → Recording.
    ///
    /// Returns the ticket a cap timer has to present, so the caller owns the
    /// timer rather than this type spawning one — which keeps the session free
    /// of an async runtime and lets a test advance a paused clock instead of
    /// waiting thirty seconds.
    ///
    /// **`Ok` does not promise the returned status is `Recording`.** The status
    /// is read after the lock is released, so a [`CaptureSession::cancel`] that
    /// lands in that window returns `Ok((Idle, ticket))` with a ticket that is
    /// already a generation behind. It is benign — the cancel released the
    /// device and the stale ticket makes [`CaptureSession::cap_reached`] a
    /// no-op — but a caller that reads `Ok` as *"it is recording"* would be
    /// wrong, so read the returned [`CaptureStatus::state`] rather than
    /// inferring it. Not closed by re-reading under the lock: that would return
    /// a status contradicting the cancel the user just made, which is the worse
    /// of the two answers.
    pub fn start(&self) -> Result<(CaptureStatus, CaptureTicket), CaptureError> {
        // Reserved UNDER the lock, before the device is touched. See
        // `SessionInner::opening` for why this is a reservation rather than a
        // re-check on the way back.
        let reserved = {
            let mut inner = self.inner();
            if !inner.state.accepts_start() {
                return Err(CaptureError::Refused(refusal(inner.state, "start")));
            }
            if inner.opening.is_some() {
                return Err(CaptureError::Refused(OPENING_REFUSAL.to_string()));
            }
            inner.generation += 1;
            inner.opening = Some(inner.generation);
            inner.generation
        };

        // Opened OUTSIDE the lock: `AudioSource::start` blocks on the OS, and
        // holding the session lock across it would park the status command for
        // as long as the device takes to open.
        let capture = match self.source.start(MAX_UTTERANCE) {
            Ok(capture) => capture,
            Err(error) => {
                // Release our own reservation, and only ours: a cancel that
                // arrived while the device was failing to open already cleared
                // it and may have handed it to a later start.
                let mut inner = self.inner();
                if inner.opening == Some(reserved) {
                    inner.opening = None;
                }
                return Err(error);
            }
        };

        let mut inner = self.inner();
        if inner.opening != Some(reserved) {
            // Cancelled (or superseded) while the device was opening. The
            // stream is dropped rather than installed — which closes the
            // device, since a stream's `Drop` is what releases it — and the
            // state the canceller left is untouched.
            let state = inner.state;
            drop(inner);
            drop(capture.stream);
            return Err(CaptureError::Refused(refusal(state, "start")));
        }
        inner.opening = None;
        inner.state = CaptureState::Recording;
        inner.live = Some(Live {
            stream: capture.stream,
            sink: capture.sink,
        });
        let ticket = CaptureTicket(reserved);
        drop(inner);
        Ok((self.status(), ticket))
    }

    /// Close the device and take the utterance. Recording → Transcribing.
    ///
    /// The buffer comes back with the state already moved, so nothing else can
    /// start a recording while the transcriber has it.
    pub fn stop(&self) -> Result<Pcm16, CaptureError> {
        let mut inner = self.inner();
        if inner.state != CaptureState::Recording {
            return Err(CaptureError::Refused(refusal(inner.state, "stop")));
        }
        let Live { stream, sink } = inner.live.take().ok_or_else(|| {
            CaptureError::Device("the recording ended with no device attached".to_string())
        })?;
        // Moved UNDER the lock, before anything slow happens, so nothing can
        // start a recording in the window the teardown below opens —
        // `Transcribing` is a state `accepts_start` refuses.
        inner.state = CaptureState::Transcribing;
        // And the lock goes before the device does. `AudioStream::drop` joins
        // the device thread and is therefore as slow as the platform's own
        // teardown; holding the session mutex across it parked every
        // concurrent `status`, `cancel` and `start` behind a driver.
        drop(inner);
        // Dropped BEFORE the buffer is read: the stream's `Drop` joins the
        // device thread, so no callback can still be writing when `finish`
        // takes the samples. That ordering is what the lock was never needed
        // for — it is between these two lines, not around them.
        drop(stream);
        Ok(sink.finish())
    }

    /// Transcribing → Done or Failed.
    pub fn settle(&self, heard: bool) -> CaptureStatus {
        let mut inner = self.inner();
        // Not a refusal: `settle` is the tail of `stop`, and a session reset out
        // from under it (a cancel while the request was in flight) has already
        // said what the state is.
        if inner.state == CaptureState::Transcribing {
            inner.state = if heard {
                CaptureState::Done
            } else {
                CaptureState::Failed
            };
        }
        drop(inner);
        self.status()
    }

    /// Abandon whatever is happening, without transcribing anything.
    ///
    /// Idempotent and never refused: it is what a closed panel, an escape key
    /// and a failed start all do, and each of those can arrive in any state.
    ///
    /// **Clearing `opening` is the load-bearing line**, not the state reset: a
    /// device that is still being opened has nothing to release here, and what
    /// stops it being installed a moment later is that the reservation it is
    /// holding is no longer the one this session recognises. Bumping the
    /// generation as well keeps a cap timer for the abandoned utterance inert.
    pub fn cancel(&self) -> CaptureStatus {
        let mut inner = self.inner();
        // TAKEN rather than cleared, so the device's teardown runs after the
        // guard below is dropped — `inner.live = None` would have run it
        // here, with the lock held. See [`CaptureSession::stop`].
        let live = inner.live.take();
        inner.state = CaptureState::Idle;
        inner.generation += 1;
        inner.opening = None;
        drop(inner);
        drop(live);
        self.status()
    }

    /// The length cap elapsed: release the device but keep the audio.
    ///
    /// Does nothing unless `ticket` names the recording still running, so a
    /// timer that outlived its own utterance is a no-op rather than a
    /// microphone closing under the next one. The state stays
    /// [`CaptureState::Recording`] — what ended is the *capture*, and the
    /// utterance is still the user's to send or discard, which is why
    /// [`CaptureStatus::capped`] exists.
    pub fn cap_reached(&self, ticket: CaptureTicket) -> CaptureStatus {
        // The stream the substitution below displaces, carried out of the
        // lock. `live.stream = Box::new(ClosedStream)` drops what it replaces
        // AT the assignment — the least visible of this type's three teardowns
        // and the same defect as the other two. See [`CaptureSession::stop`].
        let mut released: Option<Box<dyn AudioStream>> = None;
        let mut inner = self.inner();
        if inner.generation == ticket.0
            && inner.state == CaptureState::Recording
            && let Some(live) = inner.live.as_mut()
        {
            released = Some(std::mem::replace(&mut live.stream, Box::new(ClosedStream)));
            live.sink.full.store(true, Ordering::Relaxed);
        }
        drop(inner);
        drop(released);
        self.status()
    }
}

/// What replaces a real stream once the cap has released it.
///
/// Substituting rather than clearing keeps `live` present, so the sink is still
/// reachable and the captured audio survives to be transcribed.
struct ClosedStream;

impl AudioStream for ClosedStream {}

/// What a second start is told while the first is still opening the device.
///
/// Its own sentence rather than [`refusal`]'s, because the state machine cannot
/// supply one: the session is still `Idle` at that moment — the reservation is
/// what is occupied, not the state — so `refusal` would say "nothing is being
/// recorded", which is true and useless.
const OPENING_REFUSAL: &str = "cannot start the microphone: the device is already being opened";

fn refusal(state: CaptureState, verb: &str) -> String {
    let doing = match state {
        CaptureState::Idle => "nothing is being recorded",
        CaptureState::Recording => "a recording is already running",
        CaptureState::Transcribing => "the last recording is still being transcribed",
        CaptureState::Done | CaptureState::Failed => "the last recording has already finished",
    };
    format!("cannot {verb} the microphone: {doing}")
}

fn millis(duration: Duration) -> u32 {
    duration.as_millis().min(u128::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(rate: u32) -> AudioFormat {
        AudioFormat::new(rate, 1)
    }

    fn silent_session() -> CaptureSession {
        CaptureSession::new(Arc::new(StubSource::new(
            mono(TARGET_SAMPLE_RATE),
            vec![0.0; 1600],
        )))
    }

    // Nothing below opens a device, an audio server or a socket. Every test
    // drives `StubSource`, which is the reason `AudioSource` is a trait at all:
    // a CI runner has no microphone, and `cpal`'s own types cannot be stood in
    // for.

    // -- the buffer --------------------------------------------------------

    #[test]
    fn voice_capture_pcm_debug_prints_no_samples() {
        let pcm = Pcm16::new(vec![1234, -4321, 7]);
        let rendered = format!("{pcm:?}");
        assert!(!rendered.contains("1234"), "{rendered}");
        assert!(!rendered.contains("4321"), "{rendered}");
        assert_eq!(rendered, "Pcm16(<3 samples / 0 ms, not printed>)");
    }

    #[test]
    fn voice_capture_pcm_debug_hides_samples_through_a_container() {
        // The route that matters: a `{:?}` on something that HOLDS audio, which
        // is how a buffer would reach a log line by accident.
        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)]
            audio: Pcm16,
        }
        let rendered = format!(
            "{:?}",
            Holder {
                audio: Pcm16::new(vec![31337; 4]),
            }
        );
        assert!(!rendered.contains("31337"), "{rendered}");
    }

    #[test]
    fn voice_capture_pcm_duration_is_the_target_rate() {
        assert_eq!(
            Pcm16::new(vec![0; TARGET_SAMPLE_RATE as usize]).duration(),
            Duration::from_secs(1)
        );
        assert_eq!(Pcm16::new(Vec::new()).duration(), Duration::ZERO);
    }

    #[test]
    fn voice_capture_pcm_silence_is_the_floor_not_exact_zero() {
        assert!(Pcm16::new(vec![0; 64]).is_silent());
        assert!(Pcm16::new(vec![7, -11, 3]).is_silent());
        assert!(!Pcm16::new(vec![0, 0, 9_000]).is_silent());
    }

    // -- voice-activity detection ------------------------------------------

    /// Output samples of speech, well over [`SPEECH_FLOOR`] at any frame
    /// boundary: a square wave rather than a sine, so no frame can land on a
    /// quiet part of a cycle and make a test depend on its own arithmetic.
    fn speech(samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect()
    }

    /// Output samples at exactly the target rate, as a count.
    fn out_samples(millis: u64) -> usize {
        (TARGET_SAMPLE_RATE as usize * millis as usize) / 1_000
    }

    // -- eligibility to transcribe -----------------------------------------

    /// The buffer PRD #802's product owner was being charged for, and told he
    /// had said "Don't forget to subscribe" about.
    ///
    /// Thirty seconds of a quiet room with one impulse in it — a keyboard tap,
    /// which is what ends a segment when nobody is speaking.
    fn room_with_one_tap() -> Pcm16 {
        let mut samples = vec![0i16; out_samples(30_000)];
        samples[out_samples(4_000)] = i16::MAX;
        Pcm16::new(samples)
    }

    #[test]
    fn voice_capture_near_silence_with_one_loud_sample_holds_no_speech() {
        let audio = room_with_one_tap();
        // The old guard's whole failure, stated: `all()` needs EVERY sample
        // under the floor, so one tap made thirty seconds of room tone eligible.
        assert!(
            !audio.is_silent(),
            "the fixture must be one `is_silent` passed, or this proves nothing"
        );
        assert!(!audio.has_speech(), "{:?}", audio.speech_run());
        assert!(audio.speech_run() < MIN_SPEECH);
    }

    #[test]
    fn voice_capture_an_impulse_train_never_accumulates_into_speech() {
        // Fifty taps 100 ms apart — a fast typist through a whole segment. A
        // rule that summed speech-level frames would reach a full second here;
        // an unbroken run reaches one frame.
        let mut samples = vec![0i16; out_samples(5_000)];
        for tap in 0..50 {
            samples[out_samples(tap * 100) + 1] = i16::MAX;
        }
        let audio = Pcm16::new(samples);
        assert!(!audio.is_silent());
        assert!(
            !audio.has_speech(),
            "an impulse train read as speech: {:?}",
            audio.speech_run()
        );
    }

    #[test]
    fn voice_capture_an_empty_or_silent_buffer_holds_no_speech() {
        assert!(!Pcm16::new(Vec::new()).has_speech());
        assert!(!Pcm16::new(vec![0; out_samples(30_000)]).has_speech());
        // Room tone under `SPEECH_FLOOR` for the whole buffer, which is the
        // ordinary case rather than the digital-zero one.
        let tone: Vec<i16> = (0..out_samples(30_000))
            .map(|i| if i % 2 == 0 { 180 } else { -180 })
            .collect();
        assert!(!Pcm16::new(tone).has_speech());
    }

    #[test]
    fn voice_capture_a_spoken_word_is_eligible_to_transcribe() {
        // 200 ms of speech-level audio in a quiet second — the shape of a bare
        // "back", which is a real command on the overview and must be sent.
        let mut samples = vec![0i16; out_samples(400)];
        samples.extend(speech(out_samples(200)));
        samples.extend(std::iter::repeat_n(0i16, out_samples(400)));
        let audio = Pcm16::new(samples);
        assert!(audio.has_speech(), "{:?}", audio.speech_run());
        assert!(audio.speech_run() >= Duration::from_millis(200));
    }

    #[test]
    fn voice_capture_speech_run_is_the_longest_unbroken_stretch() {
        // Two runs, the shorter one first, with a gap wider than a frame.
        let mut samples = speech(out_samples(60));
        samples.extend(std::iter::repeat_n(0i16, out_samples(200)));
        samples.extend(speech(out_samples(160)));
        let audio = Pcm16::new(samples);
        assert_eq!(audio.speech_run(), Duration::from_millis(160));
        // Frame-quantised, and it must not count a run the gap broke: 60 + 160
        // is over the threshold and neither run alone would be if it were not.
        assert!(audio.has_speech());
    }

    #[test]
    fn voice_capture_the_threshold_is_exactly_min_speech_and_one_frame_less_fails() {
        // The boundary pinned rather than inferred from a comfortable fixture.
        // `speech_run` divides a sample count by the rate and compares the
        // resulting `Duration`, so the question is whether exactly `MIN_SPEECH`
        // of speech lands on or under the threshold — a one-frame drift here
        // moves the gate for every short command, and it is the kind of drift a
        // rounding change makes silently.
        let frames = MIN_SPEECH.as_millis() as usize / 20;
        assert_eq!(
            frames, 6,
            "MIN_SPEECH moved; this test's arithmetic has not"
        );
        let exactly = Pcm16::new(speech(frames * VAD_FRAME));
        assert_eq!(exactly.speech_run(), MIN_SPEECH);
        assert!(exactly.has_speech(), "exactly MIN_SPEECH must be eligible");
        let one_frame_short = Pcm16::new(speech((frames - 1) * VAD_FRAME));
        assert!(one_frame_short.speech_run() < MIN_SPEECH);
        assert!(!one_frame_short.has_speech());
    }

    #[test]
    fn voice_capture_speech_run_agrees_with_the_vad_on_what_counts() {
        // A hair under `SPEECH_FLOOR` at every frame boundary is not speech to
        // either of them; a hair over is speech to both. One threshold, two
        // readers, and a drift between them would make the live segmentation and
        // the eligibility gate disagree about the same audio.
        for (amplitude, speech_expected) in [(599i16, false), (601i16, true)] {
            let samples: Vec<i16> = (0..out_samples(1_000))
                .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
                .collect();
            let mut vad = Vad::default();
            vad.push(&samples);
            assert_eq!(vad.heard_speech(), speech_expected, "vad at {amplitude}");
            assert_eq!(
                Pcm16::new(samples).has_speech(),
                speech_expected,
                "buffer at {amplitude}"
            );
        }
    }

    #[test]
    fn voice_capture_a_trailing_partial_frame_is_not_judged() {
        // Shorter than one `VAD_FRAME`, however loud: the same rule `Vad::push`
        // applies to the tail it is still filling.
        assert!(!Pcm16::new(speech(VAD_FRAME - 1)).has_speech());
        assert_eq!(
            Pcm16::new(speech(VAD_FRAME - 1)).speech_run(),
            Duration::ZERO
        );
    }

    #[test]
    fn voice_capture_vad_never_ends_an_utterance_nobody_started() {
        let mut vad = Vad::default();
        // Three seconds of a quiet room, which is nearly four holds.
        vad.push(&vec![0; out_samples(3_000)]);
        assert!(!vad.ended(), "silence alone ended an utterance");
        assert!(!vad.heard_speech());
    }

    #[test]
    fn voice_capture_vad_ends_after_the_hold_and_not_before() {
        let mut vad = Vad::default();
        vad.push(&speech(out_samples(500)));
        assert!(vad.heard_speech());
        // One frame short of the hold.
        vad.push(&vec![0; out_samples(800) - VAD_FRAME]);
        assert!(!vad.ended(), "ended before {SILENCE_HOLD:?} of quiet");
        vad.push(&vec![0; VAD_FRAME]);
        assert!(vad.ended(), "did not end after {SILENCE_HOLD:?} of quiet");
    }

    #[test]
    fn voice_capture_vad_restarts_the_hold_at_the_next_word() {
        let mut vad = Vad::default();
        // Two pauses that would each end it if they were counted together.
        vad.push(&speech(out_samples(200)));
        vad.push(&vec![0; out_samples(600)]);
        vad.push(&speech(out_samples(200)));
        vad.push(&vec![0; out_samples(600)]);
        assert!(!vad.ended(), "two pauses under the hold summed into one");
        vad.push(&vec![0; out_samples(200)]);
        assert!(vad.ended());
    }

    #[test]
    fn voice_capture_vad_carries_a_partial_frame_between_pushes() {
        // What a device actually delivers: callback-sized chunks that do not
        // divide by the frame. A detector that dropped the remainder would
        // never accumulate a hold at all.
        let mut vad = Vad::default();
        for chunk in speech(out_samples(400)).chunks(333) {
            vad.push(chunk);
        }
        for chunk in vec![0i16; out_samples(1_000)].chunks(333) {
            vad.push(chunk);
        }
        assert!(vad.ended());
    }

    #[test]
    fn voice_capture_vad_measures_rms_and_not_a_peak() {
        // One click in an otherwise quiet frame: loud enough that
        // `Pcm16::is_silent`'s per-sample floor calls the buffer noisy, and
        // nowhere near enough energy to be somebody speaking.
        let mut click = vec![0i16; VAD_FRAME];
        click[7] = 5_000;
        assert!(!Pcm16::new(click.clone()).is_silent());

        let mut vad = Vad::default();
        vad.push(&speech(out_samples(300)));
        for _ in 0..(out_samples(1_000) / VAD_FRAME) {
            vad.push(&click);
        }
        assert!(vad.ended(), "a click held the utterance open");
    }

    #[test]
    fn voice_capture_vad_ending_is_latched() {
        let mut vad = Vad::default();
        vad.push(&speech(out_samples(300)));
        vad.push(&vec![0; out_samples(1_000)]);
        assert!(vad.ended());
        // A device callback that lands after the surface has been told the
        // utterance is over must not re-open it.
        vad.push(&speech(out_samples(300)));
        assert!(vad.ended(), "late audio un-ended a finished utterance");
    }

    #[test]
    fn voice_capture_sink_reports_the_utterance_ending() {
        let format = mono(TARGET_SAMPLE_RATE);
        let sink = PcmSink::new(format, MAX_UTTERANCE);
        let mut samples: Vec<f32> = speech(out_samples(400))
            .into_iter()
            .map(|s| f32::from(s) / f32::from(i16::MAX))
            .collect();
        sink.push(&samples);
        assert!(!sink.utterance_ended(), "ended while still being spoken");
        samples = vec![0.0; out_samples(1_000)];
        sink.push(&samples);
        assert!(sink.utterance_ended());
    }

    #[test]
    fn voice_capture_status_says_done_when_the_speaking_stops() {
        let format = mono(TARGET_SAMPLE_RATE);
        let mut samples: Vec<f32> = speech(out_samples(400))
            .into_iter()
            .map(|s| f32::from(s) / f32::from(i16::MAX))
            .collect();
        samples.extend(std::iter::repeat_n(0.0, out_samples(1_000)));
        let session = CaptureSession::new(Arc::new(StubSource::new(format, samples)));
        let (_, _ticket) = session.start().expect("opens");

        let status = session.status();
        assert_eq!(status.state, CaptureState::Done, "{status:?}");
        assert!(!status.capped, "nothing reached the cap");
        // And the audio is still there to be taken, which is the whole point of
        // reporting it rather than tearing the recording down.
        let audio = session.stop().expect("stops");
        assert!(!audio.is_silent());
    }

    #[test]
    fn voice_capture_status_stays_recording_while_speech_continues() {
        let format = mono(TARGET_SAMPLE_RATE);
        let samples: Vec<f32> = speech(out_samples(2_000))
            .into_iter()
            .map(|s| f32::from(s) / f32::from(i16::MAX))
            .collect();
        let session = CaptureSession::new(Arc::new(StubSource::new(format, samples)));
        session.start().expect("opens");
        assert_eq!(session.status().state, CaptureState::Recording);
    }

    #[test]
    fn voice_capture_wav_declares_sixteen_bit_mono_at_the_target_rate() {
        let wav = Pcm16::new(vec![-1, 0, 1]).to_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(wav[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
        assert_eq!(
            u32::from_le_bytes(wav[24..28].try_into().unwrap()),
            TARGET_SAMPLE_RATE
        );
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16); // bits
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        // RIFF size is everything after the first eight bytes.
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()) as usize + 8,
            wav.len()
        );
        assert_eq!(wav.len(), 44 + 6);
    }

    // -- resampling --------------------------------------------------------

    /// One second of input at `rate` becomes one second at 16 kHz, whatever the
    /// device offered. The tolerance is one sample: the tail is interpolated
    /// against a held final frame, so the count can land either side of exact.
    fn resamples_one_second(rate: u32, channels: u16) {
        let format = AudioFormat::new(rate, channels);
        let source = StubSource::tone(format, 1.0);
        let capture = source.start(MAX_UTTERANCE).expect("opens");
        let pcm = capture.sink.finish();
        let got = pcm.samples().len() as i64;
        let want = i64::from(TARGET_SAMPLE_RATE);
        assert!(
            (got - want).abs() <= 1,
            "{rate} Hz x{channels} gave {got} samples, wanted {want}"
        );
    }

    #[test]
    fn voice_capture_resamples_every_common_device_rate_to_sixteen_kilohertz() {
        // 48 kHz is what most Linux and Windows devices default to, 44.1 kHz is
        // the other one in the wild, and 16 kHz is the pass-through case that
        // must not be special-cased into a different code path.
        for rate in [8_000, 16_000, 22_050, 44_100, 48_000, 96_000] {
            resamples_one_second(rate, 1);
        }
    }

    #[test]
    fn voice_capture_downmixes_a_stereo_device() {
        resamples_one_second(48_000, 2);
        // And the downmix is a mean, not a channel pick: opposed channels
        // cancel, which a "take the left one" implementation would not do.
        let format = AudioFormat::new(TARGET_SAMPLE_RATE, 2);
        let source = StubSource::new(format, vec![1.0, -1.0, 0.5, -0.5, 1.0, -1.0]);
        let pcm = source.start(MAX_UTTERANCE).expect("opens").sink.finish();
        assert!(pcm.is_silent(), "{pcm:?} was not cancelled to silence");
    }

    #[test]
    fn voice_capture_pass_through_keeps_the_signal() {
        // At the target rate the resampler must be a no-op on the values, not
        // merely on the count.
        let format = mono(TARGET_SAMPLE_RATE);
        let source = StubSource::new(format, vec![1.0, -1.0, 1.0, -1.0]);
        let pcm = source.start(MAX_UTTERANCE).expect("opens").sink.finish();
        assert_eq!(pcm.samples(), &[i16::MAX, -i16::MAX, i16::MAX, -i16::MAX]);
    }

    #[test]
    fn voice_capture_resampling_is_continuous_across_callback_boundaries() {
        // The property the incremental resampler exists for. A ramp pushed in
        // many small callbacks must come out monotonic: a per-chunk resampler
        // restarts its phase at every boundary and produces a sawtooth.
        let format = mono(48_000);
        let sink = PcmSink::new(format, MAX_UTTERANCE);
        let frames = 4_800;
        for chunk in (0..frames)
            .map(|i| i as f32 / frames as f32)
            .collect::<Vec<_>>()
            .chunks(37)
        {
            sink.push(chunk);
        }
        let pcm = sink.finish();
        assert!(pcm.samples().len() > 1_500);
        for pair in pcm.samples().windows(2) {
            assert!(
                pair[1] >= pair[0],
                "a ramp came back non-monotonic at {pair:?}"
            );
        }
    }

    /// Scenario: convert a full-scale buffer in each encoding this build
    /// accepts, and check where the rails and the origins land.
    ///
    /// **The name says "accepts" rather than "a device can deliver", and the
    /// difference is the whole of PR #1163's finding 2.** It was the wider
    /// name, while the module handled eight of the twelve PCM formats `cpal`
    /// declares and dropped the other four — so a test that passed said
    /// nothing about the formats that were broken. What a device can deliver
    /// and what this converts are now the same set only because the DSD trio
    /// is refused at open (see
    /// `voice_capture_refuses_a_device_format_it_cannot_convert`), and the two
    /// tests together are what makes the pair complete.
    #[test]
    fn voice_capture_converts_every_sample_type_it_accepts() {
        // Full scale in each encoding lands at full scale in `i16`.
        let format = mono(TARGET_SAMPLE_RATE);
        fn one<S: DeviceSample>(format: AudioFormat, samples: &[S]) -> Vec<i16> {
            let sink = PcmSink::new(format, MAX_UTTERANCE);
            sink.push(samples);
            sink.finish().samples().to_vec()
        }
        // Within a quantisation step of full scale, not AT it: two's complement
        // is asymmetric, so full scale is `-MIN` and `MAX` is one step short of
        // it — `i16::MAX` converts to 32766 and `i8::MAX` to 32511. Asserting
        // `i16::MAX` exactly would demand a conversion that quietly stretches a
        // device's range; only a float device, whose 1.0 IS full scale, lands
        // on the rail.
        let near_full = |got: i16| {
            assert!(
                (32_500..=i16::MAX).contains(&got),
                "{got} is not within a quantisation step of full scale"
            );
        };
        assert_eq!(one(format, &[1.0f32, 1.0, 1.0])[0], i16::MAX);
        assert_eq!(one(format, &[1.0f64, 1.0, 1.0])[0], i16::MAX);
        near_full(one(format, &[i8::MAX, i8::MAX, i8::MAX])[0]);
        near_full(one(format, &[i16::MAX, i16::MAX, i16::MAX])[0]);
        near_full(one(format, &[i32::MAX, i32::MAX, i32::MAX])[0]);
        near_full(one(format, &[u8::MAX, u8::MAX, u8::MAX])[0]);
        near_full(one(format, &[u16::MAX, u16::MAX, u16::MAX])[0]);
        near_full(one(format, &[u32::MAX, u32::MAX, u32::MAX])[0]);
        near_full(one(format, &[i64::MAX, i64::MAX, i64::MAX])[0]);
        near_full(one(format, &[u64::MAX, u64::MAX, u64::MAX])[0]);
        // The two 24-bit formats, which have no Rust primitive and arrive as
        // `cpal`'s own newtypes over `i32`.
        let i24 = |value: i32| cpal::I24::new(value).expect("in range");
        let u24 = |value: i32| cpal::U24::new(value).expect("in range");
        near_full(one(format, &[i24(8_388_607); 3])[0]);
        near_full(one(format, &[u24(16_777_215); 3])[0]);
        // And the negative rail saturates rather than wrapping.
        assert_eq!(one(format, &[i8::MIN, i8::MIN, i8::MIN])[0], -i16::MAX);
        assert_eq!(one(format, &[u8::MIN, u8::MIN, u8::MIN])[0], -i16::MAX);
        assert_eq!(one(format, &[i24(-8_388_608); 3])[0], -i16::MAX);
        assert_eq!(one(format, &[u24(0); 3])[0], -i16::MAX);
        // And the unsigned origins are silence, not full negative scale.
        assert!(Pcm16::new(one(format, &[128u8; 8])).is_silent());
        assert!(Pcm16::new(one(format, &[32_768u16; 8])).is_silent());
        assert!(Pcm16::new(one(format, &[u24(8_388_608); 8])).is_silent());
        assert!(Pcm16::new(one(format, &[1u64 << 63; 8])).is_silent());
    }

    /// One callback buffer, built the way a host builds one.
    ///
    /// This is what lets the tests reach [`feed`] — the routing seam — rather
    /// than only [`PcmSink::push`] behind it. The whole of the defect it was
    /// written for lived in the routing: every conversion was correct and four
    /// formats never reached one.
    ///
    /// `Data::from_parts` is `unsafe` because it cannot check the pointer
    /// against the format it is told. Here the generic parameter IS the
    /// format — `S::FORMAT` is written into the `Data` and the pointer comes
    /// from a `&mut [S]` — so the obligation is discharged by construction.
    fn callback<S: cpal::SizedSample>(samples: &mut [S]) -> cpal::Data {
        unsafe { cpal::Data::from_parts(samples.as_mut_ptr().cast(), samples.len(), S::FORMAT) }
    }

    /// Scenario: hand the callback seam one full-scale buffer in every format
    /// this build accepts from a device. Each one contributes audible samples.
    ///
    /// The regression is the four that did not. `I24`, `U24`, `I64` and `U64`
    /// fell through an empty match arm, so a device whose default input config
    /// named one of them opened, recorded, and delivered an empty buffer — the
    /// user got "the microphone heard nothing" with nothing to act on. 24-bit
    /// is ordinary on real interfaces, so this was not a corner.
    #[test]
    fn voice_capture_delivers_samples_for_every_format_it_accepts() {
        fn heard<S: cpal::SizedSample>(samples: &mut [S]) {
            let sink = PcmSink::new(mono(TARGET_SAMPLE_RATE), MAX_UTTERANCE);
            feed(&sink, &callback(samples));
            let pcm = sink.finish();
            assert!(
                !pcm.is_empty(),
                "a {} device contributed no samples at all",
                S::FORMAT
            );
            assert!(
                !pcm.is_silent(),
                "a {} device contributed silence from a full-scale buffer",
                S::FORMAT
            );
        }
        heard(&mut [1.0f32; 4]);
        heard(&mut [1.0f64; 4]);
        heard(&mut [i8::MAX; 4]);
        heard(&mut [i16::MAX; 4]);
        heard(&mut [cpal::I24::new(8_388_607).expect("in range"); 4]);
        heard(&mut [i32::MAX; 4]);
        heard(&mut [i64::MAX; 4]);
        heard(&mut [u8::MAX; 4]);
        heard(&mut [u16::MAX; 4]);
        heard(&mut [cpal::U24::new(16_777_215).expect("in range"); 4]);
        heard(&mut [u32::MAX; 4]);
        heard(&mut [u64::MAX; 4]);
    }

    /// Scenario: ask what this build would do with a device whose format it
    /// cannot convert. It refuses the device by name rather than opening one
    /// that records nothing.
    ///
    /// The DSD trio is the whole of that set today, and it is a deliberate
    /// refusal rather than an omission: a 1-bit sigma-delta stream is not PCM
    /// and needs a decimation filter to become samples. What a user must never
    /// get is a microphone that appears to work and yields an empty buffer, so
    /// the refusal names the format they can change.
    #[test]
    fn voice_capture_refuses_a_device_format_it_cannot_convert() {
        use cpal::SampleFormat as Format;

        for format in [Format::DsdU8, Format::DsdU16, Format::DsdU32] {
            let refusal = unsupported_format(format)
                .unwrap_or_else(|| panic!("a {format} device must not be opened"));
            assert!(
                refusal.contains(&format.to_string()),
                "the refusal does not name the format: {refusal}"
            );
            assert!(
                refusal.contains("audio settings"),
                "the refusal gives the user nothing to do: {refusal}"
            );
        }

        // And nothing convertible is refused, which is the half that would
        // otherwise turn a recording bug into a device nobody can use.
        for format in [
            Format::F32,
            Format::F64,
            Format::I8,
            Format::I16,
            Format::I24,
            Format::I32,
            Format::I64,
            Format::U8,
            Format::U16,
            Format::U24,
            Format::U32,
            Format::U64,
        ] {
            assert_eq!(
                unsupported_format(format),
                None,
                "{format} is refused although it converts"
            );
        }
    }

    #[test]
    fn voice_capture_clamps_a_device_that_overshoots() {
        // A float device is not obliged to stay inside -1.0..=1.0, and an
        // unclamped cast wraps rather than saturating.
        let sink = PcmSink::new(mono(TARGET_SAMPLE_RATE), MAX_UTTERANCE);
        sink.push(&[4.0f32, 4.0, -4.0, -4.0]);
        let pcm = sink.finish();
        assert!(
            pcm.samples().iter().all(|s| s.abs() == i16::MAX),
            "{:?}",
            pcm.samples()
        );
    }

    // -- the length cap ----------------------------------------------------

    #[test]
    fn voice_capture_stops_growing_at_the_cap() {
        // Twice the cap's worth of audio in, exactly the cap's worth out.
        let format = mono(TARGET_SAMPLE_RATE);
        let source = StubSource::tone(format, MAX_UTTERANCE.as_secs_f64() * 2.0);
        let capture = source.start(MAX_UTTERANCE).expect("opens");
        assert!(capture.sink.is_full(), "the cap was not noticed");
        let pcm = capture.sink.finish();
        assert_eq!(
            pcm.samples().len(),
            MAX_UTTERANCE.as_secs() as usize * TARGET_SAMPLE_RATE as usize
        );
        assert_eq!(pcm.duration(), MAX_UTTERANCE);
    }

    #[test]
    fn voice_capture_cap_is_reported_before_it_is_reached() {
        let source = StubSource::tone(mono(TARGET_SAMPLE_RATE), 1.0);
        let capture = source.start(MAX_UTTERANCE).expect("opens");
        assert!(!capture.sink.is_full());
        // One second in, one second out, give or take the frame the resampler
        // is still holding — see `PcmSink::captured`.
        let captured = capture.sink.captured().as_micros();
        assert!(
            (999_900..=1_000_000).contains(&captured),
            "{captured} us for one second of audio"
        );
        // And the flush settles it exactly.
        assert_eq!(capture.sink.finish().duration(), Duration::from_secs(1));
    }

    #[test]
    fn voice_capture_cap_releases_the_device_and_keeps_the_audio() {
        // The half a data-path cap cannot do on its own: the microphone is
        // actually closed, rather than left open feeding a sink that discards.
        let source = StubSource::tone(mono(TARGET_SAMPLE_RATE), 1.0);
        let stopped = source.stopped();
        let session = CaptureSession::new(Arc::new(source));
        let (_, ticket) = session.start().expect("starts");
        assert!(!stopped.load(Ordering::Relaxed));

        let status = session.cap_reached(ticket);
        assert!(
            stopped.load(Ordering::Relaxed),
            "the device was not released"
        );
        assert_eq!(status.state, CaptureState::Recording);
        assert!(status.capped);

        // And the utterance survived, which is the whole point of not simply
        // cancelling.
        let pcm = session.stop().expect("stops");
        assert_eq!(pcm.duration(), Duration::from_secs(1));
    }

    #[test]
    fn voice_capture_a_stale_cap_timer_closes_nothing() {
        // record, stop, start again — all inside MAX_UTTERANCE, so the first
        // recording's timer is still pending when the second one is live.
        let source = StubSource::tone(mono(TARGET_SAMPLE_RATE), 0.5);
        let stopped = source.stopped();
        let session = CaptureSession::new(Arc::new(source));
        let (_, first) = session.start().expect("starts");
        session.stop().expect("stops");
        session.settle(true);
        stopped.store(false, Ordering::Relaxed);
        session.start().expect("starts again");

        let status = session.cap_reached(first);
        assert!(
            !stopped.load(Ordering::Relaxed),
            "a stale timer closed the next recording's device"
        );
        assert!(!status.capped);
        assert_eq!(status.state, CaptureState::Recording);
    }

    #[test]
    fn voice_capture_status_reports_the_bound_it_actually_enforces() {
        // The surface renders a countdown from this, so a second constant over
        // there could drift from MAX_UTTERANCE without anything noticing.
        let session = silent_session();
        assert_eq!(session.status().max_ms, MAX_UTTERANCE.as_millis() as u32);
    }

    // -- the state machine -------------------------------------------------

    #[test]
    fn voice_capture_walks_the_whole_legal_path() {
        let session = silent_session();
        assert_eq!(session.status().state, CaptureState::Idle);

        let (status, _) = session.start().expect("idle accepts a start");
        assert_eq!(status.state, CaptureState::Recording);
        // 1600 samples at 16 kHz, less the frame the resampler still holds.
        assert_eq!(status.captured_ms, 99);

        session.stop().expect("recording accepts a stop");
        assert_eq!(session.status().state, CaptureState::Transcribing);

        assert_eq!(session.settle(true).state, CaptureState::Done);
        // And done accepts another start, so the panel is usable twice.
        session.start().expect("done accepts a start");
        assert_eq!(session.status().state, CaptureState::Recording);
    }

    #[test]
    fn voice_capture_a_failed_transcription_is_a_terminal_state_that_restarts() {
        let session = silent_session();
        session.start().expect("starts");
        session.stop().expect("stops");
        assert_eq!(session.settle(false).state, CaptureState::Failed);
        session.start().expect("failed accepts a start");
    }

    #[test]
    fn voice_capture_refuses_a_double_start_rather_than_panicking() {
        let source = StubSource::tone(mono(TARGET_SAMPLE_RATE), 0.1);
        let stopped = source.stopped();
        let session = CaptureSession::new(Arc::new(source));
        session.start().expect("starts");

        let error = session.start().expect_err("refuses");
        assert!(matches!(error, CaptureError::Refused(_)), "{error:?}");
        assert!(error.detail().contains("already running"), "{error}");
        // The first recording is untouched — a refused start must not close the
        // device the user is currently speaking into.
        assert!(!stopped.load(Ordering::Relaxed));
        assert_eq!(session.status().state, CaptureState::Recording);
    }

    #[test]
    fn voice_capture_refuses_a_stop_when_idle_rather_than_panicking() {
        let session = silent_session();
        let error = session.stop().expect_err("refuses");
        assert!(matches!(error, CaptureError::Refused(_)), "{error:?}");
        assert!(
            error.detail().contains("nothing is being recorded"),
            "{error}"
        );
        assert_eq!(session.status().state, CaptureState::Idle);
    }

    #[test]
    fn voice_capture_refuses_a_stop_while_transcribing_rather_than_panicking() {
        let session = silent_session();
        session.start().expect("starts");
        session.stop().expect("stops");

        let error = session.stop().expect_err("refuses");
        assert!(matches!(error, CaptureError::Refused(_)), "{error:?}");
        assert!(
            error.detail().contains("still being transcribed"),
            "{error}"
        );
        assert_eq!(session.status().state, CaptureState::Transcribing);
    }

    #[test]
    fn voice_capture_refuses_a_start_while_transcribing_rather_than_panicking() {
        let session = silent_session();
        session.start().expect("starts");
        session.stop().expect("stops");

        let error = session.start().expect_err("refuses");
        assert!(matches!(error, CaptureError::Refused(_)), "{error:?}");
        assert_eq!(session.status().state, CaptureState::Transcribing);
    }

    #[test]
    fn voice_capture_refuses_a_stop_after_the_last_one_settled() {
        let session = silent_session();
        session.start().expect("starts");
        session.stop().expect("stops");
        session.settle(true);

        let error = session.stop().expect_err("refuses");
        assert!(error.detail().contains("already finished"), "{error}");
    }

    #[test]
    fn voice_capture_cancel_releases_the_device_from_any_state() {
        let source = StubSource::tone(mono(TARGET_SAMPLE_RATE), 0.1);
        let stopped = source.stopped();
        let session = CaptureSession::new(Arc::new(source));

        // Idle: a no-op rather than a refusal, because a closed panel does not
        // know what the session was doing.
        assert_eq!(session.cancel().state, CaptureState::Idle);

        session.start().expect("starts");
        assert_eq!(session.cancel().state, CaptureState::Idle);
        assert!(
            stopped.load(Ordering::Relaxed),
            "the device was not released"
        );
        // And nothing is left to transcribe.
        assert!(session.stop().is_err());
    }

    /// A device whose `start` blocks until the test lets it through, so the
    /// cancel/open race can be driven deterministically rather than raced.
    ///
    /// It also runs a callback *while* the open is in flight, which is the only
    /// way to get code to run in the window the audit describes: between the
    /// reservation and the stream coming back.
    struct DeferredSource {
        inner: StubSource,
        during_open: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl DeferredSource {
        fn new(inner: StubSource, during_open: impl FnOnce() + Send + 'static) -> Self {
            Self {
                inner,
                during_open: Mutex::new(Some(Box::new(during_open))),
            }
        }
    }

    impl AudioSource for DeferredSource {
        fn start(&self, cap: Duration) -> Result<Capture, CaptureError> {
            if let Some(during_open) = self
                .during_open
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                during_open();
            }
            self.inner.start(cap)
        }
    }

    /// Scenario: the microphone is pressed, and the panel is closed (or Escape
    /// pressed) while the OS permission prompt is still up. The device then
    /// finishes opening. The returned stream must be dropped and the session
    /// must stay idle.
    ///
    /// The blocker PRD #802's security audit found. Before the reservation in
    /// `SessionInner::opening`, the returning `start` saw a state that still
    /// accepted a start — idle does — installed the stream anyway, and the
    /// microphone recorded with the panel closed until the thirty-second cap,
    /// with the buffer available for transcription when the panel reopened.
    #[test]
    fn voice_capture_a_cancel_during_the_device_open_is_observed_not_overwritten() {
        let inner = StubSource::tone(mono(TARGET_SAMPLE_RATE), 0.1);
        let stopped = inner.stopped();
        // The session has to exist before the callback can cancel it, and the
        // source has to exist before the session — so the callback reaches it
        // through a slot filled immediately afterwards.
        let slot: Arc<Mutex<Option<Arc<CaptureSession>>>> = Arc::new(Mutex::new(None));
        let cancelling = Arc::clone(&slot);
        let source = Arc::new(DeferredSource::new(inner, move || {
            let session = cancelling
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .expect("the session is installed before start is called");
            // Exactly the window: the reservation is taken, the device is
            // opening, and the user closes the panel.
            assert_eq!(session.cancel().state, CaptureState::Idle);
        }));
        let session = Arc::new(CaptureSession::new(source));
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&session));

        let error = session
            .start()
            .expect_err("a cancelled start must not install a stream");
        assert!(error.detail().contains("cannot start"), "{error}");

        // The cancellation stands, rather than having been overwritten.
        assert_eq!(session.status().state, CaptureState::Idle);
        assert_eq!(session.status().captured_ms, 0);
        // The stream that came back was dropped, which is what closes the
        // device — the whole user-visible point of the finding.
        assert!(
            stopped.load(Ordering::Relaxed),
            "the opened stream was installed rather than dropped"
        );
        // And nothing is left behind for a later panel opening to transcribe.
        assert!(session.stop().is_err());
    }

    /// Scenario: a second press arrives while the first is still opening the
    /// device. It is refused with a sentence rather than opening a second one.
    ///
    /// The sibling property of the reservation: `start` used to re-check the
    /// state on the way back, which meant two racing starts both opened a
    /// device and the loser's stream was dropped on the floor. Now the loser
    /// never opens one.
    #[test]
    fn voice_capture_refuses_a_second_start_while_the_device_is_opening() {
        let inner = StubSource::tone(mono(TARGET_SAMPLE_RATE), 0.1);
        let slot: Arc<Mutex<Option<Arc<CaptureSession>>>> = Arc::new(Mutex::new(None));
        let racing = Arc::clone(&slot);
        let second: Arc<Mutex<Option<CaptureError>>> = Arc::new(Mutex::new(None));
        let record = Arc::clone(&second);
        let source = Arc::new(DeferredSource::new(inner, move || {
            let session = racing
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .expect("the session is installed before start is called");
            let outcome = session.start().expect_err("a second start is refused");
            *record
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
        }));
        let session = Arc::new(CaptureSession::new(source));
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::clone(&session));

        let (status, _) = session.start().expect("the first start wins");
        assert_eq!(status.state, CaptureState::Recording);
        let refused = second
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .expect("the second start ran");
        assert!(
            refused.detail().contains("already being opened"),
            "{refused}"
        );
    }

    #[test]
    fn voice_capture_cancel_invalidates_the_ticket_the_start_handed_out() {
        let source = StubSource::tone(mono(TARGET_SAMPLE_RATE), 0.1);
        let session = CaptureSession::new(Arc::new(source));
        let (_, ticket) = session.start().expect("starts");
        session.cancel();
        assert_eq!(session.cap_reached(ticket).state, CaptureState::Idle);
    }

    #[test]
    fn voice_capture_settle_outside_transcribing_changes_nothing() {
        // A transcription that returns after the panel was closed must not
        // resurrect a session the user already abandoned.
        let session = silent_session();
        session.start().expect("starts");
        session.stop().expect("stops");
        session.cancel();
        assert_eq!(session.settle(true).state, CaptureState::Idle);
    }

    #[test]
    fn voice_capture_a_device_that_will_not_open_leaves_the_session_idle() {
        let session = CaptureSession::new(Arc::new(StubSource::failing(CaptureError::Device(
            "no microphone was found on this machine".into(),
        ))));
        let error = session.start().expect_err("fails");
        assert!(matches!(error, CaptureError::Device(_)), "{error:?}");
        assert_eq!(session.status().state, CaptureState::Idle);
        // And the next attempt is still allowed — plugging a microphone in is
        // the remedy, and it must not need an app restart.
        assert!(session.start().is_err());
    }

    #[test]
    fn voice_capture_status_serializes_the_tokens_the_webview_reads() {
        let session = silent_session();
        let json = serde_json::to_value(session.status()).expect("serializes");
        assert_eq!(json["state"], "idle");
        assert_eq!(json["capturedMs"], 0);
        assert_eq!(json["maxMs"], MAX_UTTERANCE.as_millis() as u64);
        assert_eq!(json["capped"], false);

        session.start().expect("starts");
        let json = serde_json::to_value(session.status()).expect("serializes");
        assert_eq!(json["state"], "recording");

        session.stop().expect("stops");
        assert_eq!(
            serde_json::to_value(session.status()).expect("serializes")["state"],
            "transcribing"
        );
        assert_eq!(
            serde_json::to_value(session.settle(false)).expect("serializes")["state"],
            "failed"
        );
    }

    #[test]
    fn voice_capture_session_is_shareable_across_threads() {
        // The property the Tauri state needs: one session, reachable from every
        // command, on whatever thread the runtime picks.
        fn assert_shared<T: Send + Sync>() {}
        assert_shared::<CaptureSession>();
        assert_shared::<Arc<dyn AudioSource>>();
    }

    // -- teardown off the lock ---------------------------------------------

    /// A stream whose `Drop` blocks, the way a real driver's does.
    ///
    /// [`CpalStream::drop`] hangs up a channel and **joins** the device
    /// thread, and that join is as slow as the platform's own teardown. The
    /// three tests below are about what else may happen while it runs.
    struct BlockingStream {
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl AudioStream for BlockingStream {}

    impl Drop for BlockingStream {
        fn drop(&mut self) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
        }
    }

    /// Hands out exactly one [`BlockingStream`].
    struct BlockingSource {
        entered: std::sync::mpsc::Sender<()>,
        release: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl AudioSource for BlockingSource {
        fn start(&self, cap: Duration) -> Result<Capture, CaptureError> {
            let release = self
                .release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .expect("this source hands out one stream");
            Ok(Capture {
                stream: Box::new(BlockingStream {
                    entered: self.entered.clone(),
                    release,
                }),
                sink: Arc::new(PcmSink::new(mono(TARGET_SAMPLE_RATE), cap)),
            })
        }
    }

    /// A session recording through a device whose teardown blocks, plus the
    /// two ends of that block: *it has started* and *let it finish*.
    fn blocking_teardown() -> (
        Arc<CaptureSession>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let session = Arc::new(CaptureSession::new(Arc::new(BlockingSource {
            entered: entered_tx,
            release: Mutex::new(Some(release_rx)),
        })));
        session.start().expect("the stub device opens");
        (session, entered_rx, release_tx)
    }

    /// Answer [`CaptureSession::status`] from another thread, giving up after
    /// two seconds rather than hanging the suite.
    ///
    /// A hung test is a worse failure than a failed one: it reports nothing
    /// until a CI timeout kills the whole job.
    fn status_within(session: &Arc<CaptureSession>, what: &str) -> CaptureStatus {
        let polling = Arc::clone(session);
        let (answered, answer) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = answered.send(polling.status());
        });
        answer
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| {
                panic!("a status poll blocked behind the device teardown in {what}")
            })
    }

    /// Scenario: stop a recording whose device is slow to let go. While the
    /// teardown runs, a concurrent status poll is still answered.
    ///
    /// The finding this is for: `stop` dropped the stream **while holding the
    /// session mutex**, and that drop joins the device thread. On the async
    /// side it is worse than a slow call — `desktop_voice_stop` is a runtime
    /// task, so the join parked a runtime worker and every concurrent
    /// `status`, `cancel` and `start` queued behind the mutex it was still
    /// holding.
    #[test]
    fn voice_capture_stop_releases_the_lock_before_the_device_teardown() {
        let (session, entered, release) = blocking_teardown();

        let stopping = Arc::clone(&session);
        let stopper = std::thread::spawn(move || stopping.stop());
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the teardown started");

        let status = status_within(&session, "stop");
        // The state moved under the lock even though the device has not let
        // go yet, which is what keeps a start from beginning here.
        assert_eq!(status.state, CaptureState::Transcribing);

        release.send(()).expect("the teardown is still waiting");
        stopper
            .join()
            .expect("the stopping thread finished")
            .expect("the stop succeeded");
    }

    /// Scenario: the same, for the cancel a closing panel makes. A status poll
    /// arriving during the teardown is answered, and answered `Idle`.
    #[test]
    fn voice_capture_cancel_releases_the_lock_before_the_device_teardown() {
        let (session, entered, release) = blocking_teardown();

        let cancelling = Arc::clone(&session);
        let canceller = std::thread::spawn(move || cancelling.cancel());
        entered
            .recv_timeout(Duration::from_secs(5))
            .expect("the teardown started");

        assert_eq!(status_within(&session, "cancel").state, CaptureState::Idle);

        release.send(()).expect("the teardown is still waiting");
        canceller.join().expect("the cancelling thread finished");
    }

    /// Scenario: the same, for the length cap. It substitutes a closed stream
    /// for the live one, and the live one's teardown must not run under the
    /// lock either.
    ///
    /// This one is the least obvious of the three because the drop is
    /// implicit: `live.stream = Box::new(ClosedStream)` drops what it
    /// replaces, at the assignment, with the guard still in scope.
    #[test]
    fn voice_capture_the_cap_releases_the_lock_before_the_device_teardown() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let session = Arc::new(CaptureSession::new(Arc::new(BlockingSource {
            entered: entered_tx,
            release: Mutex::new(Some(release_rx)),
        })));
        let (_, ticket) = session.start().expect("the stub device opens");

        let capping = Arc::clone(&session);
        let capper = std::thread::spawn(move || capping.cap_reached(ticket));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the teardown started");

        // Still recording: the cap released the device, not the utterance.
        assert_eq!(
            status_within(&session, "cap_reached").state,
            CaptureState::Recording
        );

        release_tx.send(()).expect("the teardown is still waiting");
        capper.join().expect("the capping thread finished");
    }
}
