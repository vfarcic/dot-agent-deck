//! Issue #1450: whether a finished segment holds a human voice at all.
//!
//! # Why a second gate, after the audio gate
//!
//! [`super::capture::MIN_SPEECH`] asks whether enough of the buffer rose out of
//! the room, densely enough to be a word. That is a level-and-density rule and
//! it cannot tell a voice from any other sound with the same envelope: a
//! breath, a cough, a bag put down, a knock on the desk and a notification
//! beep all clear it. What then reached the Speech backend was a quiet room
//! with one non-speech sound in it, and Whisper-family models answer that with
//! their captioned-video training artefacts. In typing mode the artefact is
//! typed into an agent's prompt; the maintainer found *"Until then, I'm
//! signing off. I hope you have a wonderful day, and God bless you."* there
//! having said nothing.
//!
//! # Why not the backend's own verdict alone
//!
//! [`super::transcribe::parse_response`] already drops segments a whisper model
//! marks as no-speech, and that is not enough on either side:
//!
//! * **It is missing where it is needed.** OpenAI's `gpt-*` transcription
//!   models answer plain `json` with no segments, and a whisper model can mark
//!   its own artefact as speech — the reported sentence reached the prompt past
//!   that guard.
//! * **It is wrong where it is present.** Measured 2026-10-04 against the
//!   default local container, `faster-whisper-tiny.en` scored real short
//!   commands — "Send it.", "Yes." — at a no-speech probability up to 0.705, over
//!   the guard's 0.6, so commands somebody said were being dropped.
//!
//! So the question is asked of the audio, before any backend is called, the
//! same way for every backend.
//!
//! # Why a model, and why this one
//!
//! Three hand-built features were measured first against the same corpus — a
//! speech-level-frames-per-word plausibility check, and two pitch (periodicity)
//! detectors — and each one refused some real speech: quiet dictation holds
//! fewer speech-level frames than a burst does, and breathy male voices show no
//! steady pitch on a vowel as short as the one in "stop". A trained
//! voice-activity model is the tool that tells a voice from a sound, and
//! `earshot` is the one that fits the rule `Cargo.toml` states for this crate's
//! dependencies (no C/C++ toolchain, no inference runtime): pure Rust, no
//! dependencies of its own, a 40 KiB network compiled into the binary, MIT OR
//! Apache-2.0. Scoring thirty seconds takes milliseconds.
//!
//! # What it was measured against
//!
//! 2026-10-04, `earshot` 1.2.2 on 156 clips: seven short commands and two
//! sentences in four public-domain or CC0 voices at one and one-and-a-half times
//! speed, loud (a peak of 3 000 over a room at 120) and quiet (400 over 64);
//! and twelve non-speech buffers — silence, a burst, a breath, a cough-like
//! pair of bursts, clicks, a keyboard, two knocks, a beep, a mains hum with a
//! thud. With the level normalised ([`VOICE_LEVEL`]), no non-speech frame
//! scored [`VOICE_SCORE`] (the highest was 0.45), and every speech clip held at
//! least fifteen frames over it, against the [`MIN_VOICE`] of five this gate
//! needs. The speech was synthesised (piper voices through the local speech
//! container); a real speaker on a real microphone is what the maintainer's
//! manual walk checks.
//!
//! # What it does not refuse
//!
//! A sound that IS voice-like: another person talking, a television, singing,
//! a laugh, a squeaking chair with a steady pitch (a synthetic one scored like
//! speech). Those reach the backend as before, where the per-segment guard
//! still applies.

use std::time::Duration;

use super::capture::{Pcm16, TARGET_SAMPLE_RATE};

/// The frame `earshot` scores: 256 samples, 16 ms at [`TARGET_SAMPLE_RATE`].
const VOICE_FRAME: usize = 256;

/// The score at and over which one frame counts as voice. `earshot`'s own
/// recommended default; the measurement in the module doc found no non-speech
/// frame over 0.45.
pub const VOICE_SCORE: f32 = 0.5;

/// How much voice a segment has to hold before any backend is asked about it:
/// five [`VOICE_FRAME`]s, not necessarily consecutive.
///
/// Set low on purpose, for [`super::capture::MIN_SPEECH`]'s reason: a command
/// refused here is silently gone, which is worse than an artefact a user can
/// see and delete. The quietest, fastest command measured held fifteen.
pub const MIN_VOICE: Duration = Duration::from_millis(80);

/// The RMS the loudest 20 ms of a segment is scaled to before it is scored.
///
/// The model scores a quiet voice low — unscaled, speech at a peak of 400
/// over a room at 64 scored no frame over 0.54 — so the segment is brought to
/// one level first. Scaling moves the voice and the room together, so the
/// signal-to-noise ratio the model judges is the one the microphone delivered.
/// 3 000 was the best of the targets measured (1 000 to 6 000): the widest gap
/// between the highest-scoring non-speech and the lowest-scoring speech.
const VOICE_LEVEL: f64 = 3_000.0;

/// The most a segment is ever amplified, so a buffer of near-digital silence
/// is not raised into something the model has to guess about. A segment that
/// quiet never gets here anyway: the audio gate refuses it first.
const MAX_GAIN: f64 = 64.0;

/// What [`measure`] found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceMeasure {
    /// How much of the segment scored as voice, in whole [`VOICE_FRAME`]s.
    pub voice: Duration,
    /// The highest score any frame reached, `0.0..=1.0` — for the refusal's
    /// detail, so a maintainer can see how close a refused segment came.
    pub peak_score: f32,
}

impl VoiceMeasure {
    /// Whether the segment holds [`MIN_VOICE`] of voice.
    pub fn heard(&self) -> bool {
        self.voice >= MIN_VOICE
    }
}

/// Score `audio` for human voice.
pub fn measure(audio: &Pcm16) -> VoiceMeasure {
    let samples = audio.samples();
    let loudest = samples
        .chunks_exact(TARGET_SAMPLE_RATE as usize / 50)
        .map(|frame| {
            let energy: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            (energy / frame.len() as f64).sqrt()
        })
        .fold(0.0, f64::max);
    let gain = (VOICE_LEVEL / loudest.max(1.0)).min(MAX_GAIN);

    let mut detector = earshot::Detector::default();
    let mut frame = [0i16; VOICE_FRAME];
    let mut voiced = 0usize;
    let mut peak_score = 0.0f32;
    for chunk in samples.chunks_exact(VOICE_FRAME) {
        for (slot, &sample) in frame.iter_mut().zip(chunk) {
            *slot = (f64::from(sample) * gain)
                .round()
                .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16;
        }
        let score = detector.predict_i16(&frame);
        peak_score = peak_score.max(score);
        if score >= VOICE_SCORE {
            voiced += 1;
        }
    }
    VoiceMeasure {
        voice: Duration::from_secs_f64(
            (voiced * VOICE_FRAME) as f64 / f64::from(TARGET_SAMPLE_RATE),
        ),
        peak_score,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_human_voice_scores_an_empty_buffer_as_no_voice() {
        let measure = measure(&Pcm16::new(Vec::new()));
        assert_eq!(measure.voice, Duration::ZERO);
        assert!(!measure.heard());
    }

    #[test]
    fn voice_human_voice_min_voice_is_whole_frames() {
        // Five 16 ms frames, so `heard` flips exactly on the fifth.
        let frames = MIN_VOICE.as_secs_f64() * f64::from(TARGET_SAMPLE_RATE) / VOICE_FRAME as f64;
        assert_eq!(frames, 5.0);
    }

    #[test]
    fn voice_human_voice_does_not_amplify_silence_without_bound() {
        // Digital silence: the gain is capped, and nothing scores as voice.
        let measure = measure(&Pcm16::new(vec![0; 48_000]));
        assert!(!measure.heard(), "{measure:?}");
    }
}
