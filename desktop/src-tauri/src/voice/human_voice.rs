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

/// The RMS the loudest 20 ms of each sound in a segment is scaled to before
/// it is scored.
///
/// The model scores a quiet voice low — unscaled, speech at a peak of 400
/// over a room at 64 scored no frame over 0.54 — so each sound is brought to
/// one level first. Scaling moves the voice and the room together, so the
/// signal-to-noise ratio the model judges is the one the microphone delivered.
/// 3 000 was the best of the targets measured (1 000 to 6 000): the widest gap
/// between the highest-scoring non-speech and the lowest-scoring speech.
///
/// **Per sound, not per segment** ([`SOUND_GAP`]): a knock on the desk half a
/// second before a quiet "send it" set the level for the whole segment when it
/// was taken from the segment's loudest moment, turned the command down by the
/// knock's level, and the command was refused (Greptile on PR #1550,
/// `voice_transcribe_a_quiet_command_after_a_loud_knock_is_still_heard`).
const VOICE_LEVEL: f64 = 3_000.0;

/// How much quiet separates two sounds, each brought to [`VOICE_LEVEL`] on its
/// own: 300 ms of audio under the speech bar. Longer than the pause between
/// the words of a command, so a command is one sound.
const SOUND_GAP: usize = 15;

/// How far each side of a sound its level applies, in 20 ms frames — 200 ms,
/// so the model's view of the sound's onset and decay is at the sound's level
/// rather than at the segment's.
const SOUND_MARGIN: usize = 10;

/// The 20 ms frame the level rule works in, in samples.
const LEVEL_FRAME: usize = TARGET_SAMPLE_RATE as usize / 50;

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

/// The gain each 20 ms frame of `audio` is scored at.
///
/// Each run of speech-level frames ([`Pcm16::speech_frames`]), with gaps
/// shorter than [`SOUND_GAP`] closed, is one sound; its frames and
/// [`SOUND_MARGIN`] either side are scaled so its loudest frame reaches
/// [`VOICE_LEVEL`]. Where two sounds' margins overlap the smaller gain wins.
/// Frames belonging to no sound — the room — keep the gain the segment's
/// loudest frame gives, which is how the whole segment was scaled before.
fn frame_gains(audio: &Pcm16) -> Vec<f64> {
    let rms: Vec<f64> = audio
        .samples()
        .chunks_exact(LEVEL_FRAME)
        .map(|frame| {
            let energy: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            (energy / frame.len() as f64).sqrt()
        })
        .collect();
    let gain_for = |peak: f64| (VOICE_LEVEL / peak.max(1.0)).min(MAX_GAIN);
    let segment_gain = gain_for(rms.iter().copied().fold(0.0, f64::max));

    let speech = audio.speech_frames();
    let mut sounds: Vec<(usize, usize)> = Vec::new();
    for index in (0..speech.len()).filter(|&index| speech[index]) {
        match sounds.last_mut() {
            Some((_, end)) if index - *end <= SOUND_GAP => *end = index,
            _ => sounds.push((index, index)),
        }
    }

    let mut gains: Vec<Option<f64>> = vec![None; rms.len()];
    for (start, end) in sounds {
        let gain = gain_for(rms[start..=end].iter().copied().fold(0.0, f64::max));
        let from = start.saturating_sub(SOUND_MARGIN);
        let to = (end + SOUND_MARGIN).min(rms.len().saturating_sub(1));
        for slot in &mut gains[from..=to] {
            *slot = Some(slot.map_or(gain, |other| other.min(gain)));
        }
    }
    gains
        .into_iter()
        .map(|gain| gain.unwrap_or(segment_gain))
        .collect()
}

/// Score `audio` for human voice.
pub fn measure(audio: &Pcm16) -> VoiceMeasure {
    let samples = audio.samples();
    let gains = frame_gains(audio);
    // The last partial 20 ms frame has no verdict; it takes its neighbour's.
    let gain_at = |sample: usize| {
        gains
            .get(sample / LEVEL_FRAME)
            .or(gains.last())
            .copied()
            .unwrap_or(1.0)
    };

    let mut detector = earshot::Detector::default();
    let mut frame = [0i16; VOICE_FRAME];
    let mut voiced = 0usize;
    let mut peak_score = 0.0f32;
    for (index, chunk) in samples.chunks_exact(VOICE_FRAME).enumerate() {
        let start = index * VOICE_FRAME;
        for (offset, (slot, &sample)) in frame.iter_mut().zip(chunk).enumerate() {
            *slot = (f64::from(sample) * gain_at(start + offset))
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
