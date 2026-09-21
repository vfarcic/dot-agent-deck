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

use std::collections::VecDeque;
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

    /// How much speech is in the buffer, how loud the loudest moment was, and
    /// what the room under it measured.
    ///
    /// Runs [`SpeechDetector`] — the **one** speech rule in this module — over
    /// the finished buffer, which is the same object [`Vad`] feeds on the data
    /// path. The two cannot disagree about what counts as somebody speaking
    /// because there is no second implementation to disagree with. A trailing
    /// partial frame is not judged, for the same reason it is not judged there.
    ///
    /// **Density over a window rather than an unbroken run, because speech
    /// contains silence and impulses contain nothing else.** This used to
    /// return the longest *unbroken* stretch, which separated a word from a
    /// train of keystrokes by run length alone — and that is the wrong axis.
    /// The two signals differ in **density**: a keyboard tap is one frame with
    /// large gaps on either side, while a word is dense frames with **small**
    /// ones. Small ones it really has: a stop consonant is a closure, so
    /// *"next"* is a 80 ms vowel, a 40 ms silence and a 60 ms /st/ cluster, and
    /// it holds 140 ms of speech-level audio without ever holding 120 ms of it
    /// *consecutively*. PRD #802's product owner met the consequence with a
    /// real microphone: words he had said came back as "Nothing was said".
    ///
    /// So a window slides and the frames inside it are counted. An impulse
    /// train is still refused — fifty taps 100 ms apart put **two** frames in
    /// any [`SPEECH_WINDOW`], which is 40 ms against the 120 ms
    /// [`Pcm16::has_speech`] needs, and the rule only stops discriminating past
    /// one impulse every 33 ms, which is not a train but a sustained noise
    /// ([`Vad`]'s own doc says a sustained noise is out of scope and bounded by
    /// [`MAX_UTTERANCE`] instead).
    pub fn measure_speech(&self) -> SpeechMeasure {
        let mut detector = SpeechDetector::default();
        for frame in self.samples.chunks_exact(VAD_FRAME) {
            detector.push_frame(mean_square(frame));
        }
        detector.measure()
    }

    /// Whether the buffer holds enough speech to be worth transcribing at all.
    ///
    /// The eligibility test in front of every transcription call
    /// ([`super::handle_audio`]), and a strictly stronger one than
    /// [`Pcm16::is_silent`]: it subsumes both an empty buffer and an all-silent
    /// one, because neither puts [`MIN_SPEECH`] of speech-level audio inside a
    /// [`SPEECH_WINDOW`].
    pub fn has_speech(&self) -> bool {
        self.measure_speech().voiced >= MIN_SPEECH
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

/// What [`Pcm16::measure_speech`] found: the gate's number, and the two numbers
/// that say whether the gate is even the thing that refused you.
///
/// **Three fields because the rule is a comparison and a comparison has two
/// sides.** `peak_rms` alone was not enough, and PRD #802's product owner
/// proved it: his refusal reported a voiced duration, he reached the density
/// branch rather than the level one, and working out that his peak had cleared
/// the old absolute floor while most of his frames had not cost a round trip to
/// the person holding the microphone. The rule now measures the loudest moment
/// **against the room under it**, so both sides of that comparison travel with
/// the verdict.
///
/// All three are rendered to the user on **both** refusals
/// ([`super::transcribe::handle_audio`]) rather than logged, because the person
/// who can answer "is my microphone quiet?" is the one holding it — and because
/// a refusal that names only one side of a ratio cannot be self-diagnosed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeechMeasure {
    /// The most speech-level audio found inside any one [`SPEECH_WINDOW`], so
    /// at most [`SPEECH_WINDOW`] itself. [`Pcm16::has_speech`]'s input.
    pub voiced: Duration,
    /// The loudest frame's RMS, out of `i16::MAX`.
    ///
    /// A peak rather than an average on purpose: the question it answers is
    /// *did anything in this buffer ever rise out of the room*, and an average
    /// over a mostly-quiet segment answers a different one.
    pub peak_rms: u16,
    /// The room estimate in force **at that loudest frame** — the quietest
    /// 20 ms in the [`NOISE_WINDOW`] ending there, never taken below
    /// [`SILENCE_RMS`], in the same units.
    ///
    /// Deliberately not the quietest frame in the whole buffer, which would be
    /// a different number whenever the room changes: this is the bar the peak
    /// was actually judged against, so `peak_rms` and `room_rms` together
    /// explain the verdict rather than merely describing the audio. A buffer
    /// that is silent and then noisy reports the noise as the room, which is
    /// the honest answer to *why did the loudest moment not count*.
    pub room_rms: u16,
}

impl SpeechMeasure {
    /// The RMS the loudest frame had to reach to count as speech:
    /// [`SPEECH_MARGIN`] times the room, where the room is already floored at
    /// [`SILENCE_RMS`] — so this never falls under 192 however silent the
    /// buffer is.
    ///
    /// The third number in every refusal sentence, and the one that makes the
    /// other two actionable — *"reached 410 against a room at 180, where speech
    /// has to reach 540"* says what to do; *"reached 410"* does not.
    pub fn required_rms(&self) -> u16 {
        (u32::from(self.room_rms) * u32::from(SPEECH_MARGIN)).min(u32::from(u16::MAX)) as u16
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

/// How many times **the room** one [`VAD_FRAME`] has to be before it counts as
/// speech rather than as the room.
///
/// An amplitude ratio of 3, which is a little over 9 dB of signal-to-noise —
/// the operating point speech detectors are usually set at, and the number this
/// module's whole threshold now is.
///
/// # Why a ratio and not a level, which is the correction PRD #802 bought
///
/// This constant replaces `SPEECH_FLOOR`, an **absolute** RMS of 600 (about
/// -35 dBFS) chosen from where near-field speech and room tone sit on a device
/// whose gain nobody had measured. It cannot be right: a microphone is a gain
/// stage, and two devices recording the same voice in the same room deliver
/// buffers an order of magnitude apart. PRD #802's product owner supplied the
/// falsification from a real microphone — *"60 ms of speech inside the loudest
/// 200 ms, where 120 ms is needed"*, which is the **density** refusal, so his
/// peak did clear 600 while only 30% of his densest window did. In the loudest
/// part of a word a correctly-set floor sees near-continuous energy; his saw a
/// third. His speech straddled the constant.
///
/// **Lowering the constant was the obvious move and it is the wrong one.** 600
/// is too high for that microphone and too low for a hotter one, where room
/// tone alone clears it, every frame reads as speech and the Whisper
/// hallucinations this gate exists to stop come straight back. Any number
/// fitted to one report is fitted to one microphone. So the rule is now
/// **relative to the utterance's own noise floor** and the level cancels: a
/// quiet input and a loud one produce the same verdict about the same voice.
/// Nothing is stored per device and there is no setup step — the reference is
/// in the buffer, because a microphone always delivers its own room.
///
/// **Erring high is still deliberate and the direction is unchanged**, but what
/// it now costs is different. Too small a ratio admits a modulated noise — a
/// fan whose own peaks sit a few dB over its own minimum — and the utterance
/// never ends in a noisy room. Too large a one refuses a speaker in a poor
/// room, and only after [`SILENCE_HOLD`], which is a pause somebody took. 3
/// sits between them by a wide margin on both sides: stationary noise varies by
/// well under 1 dB frame to frame over [`NOISE_WINDOW`], while speech modulates
/// by 20 dB and more.
pub const SPEECH_MARGIN: u16 = 3;

/// The quietest **the room** is ever taken to be — the floor that stops a
/// purely relative rule from calling true silence speech.
///
/// About -54 dBFS. A buffer of digital zero has a quietest frame, a loudest
/// frame and a ratio of one between them, so a rule that only ever compared a
/// frame against its own room would divide by nothing and pass silence. This is
/// the answer to that and nothing else.
///
/// # It floors the ROOM, not the frame, and the difference is load-bearing
///
/// [`SpeechDetector`] clamps its noise estimate here and then applies
/// [`SPEECH_MARGIN`] to the result, so the absolute minimum a frame must reach
/// works out at `SILENCE_RMS * SPEECH_MARGIN` — 192, about -45 dBFS. Flooring
/// the *frame* at 64 instead would have been a different and worse rule: a
/// buffer that holds a stretch of digital zero would then have its room
/// estimated at zero for a whole [`NOISE_WINDOW`], and ordinary room tone
/// around 180 would read as speech for that second — a driver's initial
/// zero-fill is enough to produce one, and the segment it yields is exactly
/// the near-silence Whisper answers with a training artefact.
///
/// It is set far below anything anybody means as speech and stays there: a
/// converter's own dither sits two orders of magnitude under it, and 192 is
/// still 10 dB under the quietest level this module's own docs put room tone
/// at. It is **not** a speech threshold and must not be read as one — raising
/// it towards speech level reintroduces exactly the device-specific constant
/// [`SPEECH_MARGIN`] exists to remove.
///
/// It is the RMS counterpart of [`SILENCE_FLOOR`], which is per-sample, and the
/// two answer different questions: that one asks whether a device delivered
/// anything at all, this one is a clamp inside the speech rule.
pub const SILENCE_RMS: u16 = 64;

/// How far back **the room** is measured — the span [`SpeechDetector`] takes
/// its noise estimate as the minimum over.
///
/// One second, which is five [`SPEECH_WINDOW`]s. The minimum over a window is
/// the standard noise estimator and it works because *every* real signal visits
/// its own floor: speech has closures, inter-word gaps and a lead-in before the
/// first syllable, while stationary noise simply sits there. So within a second
/// a spoken phrase always reaches back to the room it is being spoken in.
///
/// **Both directions cost something and one second is between them.** Longer
/// tracks a changing room more slowly, so a fan switched on mid-utterance is
/// read as speech for longer. Shorter risks taking a **sustained** sound — a
/// held vowel with no 20 ms dip in it — as the room and refusing the rest of
/// it. A second is longer than any stop closure (80 ms, the tolerance
/// [`SPEECH_WINDOW`] names) and than any inter-word gap inside a phrase, and
/// well short of [`MAX_UTTERANCE`].
const NOISE_WINDOW: Duration = Duration::from_secs(1);

/// How much speech-level audio one [`SPEECH_WINDOW`] has to hold before a
/// buffer is worth sending to a transcription backend — [`Pcm16::has_speech`]'s
/// threshold, and the same number [`Vad::speaking`] latches on.
///
/// 120 ms, which is six [`VAD_FRAME`]s out of the ten in a window. The two
/// things it has to tell apart sit on either side of it by a wide margin:
///
/// * an **impulse** — a keyboard tap, a click, a chair creak — is loud for a few
///   milliseconds, so RMS over a 20 ms window puts it clear of the room for
///   one frame and occasionally two. A typist at 100 ms between keystrokes gets
///   two frames into a window, which is 40 ms.
/// * a **spoken word** carries its energy in a voiced nucleus and the
///   consonants around it, and reaches 120 ms inside 200 ms even when a stop
///   closure splits it in half.
///
/// **Erring low is deliberate here, and it is the opposite direction from
/// [`SPEECH_MARGIN`]'s**, because these two constants fail differently. Too high
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

/// The stretch [`MIN_SPEECH`] has to be found inside — how much quiet may sit
/// **within one word** before the speech on either side of it stops counting as
/// one thing.
///
/// 200 ms, so the tolerance is 200 − 120 = **80 ms**, which is a stop closure.
/// That is the number this is chosen against rather than a round figure: a
/// voiceless stop is a silence with a burst after it, so *"back"* is /b/-closure,
/// vowel, /k/-closure, burst, and *"next"* is an 80 ms vowel, a 40 ms closure
/// and a 60 ms /st/ cluster. Measured against this module's own detector,
/// *"next"* holds 140 ms of speech-level audio and never 120 ms of it
/// consecutively.
///
/// # This is the rung that was missing, and it is one policy with the other two
///
/// There is a single question — *is this 20 ms frame at speech level?*, which
/// is [`SPEECH_MARGIN`] against the room and nothing else — and then a ladder
/// of how much quiet each larger thing tolerates before it is over:
///
/// | inside a… | quiet tolerated | which is |
/// | --- | --- | --- |
/// | word | 80 ms (`SPEECH_WINDOW − MIN_SPEECH`) | a stop closure |
/// | utterance | 800 ms ([`SILENCE_HOLD`]) | a pause somebody took |
/// | recording | [`MAX_UTTERANCE`] | the cap, not a judgement |
///
/// Before this constant the word rung was **0 ms** — an unbroken run — while
/// [`Vad`] armed an utterance on **one** frame. So a signal strong enough to
/// start an utterance and survive [`SILENCE_HOLD`] was then classified as
/// silence by a rule five times stricter, and the user was told *"Nothing was
/// said"* about a word they had said. The two numbers no longer differ by 6×
/// for no stated reason: they are two rungs of one ladder, on one floor, and
/// each names the pause it is tolerating.
///
/// [`Vad::heard_speech`]'s single frame is the fourth rung and deliberately not
/// on this ladder — see its own doc for why it is not a claim that anybody
/// spoke.
pub const SPEECH_WINDOW: Duration = Duration::from_millis(200);

/// [`SPEECH_WINDOW`] in whole [`VAD_FRAME`]s — the length of the sliding window
/// both [`Pcm16::measure_speech`] and [`Vad`] count frames in.
///
/// A function rather than a `const` because the division goes through
/// [`Duration`] and [`TARGET_SAMPLE_RATE`]; it is computed twice per process in
/// practice and the result is ten.
fn speech_window_frames() -> usize {
    let samples = (SPEECH_WINDOW.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)) as usize;
    (samples / VAD_FRAME).max(1)
}

/// [`NOISE_WINDOW`] in whole [`VAD_FRAME`]s — fifty, computed the same way and
/// for the same reason as [`speech_window_frames`].
fn noise_window_frames() -> usize {
    let samples = (NOISE_WINDOW.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)) as usize;
    (samples / VAD_FRAME).max(1)
}

/// [`MIN_SPEECH`] in whole [`VAD_FRAME`]s — six.
fn min_speech_frames() -> usize {
    ((MIN_SPEECH.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE)) as usize / VAD_FRAME).max(1)
}

/// One frame's mean square, which is its RMS without the square root.
///
/// Every comparison in [`SpeechDetector`] is between two of these, so the root
/// is paid once per buffer at the reporting seam rather than once per frame.
fn mean_square(frame: &[i16]) -> f64 {
    let energy: f64 = frame
        .iter()
        .map(|&sample| {
            let value = f64::from(sample);
            value * value
        })
        .sum();
    energy / frame.len().max(1) as f64
}

/// A mean square back to an RMS, clamped into the `u16` the measurement is
/// reported in.
fn rms(mean_square: f64) -> u16 {
    mean_square.sqrt().round().min(f64::from(u16::MAX)) as u16
}

/// **The** speech rule: one 20 ms frame at a time, judged against the room it
/// arrived in, and counted for density inside a [`SPEECH_WINDOW`].
///
/// # There is one of these and both callers hold it
///
/// [`Vad`] feeds it on the data path and [`Pcm16::measure_speech`] feeds it over
/// a finished buffer. They used to be two implementations of one rule held
/// together by a test; now the test holds two *callers* of one implementation,
/// which is a weaker thing to have to prove and cannot drift at all.
///
/// # Strictly causal, which is a property and not an accident
///
/// The room estimate looks only at frames that have already arrived, because
/// [`Vad`] runs on a device callback and has no others. A pass over a finished
/// buffer could do better — it could take the minimum over the whole thing —
/// and deliberately does not, because the moment it did, the live countdown and
/// the transcription gate would answer differently about the same audio, which
/// is the exact defect PRD #802 spent a round on.
///
/// The visible consequence: **a buffer that opens with speech and contains no
/// quiet before it has no room to be measured against**, and its opening frames
/// do not count. Real audio always has the lead-in — the recording starts when
/// a key is pressed and the first syllable arrives some hundreds of
/// milliseconds later, and the segment ends only after [`SILENCE_HOLD`] of
/// below-threshold audio, so a finished segment holds room tone at both ends by
/// construction. Synthetic audio does not get that for free, which is why the
/// fixtures in this module's tests carry room tone rather than being bare tone.
///
/// # A uniform buffer is refused, and that is the design rather than a gap
///
/// A signal that sits at one level for its whole length has a ratio of one
/// between its loudest and quietest moments, so no relative rule can tell a
/// held tone at speech level from a fan at speech level — the information is
/// not in the buffer. This refuses it. That is the safe direction and the
/// physical one: speech is modulated at the syllable rate and is never uniform,
/// [`Vad`]'s own doc already puts sustained noise out of scope and bounds it
/// with [`MAX_UTTERANCE`], and accepting it instead would mean accepting a
/// hummed fan, which is the class of buffer the whole gate exists to refuse.
#[derive(Debug)]
struct SpeechDetector {
    /// Frame mean squares over the last [`NOISE_WINDOW`], oldest first. The
    /// room is the smallest of them.
    room: VecDeque<f64>,
    /// The last [`SPEECH_WINDOW`] of verdicts, oldest first — a queue rather
    /// than a counter because the density rule needs to know *which* frame is
    /// leaving the window.
    window: VecDeque<bool>,
    /// How many of `window` are `true`, kept alongside it so a frame costs one
    /// increment rather than a pass over ten.
    voiced: usize,
    /// The most `voiced` has ever been, which is what both callers read. It is
    /// a running maximum, so the latch [`Vad::speaking`] needs is free.
    densest: usize,
    /// The loudest frame's mean square, and the room in force at that frame.
    peak: f64,
    peak_room: f64,
}

impl Default for SpeechDetector {
    fn default() -> Self {
        Self {
            room: VecDeque::with_capacity(noise_window_frames()),
            window: VecDeque::with_capacity(speech_window_frames()),
            voiced: 0,
            densest: 0,
            peak: 0.0,
            // The floor rather than zero, so a buffer with no frame louder
            // than digital silence still reports the room the rule would have
            // used and a `required_rms` that is three times it.
            peak_room: f64::from(SILENCE_RMS) * f64::from(SILENCE_RMS),
        }
    }
}

impl SpeechDetector {
    /// Judge one whole frame and return whether it counted as speech.
    ///
    /// The current frame is part of its own room estimate, so a frame that is
    /// the quietest in its window is never speech — which is right, and is what
    /// makes the first frame of a buffer that opens with speech fall outside.
    fn push_frame(&mut self, mean_square: f64) -> bool {
        if self.room.len() == noise_window_frames() {
            self.room.pop_front();
        }
        self.room.push_back(mean_square);
        // Linear in [`NOISE_WINDOW`] rather than a monotonic deque: fifty `f64`
        // comparisons per 20 ms of audio is not a cost worth buying a second
        // invariant to avoid, and this one is obviously correct by reading.
        let quietest = self.room.iter().copied().fold(f64::INFINITY, f64::min);
        // Clamped at [`SILENCE_RMS`], which is where the absolute floor lives:
        // a device that delivered nothing has a quietest frame of zero, and
        // three times nothing is nothing. See [`SILENCE_RMS`] for why the floor
        // is on the ROOM rather than on the frame.
        let room = quietest.max(f64::from(SILENCE_RMS) * f64::from(SILENCE_RMS));
        // Strictly greater, so a plateau reports the room as it stood when the
        // signal FIRST reached its peak — which for speech is the lead-in, and
        // is the more diagnostic of the two numbers a user could be shown.
        if mean_square > self.peak {
            self.peak = mean_square;
            self.peak_room = room;
        }
        let speech = mean_square >= required_mean_square(room);
        if self.window.len() == speech_window_frames() && self.window.pop_front() == Some(true) {
            self.voiced -= 1;
        }
        self.window.push_back(speech);
        if speech {
            self.voiced += 1;
        }
        self.densest = self.densest.max(self.voiced);
        speech
    }

    /// Whether [`MIN_SPEECH`] of speech-level audio has been seen inside one
    /// [`SPEECH_WINDOW`] — the question both callers ask, latched by
    /// `densest` being a running maximum.
    fn spoken(&self) -> bool {
        self.densest >= min_speech_frames()
    }

    fn measure(&self) -> SpeechMeasure {
        SpeechMeasure {
            voiced: Duration::from_secs_f64(
                (self.densest * VAD_FRAME) as f64 / f64::from(TARGET_SAMPLE_RATE),
            ),
            peak_rms: rms(self.peak),
            room_rms: rms(self.peak_room),
        }
    }
}

/// The bar one frame has to clear, as a mean square, given the room under it.
///
/// [`SpeechMeasure::required_rms`] is this same bar in whole RMS units, which
/// is the form a user reads. The two are the same rule and differ only by the
/// rounding of the room to an integer before it is multiplied — a unit or two
/// on a number printed in a sentence, and never the reason a verdict went one
/// way.
fn required_mean_square(room: f64) -> f64 {
    let margin = f64::from(SPEECH_MARGIN);
    room * margin * margin
}

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

/// Voice-activity detection: [`SpeechDetector`]'s verdict per frame, and a
/// quiet that has run long enough to be the end of what somebody said.
///
/// This is what [`Pcm16::is_silent`] explicitly is not, and the difference is
/// the whole reason both exist. That one looks at a finished buffer and decides
/// whether a backend call is worth making. This one runs on the data path,
/// frame by frame, and answers a question with a **time** in it — *has the
/// speaking stopped?* — which is what turns one open microphone into a sequence
/// of separate utterances.
///
/// **It is not a speech/noise classifier and does not try to be.** A steady
/// loud noise never ends an utterance here, and what bounds it is
/// [`MAX_UTTERANCE`]; PRD #802's surface discards a capped segment rather than
/// paying to transcribe it. Since the rule became relative that happens for the
/// opposite reason to the one this paragraph used to give: a stationary noise
/// is its own room, so no frame of it counts as speech, [`Vad::heard_speech`]
/// never arms and the [`SILENCE_HOLD`] clock never starts. Before, every frame
/// of it counted and the hold could never accumulate. Same outcome, and the new
/// mechanism is the one that also keeps the noise out of [`Pcm16::has_speech`].
/// A more discriminating detector is a model, with a model's size, licence and
/// failure modes, and nothing in a navigation vocabulary of one-to-four-word
/// commands needs one.
///
/// Silence before the first speech is ignored, so a microphone switched on in a
/// quiet room does not immediately "end" an utterance nobody started. That is
/// also why an open microphone nobody speaks into ends at the cap rather than
/// here.
pub struct Vad {
    /// The speech rule, fed frame by frame. The **same** object
    /// [`Pcm16::measure_speech`] runs over a finished buffer.
    detector: SpeechDetector,
    /// [`SILENCE_HOLD`] in output samples.
    hold: usize,
    /// Sum of squares of the frame being filled.
    energy: f64,
    /// How much of that frame has arrived.
    filled: usize,
    /// Whether any frame has counted as speech yet.
    heard: bool,
    /// Output samples of below-threshold audio since the last one that was not.
    quiet: usize,
    /// Latched: an utterance that has ended does not un-end.
    ended: bool,
}

impl Default for Vad {
    fn default() -> Self {
        Self::new(SILENCE_HOLD)
    }
}

impl Vad {
    /// **The speech threshold is no longer a parameter**, because there is no
    /// longer a threshold to pass: [`SpeechDetector`] derives its own from the
    /// room in the signal. The hold remains one because it is a policy about
    /// how long a pause may be, which no signal can supply.
    pub fn new(hold: Duration) -> Self {
        Self {
            detector: SpeechDetector::default(),
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
            // The one speech rule, incrementally: same detector type, same
            // frame alignment, same room estimate as the pass
            // [`Pcm16::measure_speech`] makes over the finished buffer. The two
            // answer the same question at two moments and cannot disagree,
            // because there is one implementation of it.
            let speech = self.detector.push_frame(mean_square);
            if speech {
                self.heard = true;
                self.quiet = 0;
            } else if self.heard {
                self.quiet += VAD_FRAME;
                if self.quiet >= self.hold {
                    self.ended = true;
                    return;
                }
            }
            // Quiet before the first speech falls through both arms on purpose:
            // the hold must not be counted there, or a microphone opened in a
            // quiet room would "end" an utterance nobody started. The detector
            // still advances, because a gap is exactly what it is measuring.
        }
    }

    /// Whether the utterance is over: speech was heard, and the quiet after it
    /// has run for [`SILENCE_HOLD`].
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Whether any frame has counted as speech at all.
    ///
    /// **Not a claim that anybody spoke, and it is on no rung of
    /// [`SPEECH_WINDOW`]'s ladder.** One frame out of the room is a keyboard tap
    /// as readily as a syllable. It exists for one job — starting the
    /// [`SILENCE_HOLD`] clock — and it is deliberately the most permissive test
    /// in the module because the alternative costs more: arm on a stricter rule
    /// and a segment of nothing but typing never ends, so it holds the
    /// microphone open to [`MAX_UTTERANCE`] and thirty seconds of audio are
    /// discarded instead of one second. Arming cheaply and then judging the
    /// finished segment with [`Pcm16::has_speech`] is the same answer for a
    /// twentieth of the wait.
    ///
    /// So this is an early exit, and [`Vad::speaking`] is the speech question.
    /// PRD #802's defect was reading the gap between them as a contradiction
    /// rather than as a division of labour — it was both, because the gate was
    /// also five times stricter than a word.
    pub fn heard_speech(&self) -> bool {
        self.heard
    }

    /// Whether somebody has actually SPOKEN in this recording — [`MIN_SPEECH`]
    /// of speech-level audio inside one [`SPEECH_WINDOW`] (PRD #802's dictation
    /// countdown).
    ///
    /// **Deliberately not [`Vad::heard_speech`]**, which latches on a single
    /// 20 ms frame and therefore on a keyboard tap or a chair creak. The
    /// discriminator is the one [`Pcm16::measure_speech`] documents at length —
    /// density over a window, because a train of impulses totals what a word
    /// totals and is never as dense — computed here on the data path instead of
    /// by a pass over the finished buffer, which is what makes it answerable
    /// WHILE the recording is open. It is the **same** predicate and not a
    /// second one: same floor, same frame alignment, same window span, so the
    /// countdown and the transcription gate cannot disagree about whether a
    /// word was spoken.
    ///
    /// Latched for the same reason [`Vad::ended`] is: the question a caller
    /// asks is *has anything been said since this recording started*, and a
    /// flag that fell back to false during the pause inside a sentence would
    /// answer *no* in the middle of one.
    pub fn speaking(&self) -> bool {
        self.detector.spoken()
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
    /// Whether [`Vad`] has heard somebody speak in this recording, mirrored and
    /// latched for the same two reasons.
    speech: AtomicBool,
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
            speech: AtomicBool::new(false),
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

    /// Whether [`Vad`] has heard somebody speak in this recording.
    ///
    /// An atomic rather than a look inside [`SinkState`] for
    /// [`PcmSink::utterance_ended`]'s reason: this is read by a status poll
    /// four times a second while the device thread may be holding the sink
    /// lock, and a poll that queued behind a callback would be exactly the
    /// latency the poll interval was cut to avoid.
    pub fn speech_heard(&self) -> bool {
        self.speech.load(Ordering::Relaxed)
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
        if state.vad.speaking() {
            self.speech.store(true, Ordering::Relaxed);
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
    /// Whether somebody has spoken since this recording started (PRD #802's
    /// dictation countdown).
    ///
    /// **This is what a pending send is cancelled by, and nothing else here
    /// could answer it.** [`CaptureStatus::captured_ms`] counts audio, not
    /// speech, so it grows in a silent room; [`CaptureState::Done`] arrives
    /// only after [`SILENCE_HOLD`] past the END of a sentence, which for a long
    /// one is well past the countdown it was supposed to cancel. So the surface
    /// would have submitted half an instruction to an agent while the user was
    /// still saying the rest of it — the precise failure the countdown exists
    /// to prevent.
    ///
    /// Latched per recording: it answers *has anything been said since this
    /// microphone opened*, which resets when the next one does.
    pub speech: bool,
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
        let (captured, capped, ended, speech) = match &inner.live {
            Some(live) => (
                live.sink.captured(),
                live.sink.is_full(),
                live.sink.utterance_ended(),
                live.sink.speech_heard(),
            ),
            None => (Duration::ZERO, false, false, false),
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
            speech,
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

    /// Output samples of speech, far enough over any room in this file to be
    /// unambiguous: a square wave rather than a sine, so no frame can land on a
    /// quiet part of a cycle and make a test depend on its own arithmetic.
    ///
    /// **This is half of a fixture and never a whole one.** The rule is
    /// relative, so a buffer of nothing but this has no room to be measured
    /// against and is refused — see [`SpeechDetector`], which documents that as
    /// the design. Every fixture below wraps it in [`room`], which is what a
    /// real segment carries at both ends by construction.
    fn speech(samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|i| if i % 2 == 0 { 8_000 } else { -8_000 })
            .collect()
    }

    /// The room a microphone delivers before and after anything is said — over
    /// [`SILENCE_FLOOR`], far under anything anybody means as speech, and the
    /// reference the relative rule takes its threshold from.
    ///
    /// 180 is the figure this module's own docs use for room tone. A real
    /// segment has it at both ends without anybody arranging it: recording
    /// starts when a key is pressed and the first syllable arrives later, and
    /// the segment ends only after [`SILENCE_HOLD`] of below-threshold audio.
    fn room(millis: u64) -> Vec<i16> {
        at(millis, 180)
    }

    /// Output samples at exactly the target rate, as a count.
    fn out_samples(millis: u64) -> usize {
        (TARGET_SAMPLE_RATE as usize * millis as usize) / 1_000
    }

    // -- eligibility to transcribe -----------------------------------------

    /// A square wave at `level`, so the RMS of any whole frame inside it is
    /// exactly `level` and no fixture here depends on its own arithmetic.
    fn at(millis: u64, level: i16) -> Vec<i16> {
        (0..out_samples(millis))
            .map(|i| if i % 2 == 0 { level } else { -level })
            .collect()
    }

    /// A word built out of `(milliseconds, RMS level)` segments.
    ///
    /// The shape that matters is the **closure**: a voiceless stop is a silence
    /// with a burst after it, so a one-syllable command is not one block of
    /// energy but two or three with real quiet between them. The 400 ms of
    /// [`room`] at each end is the second thing that matters, and it stopped
    /// being decoration when the rule became relative: it is what the
    /// threshold is derived from.
    fn word(parts: &[(u64, i16)]) -> Pcm16 {
        let mut samples = room(400);
        for &(millis, level) in parts {
            samples.extend(at(millis, level));
        }
        samples.extend(room(400));
        Pcm16::new(samples)
    }

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
        assert!(!audio.has_speech(), "{:?}", audio.measure_speech());
        assert!(audio.measure_speech().voiced < MIN_SPEECH);
    }

    #[test]
    fn voice_capture_an_impulse_train_never_accumulates_into_speech() {
        // Fifty taps 100 ms apart — a fast typist through a whole segment. A
        // rule that summed speech-level frames would reach a full second here;
        // the window rule reaches two frames, because what separates typing
        // from speech is DENSITY and 100 ms apart is not dense.
        let mut samples = vec![0i16; out_samples(5_000)];
        for tap in 0..50 {
            samples[out_samples(tap * 100) + 1] = i16::MAX;
        }
        let audio = Pcm16::new(samples);
        assert!(!audio.is_silent());
        assert!(
            !audio.has_speech(),
            "an impulse train read as speech: {:?}",
            audio.measure_speech()
        );
        assert_eq!(
            audio.measure_speech().voiced,
            Duration::from_millis(40),
            "two frames per 200 ms window is the whole margin this rule has"
        );
    }

    #[test]
    fn voice_capture_a_faster_impulse_train_is_still_refused() {
        // The question the window rule has to answer that the unbroken-run rule
        // answered for free: how fast can a train get before it passes? At
        // 40 ms between taps — 25 keystrokes a second, four times a fast
        // typist — five frames land in a window, which is 100 ms against 120.
        // Past roughly one impulse every 33 ms the rule stops discriminating,
        // and at that point the signal is a sustained rattle rather than a
        // train; `Vad`'s own doc puts sustained noise out of scope and bounds
        // it with `MAX_UTTERANCE` instead.
        let mut samples = vec![0i16; out_samples(5_000)];
        for tap in 0..125 {
            samples[out_samples(tap * 40) + 1] = i16::MAX;
        }
        let audio = Pcm16::new(samples);
        assert!(
            !audio.has_speech(),
            "a 25 Hz impulse train read as speech: {:?}",
            audio.measure_speech()
        );
    }

    #[test]
    fn voice_capture_an_empty_or_silent_buffer_holds_no_speech() {
        assert!(!Pcm16::new(Vec::new()).has_speech());
        assert!(!Pcm16::new(vec![0; out_samples(30_000)]).has_speech());
        // Room tone and nothing else for thirty seconds, which is the ordinary
        // case rather than the digital-zero one. It is refused because it IS
        // its own room: the quietest 20 ms in any window equals the loudest, so
        // nothing reaches `SPEECH_MARGIN` times it.
        assert!(!Pcm16::new(room(30_000)).has_speech());
    }

    #[test]
    fn voice_capture_a_spoken_word_is_eligible_to_transcribe() {
        // 200 ms of speech-level audio in a quiet second — the shape of a bare
        // "back", which is a real command on the overview and must be sent.
        // The quiet at both ends is the room the rule measures against, and a
        // digitally silent one puts the threshold on `SILENCE_RMS`.
        let mut samples = vec![0i16; out_samples(400)];
        samples.extend(speech(out_samples(200)));
        samples.extend(std::iter::repeat_n(0i16, out_samples(400)));
        let audio = Pcm16::new(samples);
        assert!(audio.has_speech(), "{:?}", audio.measure_speech());
        assert!(audio.measure_speech().voiced >= Duration::from_millis(200));
    }

    /// The defect PRD #802's product owner reported from a real microphone:
    /// *"Nothing was said"* about words he had said.
    ///
    /// Every fixture here was refused by the unbroken-run rule and is accepted
    /// by the window rule, and each is refused for the same reason — a **stop
    /// closure** is silence inside a word, so the energy arrives in two blocks
    /// neither of which reaches [`MIN_SPEECH`] on its own while the two
    /// together reach it comfortably.
    #[test]
    fn voice_capture_a_word_split_by_a_stop_closure_is_eligible() {
        for (name, audio, longest_run) in [
            (
                // 80 ms vowel, 40 ms closure, 60 ms /st/ cluster: 140 ms of
                // speech-level audio inside 180 ms, longest run 80 ms.
                "next",
                word(&[(80, 2_000), (40, 200), (60, 1_000)]),
                Duration::from_millis(80),
            ),
            (
                // A quiet speaker's vowel with a 20 ms dip through it, which is
                // ordinary amplitude modulation rather than a pathological case.
                "a vowel that dips below the floor mid-way",
                word(&[(60, 2_000), (20, 400), (60, 2_000)]),
                Duration::from_millis(60),
            ),
            (
                // Two closures, as "back" really has: /b/-closure, vowel,
                // /k/-closure, burst.
                "back, with both closures",
                word(&[(60, 180), (100, 900), (60, 180), (40, 900)]),
                Duration::from_millis(100),
            ),
        ] {
            let measure = audio.measure_speech();
            // The premise: the old rule genuinely refused this, so the fixture
            // is a regression test and not a comfortable one.
            assert!(
                longest_unbroken(&audio) < MIN_SPEECH,
                "`{name}` was not refused by the OLD rule either — it proves nothing: {:?}",
                longest_unbroken(&audio)
            );
            assert_eq!(longest_unbroken(&audio), longest_run, "`{name}`");
            assert!(
                audio.has_speech(),
                "`{name}` is a word somebody said and was refused: {measure:?}"
            );
        }
    }

    /// The rule that used to gate every utterance, kept **only** as the premise
    /// of the fixtures above: it is what a run-based gate would have measured.
    ///
    /// Not production code and deliberately not a method on [`Pcm16`] — there
    /// is one speech rule now and this is the one that was wrong.
    fn longest_unbroken(audio: &Pcm16) -> Duration {
        // `SPEECH_FLOOR`, the absolute RMS this module used to judge a frame
        // by, frozen here as a literal because it is history rather than a
        // constant: PRD #802 replaced it with `SPEECH_MARGIN` against the room.
        // Both halves of the old rule live in this one helper, which is why it
        // can still state the premise these fixtures need.
        const SPEECH_FLOOR: f64 = 600.0;
        let floor = SPEECH_FLOOR * SPEECH_FLOOR;
        let (mut longest, mut run) = (0usize, 0usize);
        for frame in audio.samples().chunks_exact(VAD_FRAME) {
            let energy: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            if energy / VAD_FRAME as f64 >= floor {
                run += VAD_FRAME;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        Duration::from_secs_f64(longest as f64 / f64::from(TARGET_SAMPLE_RATE))
    }

    #[test]
    fn voice_capture_speech_either_side_of_a_long_gap_is_not_one_word() {
        // The window tolerates a stop closure and nothing larger. 60 ms of
        // speech and 160 ms of speech, 200 ms apart: the two together are over
        // the threshold and neither alone is, so a rule with no upper bound on
        // the gap would pass this and the utterance rung (`SILENCE_HOLD`) would
        // have swallowed the word rung.
        let mut samples = room(400);
        samples.extend(speech(out_samples(60)));
        samples.extend(std::iter::repeat_n(0i16, out_samples(200)));
        samples.extend(speech(out_samples(160)));
        samples.extend(room(400));
        let audio = Pcm16::new(samples);
        assert_eq!(audio.measure_speech().voiced, Duration::from_millis(160));
        // Eligible on the strength of the 160 ms alone, which is the point:
        // the gap did not contribute.
        assert!(audio.has_speech());
    }

    #[test]
    fn voice_capture_the_gap_a_word_may_contain_is_exactly_one_closure() {
        // The tolerance pinned in both directions rather than inferred. It is
        // `SPEECH_WINDOW - MIN_SPEECH`, so 120 ms of speech survives exactly
        // 80 ms of quiet inside it and not 100 ms.
        let tolerated = SPEECH_WINDOW.as_millis() as u64 - MIN_SPEECH.as_millis() as u64;
        assert_eq!(
            tolerated, 80,
            "the constants moved; this test's prose has not"
        );
        let split = |gap: u64| {
            let mut samples = room(400);
            samples.extend(speech(out_samples(60)));
            samples.extend(at(gap, 180));
            samples.extend(speech(out_samples(60)));
            samples.extend(room(400));
            Pcm16::new(samples)
        };
        assert!(
            split(tolerated).has_speech(),
            "a closure of exactly the tolerance broke the word"
        );
        assert!(
            !split(tolerated + 20).has_speech(),
            "one frame past the tolerance still joined two halves"
        );
    }

    #[test]
    fn voice_capture_the_threshold_is_exactly_min_speech_and_one_frame_less_fails() {
        // The boundary pinned rather than inferred from a comfortable fixture.
        // `measure_speech` divides a frame count by the rate and compares the
        // resulting `Duration`, so the question is whether exactly `MIN_SPEECH`
        // of speech lands on or under the threshold — a one-frame drift here
        // moves the gate for every short command, and it is the kind of drift a
        // rounding change makes silently.
        let frames = MIN_SPEECH.as_millis() as usize / 20;
        assert_eq!(
            frames, 6,
            "MIN_SPEECH moved; this test's arithmetic has not"
        );
        assert_eq!(
            speech_window_frames(),
            10,
            "SPEECH_WINDOW moved; this test's arithmetic has not"
        );
        // In a room, because the threshold is derived from one: a bare tone
        // with nothing under it is its own room and is refused by design.
        let spoken = |frames: usize| {
            let mut samples = room(400);
            samples.extend(speech(frames * VAD_FRAME));
            samples.extend(room(400));
            Pcm16::new(samples)
        };
        let exactly = spoken(frames);
        assert_eq!(exactly.measure_speech().voiced, MIN_SPEECH);
        assert!(exactly.has_speech(), "exactly MIN_SPEECH must be eligible");
        let one_frame_short = spoken(frames - 1);
        assert!(one_frame_short.measure_speech().voiced < MIN_SPEECH);
        assert!(!one_frame_short.has_speech());
        // And the same boundary when the frames are SPREAD across a window
        // rather than consecutive, which is the axis that changed: six frames
        // of speech and four of quiet, in the order a word puts them.
        let spread = |voiced: usize| {
            let mut samples = room(400);
            for i in 0..10 {
                samples.extend(if i % 10 < voiced {
                    speech(VAD_FRAME)
                } else {
                    at(20, 180)
                });
            }
            Pcm16::new(samples)
        };
        assert!(
            spread(6).has_speech(),
            "six frames in a window is MIN_SPEECH"
        );
        assert!(!spread(5).has_speech(), "five frames in a window is not");
    }

    #[test]
    fn voice_capture_the_measure_reports_the_loudest_frame_against_its_room() {
        // The diagnostic half, and the reason `SpeechMeasure` has three fields:
        // a buffer refused because nothing ever rose out of the room needs the
        // user to speak up, and one refused because the speech was too short
        // does not. Nothing outside this could tell the two apart — and with
        // `peak_rms` alone nothing could tell them apart either, which is how
        // PRD #802's product owner ended up reporting a duration and no level.
        let quiet = word(&[(2_000, 400)]);
        let measure = quiet.measure_speech();
        assert_eq!(measure.peak_rms, 400);
        assert_eq!(measure.room_rms, 180, "the room `word` puts under it");
        assert_eq!(measure.required_rms(), 540, "3 x 180");
        assert!(
            measure.peak_rms < measure.required_rms(),
            "the fixture must be a LEVEL refusal, or it tests the wrong branch"
        );
        assert!(!quiet.has_speech());
        assert_eq!(measure.voiced, Duration::ZERO);

        let brief = word(&[(60, 5_000)]);
        let measure = brief.measure_speech();
        assert!(!brief.has_speech(), "60 ms is under MIN_SPEECH");
        assert_eq!(measure.peak_rms, 5_000);
        assert!(
            measure.peak_rms >= measure.required_rms(),
            "the fixture must be a LENGTH refusal"
        );
        assert_eq!(measure.voiced, Duration::from_millis(60));

        // Empty is not a division by zero and not a panic, and its required
        // level falls back on the absolute floor rather than on 3 x nothing.
        let empty = Pcm16::new(Vec::new()).measure_speech();
        assert_eq!(empty.peak_rms, 0);
        assert_eq!(empty.room_rms, SILENCE_RMS);
        assert_eq!(empty.required_rms(), SILENCE_RMS * SPEECH_MARGIN);
        assert_eq!(empty.voiced, Duration::ZERO);
    }

    #[test]
    fn voice_capture_the_live_and_finished_rules_are_one_predicate() {
        // `Vad::speaking` and `Pcm16::has_speech` are the same question asked at
        // two moments, and PRD #802's defect was two rules that differed by 6x.
        // They are now two CALLERS of one `SpeechDetector` rather than two
        // implementations of one rule, so this holds something weaker than it
        // used to and something that cannot drift: that both feed it the same
        // frames, in the same alignment, and read the same answer out.
        //
        // The last three fixtures are the ones the relative rule added, and the
        // gain sweep is the reason they are here: a rule that derived its
        // threshold differently on the two paths would disagree about exactly
        // these — a quiet voice, a loud room, and a bare tone with no room
        // under it at all.
        let mut quiet_voice = at(400, 60);
        quiet_voice.extend(at(80, 500));
        quiet_voice.extend(at(40, 120));
        quiet_voice.extend(at(60, 700));
        quiet_voice.extend(at(400, 60));
        let fixtures = [
            word(&[(80, 2_000), (40, 200), (60, 1_000)]),
            word(&[(60, 2_000), (20, 400), (60, 2_000)]),
            word(&[(2_000, 400)]),
            word(&[(60, 5_000)]),
            room_with_one_tap(),
            Pcm16::new(speech(out_samples(300))),
            Pcm16::new(Vec::new()),
            Pcm16::new(quiet_voice),
            Pcm16::new(at(3_000, 900)),
            Pcm16::new(room(3_000)),
        ];
        for audio in fixtures {
            let mut vad = Vad::default();
            vad.push(audio.samples());
            assert_eq!(
                vad.speaking(),
                audio.has_speech(),
                "the live and finished rules disagreed: {:?}",
                audio.measure_speech()
            );
        }
    }

    // -- the live speech flag (PRD #802 D6's countdown) ---------------------

    #[test]
    fn voice_capture_vad_speaking_needs_an_unbroken_min_speech_run() {
        // The discriminator the DICTATION COUNTDOWN rests on, and the reason
        // `speaking()` is not `heard_speech()`: a single frame over the floor
        // is a keyboard tap, and a tap must not hold a pending send open.
        let frames = MIN_SPEECH.as_millis() as usize / 20;
        let mut tapped = Vad::default();
        // The room first, in both halves: the live rule is strictly causal, so
        // a recording that opens on speech has nothing to measure it against.
        tapped.push(&room(400));
        tapped.push(&speech((frames - 1) * VAD_FRAME));
        assert!(tapped.heard_speech(), "the room was never risen out of");
        assert!(
            !tapped.speaking(),
            "one frame short of MIN_SPEECH counted as speaking"
        );

        let mut spoken = Vad::default();
        spoken.push(&room(400));
        spoken.push(&speech(frames * VAD_FRAME));
        assert!(spoken.speaking(), "exactly MIN_SPEECH is somebody speaking");
    }

    #[test]
    fn voice_capture_vad_speaking_is_not_reached_by_typing() {
        // Dense rather than merely present, exactly as `Pcm16::measure_speech`
        // is: taps inside one silence hold total what a word totals and never
        // arrive as closely, and a countdown held open by typing would never
        // send.
        //
        // **The fixture is one-frame taps, and it used to be 100 ms blocks of
        // speech-level audio with 40 ms between them** — which its own comment
        // called "ten taps" and which nothing that types produces. Under the
        // unbroken-run rule that passed for accumulation because each block was
        // one frame short of the threshold; under the window rule it is simply
        // somebody speaking with pauses, which it always was. So the fixture
        // now matches the prose rather than the prose being widened to fit it.
        let mut vad = Vad::default();
        for _ in 0..25 {
            vad.push(&speech(VAD_FRAME));
            vad.push(&vec![0; out_samples(100) - VAD_FRAME]);
        }
        assert!(
            !vad.speaking(),
            "an impulse train every 100 ms accumulated into speech"
        );
        assert!(vad.heard_speech(), "the taps did cross the floor");
    }

    #[test]
    fn voice_capture_vad_speaking_latches_through_a_pause() {
        // Latched for `ended()`'s reason. The question a caller asks is *has
        // anything been said since this recording opened*, and a flag that fell
        // back to false during the pause inside a sentence would answer `no`
        // in the middle of one — which is when a pending send would fire.
        let frames = MIN_SPEECH.as_millis() as usize / 20;
        let mut vad = Vad::default();
        vad.push(&room(400));
        vad.push(&speech(frames * VAD_FRAME));
        assert!(vad.speaking());
        vad.push(&vec![0; out_samples(300)]);
        assert!(vad.speaking(), "a pause un-said what had been said");
        assert!(!vad.ended(), "300 ms is under SILENCE_HOLD");
    }

    #[test]
    fn voice_capture_status_reports_speech_only_once_somebody_has_spoken() {
        // The whole path the surface actually reads: device callback → sink →
        // status. Nothing else on this status could answer the question —
        // `captured_ms` grows in a silent room and `Done` arrives only after
        // the hold past the END of a sentence, which is well past the countdown
        // it would have to cancel.
        let quiet = silent_session();
        quiet.start().expect("idle accepts a start");
        let status = quiet.status();
        assert!(status.captured_ms > 0, "no audio reached the sink");
        assert!(!status.speech, "a silent room reported somebody speaking");

        let format = mono(TARGET_SAMPLE_RATE);
        let mut spoken_pcm = room(400);
        spoken_pcm.extend(speech(out_samples(300)));
        let samples: Vec<f32> = spoken_pcm
            .into_iter()
            .map(|s| f32::from(s) / f32::from(i16::MAX))
            .collect();
        let spoken = CaptureSession::new(Arc::new(StubSource::new(format, samples)));
        spoken.start().expect("idle accepts a start");
        assert!(spoken.status().speech, "speech was not reported");

        // Per RECORDING, which is what makes it answer "has anybody spoken
        // since this microphone opened": a start builds a fresh sink, and a
        // session with none live answers `false` rather than the last one's
        // value.
        spoken.cancel();
        assert!(
            !spoken.status().speech,
            "the flag outlived the recording it was about"
        );
    }

    #[test]
    fn voice_capture_the_speech_threshold_agrees_between_the_vad_and_the_buffer() {
        // A hair under `SPEECH_MARGIN` times the room is not speech to either
        // of them; at it and over it is speech to both. One rule, two readers,
        // and a drift between them would make the live segmentation and the
        // eligibility gate disagree about the same audio.
        //
        // The threshold is no longer a constant to name here — it is derived
        // from the room this fixture puts under the signal, which is the whole
        // change: the same three lines at half the level, or at five times it,
        // return the same two verdicts.
        assert_eq!(
            SPEECH_MARGIN, 3,
            "SPEECH_MARGIN moved; this test's arithmetic has not"
        );
        for tone in [180i16, 90, 900] {
            let required = tone * 3;
            for (amplitude, speech_expected) in [
                (required - 1, false),
                (required, true),
                (required + 1, true),
            ] {
                let mut samples = at(400, tone);
                samples.extend(at(300, amplitude));
                samples.extend(at(400, tone));
                let mut vad = Vad::default();
                vad.push(&samples);
                assert_eq!(
                    vad.speaking(),
                    speech_expected,
                    "vad at {amplitude} in a room at {tone}"
                );
                let measure = Pcm16::new(samples.clone()).measure_speech();
                assert_eq!(
                    Pcm16::new(samples).has_speech(),
                    speech_expected,
                    "buffer at {amplitude} in a room at {tone}: {measure:?}"
                );
                assert_eq!(measure.room_rms, tone as u16);
                assert_eq!(measure.required_rms(), required as u16);
            }
        }
    }

    /// The correction PRD #802's product owner forced: the SAME voice on a
    /// quiet microphone and on a loud one has to get the same verdict, and an
    /// absolute floor cannot give it one.
    ///
    /// A gain stage multiplies the room and the speech together, so every level
    /// in the fixture scales and the ratio does not. The old `SPEECH_FLOOR` of
    /// 600 is inside the range swept here on purpose: it passes the middle of
    /// it and refuses both ends, which is the defect stated as a test.
    #[test]
    fn voice_capture_the_same_voice_reads_the_same_at_every_gain() {
        for gain in [1i32, 3, 10, 30, 100] {
            // A room 24 dB under the voice, which is an ordinary near-field
            // recording, at 1/100th of full scale and at half of it alike.
            let tone = (12 * gain) as i16;
            let voice = (192 * gain) as i16;
            let audio = {
                let mut samples = at(400, tone);
                samples.extend(at(80, voice));
                samples.extend(at(40, tone));
                samples.extend(at(60, voice));
                samples.extend(at(400, tone));
                Pcm16::new(samples)
            };
            let measure = audio.measure_speech();
            assert!(
                audio.has_speech(),
                "the same voice was refused at gain {gain}: {measure:?}"
            );
            assert_eq!(
                measure.voiced,
                Duration::from_millis(140),
                "gain {gain} changed WHAT was measured, not just the levels"
            );
        }
    }

    /// The other half of the same correction, and the one a lower absolute
    /// floor would have made worse rather than better: a loud room is still a
    /// room.
    ///
    /// Stationary noise is its own reference — the quietest 20 ms inside a
    /// `NOISE_WINDOW` is within a fraction of a dB of the loudest — so nothing
    /// in it reaches `SPEECH_MARGIN` times it at any gain. The levels swept
    /// here run from under the old `SPEECH_FLOOR` to fifteen times it, and the
    /// verdict does not move.
    #[test]
    fn voice_capture_steady_noise_is_never_speech_however_loud_it_is() {
        for level in [180i16, 400, 700, 1_500, 4_000, 9_000] {
            // Dithered rather than a flat square wave, so the frames genuinely
            // vary the way a noise source does and the minimum is not the
            // arithmetic of a constant. The wobble is +/-3%, which is an order
            // of magnitude more than white noise gives a 320-sample frame.
            let samples: Vec<i16> = (0..out_samples(5_000))
                .map(|i| {
                    let wobble = 1.0 + 0.03 * ((i / VAD_FRAME) % 7) as f64 / 7.0;
                    let value = (f64::from(level) * wobble) as i16;
                    if i % 2 == 0 { value } else { -value }
                })
                .collect();
            let audio = Pcm16::new(samples.clone());
            let measure = audio.measure_speech();
            assert!(
                !audio.has_speech(),
                "steady noise at {level} read as speech: {measure:?}"
            );
            assert_eq!(
                measure.voiced,
                Duration::ZERO,
                "not one frame of steady noise may count, at {level}"
            );
            // And the live reader agrees, so a noisy room never arms the
            // dictation countdown either.
            let mut vad = Vad::default();
            vad.push(&samples);
            assert!(
                !vad.speaking(),
                "steady noise at {level} armed the countdown"
            );
        }
    }

    /// The old rule, at the absolute floor it used to carry: the most frames
    /// over `floor` inside any one [`SPEECH_WINDOW`].
    ///
    /// Not production code and deliberately not a method on [`Pcm16`] — like
    /// [`longest_unbroken`] it exists so the two fixtures below can state
    /// their premise instead of asserting it. `floor` is a parameter rather
    /// than a constant because the argument against `SPEECH_FLOOR` is that
    /// **no** value of it works, and a test that could only speak about 600
    /// could not say that.
    fn density_over(audio: &Pcm16, floor: u16) -> Duration {
        let squared = f64::from(floor) * f64::from(floor);
        let span = speech_window_frames();
        let mut window: VecDeque<bool> = VecDeque::with_capacity(span);
        let (mut inside, mut densest) = (0usize, 0usize);
        for frame in audio.samples().chunks_exact(VAD_FRAME) {
            if window.len() == span && window.pop_front() == Some(true) {
                inside -= 1;
            }
            let over = mean_square(frame) >= squared;
            window.push_back(over);
            if over {
                inside += 1;
            }
            densest = densest.max(inside);
        }
        Duration::from_secs_f64((densest * VAD_FRAME) as f64 / f64::from(TARGET_SAMPLE_RATE))
    }

    /// PRD #802's product owner, on his own microphone, reproduced — and the
    /// measurement that falsified `SPEECH_FLOOR`.
    ///
    /// He was refused with *"60 ms of speech inside the loudest 200 ms, where
    /// 120 ms is needed"*, which is the DENSITY branch and not the level one,
    /// so his peak did clear 600 while two thirds of his densest window did
    /// not. In the loudest part of a word a correctly-set floor sees
    /// near-continuous energy; his saw a third. His speech straddled the
    /// constant, which is what a low-gain input does to any constant.
    #[test]
    fn voice_capture_a_low_gain_voice_passes_where_the_absolute_floor_refused_it() {
        // A word on a quiet input: a room at 60, a voiced nucleus at 500, a
        // closure, and one burst at 700 that clears 600 the way his peak did.
        let mut samples = at(400, 60);
        samples.extend(at(80, 500));
        samples.extend(at(40, 120));
        samples.extend(at(60, 700));
        samples.extend(at(400, 60));
        let audio = Pcm16::new(samples);

        // The premise, stated as his own number rather than as a claim: the
        // absolute floor saw 60 ms in the densest window and needed 120.
        assert_eq!(
            density_over(&audio, 600),
            Duration::from_millis(60),
            "the fixture is not the report it is named after"
        );
        let measure = audio.measure_speech();
        assert!(
            audio.has_speech(),
            "a word on a quiet microphone was refused: {measure:?}"
        );
        assert_eq!(measure.voiced, Duration::from_millis(140));
        // And the numbers the refusal would have printed are the comparison
        // that let it through: a room far under the voice.
        assert_eq!(measure.peak_rms, 700);
        assert_eq!(measure.room_rms, SILENCE_RMS, "60 is under the floor on it");
        assert_eq!(measure.required_rms(), 192);
    }

    /// The other direction, and the reason LOWERING the constant was the wrong
    /// repair: on a hotter input the room alone clears 600, and every frame of
    /// it reads as speech.
    ///
    /// This is the buffer the whole gate exists to refuse — near-silence that a
    /// Whisper-family model answers with a training artefact — and an absolute
    /// floor set low enough for the fixture above admits it. There is no value
    /// of one constant that gets both, which is the argument in a test.
    #[test]
    fn voice_capture_a_loud_room_is_refused_where_the_absolute_floor_admitted_it() {
        let samples: Vec<i16> = (0..out_samples(5_000))
            .map(|i| {
                let wobble = 1.0 + 0.03 * ((i / VAD_FRAME) % 7) as f64 / 7.0;
                let value = (900.0 * wobble) as i16;
                if i % 2 == 0 { value } else { -value }
            })
            .collect();
        let noise = Pcm16::new(samples);

        // The premise: the absolute floor passed this whole, at its own value
        // and at every smaller one.
        for floor in [600u16, 400, 200] {
            assert!(
                density_over(&noise, floor) >= MIN_SPEECH,
                "the fixture is not one the old rule admitted, at {floor}"
            );
        }
        let measure = noise.measure_speech();
        assert!(
            !noise.has_speech(),
            "a loud quiet room read as speech: {measure:?}"
        );
        assert_eq!(measure.voiced, Duration::ZERO);
    }

    #[test]
    fn voice_capture_a_trailing_partial_frame_is_not_judged() {
        // Shorter than one `VAD_FRAME`, however loud: the same rule `Vad::push`
        // applies to the tail it is still filling.
        assert!(!Pcm16::new(speech(VAD_FRAME - 1)).has_speech());
        assert_eq!(
            Pcm16::new(speech(VAD_FRAME - 1)).measure_speech().voiced,
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
        vad.push(&room(400));
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
        vad.push(&room(400));
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
        let mut spoken = room(400);
        spoken.extend(speech(out_samples(400)));
        for chunk in spoken.chunks(333) {
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
        vad.push(&room(400));
        vad.push(&speech(out_samples(300)));
        for _ in 0..(out_samples(1_000) / VAD_FRAME) {
            vad.push(&click);
        }
        assert!(vad.ended(), "a click held the utterance open");
    }

    #[test]
    fn voice_capture_vad_ending_is_latched() {
        let mut vad = Vad::default();
        vad.push(&room(400));
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
        let mut spoken = room(400);
        spoken.extend(speech(out_samples(400)));
        let mut samples: Vec<f32> = spoken
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
        let mut spoken = room(400);
        spoken.extend(speech(out_samples(400)));
        let mut samples: Vec<f32> = spoken
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
