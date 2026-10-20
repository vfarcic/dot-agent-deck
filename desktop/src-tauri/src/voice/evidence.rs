//! Issue #1450: whether the audio holds the transcript a backend returned.
//!
//! # Why the text is checked, after the audio was
//!
//! [`super::human_voice`] refuses a segment with no voice in it before any
//! backend is called, and that does not hold on its own: any sound that clears
//! it — a cough, a laugh, a hum, a chair — still comes back from a
//! Whisper-family model as a fluent sentence. The maintainer found "Then,
//! thank you very much for having me." typed into an agent's prompt on
//! 2026-10-20, having said nothing, with that check shipped. Tuning the check
//! for more kinds of noise would chase sounds one at a time, so the question
//! moves to the other side of the backend: **a transcript is accepted only
//! when the audio could hold it**, whatever sound produced it, and dropped
//! otherwise. Dropping is the safe failure: a command not heard is said again,
//! and a sentence typed into an agent's prompt is acted on.
//!
//! # The three checks, and which backends each applies to
//!
//! Each is against the [`VoiceTimeline`] [`super::human_voice`] already
//! measured for the segment, so none of them costs a second pass over the
//! audio.
//!
//! | check | needs | local (`faster-whisper` via speaches) | hosted `whisper-1` | hosted `gpt-*` |
//! | --- | --- | --- | --- | --- |
//! | the words fit in the segment's voice ([`fits`]) | the text | yes | yes | yes |
//! | each segment's words fit in the voice inside its own time | segment timestamps (`verbose_json`) | yes | yes | no |
//! | each word is timed where there was voice | word timestamps (`timestamp_granularities[]=word`) | yes | yes | no |
//!
//! The `gpt-*` models answer only plain `json`, with no timing at all
//! ([`super::transcribe::response_format_for`]), so for them only the first
//! check applies: a short invented phrase over a sound with voice in it is a
//! documented gap there.
//!
//! # Not confidence
//!
//! The model's own confidence (`avg_logprob`) already takes part in the
//! per-segment no-speech guard ([`super::transcribe::NO_SPEECH_LIMIT`]), and
//! it is not added as a check of its own here: whisper is confident about its
//! inventions. whisper-1 answered a quiet room with one burst in it at an
//! `avg_logprob` of -0.515, and the Beadaholique artefact typed into a prompt
//! at -0.42, while real short commands measured on the local model went as low
//! as -0.935 (2026-10-01 and 2026-10-04). No threshold refuses the first two
//! and keeps the third. The local container's word timestamps carry no
//! per-word probability (measured 2026-10-20), and whisper-1's carry none in
//! OpenAI's API reference.
//!
//! # What it does not refuse
//!
//! A short phrase over a sound that has about as much voice as the phrase
//! would take to say: "All right." for a 100 ms hum. The local model answered
//! one exactly so on 2026-10-20, with both words timed over the hum. A real
//! "All right." can be that short, so nothing in the reply tells the two
//! apart.

use std::time::Duration;

use super::human_voice::VoiceTimeline;

/// The most words one second of measured voice can hold.
///
/// Measured 2026-10-20 on 174 clips of real (synthesised) speech — eight
/// dictated sentences in the three voices `fixtures/speech/` uses, at one,
/// 1.3 and 1.6 times speed, and every short command fixture, each loud and
/// quiet: the most was 7.9 words per second of voice, for "I want to do it now
/// so that it is in by the end of the day." at 1.6 times speed, which is about
/// 460 words a minute. Ten leaves room over that for a real voice on a real
/// microphone, which is what the maintainer's manual walk checks.
///
/// Syllables were measured too and fit no better: an invented sentence has as
/// many syllables per word as a real one.
pub const MAX_WORDS_PER_SECOND: f64 = 10.0;

/// Words allowed on top of [`MAX_WORDS_PER_SECOND`], so a one-word command
/// whose voice measured short is never refused for its length.
pub const SPARE_WORDS: f64 = 1.0;

/// How far from the voice a backend may time a word, or a segment's edges,
/// and still count as timing it on the voice.
///
/// Whisper's timestamps are estimates. On the same 174 clips the local model
/// timed no real word further than 0.16 s from voice — a zero-length word at a
/// segment's start — and timed the invented "you." of "Thank you." for a laugh
/// 0.73 s after the laugh's voice ended.
pub const TIMING_TOLERANCE: Duration = Duration::from_millis(300);

/// A stretch of a backend's reply and when in the audio the backend says it
/// was spoken, in seconds from the segment's start. Only the word count is
/// kept, never the text: the reason [`super::Transcript`]'s `Debug` prints
/// none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timed {
    pub start: f64,
    pub end: f64,
    pub words: usize,
}

/// What a backend said about WHEN its text was spoken, as far as it said.
/// Empty for a backend that returns no timing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Timing {
    /// The segments the transcript was built from.
    pub segments: Vec<Timed>,
    /// One entry per word, where the backend timed each word.
    pub words: Vec<Timed>,
}

/// How many words `text` holds: whitespace-separated runs with a letter or a
/// digit in them, so punctuation on its own is not a word.
pub fn word_count(text: &str) -> usize {
    text.split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .count()
}

/// Whether `words` fit in `voice` ([`MAX_WORDS_PER_SECOND`], [`SPARE_WORDS`]).
pub fn fits(words: usize, voice: Duration) -> bool {
    words as f64 <= SPARE_WORDS + MAX_WORDS_PER_SECOND * voice.as_secs_f64()
}

/// Why the audio does not hold a transcript of `words` words with `timing`,
/// or `None` when it does. The reason states counts and times and never the
/// text.
pub fn unsupported(words: usize, timing: &Timing, voice: &VoiceTimeline) -> Option<String> {
    let total = voice.voice();
    if !fits(words, total) {
        return Some(format!(
            "{words} words in {} ms of voice, where at most {} fit",
            total.as_millis(),
            allowed(total)
        ));
    }
    // A time that is not a time — missing a number, or ending before it
    // starts — is no evidence of where anything was said.
    if let Some(item) =
        timing.segments.iter().chain(&timing.words).find(|item| {
            !(item.start.is_finite() && item.end.is_finite() && item.start <= item.end)
        })
    {
        return Some(format!(
            "the backend timed text {:.2}–{:.2} s, which is not a stretch of the audio",
            item.start, item.end
        ));
    }
    let tolerance = TIMING_TOLERANCE.as_secs_f64();
    for segment in &timing.segments {
        let inside = voice.voice_between(segment.start - tolerance, segment.end + tolerance);
        if !fits(segment.words, inside) {
            return Some(format!(
                "a segment timed {:.2}–{:.2} s holds {} words in {} ms of voice, where at most \
                 {} fit",
                segment.start,
                segment.end,
                segment.words,
                inside.as_millis(),
                allowed(inside)
            ));
        }
    }
    for word in &timing.words {
        if voice
            .voice_between(word.start - tolerance, word.end + tolerance)
            .is_zero()
        {
            return Some(format!(
                "a word timed {:.2}–{:.2} s, where no voice was detected",
                word.start, word.end
            ));
        }
    }
    None
}

/// The most words `voice` holds, for a reason's wording.
fn allowed(voice: Duration) -> usize {
    (SPARE_WORDS + MAX_WORDS_PER_SECOND * voice.as_secs_f64()).floor() as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One timeline entry per 16 ms frame, `true` from `from` to `to` seconds.
    fn voiced(seconds: f64, from: f64, to: f64) -> VoiceTimeline {
        let frame = 0.016;
        VoiceTimeline::from_frames(
            (0..(seconds / frame) as usize)
                .map(|index| {
                    let at = index as f64 * frame;
                    at >= from && at < to
                })
                .collect(),
        )
    }

    fn timed(start: f64, end: f64, words: usize) -> Timed {
        Timed { start, end, words }
    }

    #[test]
    fn voice_evidence_counts_words_and_not_punctuation() {
        assert_eq!(word_count("Then, thank you very much for having me."), 8);
        assert_eq!(word_count(" ... . "), 0);
        assert_eq!(word_count("Stop."), 1);
        assert_eq!(word_count("don't stop"), 2);
    }

    /// Scenario: one word said in about a third of a second is never refused
    /// for its length, and eight words in the same voice are.
    #[test]
    fn voice_evidence_a_short_command_fits_and_a_sentence_in_the_same_voice_does_not() {
        let third = Duration::from_millis(288);
        assert!(fits(1, third));
        assert!(fits(2, third));
        assert!(!fits(8, third));
        // The 80 ms the voice check lets through holds one word at most.
        assert!(fits(1, Duration::from_millis(80)));
        assert!(!fits(2, Duration::from_millis(80)));
    }

    /// Scenario: a reply with no timing is held to the whole segment's voice
    /// alone — what a `gpt-*` model's reply gets.
    #[test]
    fn voice_evidence_without_timing_only_the_total_applies() {
        let voice = voiced(3.0, 1.0, 1.3);
        assert_eq!(unsupported(2, &Timing::default(), &voice), None);
        let refused = unsupported(8, &Timing::default(), &voice).expect("refused");
        assert!(refused.starts_with("8 words in "), "{refused}");
    }

    /// Scenario: the user dictates a sentence and pauses; the backend appends
    /// a second segment, timed over the quiet after the sentence. The total
    /// fits in the sentence's voice, and the appended segment does not fit in
    /// the voice inside its own time, so the reply is refused.
    #[test]
    fn voice_evidence_a_segment_timed_over_quiet_is_refused() {
        let voice = voiced(6.0, 0.6, 2.7);
        let timing = Timing {
            segments: vec![timed(0.5, 2.7, 8), timed(3.5, 5.5, 4)],
            words: Vec::new(),
        };
        let refused = unsupported(12, &timing, &voice).expect("refused");
        assert!(
            refused.starts_with("a segment timed 3.50–5.50 s"),
            "{refused}"
        );
        // The same sentence on its own is accepted.
        let alone = Timing {
            segments: vec![timed(0.5, 2.7, 8)],
            words: Vec::new(),
        };
        assert_eq!(unsupported(8, &alone, &voice), None);
    }

    /// Scenario: a laugh, and the local model's "Thank you." for it, measured
    /// 2026-10-20 — "Thank" timed over the laugh and "you." at 2.57 s, 0.73 s
    /// after its voice ended. A word timed where there was no voice was not
    /// said.
    #[test]
    fn voice_evidence_a_word_timed_where_there_was_no_voice_is_refused() {
        let voice = voiced(3.0, 1.0, 1.84);
        let timing = Timing {
            segments: vec![timed(0.59, 2.57, 2)],
            words: vec![timed(0.59, 2.57, 1), timed(2.57, 2.57, 1)],
        };
        let refused = unsupported(2, &timing, &voice).expect("refused");
        assert!(refused.contains("2.57–2.57 s"), "{refused}");
    }

    /// Scenario: the local model's word timing for real speech starts a word
    /// early and ends one late — "Please" at 0.24 s for voice from 0.6 s, a
    /// zero-length first word 0.16 s from the voice — and all of it is kept.
    #[test]
    fn voice_evidence_word_timing_near_the_voice_is_accepted() {
        let voice = voiced(3.5, 0.6, 2.7);
        let timing = Timing {
            segments: vec![timed(0.24, 2.54, 8)],
            words: vec![
                timed(0.24, 0.94, 1),
                timed(0.44, 0.44, 1),
                timed(2.3, 2.9, 1),
                timed(2.95, 2.95, 1),
            ],
        };
        assert_eq!(unsupported(8, &timing, &voice), None);
    }

    /// Scenario: a backend that times a word outside the segment altogether,
    /// or with an end before its start, has not timed it where there was
    /// voice, and the reply is refused.
    #[test]
    fn voice_evidence_timing_outside_the_segment_is_refused() {
        let voice = voiced(3.0, 1.0, 1.5);
        for word in [
            timed(40.0, 41.0, 1),
            timed(-5.0, -4.0, 1),
            timed(1.2, 1.1, 1),
            timed(f64::NAN, 1.2, 1),
        ] {
            let timing = Timing {
                segments: Vec::new(),
                words: vec![word],
            };
            assert!(unsupported(1, &timing, &voice).is_some(), "{word:?}");
        }
    }
}
