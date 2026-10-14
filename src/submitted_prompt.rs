//! What a hook-reported `user_prompt` is, and whether it is a given prompt.
//!
//! The hook truncates a submitted prompt to [`USER_PROMPT_MAX_LEN`] bytes and a
//! producer may wrap a bracketed paste in an envelope before reporting it, so a
//! prompt and its report are compared through [`prompt_submission_matches`],
//! never `==`. The hook's own normalization ([`paste_envelope_payload`],
//! [`strip_paste_envelope_close`]) and the test harness's check that a typed
//! prompt was submitted both read this module.

/// The byte length `crate::hook` truncates a reported `user_prompt` to before it
/// ever reaches [`crate::event::AgentEvent`].
///
/// A prompt longer than this is reported back in truncated form, so comparing a
/// locally-known prompt to a reported one with plain `==` matches for short
/// prompts and never for long ones. Always compare through
/// [`prompt_submission_matches`], never directly.
pub const USER_PROMPT_MAX_LEN: usize = 200;

/// How many accumulated copies of one prompt the repetition shapes below build
/// candidates for before giving up.
///
/// This bounds the LOOP, not the accepted submissions. It is an exact bound only
/// while the reported text is complete: past [`USER_PROMPT_MAX_LEN`] every
/// repetition count collapses onto the same 200-byte prefix, so a report of 17,
/// 50 or 500 copies is indistinguishable from the 16 this builds — see
/// [`prompt_submission_matches`]'s residual section, which withdraws the claim
/// that accepted submissions are at most this many copies. The retry schedule
/// cannot produce more than single digits inside the typed delivery's 60 s deadline (removed by PRD #1666),
/// so the number is margin, not a tuning knob.
pub(crate) const MAX_REPEATED_SUBMISSION_COPIES: u32 = 16;

/// Truncate `s` to at most `max` BYTES, appending `…` when anything was cut.
///
/// Slices on a char boundary at or below `max`: the naive `&s[..max]` panics
/// whenever the cut lands inside a multi-byte character, which in the hook
/// binary means a prompt containing any non-ASCII text past the limit kills the
/// hook process and emits no event at all.
pub fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Normalize a prompt for comparison using EXACTLY the PTY encoder's contract.
///
/// [`crate::pane_input::encode_pane_payload`] changes two things before the
/// bytes reach the PTY: it writes every line break as LF (issue #1616), and it
/// strips trailing `\n`, `\r`, space and tab. Those — and only those — are
/// the difference between what we hand the encoder and what the agent submits
/// and reports back. This is the second; the callers apply the first,
/// [`crate::pane_input::normalize_line_breaks`], before it.
///
/// Reviewer finding B10: this used to be `str::trim`, which is both too wide and
/// wrong on the wrong end. Too wide because `trim` also removes trailing Unicode
/// whitespace the encoder PRESERVES (a NBSP, an ideographic space), so a
/// different submitted prompt differing only in such a character confirmed this
/// delivery; wrong on the wrong end because `trim` also strips LEADING
/// whitespace, which the encoder preserves and which can be meaningful in a
/// prompt (an indented code block, a leading blank line).
pub(crate) fn normalize_for_match(s: &str) -> &str {
    s.trim_end_matches(['\n', '\r', ' ', '\t'])
}

/// Whether a hook-reported `user_prompt` confirms submission of `expected` —
/// the prompt text we wrote into the pane.
///
/// Four shapes are accepted, each for a specific mechanical reason:
///
/// 1. **The text verbatim.** The ordinary case.
/// 2. **Its [`USER_PROMPT_MAX_LEN`]-truncated form.** Which one arrives depends
///    only on the prompt's length; see that constant.
/// 3. **N copies of it separated by newlines** (reviewer finding B5). When the
///    submit CR is swallowed by a still-booting agent TUI in the PRD #128 mode
///    where it becomes a *newline in the input box* rather than a no-op, the
///    payload stays in the box and the next retry's submission carries
///    `seed \n seed`. That is proof the seed was submitted — twice — so it must
///    finalize the delivery. Reading it as a mismatch instead left the loop
///    ARMED after the agent had already started acting, and a dispatch prompt
///    performs deployments, deletions and issue operations: running one a third
///    and fourth time is strictly worse than a visibly idle pane a human can
///    recover. This also removes an inconsistency that made the hazard
///    length-dependent — a seed longer than [`USER_PROMPT_MAX_LEN`] already
///    matched its doubled submission (both truncate to the same prefix) while a
///    short one did not, so short and long prompts behaved oppositely.
/// 4. **Any of the three above, inside the producer's PASTE ENVELOPE**
///    (issue #1182). A payload the deck writes as bracketed paste — which is
///    every MULTI-LINE payload, see [`crate::pane_input::encode_pane_payload`]
///    — does not come back as the text we wrote: Claude Code reports it wrapped
///    in `<pasted_content id="…">`. So the first three shapes structurally
///    cannot match a multi-line delivery to such a producer, and every one of
///    them was retried to the typed delivery's 60 s deadline (removed by PRD #1666) and abandoned on work
///    the agent had received and acted on. The envelope is a DELIMITED region,
///    which is what keeps this from meaning "the report contains our prompt
///    somewhere": the payload is compared as a whole, only blank lines may
///    precede the delimiter, and the comparison budget shrinks by what the
///    envelope spent. See [`paste_envelope_payload`].
///
/// Both sides are normalized with [`crate::pane_input::normalize_line_breaks`]
/// and then [`normalize_for_match`], never `str::trim`.
///
/// # Accepted residual: the copy bound does not survive truncation (#526)
///
/// [`MAX_REPEATED_SUBMISSION_COPIES`] bounds shape 3 EXACTLY only while the
/// reported text is complete. Once the hook truncates at
/// [`USER_PROMPT_MAX_LEN`], every repetition count whose rendering exceeds that
/// limit collapses onto the SAME 200-byte prefix, so a submission of 17, 50 or
/// 500 copies is indistinguishable from the 16 this accepts — and so is an
/// arbitrary tail following a repeated expected prefix, since only the prefix
/// survives. The earlier claim that "accepted submissions are at most 16 copies
/// of the prompt" is therefore not enforceable and is withdrawn: what is
/// enforced is that the reported text BEGINS with a repetition of the expected
/// prompt. The bound is real for short prompts and is margin, not a security
/// property, for long ones.
///
/// This is the same collision class as the deferred full-prompt digest and is
/// folded into **#526**; a digest or delivery token is what would make the
/// comparison exact regardless of truncation. Not fixed here: the alternative
/// available at this seam — refusing every truncated report — would reject the
/// ordinary confirmation of any prompt over 200 bytes, i.e. reinstate issue
/// #424 for every long dispatch prompt.
///
/// # The shapes are not equally clean (#685)
///
/// Two of them mean the agent submitted a turn carrying our prompt more than
/// once, and this `bool` cannot say which one answered. That is a reporting gap
/// rather than a delivery one, so [`classify_prompt_submission`] names the
/// shape for the log. See [`ConfirmedSubmission`].
///
/// # Shape 4 is the one addition since (#1182)
///
/// The accept set was "deliberate and pinned" for three shapes and stayed that
/// way through #685, which relabelled without widening. #1182 widened it once,
/// by the fourth shape above, because a producer that REWRITES our payload into
/// an envelope before reporting it falsifies the premise the other three rest
/// on — that a submitted prompt comes back as the text we wrote. That is a
/// widening and is named as one; what it does not do is loosen the other three,
/// which keep their exact accept set and evaluation order (see
/// [`classify_reported_text`]).
pub fn prompt_submission_matches(expected: &str, reported: &str) -> bool {
    classify_prompt_submission(expected, reported).is_some()
}

/// Issue #685: WHICH of [`prompt_submission_matches`]'s shapes confirmed a
/// delivery — the fact that tells a clean single-copy turn apart from a
/// duplicated one in the delivery log.
///
/// Every variant is a confirmation, every variant is terminal for the delivery,
/// and **no delivery decision reads this**: it exists so the log can say what
/// happened, and nothing else. The shapes were never equally clean — two of
/// them mean the agent is acting on a turn that carries our prompt MORE THAN
/// ONCE — but the three that existed then all emitted the same `info` line, so
/// a duplicated turn was indistinguishable from an ordinary success in three of
/// the four repetition shapes issue #685 measured.
///
/// # The knowable / unknowable split
///
/// [`Self::RepeatedCopies`] and [`Self::TruncatedPrefix`] are DIFFERENT facts
/// and must not be reported as one. The first says the repetition is visible in
/// the text that arrived. The second says the report is a
/// [`USER_PROMPT_MAX_LEN`] prefix that one copy and five hundred copies render
/// identically, so the question cannot be answered from it in either direction —
/// and a line claiming a duplicate that may not have happened is worse than the
/// silence this issue is about. That is why the second is a weaker line at a
/// quieter level rather than the same warning; see the typed delivery's confirmation log (removed by PRD #1666).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmedSubmission {
    /// The report is the prompt verbatim: complete text, one copy. The only
    /// shape that positively shows a clean single turn.
    SingleCopy,
    /// Shape 3 — the report is the prompt repeated with newline separators, and
    /// the repetition is VISIBLE in the text that arrived.
    ///
    /// `copies` is `Some` only when the report carried the WHOLE repeated text,
    /// in which case it is the exact number submitted. It is `None` when the
    /// match came from the truncated form, and that is deliberately not a
    /// number: every count whose rendering exceeds [`USER_PROMPT_MAX_LEN`]
    /// produces the same prefix (the residual documented on
    /// [`prompt_submission_matches`]), and the loop counter at the point of that
    /// match is **not** a floor on the real count either — it is the first
    /// candidate that outgrew the reported length, which for a prompt whose
    /// copies straddle the limit can exceed what was actually submitted. A
    /// wrong number in a warning is worse than no number, so there is none.
    ///
    /// What is certain in both cases is the repetition itself, which is what
    /// makes this the case an operator can act on.
    RepeatedCopies { copies: Option<u32> },
    /// The report is the [`USER_PROMPT_MAX_LEN`]-truncated form of ONE copy —
    /// which is also, byte for byte, the truncated form of every repetition of a
    /// prompt that long. Reachable only for a prompt longer than that limit,
    /// since a shorter one is reported complete and lands on
    /// [`Self::SingleCopy`].
    ///
    /// So the delivery is confirmed and whether the turn carries the prompt more
    /// than once is UNKNOWABLE from this evidence. This is the cell that makes
    /// the issue matter: a `dispatch` or issue-dispatch prompt is routinely well
    /// over 200 bytes, so it lands here whether it was submitted once or three
    /// times. **#526's full-prompt digest is what removes the cell**; this
    /// variant only stops it from being reported as a clean single copy.
    TruncatedPrefix,
    /// Issue #1182 — the report is our payload inside the producer's PASTE
    /// ENVELOPE: `<pasted_content id="…">` … `</pasted_content id="…">`, which
    /// is how Claude Code renders a bracketed paste in the turn it reports.
    /// Reachable only for a MULTI-LINE payload, since those are the only ones
    /// [`crate::pane_input::encode_pane_payload`] wraps in bracketed paste.
    ///
    /// Everything [`Self::TruncatedPrefix`] says about undecidability applies
    /// here too and for the same reason — the enveloped payload is compared as
    /// a prefix, shortened by whatever the envelope spent — with one fact on
    /// top that the plain truncated line cannot carry: the producer rewrote our
    /// payload into an envelope, which is WHY nothing matched verbatim. An
    /// operator reading a log full of abandoned deliveries needs that fact, so
    /// it is a variant rather than a widening of the one above.
    PasteEnvelope,
}

impl ConfirmedSubmission {
    /// The stable slug the delivery log carries in its `confirmation` field, so
    /// the shapes are greppable without matching on message prose.
    pub fn log_slug(self) -> &'static str {
        match self {
            Self::SingleCopy => "single-copy",
            Self::RepeatedCopies { .. } => "repeated-copies",
            Self::TruncatedPrefix => "truncated-report",
            Self::PasteEnvelope => "paste-envelope",
        }
    }

    /// How notable this confirmation is when one delivery has more than one of
    /// them to report — which `crate::ui`'s seed and orchestrator paths can,
    /// because they scan a whole journal rather than a stream.
    ///
    /// Reporting the least notable would hide a duplicate behind a clean line,
    /// so the most notable wins. This orders LOG LINES and nothing else: all
    /// variants are equally `Confirmed`, and the delivery outcome does not
    /// depend on which one is picked.
    fn notability(self) -> u8 {
        match self {
            Self::SingleCopy => 0,
            Self::TruncatedPrefix => 1,
            // Above the plain truncated line because it carries strictly more:
            // the same undecidability PLUS the reason the verbatim comparisons
            // could not answer. Below a visible duplicate, which is the only
            // one of the four an operator has to act on.
            Self::PasteEnvelope => 2,
            Self::RepeatedCopies { .. } => 3,
        }
    }

    /// The more notable of two confirmations of the same delivery, `self` on a
    /// tie. See `notability` for the order and why it exists.
    pub fn more_notable(self, other: Self) -> Self {
        if other.notability() > self.notability() {
            other
        } else {
            self
        }
    }
}

/// The opening delimiter a producer wraps a bracketed paste in before reporting
/// the turn back through `UserPromptSubmit`.
///
/// Measured on `scheduler/dispatch/015` (three real interactive Haiku panes,
/// 2026-09-19): a payload the deck writes as bracketed paste
/// ([`crate::pane_input::encode_pane_payload`] wraps every MULTI-LINE payload in
/// `ESC[200~`/`ESC[201~`) does not come back verbatim. Claude Code reports it as
///
/// ```text
/// <pasted_content id="57b9">
/// …the payload…
/// </pasted_content id="57b9">
/// ```
///
/// so the plain comparisons above can never match it, and the delivery retries
/// until the typed delivery's 60 s deadline (removed by PRD #1666) abandons it — the exact silent-failure
/// shape [`USER_PROMPT_MAX_LEN`]'s doc warns about, arrived at from a different
/// direction. That is not hypothetical scope: PRD #220 Phase 2 (#1081) appends
/// the `work-done` completion instruction to EVERY `--single` dispatch prompt,
/// which made every one of them multi-line, so from that commit on no dispatch
/// to a Claude Code pane could be confirmed. What the operator saw was a pane
/// flagged with `DeliveryNotice`'s "it may never have arrived — check whether
/// this pane was given any task at all" on work that had in fact been delivered
/// and completed, and one extra payload write (`MAX_PAYLOAD_SUBMISSIONS`) into
/// an agent already acting on the task.
///
/// # Why this shape survives `crate::hook`'s normalization
///
/// `crate::hook::record_submitted_prompt` takes the envelope off at the
/// boundary where a producer's report enters the deck, so a report arriving
/// from THIS build is already unwrapped and lands on shape 1 or 2 above. That
/// does not make this reachable-in-tests-only: the `dot-agent-deck` a pane
/// invokes comes from the agent's own hook configuration and can be OLDER than
/// the daemon it reports to — [`crate::hook_provenance::Refusal::Missing`]'s
/// caller message names exactly that population — so enveloped reports keep
/// arriving from binaries that predate the normalization, and confirming them
/// is this shape's job.
///
/// # Accepted residual: two pastes in one turn
///
/// A turn carrying our payload TWICE — the accumulation
/// the typed delivery's accumulation detector (removed by PRD #1666) was the safety net for, produced when the
/// bounded replacement write lands before the agent has submitted the first
/// payload — arrives as two envelopes, and this shape does not read it as a
/// confirmation. It is not widened to N of them: no report carrying two was
/// measured, and a repetition grammar over a delimiter the PRODUCER chooses is
/// the same widening both reviewers rejected for the separator-free shape.
///
/// The residual is narrower than it sounds, and only because of the truncation
/// this module otherwise fights. A payload longer than [`USER_PROMPT_MAX_LEN`]
/// — which every dispatch prompt is — is cut inside the FIRST copy, so after
/// `crate::hook`'s normalization the report is exactly shape 2 and confirms.
/// What is left uncovered is a SHORT multi-line payload submitted twice in one
/// turn.
const PASTE_ENVELOPE_OPEN_HEAD: &str = "<pasted_content id=\"";

/// The rest of the opening delimiter, after the opaque id.
const PASTE_ENVELOPE_OPEN_TAIL: &str = "\">";

/// The closing delimiter's stable head. The id is repeated after it, so only the
/// part that cannot vary is matched.
const PASTE_ENVELOPE_CLOSE_HEAD: &str = "</pasted_content";

/// How long an envelope id may be before the report is refused as not being one.
/// Measured ids are four characters (`57b9`, `a833`, `7632`); this is margin,
/// and it is a BOUND rather than a guess — see [`paste_envelope_payload`] for
/// why every byte the envelope spends has to be accounted for.
const MAX_PASTE_ENVELOPE_ID_LEN: usize = 16;

/// How many bytes of blank line may precede the envelope in a report.
///
/// They come from the delivery's OWN submit probes: a CR that arrives fused to
/// an agent TUI that is not yet in submit-aware mode lands as a newline in the
/// input box (PRD #128), so a turn submitted later carries one per probe ahead
/// of the payload. `dispatch/015` measured exactly two. The retry schedule
/// cannot produce more than single digits inside the typed delivery's 60 s deadline (removed by PRD #1666)
/// — the same fact [`MAX_REPEATED_SUBMISSION_COPIES`] rests on — so this is
/// margin over that, not a tuning knob.
///
/// Bounded rather than "any leading whitespace" for one reason that matters:
/// every byte in front of the payload is a byte the hook's
/// [`USER_PROMPT_MAX_LEN`] truncation took OFF the end of it, so an unbounded
/// prefix would shrink the compared payload without limit and the comparison
/// would eventually be against nothing at all.
const MAX_PASTE_ENVELOPE_LEADING_BLANK_BYTES: usize = 16;

/// The payload a producer's paste envelope wraps, and the byte offset it starts
/// at within `reported`.
///
/// That offset is the second half of the answer and not bookkeeping: the hook
/// truncates the WHOLE reported turn at [`USER_PROMPT_MAX_LEN`], so everything
/// the envelope and its leading blank lines spend is taken off the end of our
/// payload. A caller comparing the wrapped payload has to truncate its own copy
/// to `USER_PROMPT_MAX_LEN - offset`, not to `USER_PROMPT_MAX_LEN`.
///
/// `None` for a report that is not enveloped, which is every report from a
/// single-line payload and from every producer that reports verbatim.
///
/// Returned as a slice of `reported` rather than an owned `String` so a caller
/// can key offsets off it; the leading `\n` a producer puts after the opening
/// delimiter is consumed, and the closing delimiter is left for
/// [`strip_paste_envelope_close`] because a truncated report does not reach it.
pub fn paste_envelope_payload(reported: &str) -> Option<(&str, usize)> {
    let head_at = reported.find(PASTE_ENVELOPE_OPEN_HEAD)?;
    let leading = &reported[..head_at];
    if leading.len() > MAX_PASTE_ENVELOPE_LEADING_BLANK_BYTES
        || !leading.bytes().all(|b| b == b'\n' || b == b'\r')
    {
        return None;
    }
    let after_head = &reported[head_at + PASTE_ENVELOPE_OPEN_HEAD.len()..];
    let id_len = after_head.find(PASTE_ENVELOPE_OPEN_TAIL)?;
    if id_len == 0 || id_len > MAX_PASTE_ENVELOPE_ID_LEN {
        return None;
    }
    if !after_head[..id_len]
        .bytes()
        .all(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let body = &after_head[id_len + PASTE_ENVELOPE_OPEN_TAIL.len()..];
    let body = body.strip_prefix('\n').unwrap_or(body);
    Some((body, reported.len() - body.len()))
}

/// Drop the envelope's closing delimiter from a payload that still carries one.
///
/// Only a delimiter that ENDS the report is dropped, and only one that occupies
/// a single line of its own: a payload whose own text mentions
/// `</pasted_content` must not be able to cut the comparison short at that
/// point. A report truncated at [`USER_PROMPT_MAX_LEN`] — which is every
/// dispatch prompt, since they run well over it — never reaches the closing
/// delimiter at all, so this is the short-payload path.
pub fn strip_paste_envelope_close(payload: &str) -> &str {
    let Some(at) = payload.rfind(PASTE_ENVELOPE_CLOSE_HEAD) else {
        return payload;
    };
    let close = &payload[at..];
    if !close.ends_with('>') || close.contains('\n') {
        return payload;
    }
    payload[..at].trim_end_matches('\n')
}

/// [`prompt_submission_matches`]'s body, reporting WHICH shape matched instead
/// of a bare `bool`. See that function for the shapes, the normalization
/// contract and the accepted truncation residual.
///
/// Issue #685 was a mechanical relabelling of the `true`s and not a new
/// decision: it left the set of `(expected, reported)` pairs that confirm a
/// delivery exactly as it was, in the same evaluation order, with
/// `prompt_submission_matches` becoming `.is_some()` over this. It was
/// explicitly not a widening or a narrowing — the two tests that pin the accept
/// set on both sides (`repeated_copies_of_the_seed_count_as_delivered` and
/// `bare_concatenation_is_accumulation_and_not_confirmation`) still call the
/// `bool` API unmodified and are what proves that.
///
/// Issue #1182 then DID widen it, by the paste envelope below and by nothing
/// else. The three shapes #685 relabelled are untouched — they live in
/// [`classify_reported_text`], which this asks first and unchanged — so those
/// same two tests still pin them.
///
/// The one non-mechanical detail: the single verbatim check and the single
/// TRUNCATED check used to be one `||`, and are split apart here because they
/// answer the knowable/unknowable question differently. Same disjunction, same
/// order, same short-circuit.
pub fn classify_prompt_submission(expected: &str, reported: &str) -> Option<ConfirmedSubmission> {
    // Issue #1616: line breaks first, on both sides — see
    // [`crate::pane_input::normalize_line_breaks`].
    let expected = crate::pane_input::normalize_line_breaks(expected);
    let reported = crate::pane_input::normalize_line_breaks(reported);
    let expected = normalize_for_match(&expected);
    let reported = normalize_for_match(&reported);
    if let Some(confirmed) = classify_reported_text(expected, reported, USER_PROMPT_MAX_LEN) {
        return Some(confirmed);
    }
    // Shape 4 (issue #1182), asked only after the three above have said no —
    // the same discipline `prompt_submission_accumulated` is written under, and
    // for the same reason: it can only ADD confirmations where the delivery was
    // previously retried to the deadline and abandoned, and it reclassifies
    // nothing that already matched.
    //
    // The same three shapes are then asked again INSIDE the envelope, rather
    // than a second grammar being written for it, so "what counts as our
    // payload" has one definition. The budget shrinks by what the envelope and
    // the blank lines in front of it spent, because the hook truncated the
    // whole turn and those bytes came off the end of our payload; see
    // [`paste_envelope_payload`].
    let (payload, spent) = paste_envelope_payload(reported)?;
    classify_reported_text(
        expected,
        normalize_for_match(strip_paste_envelope_close(payload)),
        USER_PROMPT_MAX_LEN.saturating_sub(spent),
    )
    .map(|_| ConfirmedSubmission::PasteEnvelope)
}

/// [`classify_prompt_submission`]'s three verbatim shapes, over text that is
/// already normalized, against a truncation `budget`.
///
/// Split out so the same grammar can be asked of an enveloped payload without
/// being written twice — and `budget` is a parameter for exactly that: inside an
/// envelope the hook's [`USER_PROMPT_MAX_LEN`] was spent partly on the envelope
/// itself. Every caller outside the envelope path passes `USER_PROMPT_MAX_LEN`,
/// so this is the same function it always was.
fn classify_reported_text(
    expected: &str,
    reported: &str,
    budget: usize,
) -> Option<ConfirmedSubmission> {
    if expected.is_empty() {
        // Nothing was written, so nothing can be evidence about it. Guards the
        // repetition loop below against an infinite family of empty candidates.
        return None;
    }
    if reported == expected {
        return Some(ConfirmedSubmission::SingleCopy);
    }
    if reported == truncate_on_char_boundary(expected, budget) {
        // Only reachable past the limit: at or under it `truncate_on_char_boundary`
        // returns the text unchanged, which the check above already settled.
        return Some(ConfirmedSubmission::TruncatedPrefix);
    }
    // Shape 3. Built incrementally and bounded by the REPORTED length so a
    // multi-kilobyte dispatch prompt never allocates more than the hook could
    // possibly have reported.
    let mut candidate = String::from(expected);
    for copies in 2..=MAX_REPEATED_SUBMISSION_COPIES {
        candidate.push('\n');
        candidate.push_str(expected);
        if candidate.len() > reported.len() {
            // Every longer candidate is longer still, so only a TRUNCATED form
            // can match from here — and truncation is a fixed 200-byte prefix
            // of the same repeating text, identical for every larger copy
            // count. One check settles all of them — and settling them all is
            // exactly why the copy count comes back `None` here rather than as
            // `copies`, which is the first candidate that outgrew the report and
            // not a bound on what was submitted.
            return (reported == truncate_on_char_boundary(&candidate, budget))
                .then_some(ConfirmedSubmission::RepeatedCopies { copies: None });
        }
        if reported == candidate {
            return Some(ConfirmedSubmission::RepeatedCopies {
                copies: Some(copies),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_prompt_matches_verbatim() {
        assert!(prompt_submission_matches("hello there", "hello there"));
        assert!(!prompt_submission_matches("hello there", "hello world"));
    }

    #[test]
    fn trailing_whitespace_is_not_a_mismatch() {
        // `encode_pane_payload` strips trailing whitespace before the bytes
        // reach the PTY, so the agent reports the trimmed form.
        assert!(prompt_submission_matches("do the thing\n", "do the thing"));
    }

    /// Reviewer finding B10: normalization must be the ENCODER's contract, not
    /// `str::trim`. Leading whitespace is preserved by the encoder and so is
    /// meaningful; trailing Unicode whitespace is preserved too, so it cannot
    /// be normalized away into a different prompt's report.
    #[test]
    fn normalization_matches_the_encoder_contract() {
        assert!(!prompt_submission_matches("  indented", "indented"));
        assert!(prompt_submission_matches("  indented", "  indented"));
        // U+00A0 NO-BREAK SPACE survives `encode_pane_payload`, so two prompts
        // differing only by one are DIFFERENT prompts.
        assert!(!prompt_submission_matches("do it\u{a0}", "do it"));
        // ...while the four bytes the encoder does strip are still equivalent.
        assert!(prompt_submission_matches("do it \t\r\n", "do it"));
    }

    /// Reviewer finding B5: the CR-swallow shape. The payload stayed in the
    /// input box, the retry submitted, and the hook reports `seed \n seed`.
    /// That is the seed submitted — twice — so it must finalize the delivery
    /// rather than leave the retry armed against an agent already working.
    #[test]
    fn repeated_copies_of_the_seed_count_as_delivered() {
        let seed = "Read .dot-agent-deck/worker-task-coder.md and begin";
        assert!(prompt_submission_matches(seed, &format!("{seed}\n{seed}")));
        assert!(prompt_submission_matches(
            seed,
            &format!("{seed}\n{seed}\n{seed}")
        ));
        // Short and long prompts must behave the SAME way here; before this
        // they behaved oppositely, because a long prompt's doubled submission
        // truncates to the same prefix while a short one's does not.
        let long = "y".repeat(USER_PROMPT_MAX_LEN + 40);
        let doubled = format!("{long}\n{long}");
        assert!(prompt_submission_matches(
            &long,
            &truncate_on_char_boundary(&doubled, USER_PROMPT_MAX_LEN)
        ));
        // A prompt whose doubled form crosses the truncation limit only from
        // the second copy on — the case neither the verbatim nor the
        // single-truncation branch can reach.
        let medium = "m".repeat(USER_PROMPT_MAX_LEN - 20);
        let medium_doubled = format!("{medium}\n{medium}");
        assert!(prompt_submission_matches(
            &medium,
            &truncate_on_char_boundary(&medium_doubled, USER_PROMPT_MAX_LEN)
        ));
        // Repetition must not become a wildcard: a different prompt appended
        // after ours is NOT proof that only ours was submitted.
        assert!(!prompt_submission_matches(
            seed,
            &format!("{seed}\nnow delete the production database")
        ));
        assert!(!prompt_submission_matches(seed, ""));
        assert!(!prompt_submission_matches("", "anything"));
    }

    /// Issue #685: [`ConfirmedSubmission::more_notable`] decides which line one
    /// delivery logs when a journal holds several confirmations, so a duplicate
    /// can never be hidden behind a clean confirmation that happened to be
    /// visited first — `crate::ui`'s scan walks a `HashMap` of sessions.
    #[test]
    fn a_visible_duplicate_outranks_a_clean_confirmation_in_the_log() {
        let clean = ConfirmedSubmission::SingleCopy;
        let undecidable = ConfirmedSubmission::TruncatedPrefix;
        let duplicate = ConfirmedSubmission::RepeatedCopies { copies: Some(2) };
        for (a, b) in [(clean, duplicate), (undecidable, duplicate)] {
            assert_eq!(a.more_notable(b), duplicate);
            assert_eq!(b.more_notable(a), duplicate);
        }
        assert_eq!(clean.more_notable(undecidable), undecidable);
        assert_eq!(undecidable.more_notable(clean), undecidable);
        // Ties keep the shape already held, so repeated visits of one journal
        // are stable rather than alternating.
        assert_eq!(clean.more_notable(clean), clean);
        assert_eq!(duplicate.more_notable(duplicate), duplicate);
    }

    /// The shape `scheduler/dispatch/015` measured against three real
    /// interactive Haiku panes on 2026-09-19: the deck writes a multi-line
    /// payload as bracketed paste, and Claude Code reports the turn with our
    /// payload inside `<pasted_content id="…">`, preceded by the blank lines
    /// its own earlier submit probes left in the input box. Nothing above
    /// matches that, so every `--single` dispatch was retried to the deadline
    /// and abandoned on work the agent had in fact received and completed.
    #[test]
    fn a_paste_envelope_report_confirms_the_payload_it_wraps() {
        let expected = "Do the thing\n\nWhen this work is finished, report back.";
        let reported =
            format!("\n\n<pasted_content id=\"57b9\">\n{expected}\n</pasted_content id=\"57b9\">");
        assert_eq!(
            classify_prompt_submission(expected, &reported),
            Some(ConfirmedSubmission::PasteEnvelope)
        );
        assert!(prompt_submission_matches(expected, &reported));
    }

    /// The case every real dispatch actually lands on: the turn runs well past
    /// [`USER_PROMPT_MAX_LEN`], so the hook truncates it and the closing
    /// delimiter is never reported. What survives is a prefix of our payload
    /// that is SHORTER than the plain truncated form by exactly what the
    /// envelope and the blank lines in front of it spent — which is why
    /// [`paste_envelope_payload`] returns that offset and this compares against
    /// the reduced budget rather than against `USER_PROMPT_MAX_LEN`.
    #[test]
    fn a_truncated_paste_envelope_report_confirms_by_the_reduced_prefix() {
        let expected = "z".repeat(USER_PROMPT_MAX_LEN * 2);
        let turn =
            format!("\n\n<pasted_content id=\"a833\">\n{expected}\n</pasted_content id=\"a833\">");
        let reported = truncate_on_char_boundary(&turn, USER_PROMPT_MAX_LEN);
        assert!(
            reported.len() < turn.len(),
            "the fixture must actually be truncated"
        );
        assert_eq!(
            classify_prompt_submission(&expected, &reported),
            Some(ConfirmedSubmission::PasteEnvelope)
        );
        // The plain truncated form is a DIFFERENT string — 23 bytes longer of
        // our payload — so the shape above is not reachable through the
        // `TruncatedPrefix` check and the reduced budget is load-bearing.
        assert_ne!(
            reported,
            truncate_on_char_boundary(&expected, USER_PROMPT_MAX_LEN)
        );
    }

    /// Issue #1616: the deck writes every line break as LF
    /// ([`crate::pane_input::encode_pane_payload`]), so a prompt that arrived
    /// with CRLF or a bare CR comes back from the agent with LF — measured on
    /// Claude Code 2.1.294, which reports a CRLF paste as LF. Compared raw, the
    /// two never matched: the delivery was retried, its payload typed into the
    /// pane of an agent already working on it, and the delivery abandoned at
    /// the deadline (`scheduler/dispatch/028`).
    #[test]
    fn a_prompt_with_crlf_or_cr_line_breaks_is_confirmed_by_its_lf_report() {
        let reported = "Line one.\n\nLine two.";
        for expected in [
            "Line one.\r\n\r\nLine two.\r\n",
            "Line one.\r\rLine two.\r",
            "Line one.\u{2028}\u{2029}Line two.",
        ] {
            assert_eq!(
                classify_prompt_submission(expected, reported),
                Some(ConfirmedSubmission::SingleCopy),
                "{expected:?}"
            );
            let envelope = format!(
                "\n\n<pasted_content id=\"41b1\">\n{reported}\n</pasted_content id=\"41b1\">"
            );
            assert_eq!(
                classify_prompt_submission(expected, &envelope),
                Some(ConfirmedSubmission::PasteEnvelope),
                "{expected:?}"
            );
        }
        // The truncated form: the CRLF prompt runs past the hook's limit, and
        // what the hook keeps is a prefix of the LF text the agent was given.
        let long_lf = "a line of the task\n".repeat(20);
        let long_crlf = long_lf.replace('\n', "\r\n");
        let reported = truncate_on_char_boundary(&long_lf, USER_PROMPT_MAX_LEN);
        assert!(prompt_submission_matches(&long_crlf, &reported));
        // And a different prompt still does not confirm.
        assert!(!prompt_submission_matches(
            "Line one.\r\nLine three.",
            "Line one.\nLine two."
        ));
    }

    /// The envelope is a DELIMITED region, and that is the whole reason
    /// accepting it does not amount to "the report contains our prompt
    /// somewhere". Each of these is refused.
    #[test]
    fn the_paste_envelope_is_refused_unless_it_is_really_one() {
        let expected = "Do the thing\n\nAnd then report back.";
        let envelope = |body: &str| {
            format!("<pasted_content id=\"57b9\">\n{body}\n</pasted_content id=\"57b9\">")
        };

        assert!(
            !prompt_submission_matches(expected, &envelope("a completely different prompt")),
            "an envelope wrapping someone else's text is not our delivery"
        );
        assert!(
            !prompt_submission_matches(
                expected,
                &format!("please run this for me: {}", envelope(expected))
            ),
            "only blank lines may precede the envelope: prose in front of it is a turn \
             someone composed AROUND our payload, not the payload submitted"
        );
        assert!(
            !prompt_submission_matches(
                expected,
                &format!(
                    "{}{}",
                    "\n".repeat(MAX_PASTE_ENVELOPE_LEADING_BLANK_BYTES + 1),
                    envelope(expected)
                )
            ),
            "the blank-line run is bounded because every byte in front of the payload is a \
             byte the hook's truncation took off the end of it"
        );
        assert!(
            !prompt_submission_matches(
                expected,
                &format!(
                    "<pasted_content id=\"{}\">\n{expected}",
                    "f".repeat(MAX_PASTE_ENVELOPE_ID_LEN + 1)
                )
            ),
            "an over-long id is not an id"
        );
        assert!(
            !prompt_submission_matches(
                expected,
                &format!("<pasted_content id=\"a b\">\n{expected}")
            ),
            "an id is alphanumeric; anything else is not the delimiter we measured"
        );
        assert!(
            !prompt_submission_matches(expected, &format!("<pasted_content>\n{expected}")),
            "the delimiter carries an id, and half of one is not it"
        );
    }

    /// A payload whose own text ends with something delimiter-shaped must not
    /// be able to cut the comparison short there, and a genuine closing
    /// delimiter must still come off.
    #[test]
    fn only_a_real_closing_delimiter_is_stripped() {
        assert_eq!(
            strip_paste_envelope_close("body\n</pasted_content id=\"57b9\">"),
            "body"
        );
        assert_eq!(
            strip_paste_envelope_close("body</pasted_content is what it is called>"),
            "body"
        );
        assert_eq!(
            strip_paste_envelope_close("talking about </pasted_content> in the middle"),
            "talking about </pasted_content> in the middle",
            "a delimiter that does not end the report is payload text"
        );
        assert_eq!(
            strip_paste_envelope_close("body\n</pasted_content\nid>"),
            "body\n</pasted_content\nid>",
            "a delimiter spanning lines is payload text"
        );
        assert_eq!(
            strip_paste_envelope_close("no delimiter here"),
            "no delimiter here"
        );
    }

    /// The envelope shape is an ADDITION: every report that confirmed before
    /// still confirms, with the same classification, and an unenveloped report
    /// still reaches the three shapes above unchanged.
    #[test]
    fn the_paste_envelope_shape_adds_and_never_reclassifies() {
        let short = "seed alpha";
        assert_eq!(
            classify_prompt_submission(short, short),
            Some(ConfirmedSubmission::SingleCopy)
        );
        let long = "q".repeat(USER_PROMPT_MAX_LEN + 10);
        assert_eq!(
            classify_prompt_submission(
                &long,
                &truncate_on_char_boundary(&long, USER_PROMPT_MAX_LEN)
            ),
            Some(ConfirmedSubmission::TruncatedPrefix)
        );
        assert_eq!(
            classify_prompt_submission(short, &format!("{short}\n{short}")),
            Some(ConfirmedSubmission::RepeatedCopies { copies: Some(2) })
        );
        assert_eq!(paste_envelope_payload(short), None);
    }

    /// The trap from the tester's finding #2: a prompt longer than the hook's
    /// truncation limit is reported back truncated, so exact equality never
    /// matches and the delivery would retry until the deadline abandoned it.
    #[test]
    fn long_prompt_matches_its_truncated_report() {
        let long = "x".repeat(USER_PROMPT_MAX_LEN + 40);
        let reported = truncate_on_char_boundary(&long, USER_PROMPT_MAX_LEN);
        assert_ne!(reported, long, "the fixture must actually be truncated");
        assert!(prompt_submission_matches(&long, &reported));
        assert!(!prompt_submission_matches(&long, &"y".repeat(50)));
    }

    /// Truncation must never split a multi-byte character — the naive
    /// `&s[..max]` panics there, which in the hook binary means no event at all.
    #[test]
    fn truncation_never_splits_a_multibyte_char() {
        // 'é' is 2 bytes, so byte 200 lands mid-character.
        let s = format!("{}é{}", "a".repeat(199), "b".repeat(60));
        let cut = truncate_on_char_boundary(&s, USER_PROMPT_MAX_LEN);
        assert_eq!(cut, format!("{}…", "a".repeat(199)));
        assert!(prompt_submission_matches(&s, &cut));
    }

    /// Issue #685: the same 2x2 fixture set the issue's table was measured with
    /// — both repetition shapes, at 2 and 3 copies, for a prompt under
    /// [`USER_PROMPT_MAX_LEN`] and one over it, each put through the hook's
    /// truncation — asserted against the CLASSIFICATION rather than the bare
    /// `bool`.
    ///
    /// What the table said before this: the `warn` fired in one of the four
    /// cells, and in NONE of them for a long prompt. What it says now is in the
    /// `expected` column below — the three cells that used to be a silent clean
    /// confirmation are now either a distinct knowable duplicate (the short
    /// newline-separated shape) or report themselves undecidable (every
    /// long-prompt shape, whichever separator).
    ///
    /// The accept set is untouched, and the row-by-row `prompt_submission_matches`
    /// cross-check at the bottom of each iteration is what says so: every shape
    /// that classifies is exactly a shape the `bool` API confirms.
    #[test]
    fn confirmation_classification_covers_every_repetition_shape() {
        // `crate::hook` truncates every reported `user_prompt` through exactly
        // this function before it can reach an `AgentEvent`, so applying it here
        // is the hook's own truncation and not a stand-in for it.
        let through_the_hook = |text: &str| truncate_on_char_boundary(text, USER_PROMPT_MAX_LEN);

        let short = "Use Bash to verify seed-confirm-alpha-7f31.txt exists then print it";
        assert!(short.len() < USER_PROMPT_MAX_LEN);
        let long = format!(
            "Read .dot-agent-deck/worker-task-coder.md and begin. {}",
            "y".repeat(USER_PROMPT_MAX_LEN)
        );
        assert!(long.len() > USER_PROMPT_MAX_LEN);

        let cases: Vec<(&str, &str, Option<ConfirmedSubmission>)> = vec![
            // The baselines: one copy of each length.
            ("short/single", short, Some(ConfirmedSubmission::SingleCopy)),
            (
                "long/single",
                &long,
                Some(ConfirmedSubmission::TruncatedPrefix),
            ),
            // Newline-separated — shape 3, the matcher's own repetition
            // grammar. Short: the repetition is visible in the report, so it is
            // a KNOWABLE duplicate. Two copies fit under the limit and are
            // counted exactly; three do not, so the count is withheld while the
            // repetition itself stays certain — see `RepeatedCopies` for why a
            // withheld count is the honest answer rather than a floor.
            (
                "short/newline/2",
                short,
                Some(ConfirmedSubmission::RepeatedCopies { copies: Some(2) }),
            ),
            (
                "short/newline/3",
                short,
                Some(ConfirmedSubmission::RepeatedCopies { copies: None }),
            ),
            // Long: every repetition collapses onto the same prefix a single
            // copy produces, so the report cannot answer the question either
            // way. This is the cell that makes the issue matter — a dispatch
            // prompt lives here.
            (
                "long/newline/2",
                &long,
                Some(ConfirmedSubmission::TruncatedPrefix),
            ),
            (
                "long/newline/3",
                &long,
                Some(ConfirmedSubmission::TruncatedPrefix),
            ),
            // Separator-free — NOT the matcher's business. Short: no
            // confirmation at all, which is what routes it to
            // `prompt_submission_accumulated` and its own `warn`. Long: the
            // doubled text truncates onto the ordinary long-prompt prefix, so
            // the matcher confirms it and the accumulation detector is never
            // consulted; undecidable is the honest report.
            ("short/bare/2", short, None),
            ("short/bare/3", short, None),
            (
                "long/bare/2",
                &long,
                Some(ConfirmedSubmission::TruncatedPrefix),
            ),
            (
                "long/bare/3",
                &long,
                Some(ConfirmedSubmission::TruncatedPrefix),
            ),
        ];

        let reported_for = |name: &str, expected: &str| -> String {
            let text = match name.rsplit_once('/') {
                Some((shape, "2")) if shape.ends_with("newline") => {
                    format!("{expected}\n{expected}")
                }
                Some((shape, "3")) if shape.ends_with("newline") => {
                    format!("{expected}\n{expected}\n{expected}")
                }
                Some((shape, "2")) if shape.ends_with("bare") => format!("{expected}{expected}"),
                Some((shape, "3")) if shape.ends_with("bare") => {
                    format!("{expected}{expected}{expected}")
                }
                _ => expected.to_string(),
            };
            through_the_hook(&text)
        };

        let mut observed = Vec::new();
        for (name, expected, want) in &cases {
            let reported = reported_for(name, expected);
            let got = classify_prompt_submission(expected, &reported);
            observed.push((*name, got));
            assert_eq!(
                got, *want,
                "{name}: classification must match the shape the issue's table measured"
            );
            // The classification never widens or narrows what confirms a
            // delivery: it is `prompt_submission_matches` with the `true`s
            // labelled, and this is the assertion that keeps it that way.
            assert_eq!(
                got.is_some(),
                prompt_submission_matches(expected, &reported),
                "{name}: classification and the bool matcher must accept exactly the same set"
            );
        }

        // Eight of the ten cells confirm, and exactly ONE of those eight is a
        // clean single copy. That ratio is the complaint restated as an
        // assertion: before this, all eight emitted the same `info` line as
        // that one clean cell.
        assert_eq!(observed.iter().filter(|(_, got)| got.is_some()).count(), 8);
        assert_eq!(
            observed
                .iter()
                .filter(|(_, got)| *got == Some(ConfirmedSubmission::SingleCopy))
                .count(),
            1,
            "observed={observed:?}"
        );
    }
}
