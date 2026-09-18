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
//! nothing persists. [`Pcm16`]'s [`fmt::Debug`] is written by hand and prints
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
    /// Not a failure. PRD #802 makes `off` a product statement: the panel works
    /// from typed input through the identical downstream path, and the sentence
    /// is a settings instruction.
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
    /// Used only to skip a backend call that could not produce words — a
    /// microphone that was muted, or a device that opened and delivered
    /// nothing. It is **not** voice-activity detection and must not be read as
    /// one: it looks at the whole buffer after the fact and decides nothing
    /// about when an utterance ended.
    pub fn is_silent(&self) -> bool {
        self.samples
            .iter()
            .all(|s| s.unsigned_abs() < SILENCE_FLOOR)
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
}

#[derive(Default)]
struct SinkState {
    out: Vec<i16>,
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
/// [`PcmSink`] names no `cpal` type at all and every test below drives it with
/// plain slices. The ranges and origins are the ones `cpal::SampleFormat`
/// documents: the signed formats sit at zero and the unsigned ones at the
/// midpoint, which is why 128 in a `u8` stream is silence and not full negative
/// scale.
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

impl DeviceSample for u32 {
    #[inline]
    fn to_unit(self) -> f32 {
        ((f64::from(self) - 2_147_483_648.0) / 2_147_483_648.0) as f32
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
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Whether the length cap has been reached.
    pub fn is_full(&self) -> bool {
        self.full.load(Ordering::Relaxed)
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
        while state.out.len() < self.cap && state.next < total {
            state.out.push(held);
            state.next += ratio;
        }
        self.finished_pushing(&state);
        Pcm16::new(std::mem::take(&mut state.out))
    }

    fn finished_pushing(&self, state: &SinkState) {
        self.written
            .store(state.out.len() as u64, Ordering::Relaxed);
        if state.out.len() >= self.cap {
            self.full.store(true, Ordering::Relaxed);
        }
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

/// Convert one raw callback buffer into the sink.
///
/// `cpal::SampleFormat` is `#[non_exhaustive]` and carries formats this build
/// has no conversion for (`I24`, `U24`, `I64`, `U64`, the DSD trio). Those fall
/// through and contribute nothing, which surfaces as an empty recording and the
/// sentence the surface already renders for one — not a panic on a real-time
/// audio thread.
fn feed(sink: &PcmSink, data: &cpal::Data) {
    use cpal::SampleFormat;

    /// `as_slice` answers `None` when the format does not match the type, which
    /// the arms below have already decided — so this cannot silently drop a
    /// buffer the match claimed to handle.
    fn take<S: cpal::SizedSample + DeviceSample>(sink: &PcmSink, data: &cpal::Data) {
        if let Some(samples) = data.as_slice::<S>() {
            sink.push(samples);
        }
    }

    match data.sample_format() {
        SampleFormat::F32 => take::<f32>(sink, data),
        SampleFormat::F64 => take::<f64>(sink, data),
        SampleFormat::I8 => take::<i8>(sink, data),
        SampleFormat::I16 => take::<i16>(sink, data),
        SampleFormat::I32 => take::<i32>(sink, data),
        SampleFormat::U8 => take::<u8>(sink, data),
        SampleFormat::U16 => take::<u16>(sink, data),
        SampleFormat::U32 => take::<u32>(sink, data),
        _ => {}
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
    /// Bumped by every start, so a cap timer that fires after its own recording
    /// has already been stopped ends nothing.
    generation: u64,
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
        let (captured, capped) = match &inner.live {
            Some(live) => (live.sink.captured(), live.sink.is_full()),
            None => (Duration::ZERO, false),
        };
        CaptureStatus {
            state: inner.state,
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
    pub fn start(&self) -> Result<(CaptureStatus, CaptureTicket), CaptureError> {
        {
            let inner = self.inner();
            if !inner.state.accepts_start() {
                return Err(CaptureError::Refused(refusal(inner.state, "start")));
            }
        }
        // Opened OUTSIDE the lock: `AudioSource::start` blocks on the OS, and
        // holding the session lock across it would park the status command for
        // as long as the device takes to open.
        let capture = self.source.start(MAX_UTTERANCE)?;
        let mut inner = self.inner();
        // Re-checked, because the lock was released: two starts racing would
        // otherwise both open a device and the second would drop the first's
        // stream on the floor.
        if !inner.state.accepts_start() {
            return Err(CaptureError::Refused(refusal(inner.state, "start")));
        }
        inner.generation += 1;
        inner.state = CaptureState::Recording;
        inner.live = Some(Live {
            stream: capture.stream,
            sink: capture.sink,
        });
        let ticket = CaptureTicket(inner.generation);
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
        let live = inner.live.take().ok_or_else(|| {
            CaptureError::Device("the recording ended with no device attached".to_string())
        })?;
        // Dropped BEFORE the buffer is read: the stream's `Drop` joins the
        // device thread, so no callback can still be writing when `finish`
        // takes the samples.
        drop(live.stream);
        let pcm = live.sink.finish();
        inner.state = CaptureState::Transcribing;
        Ok(pcm)
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
    pub fn cancel(&self) -> CaptureStatus {
        let mut inner = self.inner();
        inner.live = None;
        inner.state = CaptureState::Idle;
        inner.generation += 1;
        drop(inner);
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
        let mut inner = self.inner();
        if inner.generation == ticket.0
            && inner.state == CaptureState::Recording
            && let Some(live) = inner.live.as_mut()
        {
            live.stream = Box::new(ClosedStream);
            live.sink.full.store(true, Ordering::Relaxed);
        }
        drop(inner);
        self.status()
    }
}

/// What replaces a real stream once the cap has released it.
///
/// Substituting rather than clearing keeps `live` present, so the sink is still
/// reachable and the captured audio survives to be transcribed.
struct ClosedStream;

impl AudioStream for ClosedStream {}

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

    #[test]
    fn voice_capture_converts_every_sample_type_a_device_can_deliver() {
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
        // And the negative rail saturates rather than wrapping.
        assert_eq!(one(format, &[i8::MIN, i8::MIN, i8::MIN])[0], -i16::MAX);
        assert_eq!(one(format, &[u8::MIN, u8::MIN, u8::MIN])[0], -i16::MAX);
        // And the unsigned origins are silence, not full negative scale.
        assert!(Pcm16::new(one(format, &[128u8; 8])).is_silent());
        assert!(Pcm16::new(one(format, &[32_768u16; 8])).is_silent());
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
}
