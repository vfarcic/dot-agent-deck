/**
 * PRD #802 M6 — the voice surface: one button, and what it reports.
 *
 * The pipeline is capture → segment → transcribe → resolve → validate →
 * execute → report, and then round again without another press. Rust owns the
 * middle ({@link ../lib/bridge!DeckBridge.resolveVoice} and the three
 * microphone verbs beside it); this file owns the two ends — the button an
 * utterance starts at, and the sentence it finishes as.
 *
 * # Voice ONLY, which is a withdrawal rather than an omission
 *
 * An earlier build of this file opened a dialog with a text box and a *Run
 * command* button, and offered the microphone only once transcription was
 * configured. The typed path was not asked for; it was a build-time decision,
 * and the product owner withdrew it: *"Only voice. When I click the Voice
 * button, it should activate voice control and it should keep being controlled
 * by voice until the button is clicked again."*
 *
 * So there is no text box, no dialog, and no separate *start listening*
 * control. **The button is the control.** Press it and voice control is on
 * until it is pressed again; the utterances in between are segmented by
 * voice-activity detection in `voice::capture`, not by a second press.
 *
 * What the typed path used to buy was coverage for everybody with no
 * transcription credential, and that argument does not survive its own premise:
 * a path nobody speaks through proves the resolver and never the surface. The
 * browser fixture replaces it properly — it simulates a microphone the same way
 * it simulates a daemon, so the whole loop is driven in Playwright with no
 * credential anywhere near it.
 *
 * # It renders sentences; it never composes one
 *
 * Every situation an utterance can end in arrives carrying its own `sentence`,
 * rendered Rust-side from the command table. So this file reads `outcome.kind`
 * for exactly one decision — whether to dispatch — and prints `outcome.sentence`
 * for everything else. It never builds wording from `action`, `param`, `matches`
 * or `detail`, because a surface that could phrase a situation is a surface that
 * can phrase it differently from the one beside it, and the table would stop
 * being the single answer to what the app says.
 *
 * The sentences written here are {@link NOTHING_DISPATCHED},
 * {@link SCREEN_MOVED_ON}, {@link DIALOG_MOVED_ON}, {@link VOICE_UNAVAILABLE},
 * {@link VOICE_CAP_DISCARDED}, {@link VOICE_RELEASE_REFUSED} and
 * {@link VOICE_EMPTY_STATE}. The first three are for situations Rust
 * structurally cannot know about; the rest are about the surface's own state
 * — a release it cannot vouch for, a microphone it has nothing to open, a row
 * with nothing in it yet — rather than about an utterance. See their own
 * notes. The numbered choice (PRD #1261) adds its own `VOICE_CHOICE_*`
 * sentences, for the same reason as the first three: whether a choice was
 * closed, cancelled, expired or outlived by the screen is the panel's state,
 * which Rust never sees.
 *
 * The overlay `list_commands` opens writes no sentence of its own: it prints
 * the table's own `description` column, which is the point of generating it
 * from the table rather than maintaining a list beside one.
 *
 * # Voice gets no execution path of its own
 *
 * Nothing here navigates. A dispatch is handed to the host's `onDispatch`, which
 * runs it through `dispatchVoiceAction` — the same registry entry the rail button
 * and the palette item run through. The host owns it rather than this file
 * because running the command and knowing how to undo it are the same question,
 * and only the host knows which screen the user was on.
 *
 * # Where this is mounted, and why it matters
 *
 * Above the deck/overview screen switch, in {@link ../App!DeckShell} — which is
 * architectural rather than cosmetic. The report describes a navigation that has
 * just happened and the Undo beside it reverses one, so both have to outlive the
 * screen change they are about. Mounted inside either screen, a successful
 * `open_overview` would unmount the surface that was explaining it.
 *
 * # It renders one RESERVED ROW, not two floating boxes
 *
 * Everything this file renders is inside a single `.voice-row` pinned to the
 * bottom of the window: the button on the left, and what the microphone heard
 * plus the outcome on its right. The stylesheet sets that row's height aside on
 * every surface that would otherwise reach the bottom edge — the agent pane
 * overlay above all — so the row overlaps nothing and nothing overlaps it. The
 * comment on the returned element has the reasoning and the one it replaced.
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { Mic, MicOff, SquarePen, Undo2, X } from "lucide-react";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_PEER_PROPS } from "../hooks/useInertBackground";
import { VOICE_ACTIONS, type VoiceDispatchTarget, type VoicePanelChannel, type VoicePanelContext } from "../lib/voiceActions";
import { TerminalInputCancelled, type EndpointSettingsDto, type VoiceCommandDto, type VoiceDirectoriesDto, type VoiceNewAgentDto, type VoiceOutcomeDto, type VoiceReadingStateDto, type VoiceResolvedParamDto, type VoiceResultDto, type VoiceScreen, type VoiceStatusDto } from "../lib/bridge";
import { answerChoiceLocally, collidingChoiceEntry, VOICE_CHOICE_MAX, type VoiceChoiceAnswerDto } from "../lib/voiceChoice";
import { answerNumberLocally, hasNumbered, numberedEntry, numberedOutcome, numberedParam, SECTION_NOUNS, type VoiceNumberAnswerDto, type VoiceNumberedEntryDto, type VoiceNumberedListDto, type VoiceNumberedSectionKind, type VoiceNumberRefDto } from "../lib/voiceNumbers";
import { offPageNamed, offPageSentence, type VoicePager } from "../lib/voicePages";
import { READING_ALREADY_OFF, type ReadingTarget } from "../lib/reading";
import { useReadingMode } from "../hooks/useReadingMode";
import type { VoicePaneAgent } from "../lib/promptKeys";
import { desktopFeaturesOf, type DeckFleet, type DeckRuntimeState } from "../types";

/**
 * How long an Undo stays on offer, in milliseconds.
 *
 * Ten seconds: long enough to react to a navigation you did not mean — the gap
 * between a screen changing and a supervisor deciding it was wrong is a beat,
 * not a minute — and short enough that it never becomes a second, permanent
 * navigation control sitting in the report. An expired affordance is also
 * honest about what it was for: undoing *that* command, not going back in
 * general, which the deck's own controls already do.
 */
export const VOICE_UNDO_WINDOW_MS = 10_000;

/**
 * How often the surface asks what the microphone is doing while voice is on.
 *
 * **This is the seam an utterance ENDS at, and it was chosen over a Tauri
 * event with the latency in hand rather than by default.** `voice::Vad` decides
 * the speaking has stopped on the device thread, and the webview learns it by
 * asking: a poll that comes back `state: "done"` is the boundary, and the stop →
 * transcribe → resolve → start cycle hangs off it.
 *
 * An event emitted from the capture side would carry the boundary with no wait
 * at all. It would also be a new IPC verb in each of the three bridges plus a
 * listener seam, for a saving bounded by this interval — so the interval was cut
 * to a quarter second instead, which puts the polling tax at 125 ms of mean
 * added latency. **The pipeline it was weighed against has since got much
 * faster**: PRD #802 sized this at 4.3-6.3 s, when the default intent backend
 * was the agent CLI, and that backend is gone — speech measured a 0.653 s
 * median and commands 0.62-1.03 s, so the tax is nearer a tenth of the wait
 * than the under-3% it was. It is still a fraction of `voice::SILENCE_HOLD`,
 * which is 800 ms and remains the term that dominates the end of an utterance.
 * The event path stays available if the hold ever gets short enough for this to
 * matter.
 *
 * The cost is four state reads a second while the microphone is open. Each one
 * is an in-memory read of the capture session plus the small settings document,
 * touches no device, and stops the moment voice is turned off.
 */
export const VOICE_STATUS_POLL_MS = 250;

/**
 * The one sentence this file writes, for the one situation Rust cannot see.
 *
 * A dispatch outcome names an `invoke` that Rust has already checked against the
 * command table — but the table names an entry in a TypeScript registry, and
 * whether that entry exists is knowable only here. `xtask/linkage-check` rule 13
 * fails the build on an `invoke` that resolves to nothing, so this should be
 * unreachable; it exists because the alternative to a sentence is *silence*, and
 * a report that described a navigation nothing performed would be the one lie
 * this surface must not tell.
 */
export const NOTHING_DISPATCHED = "That command is not wired to anything in this build.";

/**
 * The second sentence this file writes, for the second situation Rust cannot
 * see: the user moved while the answer was being worked out.
 *
 * An outcome is classified against the screen declared immediately before the
 * resolve, so `unavailable` means *not on that screen*. The user can walk to
 * another one while the answer is being worked out — PRD #802 measured 0.653 s
 * for speech and 0.62-1.03 s for commands, and 4.3-6.3 s for the whole pipeline
 * before the agent-CLI intent backend was withdrawn — and that classification
 * then describes a screen nobody is standing on, so running it anyway would act
 * on the new screen with the old screen's permission.
 *
 * It is a sentence rather than silence for {@link NOTHING_DISPATCHED}'s reason:
 * the alternative is *Working out what that means…* vanishing with nothing in
 * its place, which reads as the surface having lost the command.
 */
export const SCREEN_MOVED_ON = "You moved to another screen while that was being worked out, so nothing ran. Say it again here.";

/**
 * PRD #1223 audit I1 — {@link SCREEN_MOVED_ON}'s case one level down: the
 * screen stayed put, but the New agent dialog opened or closed, or its form
 * became live or stopped being live, while the answer was being worked out.
 *
 * Opening the dialog leaves the base screen as `overview`, so the screen check
 * alone let an answer through that Rust grounded under the OTHER declaration —
 * a `close` or `open_deck` judged by the ordinary token list, dispatched into a
 * dialog whose own grounding (`heard_as_whole_while`) would have refused it,
 * and discarding the draft. See {@link sameNewAgentDeclaration} for which
 * changes count.
 */
export const DIALOG_MOVED_ON = "The New agent dialog changed while that was being worked out, so nothing ran. Say it again.";

/**
 * Whether two New agent declarations are the same CONTEXT for grounding — the
 * test {@link DIALOG_MOVED_ON} applies to a pending answer.
 *
 * Two presences, and they are exactly what the grounding reads: whether the
 * dialog is declared at all (the `new_agent_dialog` requirement, which selects
 * `close`'s and `open_deck`'s `heard_as_whole_while` lists) and whether its
 * form is (`new_agent_form`, which gates the fill rows).
 *
 * **The Rust table parser holds contextual grounding to exactly these two
 * presences** (issue #1248): `Requirement::rechecked_before_dispatch` in
 * `desktop/src-tauri/src/voice/table.rs` refuses a `heard_as_whole_while`
 * keyed on any requirement answered from something else — the directory
 * listing today — because this check would not notice it change. Comparing a
 * new dimension here is what lets that method say `true` for it.
 *
 * **Which deck and directory the form is for is deliberately not compared
 * here.** A move between two live forms changes no requirement, so the answer
 * was grounded under the rules that still hold; and every row that resolved
 * against the form's contents is already re-checked against its `{deckId,
 * path}` by the dialog at dispatch, which refuses in its own, more specific
 * words (`FORM_MOVED_ON`, `DIRECTORY_MOVED_ON`). This is the layer above those
 * re-checks, not a copy of them. Edits the declaration does not carry, such as
 * a typed Name, are not a change of context either.
 */
export function sameNewAgentDeclaration(a: VoiceNewAgentDto | undefined, b: VoiceNewAgentDto | undefined): boolean {
  return (a === undefined) === (b === undefined) && (a?.form === undefined) === (b?.form === undefined);
}

/**
 * PRD #1261 — how long a numbered choice stays on offer, in milliseconds.
 *
 * Twenty seconds, twice {@link VOICE_UNDO_WINDOW_MS}, because a list has to be
 * read before it can be answered. A starting value (the PRD's Open Question
 * 2). The countdown is on screen for all of it, and expiry runs nothing.
 */
export const VOICE_CHOICE_WINDOW_MS = 20_000;

/**
 * PRD #1261 — what the report says when a pending choice ends without an
 * entry being chosen. Surface sentences for {@link VOICE_NOTHING_TO_CLOSE}'s
 * reason: nothing Rust-side remembers that a choice was on offer.
 *
 * `CLOSED` is the one a non-answer gets, which is then resolved as the
 * ordinary utterance it is, so the report shows both.
 */
export const VOICE_CHOICE_CLOSED = "Choice closed.";
export const VOICE_CHOICE_CANCELLED = "Choice cancelled — nothing ran.";
export const VOICE_CHOICE_EXPIRED = "The choice expired, so nothing ran.";
export const VOICE_CHOICE_REFUSED = "That is not one of the entries on offer, or it is no longer there, so nothing ran. Say the command again.";
/**
 * PRD #1261 — the refusal for a bare number or cancel word that is also an
 * offered entry's name (`collidingChoiceEntry`): either reading could be
 * wrong, so the sentence says how to pick that entry unambiguously.
 */
export function voiceChoiceCollision(entry: number): string {
  return `That is both an entry's name and a number or cancel word, so nothing ran. Say the command again, then click the entry or say “number ${entry}”.`;
}

/**
 * PRD #1260 review — {@link SCREEN_MOVED_ON}'s case for the dictation mode: the
 * mode was entered or left while an utterance judged OUTSIDE it was being
 * worked out. An utterance judged INSIDE a mode that has since ended is dropped
 * without a sentence of its own, because the mode's exit already said that
 * nothing was sent and why.
 */
export const VOICE_MODE_MOVED_ON = "Typing mode changed while that was being worked out, so nothing ran. Say it again.";
/**
 * PRD #1260 review — the agent in the pane on screen was replaced by a new one
 * under the same deck and agent id while an utterance was being worked out.
 */
export const VOICE_PANE_REPLACED = "The agent in this pane was replaced while that was being worked out, so nothing ran. Say it again.";
/**
 * PRD #1260 review, round 4 — any other reason {@link contextLost} refuses to
 * type into, or press Enter in, a pane before anything was written: a
 * confirmation is open, or the pane on screen is not the one the utterance was
 * declared on.
 */
export function voiceNothingRan(why: string): string {
  return `Nothing ran — ${why}. Say it again.`;
}
/**
 * PRD #1260 review — a pending one-shot send called off because the agent it
 * would have pressed Enter for was replaced under the same id.
 */
export function voiceSendReplaced(label: string): string {
  return `The agent in ${label}'s pane was replaced, so nothing was sent. What was typed went to the agent it replaced.`;
}
/**
 * PRD #1260 review, round 3 — a pending one-shot send called off because the
 * context it was typed in moved (see {@link contextLost}).
 */
export function voiceSendCalledOff(label: string, why: string): string {
  return `Sending to ${label} was called off — ${why}. Nothing was sent; what was typed stays in its prompt.`;
}

/**
 * PRD #1261 — {@link contextLost}'s refusals for an answer to a numbered
 * choice. The gate is the same one, holding the entry to the context declared
 * with the FIRST utterance; only the words differ, because an answer can be a
 * click, and "while that was being worked out" describes a round trip a click
 * never made. What moved is the time since the list was offered — and for an
 * agent entry, since the utterance that produced it (review round 4): the
 * daemon replaced the agent under the same id, or the selected deck changed,
 * so the id now names a different agent (or none).
 */
export const VOICE_CHOICE_AGENT_REPLACED = "The agent you chose was replaced after the choice was offered, so nothing ran. Say the command again.";
/**
 * PRD #1261 — PR #1451 review: the agent an entry names was removed from the
 * selected deck after the choice was offered — distinct from a same-id
 * replacement, and refused the same way.
 */
export const VOICE_CHOICE_AGENT_GONE = "The agent you chose is gone since the choice was offered, so nothing ran. Say the command again.";
export const VOICE_CHOICE_DECK_MOVED_ON = "The selected deck changed after the choice was offered, so nothing ran. Say the command again.";
export const VOICE_CHOICE_SCREEN_MOVED_ON = "You moved to another screen after the choice was offered, so nothing ran. Say the command again here.";
export const VOICE_CHOICE_DIALOG_MOVED_ON = "The New agent dialog changed after the choice was offered, so nothing ran. Say the command again.";

/**
 * PR #1451 round 3, change 3 — a spoken number refused because the numbered
 * list it was said about changed: an item came, went or moved between the
 * moment the user began to speak and the answer, so the number may now be
 * another item's. The second is the same for a numbered choice that list
 * offered.
 */
export const VOICE_NUMBERS_MOVED_ON = "The numbers on screen changed while you were saying that, so nothing ran. Say the number again.";
export const VOICE_NUMBERS_CHOICE_MOVED_ON = "The numbers on screen changed after the choice was offered, so nothing ran. Say the number again.";
/** What a numbered answer's report names as having answered it: the app itself, with no backend asked. */
const NUMBERS_BACKEND = "local";
/**
 * A spoken number no item on screen shows — in the section said, when one
 * was ("select directory 99"). `elsewhere` names the other sections that do
 * show it ("select daemon 13" while directory 13 is on screen), which the
 * sentence points at without acting on (round 4, D7).
 */
export function voiceNumberNotShown(number: number, section?: VoiceNumberedSectionKind, elsewhere: readonly VoiceNumberedSectionKind[] = []): string {
  const item = section === undefined ? "item" : SECTION_NOUNS[section].one;
  /* Zero, and a number too long to say back (Qodo #16 on PR #1451). */
  if (number < 1 || !Number.isSafeInteger(number)) return `No ${item} on screen shows that number, so nothing ran.`;
  const refusal = `No ${item} on screen shows number ${number}, so nothing ran.`;
  const other = elsewhere[0];
  return other === undefined ? refusal : `${refusal} ${number} is ${SECTION_NOUNS[other].a}: say “${SECTION_NOUNS[other].one} ${number}”.`;
}

/** A section said with a number when that section numbers nothing on screen: "mode 2" on the dashboard (round 4, D7). */
export function voiceSectionNotNumbered(section: VoiceNumberedSectionKind, shown: readonly VoiceNumberedSectionKind[]): string {
  const names = shown.map((kind) => SECTION_NOUNS[kind].many);
  const what = names.length > 1 ? `${names.slice(0, -1).join(", ")} and ${names.at(-1)}` : names[0];
  return `No ${SECTION_NOUNS[section].many} are numbered on screen, so nothing ran.${what ? ` The numbers are on ${what}.` : ""}`;
}

/**
 * What a press gets when there is no transcription backend to listen with.
 *
 * `off` is the default and a product statement rather than a degraded mode, so
 * the button is neither hidden nor disabled: the owner's requirement is that
 * *"if voice control does not work, it should show instructions how to enable
 * it instead"*, and a greyed-out control shows nothing and reads as a fault.
 * Pressing it again after changing the setting works, because availability is
 * read at the press rather than cached from mount.
 *
 * It names the exact place, because "configure a backend" is advice and
 * *Settings → Voice* is a direction.
 */
export const VOICE_UNAVAILABLE = "Voice control has nothing to listen with yet. Choose a transcription backend under Settings → Voice, then press Voice again.";

/**
 * What a press gets when an utterance ran into the length cap.
 *
 * **The capped audio is DISCARDED rather than transcribed**, which is the one
 * place this surface decides something about an utterance instead of reporting
 * it. With `voice::Vad` ending an utterance after `voice::SILENCE_HOLD` of
 * quiet, reaching 30 seconds means no boundary was ever found — continuous
 * speech, a conversation in the room, a call, steady noise. In a navigation
 * vocabulary where every command is one to four words, that is *definitionally*
 * not a command, and transcribing it would spend a paid transcription call and
 * then a paid intent call to arrive at a no-match — once every thirty seconds,
 * for as long as the microphone is open.
 *
 * So it costs nothing, it says what happened rather than going quiet, and it
 * keeps listening. The user simply speaks again.
 */
export const VOICE_CAP_DISCARDED = "That ran to the 30 s limit with no pause in it, so nothing was sent. Say the command on its own.";

/**
 * What a press gets when Rust refused to let the microphone go.
 *
 * The fifth sentence this file writes, and the one it would most like not to
 * need. `voiceCancel` is idempotent and never refused by the session itself, so
 * a rejection here is the *call* failing — a blocking join that did not
 * complete, a command rejected before it reached the session — and what it
 * leaves behind is the one thing this surface must never guess at: whether the
 * device is still open. The honest answer is that it may be, so the button goes
 * on saying so and this says why, and what to do about it.
 *
 * Rendered with the rejection's own sentence after it. That is not this file
 * composing wording out of fields — the doctrine at the top of this file — but
 * two complete sentences printed one after the other: the surface's, because
 * only the surface knows a release was attempted, and Rust's, because only Rust
 * knows what went wrong. `DISPLAY_LIMITS.message` bounds the pair, eliding with
 * its own marker rather than passing a truncation off as complete.
 */
export const VOICE_RELEASE_REFUSED = "The microphone may still be open — releasing it was refused. Press Voice again to retry.";

/**
 * What the report row says before the first utterance (PRD #802).
 *
 * **The two things a user who has just pressed Voice does not otherwise know:
 * how to stop, and how to find out what to say.** Between the press and the
 * first utterance the row holds *Listening…* and nothing else, which is the
 * moment a new user is most looking at it and least able to act — so this is
 * the cheapest documentation in the product, and it sits at the point of use
 * rather than in a page nobody has opened.
 *
 * It names BOTH ways out, and the second one is the load-bearing half: the
 * exit phrase is a thing you have to have been told, while the button is on
 * screen — and if the phrase is misheard, or transcription has stopped
 * working, the button is the only way out that does not depend on being heard.
 *
 * Static, and one line, because the row is one line by contract. It is a
 * sixth sentence this file writes and it is about the SURFACE rather than
 * about an utterance, which is the same class as `VOICE_UNAVAILABLE` and
 * `VOICE_RELEASE_REFUSED`: nothing in the command table knows a user is
 * standing here having said nothing yet.
 */
export const VOICE_EMPTY_STATE = "Say “what can I say?” for the list, or “voice off” to stop — the Voice button stops it too.";

/**
 * PRD #802 D6 — how long the surface waits, with nobody speaking, before it
 * sends what it has typed into an agent's prompt.
 *
 * **Five seconds is the product owner's number and a starting value**, which is
 * why it is a constant rather than a literal in the timer. What it trades is
 * plain: too short and a thinking pause submits half an instruction, too long
 * and every dictated sentence ends in a wait.
 *
 * It is never reached silently. The countdown is on screen for every one of
 * these seconds and any speech at all resets it — see {@link VOICE_DICTATION_
 * TICK_MS} for how that is noticed, which is the part that makes the number
 * safe to tune rather than the number itself. It is also not the only way to
 * send: a whole-utterance submit phrase does it at once, and so does the user
 * pressing Enter in the prompt they are looking at.
 */
export const VOICE_DICTATION_SEND_MS = 5_000;

/**
 * PR #1451 — how long typing mode waits, with dictated words in the prompt
 * not yet sent and nobody speaking, before it highlights how to send them.
 *
 * **It never sends.** Typing mode sends only on "send it" or the user's own
 * Enter; this is a nudge, not a countdown. The hand test that asked for it
 * found users dictating, falling silent and waiting for something to happen,
 * so after a pause the "“send it” to send" part of the status is highlighted.
 *
 * Four seconds is longer than a pause between dictated sentences — the
 * capture's own 800 ms end-of-utterance hold plus about a second to transcribe
 * and type, then the user starting the next one — so it does not flash while
 * somebody is still talking, and short enough that someone waiting notices it
 * before they give up. Speaking again clears it, and it comes back after the
 * next pause; sending or leaving typing mode clears it for good.
 */
export const VOICE_SEND_NUDGE_MS = 4_000;

/**
 * PR #1451 — how long the panel waits for more words after an utterance that
 * matched no command, before saying so. A pause mid-command ("Set the command
 * to be … devbox run agent") ends an utterance after `voice::SILENCE_HOLD`, and
 * the half said before it matches nothing; if the user carries on inside this
 * window, the two halves are joined and worked out as one sentence. Measured
 * from when the first half's answer arrives, and the microphone records
 * throughout, so speech that began while the first half was being worked out
 * counts too. Only outcomes that ran nothing are held — see
 * `HOLDABLE_OUTCOMES` — so a complete command is as fast as before.
 */
export const VOICE_JOIN_WINDOW_MS = 2_000;

/**
 * The answers that are held for {@link VOICE_JOIN_WINDOW_MS} rather than
 * reported at once: each one ran nothing, and each is what the first half of a
 * command cut by a pause typically gets — nothing matched, a row the words did
 * not ask for, or a row missing what it acts on ("open" with no name yet).
 * Anything that did or offered something is never held.
 */
const HOLDABLE_OUTCOMES: ReadonlySet<VoiceOutcomeDto["kind"]> = new Set(["no_match", "unknown_action", "action_ungrounded", "param_missing"]);

/** Two halves of one sentence: the first loses the full stop the transcriber gave it. */
function joinUtterances(first: string, next: string): string {
  return `${first.replace(/[\s.,!?…]+$/u, "")} ${next.trim()}`;
}

/** The transcription's own sentence, as Rust's `heard_sentence` renders it. */
function heardSentence(transcript: string): string {
  return `Heard “${transcript}”${/[.?!…]$/u.test(transcript) ? "" : "."}`;
}

/** How often the countdown redraws, and the resolution it is shown at. */
export const VOICE_DICTATION_TICK_MS = 1_000;

/**
 * What actually gets typed into the agent, for one utterance.
 *
 * **Every control and format character becomes a space, and a carriage return
 * is the one that matters**: text carrying `\r` or `\n` would SUBMIT the
 * agent's prompt the moment it was written, which is precisely the silent
 * auto-submit the countdown exists to prevent. Format characters go with them
 * because a bidi override in a prompt is text that reads as one thing and says
 * another.
 *
 * **This is the ONLY transformation between the transcript and the agent, and
 * it is identity for every transcript a transcriber produces.** The words
 * arrive already sliced out of the transcript Rust-side
 * (`voice::dictation::strip_opening`), so what is left to do here is the
 * terminal-safety scrub and nothing else — no trimming of the user's own
 * spacing, no case folding, no rewording. A transcript with no control
 * characters in it reaches the agent byte for byte.
 *
 * A single trailing space so consecutive utterances do not run together — the
 * agent's input is being appended to, not replaced.
 */
export function dictationText(transcript: string): string {
  const typed = transcript.replace(/[\p{Cc}\p{Cf}]+/gu, " ").replace(/\s+/g, " ").trim();
  return typed === "" ? "" : `${typed} `;
}

/**
 * The keystroke that submits an agent's prompt.
 *
 * A carriage return, because that is what a terminal receives when somebody
 * presses Enter — the same bytes `TerminalViewport` forwards from xterm, down
 * the same `sendTerminalInput` path. Dictation gets no send verb of its own:
 * what it does is type, and then press Enter.
 */
export const VOICE_DICTATION_SUBMIT = "\r";

/**
 * How long a spoken send leaves the words it follows to settle before it
 * presses Enter, in milliseconds.
 *
 * An Enter that reaches an agent in the same read as the text before it is
 * taken as part of that text: Claude Code inserts a newline instead of
 * submitting, which is the opposite of what "… send it" asked for. Two writes
 * are not two reads — the agent reads whatever has piled up since it last
 * looked — so the Enter has to arrive LATER, not merely separately. 750 ms is
 * the settle the live prompt-key tests use before every Enter they press
 * (`tests/e2e_prompt_keys_live.rs`), so it is a gap known to submit rather
 * than a measured minimum. It counts from the moment the last write landed,
 * so a "send it" said as an utterance of its own, which comes well after the
 * words it sends, waits for nothing.
 */
export const VOICE_SUBMIT_SETTLE_MS = 750;

/**
 * PRD #1541 — how many times the clear key is pressed, by the deck's rule for
 * it. More than any voice-typed prompt needs, because an extra press on an
 * empty prompt is harmless in every supported agent and this app cannot see
 * the prompt to count. A client constant, not a deck value, because the deck
 * serves the rule and its pacing, and its pause budget is checked against
 * these counts (`desktop/src-tauri/src/dto.rs`, held in step by a drift test).
 */
export const VOICE_CLEAR_PRESSES: Record<"per_line" | "per_wrapped_row", number> = { per_line: 32, per_wrapped_row: 64 };
/**
 * PRD #1541 (PR #1558 review) — how much of a known prompt a clear is
 * counted as covering, by the deck's rule: two presses per logical line for
 * a per-line editor (one empties the line, one joins it to the line above),
 * and one per row of a pane this many columns wide for a wrapped one — a
 * conservatively narrow pane, since this app cannot see the agent's width.
 * Only a prompt within that reach is marked empty after a clear, or offered
 * an Undo ({@link clearCovers}).
 */
export const VOICE_CLEAR_PRESSES_PER_LINE = 2;
export const VOICE_CLEAR_ROW_COLUMNS = 20;

/**
 * PRD #1541 (PR #1558 review) — whether `presses` of the clear key, under the
 * deck's rule, reach the whole of `text`. Counts every `\r` or `\n` as a line
 * break and every character outside ASCII as two columns, so a wide character
 * never makes a prompt look shorter than it is.
 */
export function clearCovers(text: string, rule: "per_line" | "per_wrapped_row", presses: number): boolean {
  const lines = text.split(/[\r\n]/);
  if (rule === "per_line") return lines.length * VOICE_CLEAR_PRESSES_PER_LINE <= presses;
  let rows = 0;
  for (const line of lines) {
    let columns = 0;
    for (let index = 0; index < line.length; index += 1) columns += line.charCodeAt(index) < 0x7f ? 1 : 2;
    rows += Math.max(1, Math.ceil(columns / VOICE_CLEAR_ROW_COLUMNS));
  }
  return rows <= presses;
}

/**
 * PRD #1541 (PR #1558 review) — input made of nothing but terminal reports:
 * xterm focus in and out (`ESC [ I`, `ESC [ O`), SGR mouse reports
 * (`ESC [ < b ; x ; y M` or `m`) and X10/normal mouse reports (`ESC [ M` and
 * three bytes), alone or run together. The terminal sends them for a click or
 * a focus change; they do not type into the prompt. Anything else in the same
 * input counts as an edit. Whether one of them may still have moved the
 * prompt's cursor is {@link mouseButtonReported}.
 */
const TERMINAL_REPORTS = /^(?:\x1b\[[IO]|\x1b\[<\d+;\d+;\d+[Mm]|\x1b\[M[\x20-￿]{3})+$/;
export function terminalReportsOnly(data: string): boolean {
  return TERMINAL_REPORTS.test(data);
}
/** Each mouse report in input, capturing an SGR report's button code or an X10 report's button byte. */
const MOUSE_REPORT = /\x1b\[<(\d+);\d+;\d+[Mm]|\x1b\[M([\x20-￿])[\x20-￿]{2}/g;
/**
 * PRD #1541 (PR #1558 review) — whether terminal reports include a mouse
 * button press or release: a report whose button code is neither motion
 * (bit 32) nor the wheel (64 to 127). A click can move the cursor of an
 * agent's prompt, so voice no longer knows where its words are; focus,
 * motion and wheel reports leave the cursor where it was.
 */
export function mouseButtonReported(data: string): boolean {
  for (const [, sgr, x10] of data.matchAll(MOUSE_REPORT)) {
    const button = sgr !== undefined ? Number(sgr) : x10.codePointAt(0)! - 32;
    if ((button & 32) === 0 && (button & 0xc0) !== 64) return true;
  }
  return false;
}
/**
 * PRD #1541 — a second interrupt to the same pane this soon after the last
 * is refused: on an agent that has already stopped, a repeated `Esc` opens
 * Rewind, the transcript browser or the Session Tree instead.
 */
export const VOICE_INTERRUPT_REPEAT_MS = 3_000;
/**
 * PRD #1541 — the longest write "scratch that" removes, in characters, for
 * every agent; the deck's own `maxLiteralWriteChars` lowers it further.
 */
export const VOICE_SCRATCH_MAX_CHARS = 800;

/** PRD #1541 — what each command cannot do on an agent the deck served no keys for. */
const PROMPT_KEY_MISSING = {
  interrupt: "interrupt its turn",
  clear: "clear its prompt",
  scratch: "remove dictated words",
} as const;
export function voiceNoPromptKey(label: string, command: keyof typeof PROMPT_KEY_MISSING): string {
  return `${label} has no voice key to ${PROMPT_KEY_MISSING[command]}.`;
}

/** PRD #1541 — a prompt command said while another is still waiting or running for the same pane. */
export function voicePromptCommandPending(label: string): string {
  return `${label} already has a voice command pending — wait for it to finish, then say it again.`;
}
/** PRD #1541 — why a clear's Undo types nothing back: the prompt changed after the clear. */
const PROMPT_CHANGED = "the prompt was changed after it was cleared";
/** PRD #1541 — why a clear cannot be undone when the prompt changed while it was being cleared. */
const PROMPT_CHANGED_WHILE_CLEARING = "the prompt was changed while it was being cleared";

/** PRD #1541 — why there is nothing to scratch, by what last emptied the record of voice's writes. */
const PROMPT_EMPTIED = {
  fresh: "voice has typed nothing into this prompt since it was last sent",
  sent: "the prompt was sent after the last dictation",
  keyboard: "the prompt was typed into by hand after the last dictation",
  cleared: "the prompt was cleared after the last dictation",
  interrupted: "the agent was interrupted after the last dictation",
  replaced: "the agent in this pane was replaced",
  failed: "a write to the prompt failed",
  scratched: "every dictation since the prompt was last sent has been removed",
  clicked: "the agent's terminal was clicked after the last dictation, which may have moved its cursor",
} as const;

/**
 * PRD #1541 — one write into an agent's terminal, from anywhere: voice's
 * dictations and prompt commands, and the user's own keyboard. Its size, and
 * when it settled (`undefined` while it is in flight). "scratch that" reads a
 * run of these to tell whether the agent may have read the write it would
 * remove together with the input before it ({@link scratchCollapse}).
 */
type PaneWrite = { chars: number; landedAt?: number; evicted?: boolean };
/**
 * PRD #1541 — the recent writes into one incarnation of an agent's terminal
 * (`spawnedAtMs`), oldest first, and the boundary of what dropped off the
 * front of the list: when the latest of those writes settled
 * (`droppedLandedAt`), and how many of them have not settled yet
 * (`droppedInFlight`). While any has not, the boundary is unknown — a write
 * still on its way can land after everything kept — so a run that reaches
 * past what is kept is known to, rather than assumed not to.
 */
type PaneWrites = { writes: PaneWrite[]; droppedLandedAt?: number; droppedInFlight: number; spawnedAtMs?: number };
/** How many writes {@link PaneWrites} keeps per agent incarnation. */
const PANE_WRITES_KEPT = 256;

/** PRD #1541 — one voice write into a prompt: its text, and the terminal write that carried it. */
type PromptWrite = { text: string; sent: PaneWrite };

/**
 * PRD #1541 — what this app knows about one agent's prompt: the voice writes
 * made into it since it was last sent, cleared or interrupted (what "scratch
 * that" removes, last first), and whether those writes are the WHOLE prompt —
 * nothing typed by hand, nothing left over from before an interrupt — which
 * is the only case in which a clear can be undone by typing them again.
 *
 * `whole` holds only once this app has seen the prompt emptied — sent (by
 * voice or a plain keyboard Enter) or cleared. A pane first seen may already
 * hold a draft nobody dictated, so its record starts not whole.
 *
 * Kept per agent and per incarnation (`spawnedAtMs`): a replaced agent starts
 * a record of its own.
 */
type PromptRecord = {
  writes: PromptWrite[];
  whole: boolean;
  why: string;
  spawnedAtMs?: number;
  /**
   * The prompt's edit revision: a new value whenever something this record
   * cannot account for may have changed the prompt — keyboard input, a send,
   * a clear, an interrupt, a failed write, a replaced agent. "scratch that"
   * and a clear's Undo capture it and re-check it immediately before they
   * write, so neither edits a prompt that changed while it waited.
   */
  revision: number;
  /**
   * The last interrupt here: when its first key was delivered (`undefined`
   * while that key is still on its way), and whether it still `latched` — no
   * new turn evidenced since (a send, or the turn seen stopped (`sawIdle`) and
   * then working again). It is set before the first key is handed over, so
   * evidence that arrives while that key is in flight is kept. A latched
   * interrupt refuses another, however old: a status still reading "working"
   * may name the turn that was already interrupted.
   */
  interrupt?: { at?: number; latched: boolean; sawIdle: boolean };
  /**
   * The pane's turn epoch: a new value whenever a new turn is evidenced here —
   * a send seen (voice's Enter or the user's own), or the turn seen working
   * again after it was seen not working (`turn` is the last turn seen). An
   * interrupt captures it when it is said, and its first key goes out only
   * while it is unchanged, so an interrupt meant for one turn is never
   * delivered into the next observed turn.
   */
  epoch: number;
  turn?: string;
};

let promptRevisions = 0;
/** PRD #1541 — a revision no prompt record has held before. */
function nextRevision(): number {
  promptRevisions += 1;
  return promptRevisions;
}
/** PRD #1541 — forget voice's writes into `record`'s prompt, saying whether what is left is known (empty) and why. A new revision. */
function emptyRecord(record: PromptRecord, whole: boolean, why: string): void {
  record.writes = [];
  record.whole = whole;
  record.why = why;
  record.revision = nextRevision();
}

/** PRD #1541 — a queued write whose pane moved before its turn came; nothing was written. */
class PaneMoved extends Error {
  constructor(readonly why: string) {
    super(`Nothing was sent — ${why}.`);
  }
}

/**
 * PRD #1541 (PR #1558 review) — what a spoken send's wait settles to: every
 * write landed (`true`), one failed and said so (`false`), or why the send
 * must be dropped and say so — a prompt command that stopped partway.
 */
type WriteOutcome = boolean | string;
/** PRD #1541 (PR #1558 review) — a prompt command that delivered some of its writes and then stopped; a send waiting behind it drops its Enter. */
class StoppedPartway extends Error {
  constructor(readonly why: string) {
    super(why);
  }
}

/** The key one agent's prompt record and terminal writes are kept under. */
function paneKey(aim: { deckId: string; agentId: string }): string {
  return `${aim.deckId}\u0000${aim.agentId}`;
}
/**
 * PRD #1541 — the key one incarnation of an agent's write line is kept under:
 * a replaced agent gets a line of its own, so it never waits behind (or is
 * reserved by) a write to the agent it replaced.
 */
function lineKey(aim: { deckId: string; agentId: string }, spawnedAtMs: number | undefined): string {
  return `${paneKey(aim)}\u0000${spawnedAtMs ?? ""}`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => { window.setTimeout(resolve, ms); });
}

/** A write as a report quotes it: its words without the trailing space, and not too long to read. */
function quoted(write: string): string {
  const words = write.trim();
  return `“${words.length > 60 ? `${words.slice(0, 57)}…` : words}”`;
}

/**
 * PRD #1541 — why "scratch that" will not remove `write`, or `undefined` when
 * one delete per character removes it exactly: no longer than `limit`, and
 * nothing a single delete might not take whole — a combining mark, a
 * character outside the Basic Multilingual Plane (two UTF-16 units, one or two
 * deletes depending on the agent), a control or format character.
 */
function scratchRefusal(write: string, limit: number): string | undefined {
  if (write.length > limit) return `Cannot scratch that — ${quoted(write)} is too long to remove safely (over ${limit} characters).`;
  if (/[\p{M}\p{Cc}\p{Cf}\p{Cs}]/u.test(write) || [...write].length !== write.length) {
    return `Cannot scratch that — ${quoted(write)} has characters that cannot be removed safely one key at a time.`;
  }
  return undefined;
}

/**
 * PRD #1541 — why "scratch that" will not remove `last` because the agent may
 * have read it together with the input before it, or `undefined` when it
 * would not. An agent that reads more than about 800 characters in one go can
 * fold them into a single pasted block (Claude Code's `[Pasted text]`), which
 * one delete per character would not take back exactly.
 *
 * Reads the burst `last` belongs to: the run of writes into the terminal —
 * voice's, and the user's own keyboard — each settling less than
 * {@link VOICE_SUBMIT_SETTLE_MS} after the one before it. Refused when that
 * run, from its start through `last`, is over `limit` characters, or when its
 * size cannot be told: a write in it still in flight, or a run that reaches
 * past the writes {@link PaneWrites} keeps, or while a write dropped off the
 * front of those has not settled (its boundary is then unknown).
 */
function scratchCollapse(history: PaneWrites | undefined, last: PromptWrite, limit: number): string | undefined {
  const together = `Cannot scratch that — ${quoted(last.text)} followed other input so closely that the agent may have read them together as one paste, so it cannot safely tell what to remove.`;
  const unknown = `Cannot scratch that — the input around ${quoted(last.text)} has not all arrived yet, so it is not safe to tell whether the agent read it together as one paste.`;
  const writes = history?.writes ?? [];
  let index = writes.lastIndexOf(last.sent);
  if (index < 0 || (history?.droppedInFlight ?? 0) > 0) return unknown;
  let chars = 0;
  let next: PaneWrite | undefined;
  for (; index >= 0; index -= 1) {
    const write = writes[index];
    if (write.landedAt === undefined) return unknown;
    if (next?.landedAt !== undefined && next.landedAt - write.landedAt >= VOICE_SUBMIT_SETTLE_MS) return undefined;
    chars += write.chars;
    if (chars > limit) return together;
    next = write;
  }
  const dropped = history?.droppedLandedAt;
  if (dropped !== undefined && next?.landedAt !== undefined && next.landedAt - dropped < VOICE_SUBMIT_SETTLE_MS) return unknown;
  return undefined;
}

/**
 * What the report says when `close` was said and nothing was on top.
 *
 * A surface sentence, because only the surface knows the answer: the overlay it
 * owns is a `useState` boolean that no table column can see, and Rust cannot
 * render a not-here refusal for a row that is callable everywhere. It says what
 * was true rather than that something failed — there was nothing to close, and
 * the screen is where the user left it.
 */
export const VOICE_NOTHING_TO_CLOSE = "Nothing to close — this is the screen itself.";

/**
 * PRD #1260 — what the report says when a capped segment was TYPED rather than
 * discarded, because the dictation mode was on.
 *
 * {@link VOICE_CAP_DISCARDED}'s reasoning is about commands, which are one to
 * four words; a dictated paragraph is not, and discarding thirty seconds of it
 * is the worst outcome available. So while dictating the segment is
 * transcribed and typed like any other, and this says it ran to the limit.
 */
export const VOICE_CAP_TYPED = "That ran to the 30 s limit with no pause in it, so it was typed as one piece.";

/**
 * PRD #1260 — what the report says when `type off` was said with no mode on.
 * Rust cannot know — it keeps no memory between utterances — so the surface
 * answers for its own state.
 */
export const VOICE_NOT_DICTATING = "Not typing to any agent, so there was nothing to stop.";

/**
 * PRD #1260 — the sentences about the dictation mode's own state, which only
 * this surface holds: that it ended, and why; and that entry was refused on a
 * pane that cannot take input. The label is the deck's own text and crosses
 * `displayText` at the render seam like every other free-form string here.
 */
export function dictationStopped(label: string, why?: string): string {
  return why === undefined
    ? `Typing mode off. Nothing was sent to ${label}.`
    : `Typing mode off — ${why}. Nothing was sent to ${label}.`;
}
export function dictationRefused(label: string, reason: string): string {
  return `Typing mode not started for ${label}: ${reason}`;
}

/** The voice half of the runtime, which a runtime may not have at all. */
type Voice = Pick<DeckRuntimeState, "declareVoiceScreen" | "resolveVoice" | "answerVoiceChoice" | "answerVoiceNumber" | "voiceCommands" | "voiceStart" | "voiceStop" | "voiceStatus" | "voiceCancel" | "sendTerminalInput" | "desktopFeatures" | "voiceSpeechPlan" | "voiceSpeechAudio" | "voiceReadingStart" | "voiceReadingStop" | "onVoiceReadingConsentOff">;

/** The command table's row for the deck, whose screen issue #1198 hides by default. */
const OPEN_DECK_COMMAND = "open_deck";

/**
 * Whose prompt has words in it that nobody has sent yet (PRD #802 D6, rebuilt).
 *
 * **This is not a mode and the difference is the whole rebuild.** The old
 * `Dictation` meant *the microphone is aimed here and every utterance is
 * typed*; this means only *these words were typed and a countdown is running*.
 * Nothing about it changes how the next utterance is judged — it goes through
 * the resolver like any other — so there is no exit to miss and no false
 * positive that can truncate somebody mid-sentence.
 *
 * The composite identity, never the bare agent id: ids collide across decks,
 * and typing a user's words into the wrong machine's namesake is the worst
 * version of that collision this app has.
 */
type Pending = { deckId: string; agentId: string; label: string };

/**
 * PRD #1260 review — whether two spawn times name different incarnations of
 * one agent. Only when BOTH are known: `spawnedAtMs` is optional on the wire,
 * so a missing one is "not reported", never "a different agent" — a pane that
 * gains its spawn time is still the agent it was. The cost is that a same-id
 * replacement is not detected while the daemon omits the spawn time.
 */
function incarnationsDiffer(was: number | undefined, now: number | undefined): boolean {
  return was !== undefined && now !== undefined && was !== now;
}

/** One agent, by the composite identity — never the bare id, which collides across decks. */
type AgentAddress = { deckId: string; agentId: string };

function shows(pane: VoicePane | undefined, aim: AgentAddress): pane is VoicePane {
  return pane !== undefined && pane.deckId === aim.deckId && pane.agentId === aim.agentId;
}

/**
 * PRD #1260/#1261 review, round 4 — everything an utterance's side effects can
 * depend on, captured ONCE when the utterance is declared: before the resolve
 * is awaited, so nothing the user does during the round trip can leak into
 * it. `directories` rides along to be handed back to the dispatch and is not
 * compared; everything else is what {@link contextLost} compares.
 *
 * `generation` is the dictation mode's instance (see `modeGeneration`) and
 * `incarnations` every agent's `spawnedAtMs` on the selected deck — both as
 * they stood at the declaration, which for a numbered choice is the FIRST
 * utterance, not the moment the answer came back.
 */
type VoiceContext = {
  screen: VoiceScreen;
  directories: VoiceDirectoriesDto | undefined;
  newAgent: VoiceNewAgentDto | undefined;
  instance: string | undefined;
  generation: number;
  pane: VoicePane | undefined;
  deck: string | undefined;
  confirmation: boolean;
  incarnations: Readonly<Record<string, number | undefined>>;
};

/**
 * What one side effect acts on, which is what has to still hold for it:
 * `answer` for acting on a resolved utterance at all, `pane` for typing into
 * or pressing Enter in that agent's pane, `agent` for dispatching an outcome —
 * resolved directly or chosen from a list — whose params name that agent on
 * the selected deck (see {@link dispatchLost}).
 */
type Touches = { answer?: true; pane?: AgentAddress; agent?: string };

/** Why {@link contextLost} refused: a code the caller picks its sentence by, and the reason in words. */
type Lost = { code: "screen" | "dialog" | "mode" | "confirmation" | "pane" | "replaced" | "blocked" | "hidden" | "deck" | "agent" | "gone"; why: string };

/**
 * PRD #1260/#1261 review, rounds 4-5 — THE gate. Every side effect an
 * utterance has passes it immediately before it happens, with these touches:
 * acting on a resolved answer or offering a choice (`answer`, in
 * `resolveOne`); dispatching a resolved or chosen outcome (`answer`, plus
 * `agent` for every agent its params name — {@link dispatchLost}, in
 * `resolveOne` and `dispatchChoice`); typing, "send it" and entering the mode
 * (`pane`, in the pane seams, against the dispatch's declaration); arming the
 * one-shot countdown and pressing its Enter (`pane`, against the declaration
 * kept on `sending`). Every context change re-runs the `pane` check for the
 * mode and any pending send. It answers `undefined` while the context `was`
 * declared in still holds for what the effect `touches`, or why not. The
 * confirmation it reads is whichever screen is mounted — the overview's or
 * the deck's.
 *
 * Outside it by design: Undo, which is the user's own click on a report, and
 * a dictation-mode write already handed to the terminal before the mode ended
 * (see the dev doc's "speech still being processed" residual).
 *
 * Two of its rules are ABSOLUTE rather than about change, because a change
 * check can only be as good as the moment it was compared from: nothing is
 * typed or submitted into a pane while a D5 confirmation is open, whenever it
 * opened; and a pane write goes only to the pane that is on screen now AND was
 * on screen when the utterance was declared — a missing pane on either side
 * refuses, it never skips the check.
 *
 * One residual is change-only on purpose: `inputBlocked`. A one-shot typed
 * into a pane that already read as blocked is written, and in production the
 * daemon's refusal of the write is what reports it (the fixture runtime echoes
 * every write, so the preview shows it typed); the typing mode refuses such a pane at entry
 * (`startDictation`), where the failure would otherwise repeat every utterance.
 */
export function contextLost(was: VoiceContext, now: VoiceContext, touches: Touches): Lost | undefined {
  if (touches.answer) {
    if (now.screen !== was.screen) return { code: "screen", why: "you moved to another screen" };
    if (!sameNewAgentDeclaration(was.newAgent, now.newAgent) || now.instance !== was.instance) return { code: "dialog", why: "the New agent dialog changed" };
    if (now.generation !== was.generation) return { code: "mode", why: "typing mode changed" };
  }
  const aim = touches.pane;
  if (aim) {
    if (now.confirmation) return { code: "confirmation", why: "a confirmation is open" };
    if (!shows(was.pane, aim) || !shows(now.pane, aim)) return { code: "pane", why: now.pane === undefined ? "the pane closed" : "the pane on screen changed" };
    if (incarnationsDiffer(was.pane.spawnedAtMs, now.pane.spawnedAtMs)) return { code: "replaced", why: "the agent in the pane was replaced" };
    if (now.pane.inputBlocked !== undefined && was.pane.inputBlocked === undefined) return { code: "blocked", why: `the pane stopped taking input: ${now.pane.inputBlocked}` };
    /* PR #1451 (Qodo) — a pane showing another tab refuses whether or not it
       already did when the utterance was declared, unlike a read-only one
       above: its terminal still accepts the bytes, so nothing downstream
       would refuse words or an Enter aimed at a prompt nobody can see. */
    if (now.pane.terminalHidden) return { code: "hidden", why: "its terminal is not shown" };
  }
  if ((aim || touches.agent !== undefined) && now.deck !== was.deck) return { code: "deck", why: "the deck changed" };
  if (touches.agent !== undefined && touches.agent in was.incarnations && !(touches.agent in now.incarnations)) return { code: "gone", why: "the agent is gone" };
  if (touches.agent !== undefined && incarnationsDiffer(was.incarnations[touches.agent], now.incarnations[touches.agent])) return { code: "agent", why: "the agent was replaced" };
  return undefined;
}

/**
 * PRD #1260/#1261 review, round 5 — the param kinds whose `value` is an agent
 * id on the selected deck (see {@link VoiceResolvedParamDto}): `agent_ref`
 * names the agent itself, `orchestration_ref` one member of the card. Keyed by
 * KIND rather than by row, so a new row that takes either param is gated
 * without being listed anywhere.
 */
const AGENT_TARGETING_KINDS: ReadonlySet<string> = new Set(["agent_ref", "orchestration_ref"]);

/**
 * The gate for dispatching a resolved outcome: {@link contextLost}'s `answer`
 * touch, then its `agent` touch for every agent the outcome's params name —
 * so a stop resolved before a same-id replacement, or before the selected deck
 * changed, cannot open its confirmation for the agent that is there now.
 */
function dispatchLost(was: VoiceContext, now: VoiceContext, params: readonly VoiceResolvedParamDto[]): Lost | undefined {
  const lost = contextLost(was, now, { answer: true });
  if (lost) return lost;
  /* PR #1451 (Qodo) — a deck named in the outcome is chosen relative to the
     deck selected when it was declared; once that moved, the choice is stale
     whatever it names, exactly as an agent target on the old deck is. */
  if (params.some((param) => param.kind === "deck_ref") && now.deck !== was.deck) return { code: "deck", why: "the deck changed" };
  for (const param of params) {
    if (!AGENT_TARGETING_KINDS.has(param.kind)) continue;
    const moved = contextLost(was, now, { agent: param.value });
    if (moved) return moved;
  }
  return undefined;
}

/** What the report says when the gate refuses a resolved answer, or a pane write it dispatched, before anything ran. */
function answerRefusal(lost: Lost): string {
  if (lost.code === "screen") return SCREEN_MOVED_ON;
  if (lost.code === "dialog") return DIALOG_MOVED_ON;
  if (lost.code === "mode") return VOICE_MODE_MOVED_ON;
  if (lost.code === "replaced") return VOICE_PANE_REPLACED;
  return voiceNothingRan(lost.why);
}

/** The same for an answer to a numbered choice, whose wording counts from the offer (see `VOICE_CHOICE_*`). */
function choiceRefusal(lost: Lost): string {
  if (lost.code === "screen") return VOICE_CHOICE_SCREEN_MOVED_ON;
  if (lost.code === "dialog") return VOICE_CHOICE_DIALOG_MOVED_ON;
  if (lost.code === "deck") return VOICE_CHOICE_DECK_MOVED_ON;
  if (lost.code === "agent") return VOICE_CHOICE_AGENT_REPLACED;
  if (lost.code === "gone") return VOICE_CHOICE_AGENT_GONE;
  return voiceNothingRan(lost.why);
}

/**
 * PRD #1260 — the pane on screen, as the host sees it: whose it is, what the
 * deck calls that agent, and why its terminal cannot take input right now, if
 * it cannot. The dictation mode targets this and ends when it changes.
 *
 * `spawnedAtMs` is the agent's incarnation (`AgentRecord.spawned_at_ms`): a
 * daemon can replace an agent under the same deck and agent id, and the mode
 * ends then too, even when no snapshot ever showed the pane without an agent.
 */
/**
 * `terminalHidden`: the pane is showing another tab (Diff, Checks, …), so its prompt is not on screen (PR #1451).
 *
 * PRD #1541 — the {@link VoicePaneAgent} half: the agent's type and its name
 * for refusals, whether it is mid-turn (interrupt is sent only while
 * `turn === "working"`), and the deck's prompt keys for it (absent ⇒ the deck
 * is too old or the agent is unsupported, and the command is refused).
 */
export type VoicePane = { deckId: string; agentId: string; label: string; inputBlocked?: string; spawnedAtMs?: number; terminalHidden?: boolean } & VoicePaneAgent;

/**
 * PRD #1260 — the voice panel's state, the one model #1260, #1261 and #1184
 * share (see "Voice panel states and precedence" in the PRD).
 *
 * `idle` is one-shot commands, today's behaviour, with any pending one-shot
 * send carried beside it as {@link Pending}. `dictating` types every utterance
 * into one agent's prompt: the utterance is declared to Rust with the target,
 * which answers it locally and never calls the Commands backend. `declared` is
 * the context at entry, which {@link contextLost} holds the mode to: any
 * change it reports for the target's pane ends the mode. `awaitingChoice` (PRD #1261) holds a numbered choice
 * on offer: the next utterance is answered against it locally, and one that
 * is not an answer CLOSES it first and is then resolved as an ordinary
 * utterance — which is how "type on" reaches `dictating` from it.
 *
 * Only one state at a time, and ending one returns to `idle`, never to a state
 * that was pre-empted.
 */
type VoicePanelState =
  | { kind: "idle" }
  | { kind: "dictating"; target: Pending; declared: VoiceContext }
  | { kind: "awaitingChoice"; offer: VoiceChoiceOffer };

/**
 * PRD #1261 — a numbered choice on offer: the tie Rust reported, the backend
 * that answered it, and the context the FIRST utterance was declared in —
 * which {@link contextLost} holds the chosen entry to, exactly as it holds a
 * resolved dispatch.
 */
type VoiceChoiceOffer = {
  outcome: Extract<VoiceOutcomeDto, { kind: "param_ambiguous" }> & { invoke: string; candidates: VoiceResolvedParamDto[] };
  backend: string;
  declared: VoiceContext;
  /**
   * PR #1451 review — the wall-clock moment (`Date.now()`) the offer expires,
   * {@link VOICE_CHOICE_WINDOW_MS} after it was made. Checked before any
   * answer is dispatched, so a webview that delays the countdown's callbacks
   * cannot stretch the window.
   */
  deadline: number;
  /**
   * PR #1451 round 3, change 3 — set when the choice is between items of the
   * numbered list on screen (a number that is also an item's name): the list
   * as it was heard, and the item each candidate is, in the same order. Such a
   * choice is answered by the webview's own rule and dispatched as its item's
   * row, and refused if the list has changed since.
   */
  numbered?: { heard: VoiceNumberedListDto; entries: VoiceNumberedEntryDto[] };
};

const IDLE: VoicePanelState = { kind: "idle" };

/**
 * What the surface is doing, which is not the same as whether voice is ON.
 *
 * The two are deliberately separate state: `on` is the user's toggle and the
 * button reflects it alone, so the control never flickers off while an utterance
 * is being transcribed. This says which step of the cycle is in flight.
 *
 * The last two are the release rather than the cycle, and they exist because
 * `on` alone cannot tell the truth about it (PRD #802's audit): `stopping` is a
 * release Rust has not acknowledged yet, and `unreleased` is one it refused.
 */
type VoicePhase = "idle" | "opening" | "listening" | "transcribing" | "resolving" | "stopping" | "unreleased";

/**
 * Whether Rust is still holding something for this session.
 *
 * `CaptureState::accepts_start` takes idle, done and failed and refuses these
 * two, and that refusal is what makes a stale session unrecoverable from the
 * surface rather than merely untidy: the button looks off, and the press that
 * ought to turn voice on is refused *because* a recording is already running.
 * `recording` holds the device — or, past the cap, the audio the cap kept when
 * it released the device — and `transcribing` holds a buffer.
 */
function stillHeld(state: VoiceStatusDto["state"]): boolean {
  return state === "recording" || state === "transcribing";
}

/**
 * What the button is allowed to CLAIM, which is not always what `on` says.
 *
 * The button is the only indication the microphone is open, so every state in
 * which that is unknown or not yet settled needs its own word rather than being
 * rounded to the nearest of two. `checking` is a panel that has not heard back
 * from Rust yet — a fresh mount, including the replacement one a webview reload
 * produces — and `stopping` is a release still in flight. Both render as
 * something other than *Voice off*, because *Voice off* over a live device is
 * the one thing this control must never say.
 */
type VoiceIndicator = "off" | "on" | "stopping" | "checking";

function indicatorFor(known: boolean, on: boolean, phase: VoicePhase): VoiceIndicator {
  if (phase === "stopping") return "stopping";
  /* A release Rust REFUSED, which is a statement about the device rather than
     about the user's toggle — so it outranks both of them. `turnOff` reaches
     this with `on` still true and got the same answer by accident; the mount
     reconcile and `turnOn` reach it with `on` false and `known` either way,
     and would otherwise render `Voice off` or `Voice…` over a microphone whose
     release just failed. One presentation for one situation, wherever it
     happened: the device may still be open, and the press is the retry. */
  if (phase === "unreleased") return "on";
  if (on) return "on";
  return known ? "off" : "checking";
}

const INDICATOR_LABEL: Record<VoiceIndicator, string> = {
  off: "Voice off",
  on: "Voice on",
  stopping: "Voice stopping…",
  checking: "Voice…",
};

/**
 * The state twice over, because the two audiences read different things: the
 * word is what a sighted user sees on the control, and this is what a screen
 * reader announces. `mixed` is ARIA's own value for a toggle whose state is not
 * yet settled, which is exactly what a panel waiting on its first status read
 * is — `false` there would be the same claim the word avoids.
 */
const INDICATOR_PRESSED: Record<VoiceIndicator, "true" | "false" | "mixed"> = {
  off: "false",
  on: "true",
  stopping: "true",
  checking: "mixed",
};

const INDICATOR_TITLE: Record<VoiceIndicator, string> = {
  off: "Voice control is off — press to start listening",
  on: "Voice control is on — press to stop listening",
  stopping: "Releasing the microphone — waiting for it to close",
  checking: "Checking whether the microphone is open",
};

interface VoiceControlPanelProps {
  runtime: Voice;
  /** The mounted screen, which is what a command's availability is judged against. */
  screen: VoiceScreen;
  /**
   * Run a resolved dispatch, and answer with how to undo it.
   *
   * The host runs it because the host owns the view: this surface knows an
   * action was dispatched and nothing about where the user was standing when it
   * was.
   *
   * `undefined` means **nothing ran** — the `invoke` named no registry entry —
   * which is what {@link NOTHING_DISPATCHED} is for. A dispatch that ran but has
   * nothing to reverse answers with an object carrying no `undo`, so the two
   * cases stay distinguishable: one is a report the surface must correct, the
   * other is an ordinary command with no Undo beside it.
   */
  onDispatch: (outcome: Extract<VoiceOutcomeDto, { kind: "dispatch" }>, declaredDirectories?: VoiceDirectoriesDto, declaredNewAgent?: VoiceNewAgentDto) => { undo?: () => void } | undefined;
  /**
   * What the New agent dialog's directory browser is showing right now, or
   * `undefined` when it is showing nothing (PRD #1223).
   *
   * A getter rather than a value, read immediately before each resolve for the
   * screen's reason: a declaration that lagged the browser would judge the
   * utterance against a listing the user has left. The same declaration is
   * handed back to `onDispatch`, so the directory members can tell whether the
   * browser moved during the round trip.
   */
  directories?: () => VoiceDirectoriesDto | undefined;
  /**
   * What the New agent dialog shows besides its browser, or `undefined` while
   * it is closed (PRD #1223) — read and handed back exactly as `directories`
   * is, so the form rows can tell whether the form moved during the round
   * trip.
   */
  newAgent?: () => VoiceNewAgentDto | undefined;
  /**
   * Which MOUNT of the New agent dialog that declaration came from (PRD #1223;
   * Qodo on PR #1235) — read at the same moment and compared at the same
   * moment, so an answer resolved against a dialog the user has since closed
   * and reopened is refused with {@link DIALOG_MOVED_ON} instead of acting on
   * the replacement. The declaration itself cannot say: it compares presences,
   * and two live forms look alike.
   */
  newAgentInstance?: () => string | undefined;
  /**
   * The `[endpoints]` section the Deck selector is rendering right now
   * (PRD #1195) — declared with each utterance so "switch deck to …" resolves
   * against the decks on screen rather than `desktop.toml`, which lags an edit
   * by a queued write (Qodo on PR #1340). Read at the same moment as
   * `directories`, for the same reason.
   */
  endpoints?: () => EndpointSettingsDto | undefined;
  /**
   * Where this panel publishes the context members only IT can serve
   * (PRD #802, the `voice_off` row).
   *
   * The mirror of `DeckSurface`'s `voiceChannel` and the same mechanism: a
   * mutable slot read at DISPATCH time, so the host always dispatches through
   * this render's closures rather than a copy taken when it last rendered. It
   * points the other way round the screen switch — a screen publishes upward to
   * the shell, and this publishes sideways to the same shell — which is what
   * makes a command served from here callable on every screen.
   *
   * Optional, because a panel rendered without a host that dispatches still
   * works as a microphone; the rows naming these members simply cannot run.
   */
  channel?: VoicePanelChannel;
  /**
   * PRD #1260 — the agent pane on screen, or `undefined` for none. What the
   * dictation mode enters for, and what ends it when it closes, changes agent
   * or stops taking input.
   */
  pane?: VoicePane;
  /**
   * PRD #1541 — every deck the host observes, with its agents. What the panel
   * keeps per agent — its write line, its terminal-write history, its prompt
   * record — is let go when the agent leaves its deck or is replaced
   * ({@link VoiceControlPanel}'s fleet effect). Optional: a panel rendered
   * without a host keeps them for as long as it is mounted.
   */
  fleet?: DeckFleet;
  /** PRD #1260 — the selected deck, whose change ends the dictation mode. */
  selectedDeckId?: string;
  /**
   * PRD #1260 — a D5 confirmation is open (held by the overview). It outranks
   * the dictation mode: opening one ends the mode, and it does not resume.
   */
  confirmationOpen?: boolean;
  /** PRD #1260 — told when the dictation mode starts or ends, so the host can
   * mark the pane it is typing into. */
  onDictationChange?: (target: Pending | undefined) => void;
  /** PRD #1497 — told when reading mode starts or ends, so the host can mark
   * the pane being read. */
  onReadingChange?: (target: ReadingTarget | undefined) => void;
  /**
   * PRD #1497 — reading's Settings opt-in as the settings on screen hold it
   * (`"on"` or `"off"`). Turned off, reading ends here at once — a drain
   * still being said included, which the Rust side has no session left to
   * end. Absent, nothing is ended on its account.
   */
  readingConsent?: string;
  /** PR #1451, round 3 — told when voice turns on or off, so the host can
   * share it with the lists that render differently while it is on. */
  onVoiceChange?: (on: boolean) => void;
  /**
   * PRD #1261 review, round 4 — the incarnation (`spawnedAtMs`) of every
   * agent on the selected deck as the host sees it now, by agent id; absent or
   * `undefined` where none is reported. Captured with every declaration, so a
   * chosen agent entry whose agent was replaced since is refused.
   */
  agentIncarnations?: () => Readonly<Record<string, number | undefined>>;
  /**
   * PR #1451 round 3, change 3 — the numbered list on screen now, with its
   * generation. Read when the microphone opens, again while nobody has spoken
   * yet, and once more when a bare number is answered, so a number said about
   * a list that has since changed is refused.
   */
  numbered?: () => VoiceNumberedListDto;
  /**
   * PR #1451 round 3, change 4 — the list on screen split into pages, if one
   * is: read when an utterance matched nothing, so a name said for an item on
   * another page is told that page instead of "no matching action".
   */
  pages?: () => VoicePager | undefined;
  /** PR #1451 round 3 — told when the numbered choice opens or closes, so the lists behind it can hide their numbers. */
  onChoiceChange?: (open: boolean) => void;
  /**
   * PRD #1541 — where this panel listens for what the user types into an
   * agent's terminal by hand. The host calls it for every keyboard write, so
   * "scratch that" and a clear's Undo know when the prompt holds something
   * voice did not type.
   */
  keyboard?: { current: ((target: AgentAddress, data: string, sent: Promise<void>) => void) | undefined };
}

/**
 * A Tauri rejection is the Rust `Err(String)` itself rather than an `Error`, so
 * both shapes reach here and both are already a rendered sentence.
 */
function sentenceOf(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/**
 * `backend, latency` — or the backend alone when no call was made.
 *
 * The two travel together because they are only useful together: *4.2 s* is a
 * complaint and *claude, 4.2 s* is a reason to change backends. A `null`
 * `resolveMs` renders NO timing rather than `0 ms`, which would claim a
 * measurement nobody took — silence short-circuits before any backend call, and
 * the backend still names what would have answered.
 */
export function voiceCost(backend: string, ms: number | null): string {
  if (ms === null) return backend;
  return `${backend}, ${ms >= 1_000 ? `${(ms / 1_000).toFixed(1)} s` : `${ms} ms`}`;
}

/** What the report says the surface is doing, while it is doing it. */
function progressNote(indicator: VoiceIndicator, phase: VoicePhase): string | undefined {
  if (indicator === "stopping") return "Releasing the microphone…";
  if (indicator !== "on") return undefined;
  if (phase === "transcribing") return "Turning that into text…";
  if (phase === "resolving") return "Working out what that means…";
  if (phase === "opening") return "Opening the microphone…";
  // A release Rust refused. `VOICE_RELEASE_REFUSED` and the rejection's own
  // sentence are already in the report saying what happened; a progress note
  // over them would claim something is still in flight, and nothing is.
  if (phase === "unreleased") return undefined;
  return "Listening…";
}

/**
 * The button, and the report beside it.
 *
 * **Nothing at all when the runtime cannot resolve an utterance.** Absence is a
 * real state: a control with nothing behind it would be worse than its absence,
 * and it is the same reasoning the microphone itself gets one layer down.
 */
export function VoiceControlPanel({ runtime, screen, onDispatch, channel, directories, newAgent, newAgentInstance, endpoints, pane, fleet, selectedDeckId, confirmationOpen = false, onDictationChange, onReadingChange, readingConsent, onVoiceChange, agentIncarnations, numbered, pages, onChoiceChange, keyboard }: VoiceControlPanelProps) {
  /* Held in a ref so the resolve and the overlay read the host's latest getter
     without either callback being rebuilt when the host re-renders. */
  const directoriesRef = useRef(directories);
  directoriesRef.current = directories;
  const newAgentRef = useRef(newAgent);
  newAgentRef.current = newAgent;
  const newAgentInstanceRef = useRef(newAgentInstance);
  newAgentInstanceRef.current = newAgentInstance;
  const endpointsRef = useRef(endpoints);
  endpointsRef.current = endpoints;
  const agentIncarnationsRef = useRef(agentIncarnations);
  agentIncarnationsRef.current = agentIncarnations;
  const numberedRef = useRef(numbered);
  numberedRef.current = numbered;
  const pagesRef = useRef(pages);
  pagesRef.current = pages;
  /**
   * PR #1451 round 3, change 3 — the numbered list as it stood when the user
   * began to speak: read when the microphone opens and refreshed by every
   * poll that has heard nobody yet, then held. A bare number is answered
   * against it.
   */
  const heardNumbers = useRef<VoiceNumberedListDto | undefined>(undefined);
  /**
   * PR #1451 round 3 review (Qodo) — the {@link VoiceContext} standing when
   * {@link heardNumbers} was taken: the declaration a bare number is held to,
   * so an answer that comes back after the screen, the dialog, the mode or the
   * selected daemon moved is refused through {@link contextLost} like any
   * other answer, even when the numbered rows still look the same.
   */
  const heardContext = useRef<VoiceContext | undefined>(undefined);
  const { declareVoiceScreen, resolveVoice, answerVoiceChoice, answerVoiceNumber, voiceCommands, voiceStart, voiceStop, voiceStatus, voiceCancel, sendTerminalInput } = runtime;
  /* PRD #1497 — reading mode and the speech queue it speaks through. A
     sentence that could not be spoken is reported in the row. */
  const reader = useReadingMode(runtime, onReadingChange, (reason) => setProblem(`Could not speak: ${reason}.`));
  /**
   * PRD #1497 D8 — whether the app spoke during the recording the utterance
   * being worked on came from. Set when a segment is taken, read when it is
   * declared: while it is true only "stop" and "quiet" are answered.
   */
  const overlappedRef = useRef(false);
  /** PRD #1497 — reading mode and the app's speech, as declared with an utterance. */
  const declaredReading = (): VoiceReadingStateDto => ({ reading: reader.mode.on, speaking: overlappedRef.current });
  /* Issue #1198 — the list of what can be said leaves out the deck while the
     deck is hidden, even from its "elsewhere" half: it is not somewhere else,
     it is not there. The crate withholds the row from the model as well
     (`voice::schema::annotate_for`); this covers the overlay's own render,
     including against an older crate that still lists it as callable. */
  const showDeck = desktopFeaturesOf(runtime).showDeck;

  const [on, setOnState] = useState(false);
  const [phase, setPhaseState] = useState<VoicePhase>("idle");
  /**
   * Whether this panel has heard from the microphone yet.
   *
   * `on` starts false on every mount, and a mount is not always the first one:
   * a hard webview reload, a crashed web-content process or a destroyed and
   * recreated webview all produce a panel with no memory in front of a Rust
   * session that may still be recording. Until the reconcile below answers,
   * this panel knows nothing about the device, and the button says so rather
   * than rendering the default as if it were an observation.
   *
   * A runtime with no `voiceStatus` has nothing to ask, so there is nothing to
   * wait for and the initial value is already the answer.
   */
  const [known, setKnown] = useState(() => voiceStatus === undefined);
  /** A refusal, an instruction, or one of this file's own sentences. */
  const [problem, setProblem] = useState<string>();
  /**
   * The transcription stage's own sentence — what was heard, or why nothing
   * was — and, when something was heard, the transcript it quotes. The
   * transcript is kept so the row can tell whether the answer that follows
   * already quotes it (see `captureShown`).
   */
  const [capture, setCapture] = useState<{ sentence: string; transcript?: string }>();
  const [result, setResult] = useState<VoiceResultDto>();
  /*
    Wrapped in an object rather than held bare: `useState` treats a function
    argument as an updater, so `setUndo(fn)` would call `fn` instead of storing
    it — and `fn` here is a navigation.
  */
  const [undo, setUndo] = useState<{ run: () => void }>();
  /** Set by `reportRefused` while `resolveOne` is dispatching, so a refused
      dispatch reports only its refusal (see there). Also set where the
      dispatch is answered in this surface's own words because only the
      surface knows the state it changed — leaving the dictation mode, whose
      target Rust never remembers (PRD #1260). */
  const refusedRef = useRef(false);

  /*
    Both toggles are mirrored into refs and written through a setter, because
    the poll below has to read them BETWEEN its own awaits — after a status
    round trip, to decide whether the answer is still wanted. A `useState` value
    captured in that closure is whatever it was when the closure was built, and
    an effect-updated ref lags by a commit; a ref written in the same statement
    as the state is current the instant the decision is made.
  */
  const onRef = useRef(false);
  const setOn = useCallback((next: boolean) => { onRef.current = next; setOnState(next); }, []);

  /* PR #1451, round 3 — the host mirrors whether voice is on for the lists
     that render differently while it is (`hooks/useVoiceOn.ts`). Reported
     after the commit that changed it, and as `false` when the panel goes
     away, so no list is left showing voice's numbers for a panel that is
     gone. */
  const voiceChanged = useRef(onVoiceChange);
  voiceChanged.current = onVoiceChange;
  useEffect(() => { voiceChanged.current?.(on); }, [on]);
  useEffect(() => () => voiceChanged.current?.(false), []);

  /*
    PR #1451 — while voice is on the row is half as large again and its
    sentences wrap rather than elide (`.voice-row[data-voice="on"]` in
    `styles.css`), so its height is its content's. Every surface that keeps
    clear of the row reads `--voice-row-height`, so while voice is on that
    variable is the row's MEASURED height, kept current as the text wraps and
    unwraps, and it goes back to the stylesheet's constant when voice is off.
    A layout effect, so the first frame with the larger row already reserves
    it. jsdom has no `ResizeObserver` and lays nothing out, hence the guards.
  */
  const rowRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    const row = rowRef.current;
    if (!on || !row) return;
    const root = document.documentElement;
    const reserve = () => {
      const height = row.getBoundingClientRect().height;
      if (height > 0) root.style.setProperty("--voice-row-height", `${height}px`);
    };
    reserve();
    const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(reserve);
    observer?.observe(row);
    return () => {
      observer?.disconnect();
      root.style.removeProperty("--voice-row-height");
    };
  }, [on]);
  const phaseRef = useRef<VoicePhase>("idle");
  const setPhase = useCallback((next: VoicePhase) => { phaseRef.current = next; setPhaseState(next); }, []);

  /**
   * Whose prompt is waiting to be sent, or `undefined` for none
   * (PRD #802 D6, rebuilt).
   *
   * Mirrored into a ref and written through a setter for the reason the two
   * toggles above are: the poll and the cycle read it BETWEEN their own awaits,
   * and a `useState` value captured in one of those closures is whatever it was
   * when the closure was built.
   */
  const [pending, setPendingState] = useState<Pending>();
  const pendingRef = useRef<Pending | undefined>(undefined);
  const setPending = useCallback((next?: Pending) => { pendingRef.current = next; setPendingState(next); }, []);
  /**
   * PRD #1260 — the panel state ({@link VoicePanelState}), mirrored into a ref
   * for the reason `pending` is: the cycle reads it between its own awaits, to
   * decide what to declare and whether a capped segment is typed or dropped.
   */
  const [panelState, setPanelStateState] = useState<VoicePanelState>(IDLE);
  const panelStateRef = useRef<VoicePanelState>(IDLE);
  const dictationChanged = useRef(onDictationChange);
  dictationChanged.current = onDictationChange;
  const choiceChanged = useRef(onChoiceChange);
  choiceChanged.current = onChoiceChange;
  /**
   * PRD #1260 review — the dictation mode's generation: bumped on every entry
   * to and exit from `dictating`. A cycle notes it when it declares, and after
   * its round trip acts only if it is unchanged — so an utterance judged inside
   * a mode that has since ended types nothing and presses nothing, even when
   * the user re-entered the mode on the same pane in between, which the
   * target's identity alone cannot tell apart.
   */
  const modeGeneration = useRef(0);
  /**
   * PR #1451 — typing mode's nudge to send ({@link VOICE_SEND_NUDGE_MS}):
   * whether it is showing, its timer, and whether words dictated in the mode
   * are still unsent, which is the only time there is anything to nudge about.
   */
  const [nudge, setNudgeState] = useState(false);
  const nudgeShown = useRef(false);
  const setNudge = useCallback((next: boolean) => { nudgeShown.current = next; setNudgeState(next); }, []);
  const nudgeTimer = useRef<number | undefined>(undefined);
  const unsent = useRef(false);
  /**
   * PR #1451 (Qodo) — every typed write since the last send, settled to
   * whether ALL of them landed. A spoken send waits for it: an Enter queued
   * behind a write that then fails would submit whatever was in the prompt
   * before. A send consumes it, and typing mode's entry and exit clear it, so
   * one failure is not held against every later send.
   *
   * PRD #1541 (PR #1558 review) — settled to `true` when all landed, `false`
   * when one failed and has said so, or the sentence a send dropped behind a
   * prompt command that stopped partway must explain itself with.
   */
  const lastWrite = useRef<Promise<WriteOutcome> | undefined>(undefined);
  /**
   * PRD #1541 (PR #1558 review) — how many keyboard edits each agent's prompt
   * has had, by {@link paneKey}. A spoken send notes it when said and presses
   * Enter only while it is unchanged, so input typed (or an Enter pressed) by
   * hand while the send waits for its writes and their settle calls it off.
   */
  const keyboardEdits = useRef(new Map<string, number>());
  /** When the last tracked write landed ({@link VOICE_SUBMIT_SETTLE_MS} counts from it). */
  const lastLanded = useRef(0);
  const trackWrite = useCallback((write: Promise<unknown>) => {
    const landed = write.then((): WriteOutcome => { lastLanded.current = Date.now(); return true; }, (cause): WriteOutcome => (cause instanceof StoppedPartway ? cause.why : false));
    const before = lastWrite.current;
    lastWrite.current = before === undefined ? landed : Promise.all([before, landed]).then(([a, b]) => (a === true ? b : a));
  }, []);
  /**
   * PRD #1541 — each agent incarnation's write line: everything this panel
   * has handed its terminal and not yet seen settle, and how many of those
   * are prompt commands (interrupt, clear, scratch, a clear's Undo). It is the
   * panel's own queue of dictations and prompt commands: the user's keyboard
   * and a spoken send's Enter do not go through it.
   *
   * A prompt command waits for every write before it, and a dictated write
   * that arrives while one is running waits for it — a clear is two writes a
   * second apart in Claude Code, and words typed between them would be cleared
   * by the second. Dictated writes do not otherwise wait for each other, which
   * is how typing mode has always written them.
   */
  const paneLines = useRef(new Map<string, { tail: Promise<void>; inflight: number; commands: number; pane: string; spawnedAtMs?: number }>());
  /** The host's view of the pane and the deck, for the entry check. */
  const paneRef = useRef(pane);
  paneRef.current = pane;
  /** PRD #1541 — the incarnation of `aim` when it is the pane on screen. */
  const incarnationOf = useCallback((aim: AgentAddress) => {
    const shown = paneRef.current;
    return shows(shown, aim) ? shown.spawnedAtMs : undefined;
  }, []);
  /**
   * The write line of `aim`'s incarnation on screen: one per incarnation
   * ({@link lineKey}), so an agent replaced while a write to it never settled
   * does not hand that stuck line to its replacement.
   */
  const onPane = useCallback((aim: AgentAddress, kind: "write" | "command", write: () => Promise<void>): Promise<void> => {
    const spawnedAtMs = incarnationOf(aim);
    const key = lineKey(aim, spawnedAtMs);
    let line = paneLines.current.get(key);
    if (!line) {
      line = { tail: Promise.resolve(), inflight: 0, commands: 0, pane: paneKey(aim), spawnedAtMs };
      paneLines.current.set(key, line);
    }
    const waits = kind === "command" ? line.inflight > 0 : line.commands > 0;
    const run = waits ? line.tail.then(write) : write();
    const done = run.then(() => undefined, () => undefined);
    const mine = line;
    mine.inflight += 1;
    if (kind === "command") mine.commands += 1;
    mine.tail = Promise.all([mine.tail, done]).then(() => undefined);
    void done.then(() => {
      mine.inflight -= 1;
      if (kind === "command") mine.commands -= 1;
      if (mine.inflight === 0 && paneLines.current.get(key) === mine) paneLines.current.delete(key);
    });
    return run;
  }, [incarnationOf]);
  /**
   * PRD #1541 — every agent's recent terminal writes, by {@link paneKey}:
   * voice's and the keyboard's alike, kept apart from the prompt record so
   * that nothing which empties the record forgets them ({@link scratchCollapse}).
   */
  const paneWrites = useRef(new Map<string, PaneWrites>());
  /**
   * Note a write of `chars` characters handed to `aim`'s terminal, settling
   * with `sent`. A write called off before it went out is forgotten. A write
   * to an incarnation other than the one the kept history is for starts a
   * history of its own: what was written to the agent it replaced cannot be
   * read together with it.
   */
  const notePaneWrite = useCallback((aim: AgentAddress, chars: number, sent: Promise<unknown>): PaneWrite => {
    const key = paneKey(aim);
    const spawnedAtMs = incarnationOf(aim);
    let history = paneWrites.current.get(key);
    if (!history || incarnationsDiffer(history.spawnedAtMs, spawnedAtMs)) {
      history = { writes: [], droppedInFlight: 0, spawnedAtMs };
      paneWrites.current.set(key, history);
    }
    history.spawnedAtMs ??= spawnedAtMs;
    const entry: PaneWrite = { chars };
    const kept = history;
    kept.writes.push(entry);
    while (kept.writes.length > PANE_WRITES_KEPT) {
      const dropped = kept.writes.shift()!;
      /* One still in flight leaves the boundary unknown until it settles —
         never a guess at when it will. */
      if (dropped.landedAt === undefined) {
        dropped.evicted = true;
        kept.droppedInFlight += 1;
      } else kept.droppedLandedAt = Math.max(kept.droppedLandedAt ?? dropped.landedAt, dropped.landedAt);
    }
    const settled = (landed: boolean) => {
      if (landed) entry.landedAt = Date.now();
      if (entry.evicted) {
        kept.droppedInFlight -= 1;
        if (entry.landedAt !== undefined) kept.droppedLandedAt = Math.max(kept.droppedLandedAt ?? entry.landedAt, entry.landedAt);
      } else if (!landed) {
        const at = kept.writes.indexOf(entry);
        if (at >= 0) kept.writes.splice(at, 1);
      }
    };
    void sent.then(() => settled(true), (cause) => settled(!(cause instanceof TerminalInputCancelled)));
    return entry;
  }, [incarnationOf]);
  /**
   * PRD #1541 — hand one prompt-command write to `aim`'s terminal, guarded by
   * `check` at the last moment the app controls: immediately before the
   * bridge gives it to the daemon, after every earlier write to that terminal
   * (the keyboard's included) has settled. `check` answers why the write must
   * not go out, or `undefined`; a write it stops rejects as {@link PaneMoved}
   * with that reason, having written nothing.
   */
  const guardedSend = useCallback((aim: AgentAddress, data: string, check: () => string | undefined): { sent: PaneWrite; done: Promise<void> } => {
    let why: string | undefined;
    const write = sendTerminalInput(aim, data, () => {
      why = check();
      return why === undefined;
    });
    const sent = notePaneWrite(aim, data.length, write);
    const done = write.catch((cause: unknown) => {
      if (cause instanceof TerminalInputCancelled) throw new PaneMoved(why ?? "the prompt changed before the keys went out");
      throw cause;
    });
    return { sent, done };
  }, [notePaneWrite, sendTerminalInput]);
  /** PRD #1541 — every agent's {@link PromptRecord}, by {@link paneKey}. */
  const prompts = useRef(new Map<string, PromptRecord>());
  /** The record for `aim` as it is now: a fresh one when the agent was replaced since. */
  const promptFor = useCallback((aim: AgentAddress, spawnedAtMs: number | undefined): PromptRecord => {
    const key = paneKey(aim);
    const was = prompts.current.get(key);
    if (was && !incarnationsDiffer(was.spawnedAtMs, spawnedAtMs)) {
      was.spawnedAtMs ??= spawnedAtMs;
      return was;
    }
    const shown = paneRef.current;
    const fresh: PromptRecord = {
      writes: [],
      whole: false,
      why: was ? PROMPT_EMPTIED.replaced : PROMPT_EMPTIED.fresh,
      spawnedAtMs,
      revision: nextRevision(),
      epoch: 0,
      turn: shows(shown, aim) && !incarnationsDiffer(shown.spawnedAtMs, spawnedAtMs) ? shown.turn : undefined,
    };
    prompts.current.set(key, fresh);
    return fresh;
  }, []);
  const setPanelState = useCallback((next: VoicePanelState) => {
    const was = panelStateRef.current;
    if (was.kind === "dictating" || next.kind === "dictating") {
      modeGeneration.current += 1;
      /* PR #1451 — every entry and exit starts the nudge, and the writes a
         spoken send waits for, from nothing. */
      unsent.current = false;
      lastWrite.current = undefined;
      if (nudgeTimer.current !== undefined) window.clearTimeout(nudgeTimer.current);
      nudgeTimer.current = undefined;
      setNudge(false);
    }
    panelStateRef.current = next;
    setPanelStateState(next);
    if (was.kind === "dictating" || next.kind === "dictating") dictationChanged.current?.(next.kind === "dictating" ? next.target : undefined);
    /* PR #1451 round 3 review — told in the same tick as the state itself, so
       the host's update batches into the commit that first renders the choice
       and the lists behind it never show their numbers beside the choice's
       own. The effect on `choiceShown` below stays as the backstop. */
    if ((was.kind === "awaitingChoice") !== (next.kind === "awaitingChoice")) choiceChanged.current?.(next.kind === "awaitingChoice");
  }, [setNudge]);
  /** PRD #1541 — forget voice's writes into `aim`'s prompt, saying whether what is left is known (empty) and why. */
  const resetPrompt = useCallback((aim: AgentAddress, whole: boolean, why: string) => {
    emptyRecord(promptFor(aim, incarnationOf(aim)), whole, why);
  }, [incarnationOf, promptFor]);
  /**
   * PRD #1541 — a send was observed in `aim`'s pane (voice's Enter or the
   * user's own): evidence of a new turn, so an interrupt delivered before it
   * no longer latches the next one (the repeat floor still applies).
   */
  const noteSent = useCallback((aim: AgentAddress) => {
    const record = promptFor(aim, incarnationOf(aim));
    record.epoch += 1;
    if (record.interrupt) record.interrupt.latched = false;
  }, [incarnationOf, promptFor]);
  const selectedDeckRef = useRef(selectedDeckId);
  selectedDeckRef.current = selectedDeckId;
  const confirmationRef = useRef(confirmationOpen);
  confirmationRef.current = confirmationOpen;
  /** PRD #1261 — seconds left on a pending choice. */
  const [choiceIn, setChoiceIn] = useState<number>();
  /** Seconds left before the typed text is sent, or `undefined` for no pending send. */
  const [sendIn, setSendIn] = useState<number>();
  const sendTimer = useRef<number | undefined>(undefined);
  /**
   * PRD #1260 review, round 2 — the one-shot send's token. Every cancellation
   * bumps it ({@link cancelPendingSend} is the only writer), and a one-shot
   * notes it when its terminal write starts. The write's continuations and the
   * countdown act only while it is unchanged, so a write that finishes after
   * Voice went off, a mode began or a newer send replaced it arms nothing,
   * presses nothing, and does not clear or overwrite the newer one's report.
   *
   * Needed because the write is detached from the cycle: the microphone
   * reopens while it is outstanding, so anything can happen before it settles.
   */
  const sendEpoch = useRef(0);
  /**
   * PRD #1260 review, round 4 — the live one-shot send: whose prompt, and the
   * context its utterance was declared in, which {@link contextLost} holds it
   * to from the moment its write starts until it is called off or its Enter is
   * pressed. Cleared with every epoch bump.
   */
  const sending = useRef<{ aim: Pending; declared: VoiceContext } | undefined>(undefined);
  /**
   * PR #1451 round 3 review — a spoken send ("send it", or a trailing send
   * sentence) waiting for its earlier writes to land before it presses Enter.
   * The poll calls it off on new speech exactly like a countdown, and the
   * {@link sendEpoch} it noted refuses the Enter once anything else did.
   * Cleared with every epoch bump.
   */
  const deferredSubmit = useRef(false);

  /**
   * The screen is read at submit time rather than closed over, so a navigation
   * before the submit cannot stale it — the resolve runs from a `useCallback`
   * and would otherwise hold whichever screen was mounted when it was built.
   *
   * **A navigation AFTER the submit is a different problem and this does not
   * close it.** Reading it here fixes what the utterance is judged against; it
   * says nothing about the user moving during the round trip, which is what
   * {@link SCREEN_MOVED_ON} and the re-check below are for.
   *
   * Written from an effect rather than during render: a ref mutated in a render
   * body is a write React is allowed to discard and re-run, and the value is
   * needed at an await, which is always after a commit.
   */
  const screenRef = useRef(screen);
  useEffect(() => { screenRef.current = screen; }, [screen]);

  /**
   * PRD #1260/#1261 review, round 4 — the {@link VoiceContext} standing right
   * now. Called once to DECLARE an utterance, and again as the other side of
   * every {@link contextLost} check.
   */
  const current = useCallback((): VoiceContext => ({
    screen: screenRef.current,
    directories: directoriesRef.current?.(),
    newAgent: newAgentRef.current?.(),
    instance: newAgentInstanceRef.current?.(),
    generation: modeGeneration.current,
    pane: paneRef.current,
    deck: selectedDeckRef.current,
    confirmation: confirmationRef.current,
    incarnations: agentIncarnationsRef.current?.() ?? {},
  }), []);
  /** Take the numbered list on screen as the one heard, with the context it stands in. */
  const hearNumbers = useCallback(() => {
    heardNumbers.current = numberedRef.current?.();
    heardContext.current = current();
  }, [current]);
  /**
   * The declared context of the dispatch running right now, so the pane seams
   * it reaches synchronously (`typeIntoAgent`, `submitAgentPrompt`,
   * `startDictation`) gate against the utterance's declaration rather than
   * whatever stands when they are called. Set only around `onDispatch`; a seam
   * reached outside one gates against the present, which still enforces both
   * absolute rules.
   */
  const dispatching = useRef<VoiceContext | undefined>(undefined);
  const dispatchDeclared = useCallback((outcome: Extract<VoiceOutcomeDto, { kind: "dispatch" }>, declared: VoiceContext) => {
    /* Restored rather than cleared (review round 5), so a dispatch nested in
       another cannot drop the outer one's declaration on its way out. */
    const outer = dispatching.current;
    dispatching.current = declared;
    try {
      return onDispatch(outcome, declared.directories, declared.newAgent);
    } finally {
      dispatching.current = outer;
      /* PR #1451 round 3, change 4 — what voice just did may have changed the
         numbered list ("next page" renumbers it), and the user's next words
         are about the screen that command produced. So the list is taken
         again once that render has landed, as it is at every silent poll. */
      /* The re-read is a macrotask so it runs after React has committed what
         the dispatch set — and it reads the list that commit declared ONLY
         because `useNumberedList` publishes from a LAYOUT effect, which runs
         inside the commit. A passive `useEffect` there could still be pending
         when this fires, so this would re-read the list from before the
         command. Do not downgrade it. */
      window.setTimeout(hearNumbers, 0);
    }
  }, [hearNumbers, onDispatch]);
  /** The pane seams' gate: the dispatch's declaration against now, for writes to `aim`. */
  const paneLost = useCallback((aim: AgentAddress) => contextLost(dispatching.current ?? current(), current(), { pane: aim }), [current]);

  /**
   * Which in-flight step the surface is still waiting for.
   *
   * The same idea as [`SessionInner::opening`][] one layer down, and for the
   * same finding: an operation that outlives the user's decision to abandon it
   * must be able to tell that it did. Voice turned off mid-resolve used to be a
   * closure still holding `onDispatch`, so the app navigated — or opened an
   * overlay — after the user stopped it. The window is the ordinary one rather
   * than a contrived race: PRD #802 measured over a second of pipeline per
   * utterance (0.653 s speech plus 0.62-1.03 s commands), and 4.3-6.3 s before
   * the agent-CLI intent backend was withdrawn.
   *
   * A counter rather than a boolean because it also has to order two live
   * cycles: a superseded one must not write its report over the one that
   * replaced it.
   *
   * [`SessionInner::opening`]: the Rust capture session's reservation, in
   * `desktop/src-tauri/src/voice/capture.rs`.
   */
  const request = useRef(0);
  /** Take the surface, and hand back the question *is it still mine?*. */
  const claim = useCallback(() => {
    const mine = ++request.current;
    return () => request.current === mine;
  }, []);
  /** Abandon whatever is in flight without starting anything. */
  const abandon = useCallback(() => { request.current += 1; }, []);

  /*
    The unmount half, and the device with it. `voiceCancel` is reached through a
    ref so this effect's dependency list stays empty: a runtime that rebuilt the
    function identity would otherwise run the cleanup and cancel a live
    recording that nobody asked to stop.
  */
  const cancelRef = useRef(voiceCancel);
  useEffect(() => { cancelRef.current = voiceCancel; }, [voiceCancel]);
  const statusRef = useRef(voiceStatus);
  useEffect(() => { statusRef.current = voiceStatus; }, [voiceStatus]);

  /**
   * After a release Rust refused: is the device gone anyway?
   *
   * A rejection says the CALL failed, not what it did — the session's own
   * `cancel` is idempotent and never refused, so a rejection is a blocking join
   * that did not complete or a command that never reached the session. Asking
   * is the only way to tell the two apart, and the unknown answers all resolve
   * to *not released*: this is the one place where guessing in the direction
   * that suits the button would put *Voice off* over a live microphone.
   *
   * Reached through `statusRef` rather than the prop, and with an empty
   * dependency list, because all three callers need it: `turnOff`, `turnOn` and
   * the mount reconcile below — and the reconcile's list has to stay stable or
   * a runtime that rebuilt its function identities would re-run it and cancel a
   * live recording nobody asked to stop.
   */
  const releasedAfterRefusal = useCallback(async () => {
    const ask = statusRef.current;
    if (!ask) return false;
    try {
      return !stillHeld((await ask()).state);
    } catch {
      return false;
    }
  }, []);
  useEffect(() => () => {
    abandon();
    if (onRef.current) void cancelRef.current?.().catch(() => undefined);
  }, [abandon]);

  /*
    PRD #802's audit blocker — the mount reconcile, and the reason this panel
    is not the microphone's owner.

    Recording lifetime is Rust's. The unmount cleanup above is a React PASSIVE
    effect making an asynchronous IPC call, and a webview that is being replaced
    is not guaranteed to run it, let alone to let it finish: a hard reload, a
    web-content process crash, a destroyed webview. The replacement panel then
    initialises `on` to false, and without this it would render `Voice off` over
    a device Rust is still holding — with no way out, because the press that
    looks like it turns voice on calls `voiceStart`, which `accepts_start`
    refuses while a recording is running. At the cap it is worse: the device is
    released but up to 960 KB of captured speech stays in the session, and
    nothing on the new panel can reach it.

    So the panel asks once, on mount, and cancels anything Rust is still holding
    before it claims the device is closed. The Rust-side release in
    `desktop/src-tauri/src/lib.rs` is what actually guarantees the device is
    freed on teardown; this is what stops a *surviving* session being invisible
    to the panel in front of it.

    Everything is reached through refs so the dependency list stays empty — a
    runtime that rebuilt these function identities must not re-run a reconcile
    and cancel a live recording nobody asked to stop. The ordinary claim orders
    it against the user: a press during the round trip supersedes this, and
    `turnOn` reconciles the same way itself, so the press is never left waiting
    on it.
  */
  useEffect(() => {
    const ask = statusRef.current;
    if (!ask) return;
    const ours = claim();
    void (async () => {
      let status: VoiceStatusDto | undefined;
      try {
        status = await ask();
      } catch {
        // A status read that failed says nothing about the device, and there is
        // no press to report it against. `turnOn` reconciles again at the next
        // one, and until then the button settles to what this panel knows —
        // which is that nothing here ever opened the microphone.
      }
      if (!ours()) return;
      if (status && stillHeld(status.state)) {
        try {
          await cancelRef.current?.();
        } catch (cause) {
          /* The release was REFUSED, and this is the surface with the LEAST
             information: a fresh panel, no press to report against, and a
             device the previous panel left open. `setKnown(true)` here used to
             run anyway, rendering `Voice off` and a crossed-out microphone over
             exactly that — the same class as the blocker `turnOff` was rewritten
             for, from the one direction that had no press behind it.

             `releasedAfterRefusal` because a rejection says the CALL failed and
             not what it did, and every unknown answer resolves to *not
             released*. */
          if (!(await releasedAfterRefusal())) {
            if (!ours()) return;
            setPhase("unreleased");
            setProblem(`${VOICE_RELEASE_REFUSED} ${sentenceOf(cause)}`);
            return;
          }
        }
        if (!ours()) return;
      }
      setKnown(true);
    })();
  }, [claim, releasedAfterRefusal, setPhase]);

  /*
    Defence in depth, and explicitly NOT the mechanism. `pagehide` is the one
    webview-loss path the document itself can see, so it is worth getting there
    first — but it is an asynchronous IPC call on a document that is going away,
    which is the same thing that makes the unmount cleanup above insufficient.
    What guarantees the release is Rust's own teardown.
  */
  useEffect(() => {
    const release = () => { if (onRef.current) void cancelRef.current?.().catch(() => undefined); };
    window.addEventListener("pagehide", release);
    return () => window.removeEventListener("pagehide", release);
  }, []);

  useEffect(() => {
    if (!undo) return;
    const timer = window.setTimeout(() => setUndo(undefined), VOICE_UNDO_WINDOW_MS);
    return () => window.clearTimeout(timer);
  }, [undo]);

  /**
   * Call off a pending send.
   *
   * Idempotent, and reached from every path that could invalidate the send:
   * more speech, a new utterance, the exit phrase, voice off, the button,
   * unmount. That breadth is the point — a timer that outlived the state it was
   * about would press Enter in an agent's prompt with nobody watching, which is
   * the one thing this feature must never do.
   */
  const clearSendTimer = useCallback(() => {
    if (sendTimer.current !== undefined) window.clearInterval(sendTimer.current);
    sendTimer.current = undefined;
    setSendIn(undefined);
  }, []);
  const cancelPendingSend = useCallback(() => {
    sendEpoch.current += 1;
    sending.current = undefined;
    deferredSubmit.current = false;
    clearSendTimer();
  }, [clearSendTimer]);
  /** Take the nudge down and stop its clock; `sent` also forgets the unsent words. */
  const stopNudge = useCallback((sent = false) => {
    if (nudgeTimer.current !== undefined) window.clearTimeout(nudgeTimer.current);
    nudgeTimer.current = undefined;
    if (sent) unsent.current = false;
    setNudge(false);
  }, [setNudge]);
  /** Start the pause clock from now; it shows the nudge when it runs out. */
  const armNudge = useCallback(() => {
    stopNudge();
    nudgeTimer.current = window.setTimeout(() => {
      nudgeTimer.current = undefined;
      if (unsent.current && panelStateRef.current.kind === "dictating") setNudge(true);
    }, VOICE_SEND_NUDGE_MS);
  }, [setNudge, stopNudge]);
  useEffect(() => () => stopNudge(true), [stopNudge]);
  /**
   * PR #1451 — an answer that ran nothing, waiting {@link VOICE_JOIN_WINDOW_MS}
   * for the rest of the sentence. `continued` is set by the poll when the
   * microphone hears speech inside the window; the held answer then waits for
   * that utterance instead of the clock.
   */
  const held = useRef<{ answer: VoiceResultDto; transcript: string; continued: boolean }>(undefined);
  const heldTimer = useRef<number>(undefined);
  const clearHeld = useCallback(() => {
    if (heldTimer.current !== undefined) window.clearTimeout(heldTimer.current);
    heldTimer.current = undefined;
    held.current = undefined;
  }, []);
  /** Nothing followed after all: say what the held answer says. */
  const reportHeld = useCallback(() => {
    const waiting = held.current;
    clearHeld();
    if (waiting) setResult(waiting.answer);
  }, [clearHeld]);
  useEffect(() => clearHeld, [clearHeld]);
  /**
   * PRD #1260 review, round 4 — hold the live one-shot send to its declared
   * context ({@link contextLost}), calling it off if the context no longer
   * holds. Answers whether it was called off. Run when the write finishes,
   * immediately before Enter, and on every context change in between.
   */
  const callOffLostSend = useCallback(() => {
    const send = sending.current;
    const lost = send && contextLost(send.declared, current(), { pane: send.aim });
    if (!send || !lost) return false;
    cancelPendingSend();
    setPending(undefined);
    setProblem(lost.code === "replaced" ? voiceSendReplaced(send.aim.label) : voiceSendCalledOff(send.aim.label, lost.why));
    return true;
  }, [cancelPendingSend, current, setPending]);

  /**
   * Press Enter in the agent's prompt.
   *
   * Through `sendTerminalInput`, which is the path the user's own keystrokes
   * take — dictation gets no send verb of its own, and in particular not
   * `submit_text`, which types AND submits in one guarded call and would make
   * the text invisible until it was already gone.
   */
  const submitDictation = useCallback(async (aim: Pending) => {
    /* PRD #1260 review, round 3 — the action this Enter belongs to. A failure
       that arrives after a newer send, utterance or mode change is dropped: it
       must not cover the newer report. Every caller bumps the epoch first, so
       this names this Enter alone. */
    const epoch = sendEpoch.current;
    const generation = modeGeneration.current;
    try {
      const target = { deckId: aim.deckId, agentId: aim.agentId };
      const sent = sendTerminalInput(target, VOICE_DICTATION_SUBMIT);
      notePaneWrite(target, VOICE_DICTATION_SUBMIT.length, sent);
      await sent;
      /* PRD #1541 — sent, so the prompt is empty and wholly known again, and
         the agent has a new turn to interrupt. */
      resetPrompt(aim, true, PROMPT_EMPTIED.sent);
      noteSent(aim);
    } catch (cause) {
      if (sendEpoch.current !== epoch || modeGeneration.current !== generation) return;
      setProblem(sentenceOf(cause));
    }
  }, [notePaneWrite, noteSent, resetPrompt, sendTerminalInput]);
  /**
   * Press Enter once every typed write since the last send has landed — the
   * guarded half of a spoken send, shared by "send it" and a trailing send
   * sentence (PR #1451 round 3).
   *
   * PR #1451 (Qodo) — a write that failed has already said so (and ended
   * typing mode); the Enter is then dropped rather than submitting what was in
   * the prompt before, and so is one whose pane or mode moved while it waited.
   *
   * PR #1451 round 3 review — and so is one the user talked over or that was
   * otherwise called off: the deferred Enter notes {@link sendEpoch}, and new
   * speech, a newer utterance or any cancellation moves it.
   *
   * PRD #1541 — and once they have landed, the Enter waits out what is left of
   * {@link VOICE_SUBMIT_SETTLE_MS} since the last of them, so the agent reads
   * it apart from the words and submits instead of starting a new line. The
   * wait is held to the same checks as the wait for the writes.
   *
   * PR #1558 review — and to the prompt not being edited by hand meanwhile:
   * keyboard input (an Enter included) while the send waits calls it off and
   * says so, because the Enter would submit a prompt the user is still
   * changing, or press a second Enter after their own.
   */
  const submitAfterWrites = useCallback((aim: Pending) => {
    const written = lastWrite.current;
    lastWrite.current = undefined;
    if (written === undefined) {
      void submitDictation(aim);
      return;
    }
    const declared = current();
    const generation = modeGeneration.current;
    const epoch = sendEpoch.current;
    const edits = keyboardEdits.current.get(paneKey(aim)) ?? 0;
    deferredSubmit.current = true;
    const stale = () => sendEpoch.current !== epoch || modeGeneration.current !== generation || contextLost(declared, current(), { pane: aim }) !== undefined;
    /* Press Enter, unless the prompt was edited by hand since the send was said. */
    const submit = () => {
      if ((keyboardEdits.current.get(paneKey(aim)) ?? 0) !== edits) {
        setProblem(`Cancelled the send to ${aim.label} — the prompt was changed by hand while the send waited.`);
        return;
      }
      void submitDictation(aim);
    };
    void written.then((ok) => {
      if (sendEpoch.current !== epoch) return;
      if (ok !== true || stale()) {
        deferredSubmit.current = false;
        if (typeof ok === "string" && !stale()) setProblem(`Cancelled the send to ${aim.label} — ${ok}.`);
        return;
      }
      const settle = lastLanded.current + VOICE_SUBMIT_SETTLE_MS - Date.now();
      if (settle <= 0) {
        deferredSubmit.current = false;
        submit();
        return;
      }
      window.setTimeout(() => {
        if (sendEpoch.current !== epoch) return;
        deferredSubmit.current = false;
        if (stale()) return;
        submit();
      }, settle);
    });
  }, [current, submitDictation]);

  /**
   * Start the visible countdown to a send, replacing any already running.
   *
   * An interval rather than one timeout, because the number on screen is the
   * whole of what makes this safe: a silent five-second wait and a five-second
   * wait a user can watch and talk over are different features. The interval
   * both redraws and decides, so the two cannot disagree about how long is
   * left.
   */
  const armSend = useCallback((aim: Pending, epoch: number) => {
    clearSendTimer();
    let left = Math.max(1, Math.round(VOICE_DICTATION_SEND_MS / VOICE_DICTATION_TICK_MS));
    setSendIn(left);
    sendTimer.current = window.setInterval(() => {
      /* Every cancellation clears this interval, so this is a backstop. */
      if (sendEpoch.current !== epoch) {
        clearSendTimer();
        return;
      }
      left -= 1;
      if (left > 0) {
        setSendIn(left);
        return;
      }
      /* The gate immediately before Enter, so a change no effect has seen yet
         still calls the send off. */
      if (callOffLostSend()) return;
      cancelPendingSend();
      setPending(undefined);
      void submitDictation(aim);
    }, VOICE_DICTATION_TICK_MS);
  }, [callOffLostSend, cancelPendingSend, clearSendTimer, setPending, submitDictation]);

  /* A pending send must not survive this panel. The timer is a window timer and
     would otherwise keep running with nothing behind it. */
  useEffect(() => cancelPendingSend, [cancelPendingSend]);

  /**
   * PRD #1260 — leave the dictation mode, saying so and why. Sends nothing:
   * whatever was typed stays in the prompt, visible and editable (#802 D6's
   * "never auto-submits on exit", kept). Answers whether a mode was on.
   */
  const endDictation = useCallback((why?: string) => {
    const mode = panelStateRef.current;
    if (mode.kind !== "dictating") return false;
    setPanelState(IDLE);
    setProblem(dictationStopped(mode.target.label, why));
    return true;
  }, [setPanelState]);

  /*
    PRD #1260 — every context change ends the mode, and it never follows the
    user: whatever {@link contextLost} reports for the target's pane against
    the context at entry — the pane closing or showing another agent
    (navigation, `Escape`, `closeAgent`, a retired agent), the agent replaced
    or no longer taking input, the selected deck changing, and a D5
    confirmation opening, which outranks the mode and does not give it back
    when answered. Each is something the host already observes and hands down;
    the mode subscribes rather than polls.

    A pending one-shot send is held to the same gate on the same changes
    (review rounds 2-4): called off, never sent, from the moment its write
    starts until Enter.
  */
  const paneDeckId = pane?.deckId;
  const paneAgentId = pane?.agentId;
  const paneBlocked = pane?.inputBlocked;
  const paneSpawnedAtMs = pane?.spawnedAtMs;
  const paneHidden = pane?.terminalHidden;
  useEffect(() => {
    const mode = panelStateRef.current;
    const lost = mode.kind === "dictating" ? contextLost(mode.declared, current(), { pane: mode.target }) : undefined;
    if (lost) endDictation(lost.why);
    callOffLostSend();
  }, [callOffLostSend, confirmationOpen, current, endDictation, paneAgentId, paneBlocked, paneDeckId, paneHidden, paneSpawnedAtMs, panelState, pending, selectedDeckId]);

  /* The host marks the pane it is typing into; a panel going away takes the
     mode with it, so it must not leave the mark behind. */
  useEffect(() => () => {
    if (panelStateRef.current.kind === "dictating") dictationChanged.current?.(undefined);
  }, []);

  /* PR #1451 — the user's own Enter is a send, so it takes the nudge down for
     good. Capture phase on the window, because the key lands in the agent's
     terminal, which handles it before anything could bubble back here.
     Shift+Enter and Alt+Enter are not sends: the terminal sends them as a
     newline in every agent (#1422), so the words are still unsent; nor is an
     Enter that commits an input method's composition. */
  const dictatingNow = panelState.kind === "dictating";
  useEffect(() => {
    if (!dictatingNow) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Enter" || event.shiftKey || event.altKey || event.isComposing) return;
      if (unsent.current) stopNudge(true);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [dictatingNow, stopNudge]);

  /** Everything the last utterance left behind, cleared when the next one has words in it. */
  /**
   * PRD #1541 — bumped by {@link forget}, so a prompt command that finishes
   * after the user has said something else does not put its report, or its
   * Undo, over the newer utterance's.
   */
  const reportGeneration = useRef(0);
  const forget = useCallback(() => {
    reportGeneration.current += 1;
    setProblem(undefined);
    setCapture(undefined);
    setResult(undefined);
    setUndo(undefined);
  }, []);

  /**
   * PRD #1261 — end a pending choice, saying why when there is something to
   * say. Runs nothing. Answers whether a choice was pending.
   */
  const closeChoice = useCallback((why?: string) => {
    if (panelStateRef.current.kind !== "awaitingChoice") return false;
    setPanelState(IDLE);
    if (why !== undefined) setProblem(why);
    return true;
  }, [setPanelState]);

  /**
   * PRD #1261 — offer a tie as a numbered choice, if it can be one.
   *
   * Only from `idle`: while dictating nothing reaches the Commands backend, so
   * no tie arises, and a D5 confirmation outranks a choice — with one open,
   * the tie stays the sentence it always was. A tie with no candidates (one
   * past {@link VOICE_CHOICE_MAX}, which Rust leaves empty) or more than the
   * cap is likewise only its sentence.
   */
  const offerChoice = useCallback((answer: VoiceResultDto, declared: VoiceContext) => {
    const outcome = answer.outcome;
    if (outcome.kind !== "param_ambiguous" || outcome.invoke === undefined || outcome.candidates === undefined) return;
    if (outcome.candidates.length === 0 || outcome.candidates.length > VOICE_CHOICE_MAX) return;
    if (panelStateRef.current.kind !== "idle" || confirmationRef.current) return;
    setPanelState({
      kind: "awaitingChoice",
      offer: { outcome: { ...outcome, invoke: outcome.invoke, candidates: outcome.candidates }, backend: answer.backend, declared, deadline: Date.now() + VOICE_CHOICE_WINDOW_MS },
    });
  }, [setPanelState]);

  /**
   * PR #1451 round 3, change 3 — run what choosing a numbered item runs: the
   * row a spoken name for it would have run, with the item as its param,
   * through the same dispatch and the same target checks. No backend was
   * asked anything, so no timing is shown.
   */
  const runNumbered = useCallback((entry: VoiceNumberedEntryDto, transcript: string, declared: VoiceContext, refusal: (lost: Lost) => string) => {
    const outcome = numberedOutcome(entry, transcript);
    /* PR #1451 round 3 review (Qodo) — held to the context the number was
       said in, like a resolved dispatch: an agent entry to the deck and the
       incarnation declared then. */
    const lost = dispatchLost(declared, current(), outcome.params);
    if (lost) {
      setProblem(refusal(lost));
      return;
    }
    setResult({ outcome, resolveMs: null, backend: NUMBERS_BACKEND });
    refusedRef.current = false;
    const dispatched = dispatchDeclared(outcome, declared);
    if (refusedRef.current) setResult(undefined);
    else if (!dispatched) setProblem(NOTHING_DISPATCHED);
    else if (dispatched.undo) setUndo({ run: dispatched.undo });
  }, [current, dispatchDeclared]);

  /**
   * PR #1451 round 3, change 3 — a numbered choice's entry, chosen: refused
   * when the screen or dialog moved on, or when the numbered list it was
   * offered from has changed since, and otherwise run as its item.
   */
  const dispatchNumbered = useCallback((offer: VoiceChoiceOffer, candidate: VoiceResolvedParamDto) => {
    const numbered = offer.numbered;
    if (!numbered) return;
    const entry = numbered.entries[offer.outcome.candidates.indexOf(candidate)];
    const lost = contextLost(offer.declared, current(), { answer: true });
    if (lost) {
      setProblem(choiceRefusal(lost));
      return;
    }
    if (entry === undefined || numberedRef.current?.().generation !== numbered.heard.generation) {
      setProblem(VOICE_NUMBERS_CHOICE_MOVED_ON);
      return;
    }
    runNumbered(entry, offer.outcome.transcript, offer.declared, choiceRefusal);
  }, [current, runNumbered]);

  /**
   * PR #1451 round 3, change 3 — a number that names several items: the same
   * number in several sections (round 4, D7), or an item showing it and
   * another item of its section whose name is or ends in it. Offered as the
   * numbered choice in the order Rust gave them; an entry is labelled by its
   * section ("Directory 3: scratch", "Mode 3: Review") when the choice spans
   * more than one. With a confirmation open, or more items than a choice
   * shows, it is only refused.
   */
  const offerNumbered = useCallback((utterance: string, heard: VoiceNumberedListDto, choices: VoiceNumberRefDto[], declared: VoiceContext) => {
    const found = choices.flatMap((choice) => {
      const entry = numberedEntry(heard, choice.section, choice.number);
      return entry ? [{ choice, entry }] : [];
    });
    const entries = found.map(({ entry }) => entry);
    const spansSections = new Set(found.map(({ choice }) => choice.section)).size > 1;
    const labelOf = ({ choice, entry }: (typeof found)[number]) => (spansSections ? `${SECTION_NOUNS[choice.section].title} ${choice.number}: ${entry.label}` : entry.label);
    const first = entries[0];
    const labels = found.map((item) => `“${labelOf(item)}”`).join(" or ");
    const sentence = `“${utterance}” could mean ${labels}.`;
    if (first === undefined || entries.length > VOICE_CHOICE_MAX || confirmationRef.current) {
      setProblem(`${sentence} Nothing ran.`);
      return;
    }
    const row = numberedOutcome(first, utterance);
    /* Each candidate's `value` is its section and number: unique, where two
       decks' agents can share an id and two sections a number. The item
       itself rides in `numbered`. */
    const candidates = found.map((item) => ({ ...numberedParam(item.entry, utterance), value: `${item.choice.section}:${item.choice.number}`, label: labelOf(item) }));
    const outcome: VoiceChoiceOffer["outcome"] = {
      kind: "param_ambiguous",
      transcript: utterance,
      action: row.action,
      invoke: row.invoke,
      param: candidates[0].name,
      spoken: utterance,
      matches: found.map(labelOf),
      candidates,
      params: [],
      sentence,
    };
    setResult({ outcome, resolveMs: null, backend: NUMBERS_BACKEND });
    setPanelState({
      kind: "awaitingChoice",
      offer: { outcome, backend: NUMBERS_BACKEND, declared, deadline: Date.now() + VOICE_CHOICE_WINDOW_MS, numbered: { heard, entries } },
    });
  }, [setPanelState]);

  /**
   * PR #1451 round 3, change 3 — answer `utterance` if it is a number, bare
   * or with its section ("directory 13", "select daemon 1" — round 4), said
   * about the numbered list on screen, locally: no Commands backend
   * call, Rust's `voice::numbers::answer` where the runtime has it and the
   * webview's port where it does not. Answers whether the utterance was
   * handled here; `false` means it is not a number (or nothing is numbered)
   * and is to be resolved as usual.
   *
   * The list is the one heard — as it stood when the user began to speak —
   * and it is compared with the list on screen both when the answer is
   * worked out and again when it comes back, so a number said about a list
   * that moved in either window is refused rather than read against the new
   * one.
   */
  const answerNumber = useCallback(async (utterance: string, ours: () => boolean): Promise<boolean> => {
    const heard = heardNumbers.current;
    const read = numberedRef.current;
    if (heard === undefined || read === undefined || !hasNumbered(heard)) return false;
    const declared = heardContext.current ?? current();
    setPhase("resolving");
    let verdict: VoiceNumberAnswerDto;
    try {
      const now = read().generation;
      verdict = answerVoiceNumber ? await answerVoiceNumber(utterance, heard, now) : answerNumberLocally(utterance, heard, now);
    } catch (cause) {
      if (ours()) setProblem(sentenceOf(cause));
      return true;
    }
    if (!ours()) return true;
    if (verdict.kind === "not_number") return false;
    if (verdict.kind !== "stale" && read().generation !== heard.generation) verdict = { kind: "stale" };
    if (verdict.kind === "stale") {
      setProblem(VOICE_NUMBERS_MOVED_ON);
      return true;
    }
    if (verdict.kind === "out_of_range") {
      setProblem(voiceNumberNotShown(verdict.number, verdict.section, verdict.elsewhere));
      return true;
    }
    if (verdict.kind === "not_numbered") {
      setProblem(voiceSectionNotNumbered(verdict.section, verdict.shown));
      return true;
    }
    /* The gate for acting on the answer at all, as for a resolved one. */
    const lost = contextLost(declared, current(), { answer: true });
    if (lost) setProblem(answerRefusal(lost));
    else if (verdict.kind === "ambiguous") offerNumbered(utterance, heard, verdict.choices, declared);
    else {
      const entry = numberedEntry(heard, verdict.section, verdict.number);
      if (entry === undefined) setProblem(voiceNumberNotShown(verdict.number, verdict.section));
      else runNumbered(entry, utterance, declared, answerRefusal);
    }
    return true;
  }, [answerVoiceNumber, current, offerNumbered, runNumbered, setPhase]);

  /**
   * PRD #1261 — run the ORIGINAL command with the chosen entry, once.
   *
   * No second resolve: the row and every other param are the ones Rust
   * resolved with the first utterance, and the entry is one of the offered
   * candidates exactly as offered. What stands between the offer and the run
   * is the layers a resolved dispatch already meets — {@link contextLost}
   * against the context declared with the first utterance (and, for an agent
   * entry, that agent's incarnation then), then the target's own re-checks,
   * which get the ORIGINAL directories and form — so an entry offered against
   * a listing, a dialog, a deck address or an agent that has since moved is
   * refused in that layer's words and nothing runs.
   *
   * Through the `refusedRef` path, so a refused entry renders only its
   * refusal and offers no Undo.
   */
  const dispatchChoice = useCallback((offer: VoiceChoiceOffer, candidate: VoiceResolvedParamDto) => {
    const mode = panelStateRef.current;
    if (mode.kind !== "awaitingChoice" || mode.offer !== offer) return;
    /* The window is wall-clock time, whatever the countdown has managed to
       show: an answer after the deadline expires the choice and runs nothing. */
    if (Date.now() >= offer.deadline) {
      closeChoice(VOICE_CHOICE_EXPIRED);
      return;
    }
    setPanelState(IDLE);
    forget();
    if (offer.numbered) {
      dispatchNumbered(offer, candidate);
      return;
    }
    const at = offer.outcome.candidates.findIndex((entry) => entry.value === candidate.value);
    const chosen = offer.outcome.candidates[at];
    /* The one place both a click and a spoken answer pass, so neither can run
       an entry — or open a stop confirmation for an agent — that the gate
       says has moved on since the first utterance was declared. An agent or
       orchestration entry names its agent by id on the selected deck, which
       alone cannot tell a same-id replacement apart. */
    const lost = dispatchLost(offer.declared, current(), [...(offer.outcome.params ?? []), ...(chosen ? [chosen] : [])]);
    if (lost) {
      setProblem(choiceRefusal(lost));
      return;
    }
    if (chosen === undefined) {
      setProblem(VOICE_CHOICE_REFUSED);
      return;
    }
    const outcome: Extract<VoiceOutcomeDto, { kind: "dispatch" }> = {
      kind: "dispatch",
      transcript: offer.outcome.transcript,
      action: offer.outcome.action,
      invoke: offer.outcome.invoke,
      params: [...(offer.outcome.params ?? []), chosen],
      /* Rust's own report for this entry; a runtime whose tie carried none
         (a test fake) gets a plain statement of the choice. */
      sentence: offer.outcome.reports?.[at] ?? `Chose ${chosen.label}.`,
    };
    /* No backend was asked anything for the answer, so no timing is shown. */
    setResult({ outcome, resolveMs: null, backend: offer.backend });
    refusedRef.current = false;
    const dispatched = dispatchDeclared(outcome, offer.declared);
    if (refusedRef.current) setResult(undefined);
    else if (!dispatched) setProblem(NOTHING_DISPATCHED);
    else if (dispatched.undo) setUndo({ run: dispatched.undo });
  }, [closeChoice, current, dispatchDeclared, dispatchNumbered, forget, setPanelState]);

  /*
    PRD #1261 — the choice's countdown, and its expiry, which runs nothing.
    Keyed on the offer, so a new offer restarts it and a closed one stops it.
  */
  const offered = panelState.kind === "awaitingChoice" ? panelState.offer : undefined;
  const choiceShown = offered !== undefined;
  useEffect(() => { choiceChanged.current?.(choiceShown); }, [choiceShown]);
  useEffect(() => () => { choiceChanged.current?.(false); }, []);
  useEffect(() => {
    if (!offered) {
      setChoiceIn(undefined);
      return;
    }
    /* Counted from the offer's deadline rather than from the ticks seen, so a
       delayed callback shows the time actually left. */
    const remaining = () => Math.ceil((offered.deadline - Date.now()) / VOICE_DICTATION_TICK_MS);
    setChoiceIn(Math.max(1, remaining()));
    const timer = window.setInterval(() => {
      const left = remaining();
      if (left > 0) {
        setChoiceIn(left);
        return;
      }
      window.clearInterval(timer);
      closeChoice(VOICE_CHOICE_EXPIRED);
    }, VOICE_DICTATION_TICK_MS);
    return () => window.clearInterval(timer);
  }, [closeChoice, offered]);

  /* PRD #1261 — a D5 confirmation outranks a pending choice: opening one
     closes the choice, and it does not come back when the confirmation is
     answered. A stop CHOSEN from a list has already closed it by the time its
     confirmation opens. */
  useEffect(() => {
    if (confirmationOpen) closeChoice(VOICE_CHOICE_CLOSED);
  }, [closeChoice, confirmationOpen, panelState]);

  /**
   * Open the microphone for the next utterance.
   *
   * The same call whether voice was just switched on or an utterance has just
   * finished, which is what *continuous* means here: the cycle ends by starting
   * again, and only the button ends it for good. A refusal at this point turns
   * voice visibly off rather than leaving a control that says it is on over a
   * device that is not.
   */
  const listen = useCallback(async (ours: () => boolean) => {
    if (!voiceStart) return;
    setPhase("opening");
    try {
      await voiceStart();
      if (!ours()) return;
      reader.startSegment();
      hearNumbers();
      setPhase("listening");
    } catch (cause) {
      if (!ours()) return;
      setPhase("idle");
      setOn(false);
      setProblem(sentenceOf(cause));
    }
  }, [hearNumbers, setOn, setPhase, voiceStart]);

  /**
   * One transcript, resolved and — if it is a command that runs here — run.
   *
   * The screen is stated immediately before the resolve rather than from an
   * effect: a declaration that lagged a navigation would validate this utterance
   * against the screen the user just left, which is the `unavailable` outcome
   * misfiring in the one direction nobody would notice.
   */
  const resolveOne = useCallback(async (utterance: string, ours: () => boolean, alone?: string) => {
    if (!resolveVoice) return;
    /* PRD #1261 — a pending choice is answered first, locally, with no
       Commands backend call. Declared first, as a resolve is, so the answer
       is checked against what is on screen now. A non-answer closes the
       choice, says so, and falls through to be resolved like any other
       utterance. */
    const waiting = panelStateRef.current;
    /* PRD #1497 D8 — heard while the app was speaking: nothing is answered
       locally here either, and the resolve below drops it unless it was
       "stop" or "quiet". */
    if (waiting.kind === "awaitingChoice" && !overlappedRef.current) {
      const { offer } = waiting;
      setPhase("resolving");
      let verdict: VoiceChoiceAnswerDto;
      try {
        declareVoiceScreen?.(screenRef.current, directoriesRef.current?.(), newAgentRef.current?.(), endpointsRef.current?.());
        /* A choice between numbered items is answered by the webview's rule:
           its entries may be other decks' agents, which Rust's liveness
           check reads against the selected deck alone. What stands behind
           it is the numbered list's own generation, at dispatch. */
        verdict = answerVoiceChoice && offer.numbered === undefined
          ? await answerVoiceChoice(utterance, offer.outcome.action, offer.outcome.candidates)
          : answerChoiceLocally(utterance, offer.outcome.candidates);
      } catch (cause) {
        if (ours() && closeChoice()) setProblem(sentenceOf(cause));
        return;
      }
      if (!ours()) return;
      /* Clicked, cancelled or expired while the answer was worked out. */
      if (panelStateRef.current !== waiting) return;
      if (verdict.kind === "selected") {
        dispatchChoice(offer, verdict.candidate);
        return;
      }
      if (verdict.kind === "cancelled") {
        closeChoice(VOICE_CHOICE_CANCELLED);
        return;
      }
      if (verdict.kind === "refused") {
        const colliding = collidingChoiceEntry(utterance, offer.outcome.candidates);
        closeChoice(colliding === undefined ? VOICE_CHOICE_REFUSED : voiceChoiceCollision(colliding));
        return;
      }
      closeChoice(VOICE_CHOICE_CLOSED);
    }
    /* PR #1451 round 3, change 3 — a number said about the numbered list on
       screen, bare or with its section (round 4), is answered here, locally, with no Commands backend call. Not
       while typing, where a number is the user's words. Anything else falls
       through to be resolved. */
    if (panelStateRef.current.kind === "idle" && !overlappedRef.current && await answerNumber(utterance, ours)) return;
    if (!ours()) return;
    /* The context this utterance is JUDGED against, declared once and held
       for the round trip: `unavailable` means "not on that screen", so an
       outcome is only an answer about the context declared with it. */
    const declared = current();
    /* PRD #1260 — while the dictation mode is on, its target rides the
       declaration, and Rust answers the utterance with no Commands backend
       call at all. */
    const mode = panelStateRef.current;
    const declaredDictation = mode.kind === "dictating" ? { deckId: mode.target.deckId, agentId: mode.target.agentId } : undefined;
    setPhase("resolving");
    try {
      declareVoiceScreen?.(declared.screen, declared.directories, declared.newAgent, endpointsRef.current?.(), declaredDictation, declaredReading());
      const answer = await resolveVoice(utterance);
      // Abandoned, or replaced by a later utterance. Say nothing and run
      // nothing: voice is off, or this belongs to the cycle that replaced it.
      if (!ours()) return;
      /* The gate for acting on the answer at all: the screen, the New agent
         dialog (a different MOUNT is a different dialog even when both
         declarations look alike — Qodo on PR #1235) and the typing mode it
         was judged in. An answer judged inside a mode that has since ended is
         dropped silently, because the mode's exit already said why. What the
         answer then does to a pane is gated where it does it.

         PRD #1260/#1261 review, round 5 — a dispatch that names an agent is
         also held to that agent's incarnation and the selected deck as
         declared, exactly as a chosen entry is: a stop resolved before a
         same-id replacement or a deck change must not open the confirmation
         for whatever agent carries that id now. */
      const lost = dispatchLost(declared, current(), answer.outcome.kind === "dispatch" ? answer.outcome.params : []);
      if (lost) {
        if (lost.code !== "mode" || declaredDictation === undefined) setProblem(answerRefusal(lost));
        return;
      }
      /* PR #1451 — the first half of a command cut by a pause matches
         nothing on its own. Rather than say so at once, wait a moment for the
         rest; the next utterance joins it (`takeUtterance`). Not while
         typing, where every utterance is the user's own words.

         A JOINED sentence (`alone` is the new words in it) is never held
         again, so joins cannot chain. If it still matches nothing, the new
         words are tried on their own — the first half may have been words
         that were never meant as a command, and the user's next command must
         not be swallowed by them (Qodo on the PR). If those match nothing
         either, the joined sentence is the report, since it is everything the
         user said. */
      /* PR #1451 round 3, change 4 — an utterance nothing showing answered,
         that names an item a paged list shows on another page: said at once,
         with that page, rather than held or reported as matching nothing.
         Voice acts only on what is on screen, so nothing runs. */
      const pager = pagesRef.current?.();
      const offPage = declaredDictation === undefined && pager && (HOLDABLE_OUTCOMES.has(answer.outcome.kind) || answer.outcome.kind === "param_unresolved")
        ? offPageNamed(utterance, pager.elsewhere)
        : undefined;
      if (pager && offPage) {
        clearHeld();
        setResult({ ...answer, outcome: { ...answer.outcome, sentence: `Heard: “${utterance}” — ${offPageSentence(offPage.label, offPage.page, pager.page)}` } });
        return;
      }
      let final = answer;
      if (declaredDictation === undefined && HOLDABLE_OUTCOMES.has(answer.outcome.kind)) {
        if (alone === undefined) {
          clearHeld();
          held.current = { answer, transcript: utterance, continued: false };
          heldTimer.current = window.setTimeout(() => {
            heldTimer.current = undefined;
            if (held.current && !held.current.continued) reportHeld();
          }, VOICE_JOIN_WINDOW_MS);
          return;
        }
        declareVoiceScreen?.(declared.screen, declared.directories, declared.newAgent, endpointsRef.current?.(), declaredDictation, declaredReading());
        const own = await resolveVoice(alone);
        if (!ours()) return;
        if (!HOLDABLE_OUTCOMES.has(own.outcome.kind)) {
          const ownLost = dispatchLost(declared, current(), own.outcome.kind === "dispatch" ? own.outcome.params : []);
          if (ownLost) {
            setProblem(answerRefusal(ownLost));
            return;
          }
          final = own;
          setCapture({ sentence: heardSentence(alone), transcript: alone });
        }
      }
      setResult(final);
      if (final.outcome.kind === "param_ambiguous") {
        offerChoice(final, declared);
      } else if (final.outcome.kind === "dispatch") {
        refusedRef.current = false;
        const dispatched = dispatchDeclared(final.outcome, declared);
        /* What the dispatch reached refused it, in its own sentence, so the
           outcome's sentence — "Showing …", written before anything ran — is
           now false, and an Undo would reverse nothing. The refusal is the
           whole report, exactly as for a screen that moved on (Greptile on
           PR #1340). Every `reportRefused` caller reports synchronously from
           inside its row's `run`, which is what lets the flag be read here. */
        if (refusedRef.current) setResult(undefined);
        else if (!dispatched) setProblem(NOTHING_DISPATCHED);
        else if (dispatched.undo) setUndo({ run: dispatched.undo });
      }
    } catch (cause) {
      if (ours()) setProblem(sentenceOf(cause));
    }
  }, [answerNumber, answerVoiceChoice, clearHeld, closeChoice, current, declareVoiceScreen, dispatchChoice, dispatchDeclared, offerChoice, reportHeld, resolveVoice, setPhase]);

  /**
   * One whole utterance: take it, transcribe, resolve, listen again.
   *
   * Serial on purpose — one utterance is worked on at a time, in order.
   * Overlapping the WORK would let a second command resolve against a screen
   * the first one is still changing, and the {@link SCREEN_MOVED_ON} guard
   * would then be refusing the user's own sentences. The RECORDING does
   * overlap (PR #1451): the microphone stays open while this one is worked on,
   * so what the user says meanwhile is the start of the next utterance rather
   * than lost — the next `listen` carries on with it.
   */
  const takeUtterance = useCallback(async (capped = false) => {
    if (!voiceStop) return;
    const ours = claim();
    /* A new utterance has arrived, so whatever was about to be sent is no
       longer the whole of what the user said. The poll below cancels on SPEECH,
       which covers the sentence still being spoken; this covers the gap between
       that sentence ending and its text being appended, during which the poll
       returns early because the phase is no longer `listening`.

       Unconditional, where it used to ask whether dictation was on: there is no
       mode to ask about any more, and a pending send is a pending send whatever
       the next utterance turns out to be. */
    cancelPendingSend();
    setPhase("transcribing");
    /* PRD #1497 D8 — the segment ends here, and the next one starts. */
    overlappedRef.current = reader.takeSegment();
    try {
      const transcription = await voiceStop();
      if (!ours()) return;
      /* PR #1451's hand test — a segment with no words in it says NOTHING.
         With voice on, most of these are a keyboard, a breath or the room
         rather than anybody speaking, and a sentence about each one (it used
         to carry milliseconds and RMS levels) buried the report the user
         was reading. So the last report stays exactly as it was and the
         microphone simply reopens. `forget()` waits until there is something
         to say, which is why it is here and not before the stop. */
      /* PR #1451 — an answer held for the rest of its sentence. Words that
         follow are joined to it and the whole is worked out as one; a sound
         that was not speech, after speech was heard, means nothing more is
         coming; and a failure replaces it. */
      const waiting = held.current;
      if (waiting && transcription.outcome.kind === "heard") {
        clearHeld();
        const joined = joinUtterances(waiting.transcript, transcription.outcome.transcript);
        forget();
        setCapture({ sentence: heardSentence(joined), transcript: joined });
        await resolveOne(joined, ours, transcription.outcome.transcript);
        if (!ours()) return;
        await listen(ours);
        return;
      }
      if (waiting && transcription.outcome.kind === "silent") {
        if (waiting.continued) reportHeld();
      } else if (waiting) {
        clearHeld();
      }
      if (transcription.outcome.kind !== "silent") {
        forget();
        /* PRD #1260 — a capped segment reaches here only while dictating,
           where it is the user's own words rather than a runaway command. */
        if (capped) setProblem(VOICE_CAP_TYPED);
        setCapture({
          sentence: transcription.outcome.sentence,
          transcript: transcription.outcome.kind === "heard" ? transcription.outcome.transcript : undefined,
        });
      }
      /* **One path, where there used to be a fork.** An utterance is resolved,
         full stop. Whether it ends up typed into an agent is the resolver's
         answer — `dictate_to_agent` is a row like any other — rather than a
         mode this surface was holding. */
      if (transcription.outcome.kind === "heard") await resolveOne(transcription.outcome.transcript, ours);
    } catch (cause) {
      if (!ours()) return;
      forget();
      setProblem(sentenceOf(cause));
    }
    if (!ours()) return;
    await listen(ours);
  }, [cancelPendingSend, claim, clearHeld, forget, listen, reportHeld, resolveOne, setPhase, voiceStop]);

  /**
   * The capped utterance: thrown away unheard, and said so — see
   * {@link VOICE_CAP_DISCARDED} — unless nobody spoke in it, which is dropped
   * without a word (PR #1451): a microphone left open in a quiet room, or
   * over a keyboard, reaches the cap with no speech in it every thirty
   * seconds, and saying so each time is noise about nothing the user did.
   */
  const discardCapped = useCallback(async (quietly = false) => {
    const ours = claim();
    setPhase("opening");
    /* A held answer whose continuation ran to the cap: the sentence is gone
       either way, and the cap's own report says why. */
    if (held.current?.continued) clearHeld();
    if (!quietly) {
      forget();
      setProblem(VOICE_CAP_DISCARDED);
    }
    // `voiceCancel` rather than `voiceStop`: stopping would transcribe it,
    // which is the whole of what this path exists to avoid.
    try { await voiceCancel?.(); } catch { /* idempotent and never refused */ }
    if (!ours()) return;
    await listen(ours);
  }, [claim, clearHeld, forget, listen, setPhase, voiceCancel]);

  /**
   * One poll: ask what the microphone is doing, and act if the utterance ended.
   *
   * `capped` is checked before `done` because a capped segment is both, and the
   * two answers differ: one is transcribed and one is thrown away.
   */
  const poll = useCallback(async () => {
    if (!voiceStatus || phaseRef.current !== "listening") return;
    let status;
    try {
      status = await voiceStatus();
    } catch {
      // A status read that fails says nothing about the device. The next one is
      // a quarter second away.
      return;
    }
    // Re-read AFTER the await: voice may have been turned off, or a cycle may
    // already be running, in the round trip this answer took.
    if (!onRef.current || phaseRef.current !== "listening") return;
    /* PR #1451 round 3, change 3 — nobody has spoken into this recording yet,
       so what is on screen now is still what the coming words are about. */
    if (status.speech === false) hearNumbers();
    /*
      PRD #802 D6 — *"a visible countdown the user can cancel by continuing to
      speak"*, and this is the seam that notices the speaking.

      `speech` is the capture session's own latched answer to *has anybody
      spoken since this recording opened*, computed on the device thread by the
      same detector that finds utterance boundaries. Nothing else on this status
      could answer it: `capturedMs` counts audio and grows in a silent room, and
      `state: "done"` arrives only after `SILENCE_HOLD` past the END of a
      sentence — which for a long one is well after the countdown would have
      fired, submitting half an instruction while the user was still saying the
      rest of it.

      Cancelled rather than reset: the send is re-armed when the text of this
      new utterance is appended, which is the moment there is something new to
      send.
    */
    if (status.speech && (deferredSubmit.current || (pendingRef.current && sendTimer.current !== undefined))) cancelPendingSend();
    /* PR #1451 — the nudge to send: speaking takes it down, and a pause with
       words still unsent starts its clock again. The clock is otherwise started
       by the write that typed them, so this re-arms it only after a pause that
       followed speech which typed nothing. */
    if (status.speech && held.current && !held.current.continued) {
      held.current.continued = true;
      if (heldTimer.current !== undefined) window.clearTimeout(heldTimer.current);
      heldTimer.current = undefined;
    }
    if (status.speech) stopNudge();
    else if (unsent.current && nudgeTimer.current === undefined && !nudgeShown.current && panelStateRef.current.kind === "dictating") armNudge();
    if (status.capped) {
      /* PR #1451 — nobody spoke in it (`speech` is the same rule the
         transcription gate applies), so there is nothing to type and nothing
         to say: dropped quietly, typing mode or not. `undefined` is a runtime
         that does no speech detection, which keeps the behaviour below. */
      if (status.speech === false) await discardCapped(true);
      /* PRD #1260 — while dictating, a capped segment is typed rather than
         thrown away: a dictated paragraph is not a one-to-four-word command. */
      else if (panelStateRef.current.kind === "dictating") await takeUtterance(true);
      else await discardCapped();
      return;
    }
    if (status.state === "done") await takeUtterance();
  }, [armNudge, cancelPendingSend, discardCapped, hearNumbers, stopNudge, takeUtterance, voiceStatus]);

  /*
    The poll, reached through a ref so the interval below survives a re-render.
    Its dependencies include `onDispatch`, which the host rebuilds freely — an
    interval keyed on that identity would be torn down and recreated on every
    commit, and a timer that restarts before it fires never fires at all.
  */
  const latest = useRef(poll);
  useEffect(() => { latest.current = poll; }, [poll]);

  /*
    Running for exactly as long as voice is on, rather than for as long as the
    device is open: the ticks that land mid-cycle return immediately, and the
    one interval means there is no window in which the surface is on and nothing
    is watching.
  */
  useEffect(() => {
    if (!on || !voiceStatus) return;
    const timer = window.setInterval(() => { void latest.current(); }, VOICE_STATUS_POLL_MS);
    return () => window.clearInterval(timer);
  }, [on, voiceStatus]);

  /**
   * Turn voice on: check there is something to listen with, then listen.
   *
   * Availability is read at the press rather than cached from mount, so a user
   * who has just chosen a backend gets a microphone instead of the instructions
   * they were shown a moment ago. A status read that FAILS falls through to the
   * start rather than refusing here — `desktop_voice_start` re-checks the
   * backend itself and refuses with its own sentence, so the honest answer comes
   * from the call that actually tried.
   */
  const turnOn = useCallback(async () => {
    const ours = claim();
    forget();
    /* This press supersedes the mount reconcile, so nothing is still being
       waited for: from here the button reports this press's own progress. */
    setKnown(true);
    setPhase("opening");
    /*
      A runtime carrying `resolveVoice` and no capture verbs is representable —
      every voice member of `DeckRuntimeState` is optional, and a test runtime
      does exactly this. Without a start and a stop there is no voice control to
      turn on, so it reports the same instruction an unconfigured backend gets
      rather than latching ON over a microphone that will never open. The
      TRIGGER still renders, because `resolveVoice` is what decides that: a
      button that says how to fix the thing it cannot do is the owner's
      requirement, and silence is not.
    */
    if (!voiceStart || !voiceStop) {
      setPhase("idle");
      setProblem(VOICE_UNAVAILABLE);
      return;
    }
    if (voiceStatus) {
      /* `undefined` is the read that FAILED, and it falls through to the start
         for the reason above — not the same thing as a read that answered. */
      let status: VoiceStatusDto | undefined;
      try {
        status = await voiceStatus();
      } catch {
        status = undefined;
      }
      if (!ours()) return;
      if (status && !status.available) {
        setPhase("idle");
        setProblem(VOICE_UNAVAILABLE);
        return;
      }
      /*
        The mount reconcile again, on the path that can race it: a session Rust
        is still holding refuses a start, so without this the press would report
        "a recording is already running" and leave the button off over a live
        device. Doing it here as well as at mount means a press that arrives
        DURING the reconcile — and supersedes it — still gets a microphone.
      */
      if (status && stillHeld(status.state)) {
        try {
          await cancelRef.current?.();
        } catch (cause) {
          /* This used to fall through to `voiceStart`, which `accepts_start`
             refuses while the session is held — so the press ended at `off`
             with an error sentence beside it, over a device that is still
             open. An error sentence is better than silence and it is still the
             wrong word on the button, and it is what made
             `docs/develop/desktop-gui.md`'s "every unknown resolves to not
             released" false on this path. */
          if (!(await releasedAfterRefusal())) {
            if (!ours()) return;
            setPhase("unreleased");
            setProblem(`${VOICE_RELEASE_REFUSED} ${sentenceOf(cause)}`);
            return;
          }
        }
        if (!ours()) return;
      }
    }
    setOn(true);
    await listen(ours);
  }, [claim, forget, listen, releasedAfterRefusal, setOn, setPhase, voiceStart, voiceStatus, voiceStop]);

  /**
   * Turn voice off: abandon the pipeline, then release the device — and do not
   * say it is off until Rust says it is.
   *
   * `voiceCancel` rather than `voiceStop`, always: a user switching voice off
   * did not ask for the half-sentence in the buffer to be transcribed, and
   * charging them a backend call for it would be the opposite of what the press
   * meant. Cancel is idempotent and never refused precisely so this does not
   * have to know which state the device is in.
   *
   * **The release is AWAITED, which it was not** (PRD #802's audit). This used
   * to set the button off and fire `voiceCancel` without waiting, swallowing
   * any rejection — and that is not a scheduling instant: the command runs the
   * teardown on a blocking thread and `CpalStream::drop` joins the device
   * thread, so the sole privacy indicator read *Voice off* while the device was
   * still closing, and read it permanently if the call failed. So the button
   * now says `stopping` until the acknowledgement arrives, and says the
   * microphone may still be open if it never does.
   */
  const turnOff = useCallback(async () => {
    /* The non-voice escape from a pending send, and the one that works when
       nothing is being heard correctly. Cleared before the release rather than
       after, so a pending send cannot fire during it. */
    cancelPendingSend();
    setPending(undefined);
    clearHeld();
    /* PRD #1260 — voice off ends the dictation mode too, sending nothing. The
       report is the release's own, so the mode ends without a sentence. */
    setPanelState(IDLE);
    /* PRD #1497 D11 — and reading, which says "Reading off." aloud. */
    void reader.mode.voiceOff();
    /* The webview half of the same release. `voiceCancel` frees the DEVICE;
       this frees the pipeline behind it, which the device has no say over — a
       transcription or a resolution already handed to a backend arrives
       whatever the microphone does. The claim doubles as the abandon, and is
       what keeps this from writing state over a panel that has unmounted or
       over a later press. */
    const ours = claim();
    setPhase("stopping");
    try {
      await voiceCancel?.();
    } catch (cause) {
      if (!ours()) return;
      if (await releasedAfterRefusal()) {
        if (!ours()) return;
        // The call failed and the device is gone regardless, so `off` is the
        // truthful word — with the rejection still reported rather than
        // swallowed, because something did go wrong.
        setOn(false);
        setPhase("idle");
        setKnown(true);
        setProblem(sentenceOf(cause));
        return;
      }
      if (!ours()) return;
      setPhase("unreleased");
      setProblem(`${VOICE_RELEASE_REFUSED} ${sentenceOf(cause)}`);
      return;
    }
    if (!ours()) return;
    setOn(false);
    setPhase("idle");
    /* Rust answered, so this panel has heard from the microphone whether or not
       it was ever the one that opened it — `turnOff` is now reachable straight
       off a mount reconcile that could not release the device (the click
       handler routes `unreleased` here), and without this the button would fall
       back to `Voice…` after a release that actually succeeded. */
    setKnown(true);
  }, [cancelPendingSend, claim, clearHeld, releasedAfterRefusal, setOn, setPanelState, setPending, setPhase, voiceCancel]);

  /**
   * What the discovery overlay is showing, or `undefined` for closed
   * (PRD #802 D7).
   *
   * One piece of state for three situations rather than three booleans: an
   * empty object is *open and still asking*, `commands` is the answer, and
   * `problem` is the refusal. `undefined` is the only closed value, which is
   * what makes "is it open" a single question with a single answer.
   */
  const [vocabulary, setVocabulary] = useState<{ commands?: VoiceCommandDto[]; problem?: string }>();
  /*
    The overlay's own claim counter, deliberately NOT the pipeline's.
    `claim()` abandons whatever utterance is in flight, and opening a list must
    not cancel the command the user is in the middle of saying. This orders
    overlay answers against each other and against a close, and nothing else.
  */
  const vocabularyRequest = useRef(0);
  const closeVocabulary = useCallback(() => {
    vocabularyRequest.current += 1;
    setVocabulary(undefined);
  }, []);
  const showVoiceCommands = useCallback(() => {
    const mine = ++vocabularyRequest.current;
    setVocabulary({});
    if (!voiceCommands) return;
    /* `screenRef` rather than the prop: this runs from a dispatch, which is a
       promise continuation, and the prop captured when the callback was built
       may be a screen the user has already left. */
    void voiceCommands(screenRef.current, directoriesRef.current?.(), newAgentRef.current?.()).then(
      (commands) => { if (vocabularyRequest.current === mine) setVocabulary({ commands: showDeck ? commands : commands.filter((command) => command.id !== OPEN_DECK_COMMAND) }); },
      (cause) => { if (vocabularyRequest.current === mine) setVocabulary({ problem: sentenceOf(cause) }); },
    );
  }, [showDeck, voiceCommands]);

  /*
    Escape closes it. The overlay is the one thing this surface puts over the
    screen, so it is the one thing that needs a dismissal that is not a click —
    and a user who opened it by saying "what can I say?" has their hands free.
  */
  useEffect(() => {
    if (!vocabulary) return;
    const onKeyDown = (event: KeyboardEvent) => { if (event.key === "Escape") closeVocabulary(); };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [vocabulary, closeVocabulary]);

  /**
   * The Voice button's own action, as the registry sees it.
   *
   * Fire-and-forget rather than awaited, because a registry `run` returns
   * nothing by design — every entry is a control's click handler, and a click
   * handler does not report back. The truth about the release still reaches the
   * user: `turnOff` owns the `stopping` / `unreleased` indicator and the
   * refusal sentence, exactly as it does for a press.
   */
  const stopVoice = useCallback(() => { void turnOff(); }, [turnOff]);
  /** Say that what a dispatch reached refused, in its own sentence (PRD #1223). */
  const reportRefused = useCallback((reason: string) => {
    refusedRef.current = true;
    setProblem(reason);
  }, []);
  /**
   * Type one utterance's words into the open agent's prompt, then start the
   * countdown to a send (PRD #802 D6, rebuilt).
   *
   * **The text on the target is the user's own, and this file does not choose
   * it.** It was sliced out of the transcript by `voice::dictation` after the
   * marked boundary had been verified against that same transcript, so the only
   * thing left to do to it here is {@link dictationText}'s terminal-safety
   * scrub — which is identity for anything a transcriber produces.
   *
   * The label falls back to the agent id, which is what the surface has when a
   * dispatch carried no resolved label. Naming it badly is better than naming
   * it nothing: the countdown line has to say WHOSE prompt is about to be sent
   * to, and an unnamed one is the case where a user most needs to check.
   *
   * A failure ends the pending send rather than leaving a countdown over words
   * that never arrived — an agent's prompt this surface believes it has typed
   * into and has not is the silent failure it must not have.
   */
  /**
   * PRD #1541 — write one utterance's words into `aim`'s prompt and note them
   * as voice's, so "scratch that" can take them back out. Behind a running
   * prompt command they wait their turn ({@link onPane}), and are then written
   * only if the typing mode and the pane they were said for still stand. A
   * trailing send is noted as sent instead: there is nothing to scratch once
   * Enter is on its way.
   */
  const writeTyped = useCallback((aim: Pending, typed: string, sends: boolean): Promise<void> => {
    const generation = modeGeneration.current;
    return onPane(aim, "write", async () => {
      if (modeGeneration.current !== generation) throw new PaneMoved("typing mode changed");
      const lost = paneLost(aim);
      if (lost) throw new PaneMoved(lost.why);
      const record = promptFor(aim, incarnationOf(aim));
      if (sends) emptyRecord(record, false, PROMPT_EMPTIED.sent);
      const target = { deckId: aim.deckId, agentId: aim.agentId };
      const written = sendTerminalInput(target, typed);
      const write: PromptWrite = { text: typed, sent: notePaneWrite(target, typed.length, written) };
      if (!sends) record.writes.push(write);
      try {
        await written;
      } catch (cause) {
        resetPrompt(aim, false, PROMPT_EMPTIED.failed);
        throw cause;
      }
    });
  }, [incarnationOf, notePaneWrite, onPane, paneLost, promptFor, resetPrompt, sendTerminalInput]);
  const typeIntoAgent = useCallback((target: VoiceDispatchTarget) => {
    const mode = panelStateRef.current;
    /* PRD #1260 — while dictating, the words go to the mode's own target and
       NO countdown is armed: in a mode the user is, by definition, going to
       say more, and sending is "send it" or their own Enter. */
    const aim: Pending = mode.kind === "dictating"
      ? mode.target
      : { deckId: target.deckId, agentId: target.agentId, label: target.agentLabel ?? target.agentId };
    /* The gate, before a single character is written: the pane on screen now
       must be the one this utterance was declared on, showing the same agent,
       with no confirmation open. A refusal is reported in place of the
       dispatch's own "Typed …" (the `refusedRef` path), and ends the mode if
       one was on. */
    const lost = paneLost(aim);
    if (lost) {
      if (mode.kind !== "dictating") reportRefused(answerRefusal(lost));
      else if (endDictation(lost.why)) refusedRef.current = true;
      return;
    }
    /* Nothing to type. Rust refuses an empty remainder before it ever becomes a
       dispatch, so this is the residual — text that was nothing but control
       characters — and arming a send for it would press Enter on a prompt
       nobody added to. */
    const typed = dictationText(target.text ?? "");
    if (typed === "") return;
    if (mode.kind === "dictating") {
      /* The mode instance the words were typed for. Every exit and entry bumps
         it, and a same-id replacement ends the mode, so it names the pane's
         incarnation as well. */
      const generation = modeGeneration.current;
      /* PR #1451 round 3 — the utterance ended with a separate send sentence
         ("… Send it."), which Rust left out of `typed`: Enter follows these
         words through the same guarded path as a spoken "send it", so it is
         pressed only once they have landed. */
      const sends = target.thenSubmit === true;
      const write = writeTyped(aim, typed, sends);
      trackWrite(write);
      void write.then(
        /* PR #1451 — words are in the prompt and unsent: start the pause
           clock for the nudge to send. A write from a mode that has since
           ended arms nothing, and neither does one about to be sent. */
        () => {
          if (modeGeneration.current !== generation || sends) return;
          unsent.current = true;
          armNudge();
        },
        /* A terminal that refuses once will refuse every utterance after it,
           so the mode ends with the refusal rather than repeating it. A
           failure from a mode that has since ended is dropped: it must not end
           a mode entered after it, nor overwrite that mode's report. */
        (cause) => {
          if (modeGeneration.current !== generation) return;
          setPanelState(IDLE);
          /* The dispatch's "Typed …" sentence is now false: the terminal
             refused the words it reports (Qodo on PR #1451). */
          setResult(undefined);
          setProblem(sentenceOf(cause));
        },
      );
      if (sends) {
        stopNudge(true);
        submitAfterWrites(aim);
      }
      return;
    }
    cancelPendingSend();
    const epoch = sendEpoch.current;
    sending.current = { aim, declared: dispatching.current ?? current() };
    const write = writeTyped(aim, typed, false);
    trackWrite(write);
    void write.then(
      () => {
        /* Called off while the words were being written — see `sendEpoch` —
           or the gate no longer holds now that they are. */
        if (sendEpoch.current !== epoch || callOffLostSend()) return;
        setPending(aim);
        armSend(aim, epoch);
      },
      (cause) => {
        if (sendEpoch.current !== epoch) return;
        /* Nothing was typed, so there is no send left for a later context
           change to call off — and no "what was typed stays" report to put
           over this failure (review round 5). */
        sending.current = undefined;
        setPending(undefined);
        setProblem(sentenceOf(cause));
      },
    );
  }, [armNudge, armSend, callOffLostSend, cancelPendingSend, current, endDictation, paneLost, reportRefused, setPanelState, setPending, stopNudge, submitAfterWrites, trackWrite, writeTyped]);

  /**
   * Press Enter in the open agent's prompt, because the user said to.
   *
   * The third way to send, beside the countdown and the user's own keyboard.
   * It cancels the countdown first: a submit that raced its own timer would
   * press Enter twice, and the second one lands in whatever the agent printed
   * in between. Then the gate, as for typing.
   */
  const submitAgentPrompt = useCallback((target: VoiceDispatchTarget) => {
    cancelPendingSend();
    setPending(undefined);
    stopNudge(true);
    const lost = paneLost(target);
    if (lost) {
      reportRefused(answerRefusal(lost));
      return;
    }
    /* PRD #1541 — what voice typed is on its way out, so there is nothing
       left to scratch; the prompt is known to be empty only once the Enter
       has actually been pressed (`submitDictation`). */
    resetPrompt(target, false, PROMPT_EMPTIED.sent);
    /* After the words this Enter is meant to send. */
    submitAfterWrites({ deckId: target.deckId, agentId: target.agentId, label: target.agentLabel ?? target.agentId });
  }, [cancelPendingSend, paneLost, reportRefused, resetPrompt, setPending, stopNudge, submitAfterWrites]);

  /**
   * PRD #1541 — the checks every prompt command makes before it writes
   * anything: the pane it was said for is still the one on screen
   * ({@link paneLost}), the deck served keys for its agent, and no other
   * prompt command (or a clear's Undo) is still waiting or running for that
   * pane — one at a time, so commands said in a burst cannot pile up behind a
   * slow one. Answers what the command needs, or reports the refusal and
   * answers `undefined`.
   */
  const promptCommand = useCallback((target: VoiceDispatchTarget, command: "interrupt" | "clear" | "scratch") => {
    const aim: AgentAddress = { deckId: target.deckId, agentId: target.agentId };
    const lost = paneLost(aim);
    const shown = paneRef.current;
    if (lost || !shows(shown, aim)) {
      reportRefused(lost ? answerRefusal(lost) : voiceNothingRan("the pane closed"));
      return undefined;
    }
    if (!shown.promptKeys) {
      reportRefused(voiceNoPromptKey(shown.agentLabel ?? shown.label, command));
      return undefined;
    }
    if ((paneLines.current.get(lineKey(aim, shown.spawnedAtMs))?.commands ?? 0) > 0) {
      reportRefused(voicePromptCommandPending(shown.label));
      return undefined;
    }
    return {
      aim,
      keys: shown.promptKeys,
      label: shown.label,
      turn: shown.turn,
      record: promptFor(aim, shown.spawnedAtMs),
      /* What the command's later writes are held to: the utterance's own
         declaration, as for every pane write, and the typing mode it was
         said in. */
      declared: dispatching.current ?? current(),
      generation: modeGeneration.current,
      reported: reportGeneration.current,
    };
  }, [current, paneLost, promptFor, reportRefused]);
  /**
   * PRD #1541 — run a prompt command's writes on `aim`'s line, each held to
   * the declaration, to the typing mode it was said in and to the command's
   * own `holds` (the turn still working, the prompt still at its revision) —
   * checked before each step, and again by the bridge immediately before each
   * write is handed to the daemon ({@link guardedSend}). So ending typing mode
   * ("typing off", Stop typing, Voice off) calls off a command still waiting,
   * even one whose write is queued behind the user's own typing, and stops a
   * paced one before its next write. Reports how it ended, unless the user has
   * said something since, whose report this must not cover.
   */
  const runPromptCommand = useCallback((
    run: { aim: AgentAddress; declared: VoiceContext; generation: number; reported: number },
    writes: () => AsyncGenerator<string | number, void, void>,
    done: () => string,
    stopped: (why: string) => string,
    after?: { onDone?: () => void; onFail?: (cause: unknown) => void; holds?: () => string | undefined; partway?: string },
  ) => {
    let delivered = 0;
    const check = () => {
      if (modeGeneration.current !== run.generation) return "typing mode ended";
      const lost = contextLost(run.declared, current(), { pane: run.aim });
      if (lost) return lost.why;
      return after?.holds?.();
    };
    const command = onPane(run.aim, "command", async () => {
      for await (const step of writes()) {
        const why = check();
        if (why !== undefined) throw new PaneMoved(why);
        if (typeof step === "number") await sleep(step);
        else {
          await guardedSend(run.aim, step, check).done;
          delivered += 1;
        }
      }
      after?.onDone?.();
    });
    /* A spoken send waits for it, but its outcome is its own (PR #1558
       review): a command called off or failed before any of its writes went
       out reports itself, and must not make a later send drop its Enter as if
       a dictated write had failed. One that stopped partway left the prompt
       half-changed, so the send drops its Enter and says why. */
    trackWrite(command.catch(() => {
      if (delivered > 0) throw new StoppedPartway(after?.partway ?? "a prompt command stopped partway through its writes");
    }));
    void command.then(
      () => { if (reportGeneration.current === run.reported) setProblem(done()); },
      (cause) => {
        after?.onFail?.(cause);
        if (reportGeneration.current === run.reported) setProblem(cause instanceof PaneMoved ? stopped(cause.why) : sentenceOf(cause));
      },
    );
  }, [current, guardedSend, onPane, trackWrite]);

  /**
   * PRD #1541 — interrupt the open agent's turn with the deck's interrupt key
   * for it: each step in order, with its pause after. Never `Ctrl+C`, which
   * quits Codex and OpenCode on an empty prompt — the deck serves no key
   * containing it and the desktop crate drops any set that does.
   *
   * Only while the agent is working — checked when the command is said, and
   * again immediately before each step goes out, because a turn can end while
   * the command waits behind an earlier write or between steps. On an agent
   * that has already stopped a repeated `Esc` opens a menu instead, so once an
   * interrupt has been delivered another is refused until a new turn is
   * evidenced ({@link PromptRecord}'s `interrupt`), and never within
   * {@link VOICE_INTERRUPT_REPEAT_MS} of the delivery. An interrupt ends what
   * voice knows about the prompt, because an agent may hand an interrupted
   * prompt back — unless its first key never went out, which leaves the
   * prompt, and so its record, as it was (PR #1558 review).
   */
  const interruptAgent = useCallback((target: VoiceDispatchTarget) => {
    const run = promptCommand(target, "interrupt");
    if (!run) return;
    if (run.turn !== "working") {
      reportRefused(`${run.label} is not working on anything.`);
      return;
    }
    const interrupted = run.record.interrupt;
    if (interrupted?.latched) {
      reportRefused(`${run.label} was already interrupted and has not started a new turn since — another interrupt could open a menu instead.`);
      return;
    }
    if (interrupted?.at !== undefined && Date.now() - interrupted.at < VOICE_INTERRUPT_REPEAT_MS) {
      reportRefused(`${run.label} was just interrupted — wait a moment before interrupting it again.`);
      return;
    }
    const { record, aim, label } = run;
    /* The turn this interrupt was said for. */
    const epoch = record.epoch;
    const forgotten = { writes: record.writes, whole: record.whole, why: record.why };
    emptyRecord(record, false, PROMPT_EMPTIED.interrupted);
    const revision = record.revision;
    const steps = run.keys.interrupt;
    const working = () => {
      const shown = paneRef.current;
      return shows(shown, aim) && shown.turn === "working";
    };
    /* The latch this interrupt arms, and the one it replaces should its first
       key never go out. */
    const before = record.interrupt;
    let latch: PromptRecord["interrupt"];
    reportRefused(`Interrupting ${label}…`);
    runPromptCommand(run, async function* () {
      for (const [index, step] of steps.entries()) {
        if (index === 0) {
          /* Armed before the first key is handed over, seeded from the turn as
             it is now, so a stop, a new turn or a send seen while that key is
             in flight counts as evidence. */
          latch = { latched: true, sawIdle: !working() };
          record.interrupt = latch;
        }
        yield step.bytes;
        /* Resumed only once the step's write has settled: delivered. */
        if (index === 0 && latch) latch.at = Date.now();
        if (index < steps.length - 1 && step.pauseAfterMs > 0) yield step.pauseAfterMs;
      }
    }, () => `Interrupted ${label}.`, (why) => `Stopped interrupting ${label} — ${why}.`, {
      holds: () => {
        if (!working()) return `${label} is not working on anything anymore`;
        /* Until the first key is delivered: a new turn seen since the
           interrupt was said is not the turn it was meant for. */
        if (latch?.at === undefined && record.epoch !== epoch) return `${label} started something new before the interrupt could be sent`;
        return undefined;
      },
      partway: `the interrupt to ${label} stopped partway`,
      /* The first key never went out: nothing was interrupted, so nothing
         latches, and the prompt is as it was, so its record is too — at a
         new revision, as for a clear called off before its first key. */
      onFail: (cause) => {
        if (latch?.at !== undefined) return;
        if (record.interrupt === latch) record.interrupt = before;
        if (!(cause instanceof PaneMoved) || record.revision !== revision) return;
        emptyRecord(record, forgotten.whole, forgotten.why);
        record.writes = forgotten.writes;
      },
    });
  }, [promptCommand, reportRefused, runPromptCommand]);

  /**
   * PRD #1541 — type a cleared prompt back in, from the clear's Undo: the
   * user's own click — so it outlives typing mode — but still only into the
   * pane and agent it was cleared from, only once the clear's own writes have
   * all gone out (it waits on the same line), and only while the prompt is
   * still at the `revision` the clear left it at: anything typed, sent or
   * otherwise changed there since withdraws it, checked on the click, after
   * the wait and before every write.
   *
   * Each cleared write goes back as a write of its own, in order and
   * {@link VOICE_SUBMIT_SETTLE_MS} after the one before landed, each held to
   * the declaration before it goes out: one write of the whole prompt could be
   * read as a paste, and "scratch that" afterwards removes the last of them as
   * it would have before the clear.
   */
  const restorePrompt = useCallback((aim: AgentAddress, declared: VoiceContext, writes: readonly string[], label: string, revision: number) => {
    const changed = () => promptFor(aim, incarnationOf(aim)).revision !== revision;
    /* Why a restore write must not go out now, checked before each one and
       again as the bridge hands it over. Not the typing mode: Undo is a click. */
    const holds = () => {
      const moved = contextLost(declared, current(), { pane: aim });
      if (moved) return moved.why;
      return changed() ? PROMPT_CHANGED : undefined;
    };
    const refused = holds();
    if (refused !== undefined) {
      setProblem(`Nothing was restored — ${refused}.`);
      return;
    }
    let restored = 0;
    const restore = onPane(aim, "command", async () => {
      let landedAt: number | undefined;
      for (const text of writes) {
        if (landedAt !== undefined) await sleep(Math.max(0, landedAt + VOICE_SUBMIT_SETTLE_MS - Date.now()));
        const why = holds();
        if (why !== undefined) throw new PaneMoved(why);
        const record = promptFor(aim, incarnationOf(aim));
        const { sent, done } = guardedSend(aim, text, holds);
        if (record.whole) record.writes.push({ text, sent });
        await done;
        landedAt = Date.now();
        restored += 1;
      }
    });
    /* As for every prompt command, its own outcome (PR #1558 review). */
    trackWrite(restore.catch(() => undefined));
    void restore.then(
      () => setProblem(`Restored ${label}'s prompt.`),
      (cause) => {
        resetPrompt(aim, false, PROMPT_EMPTIED.failed);
        if (!(cause instanceof PaneMoved)) setProblem(sentenceOf(cause));
        else setProblem(restored > 0 ? `Stopped restoring ${label}'s prompt — ${cause.why}.` : `Nothing was restored — ${cause.why}.`);
      },
    );
  }, [current, guardedSend, incarnationOf, onPane, promptFor, resetPrompt, trackWrite]);

  /**
   * PRD #1541 — clear the open agent's prompt: the deck's clear key, pressed
   * {@link VOICE_CLEAR_PRESSES} times for its rule, in writes no larger than
   * `maxPressesPerWrite` and `pauseBetweenWritesMs` apart.
   *
   * Undo is offered only when voice typed the WHOLE prompt (see
   * {@link PromptRecord}), what it typed is within the clear's reach
   * ({@link clearCovers}) and nothing changed the prompt while it was being
   * cleared, and types exactly those words back; otherwise the report says
   * the clear cannot be undone, and that some of the prompt may remain when
   * voice cannot tell it was all reached. Only that covered case leaves the
   * prompt known to be empty.
   *
   * PR #1558 review — keyboard input (or anything else that moves the
   * prompt's revision) between paced writes stops the clear before its next
   * write, so the user's new words are not cleared by it. A clear called off
   * before any of its keys went out puts the record back as it was, so
   * "scratch that" still reaches the last dictation.
   */
  const clearAgentPrompt = useCallback((target: VoiceDispatchTarget) => {
    const run = promptCommand(target, "clear");
    if (!run) return;
    const { bytes, presses, maxPressesPerWrite, pauseBetweenWritesMs } = run.keys.clear;
    const total = VOICE_CLEAR_PRESSES[presses];
    const perWrite = maxPressesPerWrite ?? total;
    const { record } = run;
    const before = { writes: record.writes, whole: record.whole, why: record.why };
    const restorable = record.whole ? record.writes.map((write) => write.text) : undefined;
    const covered = restorable !== undefined && clearCovers(restorable.join(""), presses, total);
    emptyRecord(record, false, PROMPT_EMPTIED.cleared);
    const revision = record.revision;
    let delivered = false;
    stopNudge(true);
    reportRefused(`Clearing ${run.label}'s prompt…`);
    const sentence = `Cleared ${run.label}'s prompt.`;
    runPromptCommand(run, async function* () {
      for (let left = total; left > 0; left -= perWrite) {
        if (left < total && pauseBetweenWritesMs) yield pauseBetweenWritesMs;
        yield bytes.repeat(Math.min(left, perWrite));
        /* Resumed only once that write has settled: delivered. */
        delivered = true;
      }
    }, () => {
      if (record.revision !== revision) return `${sentence} It cannot be undone — ${PROMPT_CHANGED_WHILE_CLEARING}.`;
      if (restorable === undefined) return `${sentence} Some of it may remain, and it cannot be undone — voice does not know everything that was in it.`;
      if (!covered) return `${sentence} Some of it may remain, and it cannot be undone — it was longer than a clear is sure to reach.`;
      if (restorable.length > 0 && reportGeneration.current === run.reported) {
        setUndo({ run: () => restorePrompt(run.aim, run.declared, restorable, run.label, revision) });
      }
      return sentence;
    }, (why) => `Stopped clearing ${run.label}'s prompt — ${why}.`, {
      partway: `clearing ${run.label}'s prompt stopped partway through its writes`,
      holds: () => (record.revision === revision ? undefined : PROMPT_CHANGED_WHILE_CLEARING),
      /* Cleared within its reach, with nothing else written in between: the
         prompt is empty, and known to be. */
      onDone: () => { if (record.revision === revision && covered) record.whole = true; },
      /* Not one key went out: the prompt is as it was before the clear, so
         its record is too — at a new revision, so nothing that held the
         record across the clear acts on it. */
      onFail: (cause) => {
        if (delivered || !(cause instanceof PaneMoved) || record.revision !== revision) return;
        emptyRecord(record, before.whole, before.why);
        record.writes = before.writes;
      },
    });
  }, [promptCommand, reportRefused, restorePrompt, runPromptCommand, stopNudge]);

  /**
   * PRD #1541 — remove the last words voice typed into the open agent's
   * prompt, one delete per character, trailing space included. Refuses rather
   * than guesses: nothing on record (see {@link PromptRecord} for what empties
   * it), a write a delete per character might not remove exactly
   * ({@link scratchRefusal}), or one the agent may have read together with
   * the input before it, voice's or the keyboard's ({@link scratchCollapse}).
   * A second "scratch that" removes the write before.
   *
   * The write is taken off the record immediately before the deletes go out,
   * not when the command is said, and only while the prompt is still at the
   * revision it was said against — checked again as the bridge hands the
   * deletes over: keyboard input (or anything else that changes the prompt)
   * while the command waits calls it off untouched.
   */
  const scratchLastDictation = useCallback((target: VoiceDispatchTarget) => {
    const run = promptCommand(target, "scratch");
    if (!run) return;
    const { record } = run;
    const lastWrite = record.writes.at(-1);
    if (lastWrite === undefined) {
      reportRefused(`Nothing to scratch — ${record.why}.`);
      return;
    }
    const last = lastWrite.text;
    const limit = Math.min(VOICE_SCRATCH_MAX_CHARS, run.keys.deleteChar.maxLiteralWriteChars ?? VOICE_SCRATCH_MAX_CHARS);
    const refusal = scratchRefusal(last, limit) ?? scratchCollapse(paneWrites.current.get(paneKey(run.aim)), lastWrite, limit);
    if (refusal) {
      reportRefused(refusal);
      return;
    }
    const revision = record.revision;
    const deletes = run.keys.deleteChar.bytes.repeat(last.length);
    reportRefused(`Removing ${quoted(last)} from ${run.label}'s prompt…`);
    runPromptCommand(run, async function* () {
      if (record.revision !== revision) throw new PaneMoved(record.why);
      if (record.writes.at(-1) !== lastWrite) throw new PaneMoved("the words voice typed there changed");
      const why = record.why;
      record.writes.pop();
      if (record.writes.length === 0) record.why = PROMPT_EMPTIED.scratched;
      let landed = false;
      try {
        yield deletes;
        landed = true;
      } finally {
        /* Called off before the deletes went out (PR #1558 review): the
           prompt is as it was, so its record is too, and a later "scratch
           that" still removes the write. Put back before the line moves on,
           so a dictation queued behind lands on top of it. */
        if (!landed && record.revision === revision) {
          record.writes.push(lastWrite);
          record.why = why;
        }
      }
    }, () => `Removed ${quoted(last)} from ${run.label}'s prompt.`, (why) => `Nothing was removed — ${why}.`, {
      holds: () => (record.revision === revision ? undefined : record.why),
      /* Called off, nothing was deleted and the record says so already; only
         a write that failed leaves the prompt unknown. */
      onFail: (cause) => { if (!(cause instanceof PaneMoved)) resetPrompt(run.aim, false, PROMPT_EMPTIED.failed); },
    });
  }, [promptCommand, reportRefused, resetPrompt, runPromptCommand]);

  /**
   * PRD #1541 — what the user typed into an agent's terminal by hand. Voice
   * no longer knows what that prompt holds, so nothing can be scratched from
   * it and a clear cannot be undone — except after a plain Enter, which sent
   * it and left it empty. Terminal reports alone ({@link terminalReportsOnly})
   * are not an edit: a pending spoken send still goes out. A mouse button
   * among them ({@link mouseButtonReported}) may have moved the cursor, so it
   * still ends what voice knows about the prompt; focus, motion and wheel
   * reports change nothing here.
   */
  const noteKeyboard = useCallback((target: AgentAddress, data: string, sent: Promise<void>) => {
    notePaneWrite(target, data.length, sent);
    /* A click or a focus change in the terminal, not an edit (PR #1558 review). */
    if (terminalReportsOnly(data)) {
      if (mouseButtonReported(data)) resetPrompt(target, false, PROMPT_EMPTIED.clicked);
      return;
    }
    const key = paneKey(target);
    keyboardEdits.current.set(key, (keyboardEdits.current.get(key) ?? 0) + 1);
    if (data === VOICE_DICTATION_SUBMIT) {
      resetPrompt(target, true, PROMPT_EMPTIED.sent);
      noteSent(target);
    } else resetPrompt(target, false, PROMPT_EMPTIED.keyboard);
  }, [notePaneWrite, noteSent, resetPrompt]);

  /* PRD #1541 — the open agent's turn seen stopped and then working again is
     evidence of a new turn: it moves the record's turn epoch, and after an
     interrupt it releases that interrupt's latch. */
  const paneTurn = pane?.turn;
  useEffect(() => {
    const shown = paneRef.current;
    if (!shown) return;
    const record = prompts.current.get(paneKey(shown));
    if (record && !incarnationsDiffer(record.spawnedAtMs, shown.spawnedAtMs)) {
      if (paneTurn === "working" && record.turn !== undefined && record.turn !== "working") record.epoch += 1;
      record.turn = paneTurn;
    }
    const interrupted = record?.interrupt;
    if (!interrupted?.latched) return;
    if (paneTurn !== "working") interrupted.sawIdle = true;
    else if (interrupted.sawIdle) interrupted.latched = false;
  }, [paneDeckId, paneAgentId, paneSpawnedAtMs, paneTurn]);
  /*
    PRD #1541 — let go of what is kept for agents the fleet no longer has
    (retired: gone from its deck, or its deck gone) and for incarnations it
    shows replaced, on every fleet update. Each index is let go only
    once nothing can be authorized by forgetting it: what is still outstanding
    keeps its own guards, which close over the operation and not over these
    maps.

    - A write line is dropped when its incarnation is replaced, even with
      writes still stuck on it: a command for the replacement runs on a line of
      its own anyway, and each stuck write is still held to the pane it was
      said for. A retired agent's line goes when its last write settles, as
      every line does — a stuck write to an agent that left keeps its line.
    - A terminal-write history is dropped when its incarnation is replaced, and
      when its agent is retired once it is quiet: nothing in flight, nothing
      dropped off its front still in flight, and its newest write settled at
      least VOICE_SUBMIT_SETTLE_MS ago — from then on an empty history answers
      every "scratch that" the same way, even if the same agent comes back.
    - A retired agent's prompt record is moved to a new revision and epoch,
      so every command or Undo still holding it is called off, and dropped —
      unless it is still latched by an interrupt, or its last interrupt is
      still inside VOICE_INTERRUPT_REPEAT_MS: forgetting either would let a
      second interrupt past. Such a record goes on the first fleet update
      after both have cleared that still finds its agent gone. A replaced incarnation's record is replaced by
      the next one ({@link promptFor}), one per agent the fleet has.
  */
  useEffect(() => {
    if (!fleet) return;
    const live = new Map<string, number | undefined>();
    for (const { connection: { deckId }, agents } of fleet) {
      if (deckId !== undefined) for (const agent of agents) live.set(paneKey({ deckId, agentId: agent.id }), agent.spawnedAtMs);
    }
    /* The pane on screen is live whatever the fleet says. */
    const shown = paneRef.current;
    if (shown && !live.has(paneKey(shown))) live.set(paneKey(shown), shown.spawnedAtMs);
    const replaced = (pane: string, spawnedAtMs: number | undefined) => live.has(pane) && incarnationsDiffer(spawnedAtMs, live.get(pane));
    for (const [key, line] of paneLines.current) if (replaced(line.pane, line.spawnedAtMs)) paneLines.current.delete(key);
    const now = Date.now();
    for (const [key, history] of paneWrites.current) {
      const quiet = history.droppedInFlight === 0
        && history.writes.every((write) => write.landedAt !== undefined && now - write.landedAt >= VOICE_SUBMIT_SETTLE_MS);
      if (replaced(key, history.spawnedAtMs) || (!live.has(key) && quiet)) paneWrites.current.delete(key);
    }
    for (const [key, record] of prompts.current) {
      const interrupt = record.interrupt;
      const cooling = interrupt?.at !== undefined && now - interrupt.at < VOICE_INTERRUPT_REPEAT_MS;
      if (live.has(key) || interrupt?.latched || cooling) continue;
      keyboardEdits.current.delete(key);
      emptyRecord(record, false, PROMPT_EMPTIED.replaced);
      record.epoch += 1;
      prompts.current.delete(key);
    }
  }, [fleet]);
  useEffect(() => {
    if (!keyboard) return;
    keyboard.current = noteKeyboard;
    return () => { keyboard.current = undefined; };
  }, [keyboard, noteKeyboard]);

  /** Say there was nothing on top to close. See {@link VOICE_NOTHING_TO_CLOSE}. */
  const reportNothingToClose = useCallback(() => setProblem(VOICE_NOTHING_TO_CLOSE), []);
  /**
   * PRD #1260 — enter the dictation mode for the pane on screen.
   *
   * **Refused on a pane that cannot take input**, with the pane's own reason:
   * the residual one-shot dictation accepts — nothing checks before typing —
   * is not acceptable for a mode, where the failure would repeat on every
   * utterance. Refused through `reportRefused`, so the report renders only
   * the refusal and never "Typing to …" beside it.
   *
   * Entering cancels a pending one-shot send rather than sending it, and the
   * mode never arms one of its own.
   */
  const startDictation = useCallback((target: VoiceDispatchTarget) => {
    const shown = paneRef.current;
    const label = target.agentLabel ?? shown?.label ?? target.agentId;
    const lost = paneLost(target);
    if (lost || !shown) {
      reportRefused(lost?.code === "replaced" ? VOICE_PANE_REPLACED : dictationRefused(label, `${lost?.why ?? "its pane is not the one on screen"}.`));
      return;
    }
    if (shown.inputBlocked !== undefined) {
      reportRefused(dictationRefused(label, shown.inputBlocked));
      return;
    }
    cancelPendingSend();
    setPending(undefined);
    const declared = current();
    setPanelState({ kind: "dictating", target: { deckId: target.deckId, agentId: target.agentId, label }, declared });
  }, [cancelPendingSend, current, paneLost, reportRefused, setPanelState, setPending]);
  /**
   * PRD #1260 — leave the dictation mode by voice. Answered in this surface's
   * own words (the `refusedRef` path), because only the surface knows whose
   * prompt it was typing into — or that it was typing into none.
   */
  const stopDictation = useCallback(() => {
    refusedRef.current = true;
    if (!endDictation()) setProblem(VOICE_NOT_DICTATING);
  }, [endDictation]);
  /**
   * PRD #1497 — start reading mode for the pane on screen. Refused on a pane
   * that is no longer the one on screen, like typing mode. The start itself is
   * answered later (the Settings opt-in, whether this agent can be read), so a
   * refusal replaces the row's "Reading on." when it arrives — and is spoken.
   */
  const startReading = useCallback((target: VoiceDispatchTarget) => {
    const shown = paneRef.current;
    const label = target.agentLabel ?? shown?.label ?? target.agentId;
    const lost = paneLost(target);
    if (lost || !shown) {
      reportRefused(lost?.code === "replaced" ? VOICE_PANE_REPLACED : `Reading did not start for ${label}: ${lost?.why ?? "its pane is not the one on screen"}.`);
      return;
    }
    void reader.mode.turnOn({ deckId: target.deckId, agentId: target.agentId, label }).then((started) => {
      if (started.kind !== "refused") return;
      setResult(undefined);
      setProblem(started.sentence);
    });
  }, [paneLost, reader.mode, reportRefused]);
  /** PRD #1497 — leave reading mode by voice; "Reading off." is spoken. */
  const stopReading = useCallback(() => {
    if (!reader.mode.active) {
      reportRefused(READING_ALREADY_OFF);
      return;
    }
    void reader.mode.turnOff();
  }, [reader.mode, reportRefused]);
  /* PRD #1497 — the Settings opt-in turned off ends reading, and cuts off
     whatever of it is still being said, without waiting for the save. */
  useEffect(() => {
    if (readingConsent !== undefined && readingConsent !== "on") void reader.mode.consentOff();
  }, [reader.mode, readingConsent]);
  /** PRD #1497 D6 — "stop" / "quiet": silence the app. Reading stays on. */
  const quietSpeech = useCallback(() => reader.mode.quiet(), [reader.mode]);
  /* PRD #1497 D11 — reading ends when the pane on screen stops being the
     agent it was turned on for: another agent's pane, or none. */
  const shownDeckId = pane?.deckId;
  const shownAgentId = pane?.agentId;
  useEffect(() => {
    void reader.mode.paneChanged(shownDeckId !== undefined && shownAgentId !== undefined ? { deckId: shownDeckId, agentId: shownAgentId } : undefined);
  }, [reader.mode, shownAgentId, shownDeckId]);

  /*
    PRD #802 — publish the members only this surface can serve, so a row naming
    one of them is callable on every screen.

    `DeckSurface`'s own publish is the model and the reasoning is the same:
    **no dependency array on purpose**, because the object closes over this
    render's callbacks and a host reading the slot at dispatch time must get the
    last committed ones rather than the first. Nothing dispatches between a
    commit and its effects, so the momentary `undefined` the cleanup leaves is
    unobservable.

    Before the early return below, because it is a hook. A runtime with no
    `resolveVoice` renders nothing and still publishes — harmless, since nothing
    can dispatch a row without a resolver to produce one.
  */
  useEffect(() => {
    if (!channel) return;
    /* `showVoiceCommands` only where the runtime can actually list something.
       Publishing it regardless would open an overlay that has to explain its
       own emptiness — a sentence this file would have to write — where leaving
       it out gets the refusal the surface already renders. */
    const published: Partial<VoicePanelContext> = {
      stopVoice,
      typeIntoAgent,
      submitAgentPrompt,
      startDictation,
      stopDictation,
      startReading,
      stopReading,
      quietSpeech,
      interruptAgent,
      clearAgentPrompt,
      scratchLastDictation,
      reportNothingToClose,
      reportRefused,
      ...(voiceCommands ? { showVoiceCommands } : {}),
      /* PRD #802 — published only while the overlay is OPEN, and that is how
         "an overlay is open" reaches a dispatch at all: it is a `useState`
         boolean here and `screens` draws on `DeckView`, so the fact cannot be a
         column. `closeTopmost` reads its presence, which is why this must not
         become an always-published no-op. */
      ...(vocabulary !== undefined ? { dismissVoiceOverlay: closeVocabulary } : {}),
    };
    channel.current = published;
    return () => { channel.current = undefined; };
  });

  if (!resolveVoice) return null;
  const undoButton = undo && <button className="button secondary compact" onClick={() => { undo.run(); setUndo(undefined); }}><Undo2 size={13} /> Undo</button>;

  const indicator = indicatorFor(known, on, phase);
  const note = progressNote(indicator, phase);
  /*
    The empty state: voice is on and this session has nothing to report yet.

    Keyed on the three report slots rather than on a "have we spoken" flag,
    because that is exactly the question — the hint is what stands in the row
    while none of them holds anything, and it goes the moment one does.
    `forget()` clears all three when an utterance with words in it arrives,
    so it reappears between utterances too, which is right: it is a label for
    an empty row rather than a first-run tutorial.
  */
  /* PRD #1260 — whose prompt the dictation mode is typing into, if it is on. */
  const dictating = panelState.kind === "dictating" ? panelState.target : undefined;
  const dictatingLabel = dictating ? displayText(dictating.label, DISPLAY_LIMITS.name) : undefined;
  /* PRD #1261 — the numbered choice on offer, if there is one. */
  const choice = panelState.kind === "awaitingChoice" ? panelState.offer : undefined;
  /* PR #1451's hand test — what was heard is said ONCE. The capture sentence
     is what the row shows while the answer is being worked out; an answer
     whose own sentence quotes the transcript ("Heard: “…” — no matching
     action.") replaces it, rather than the row reading the transcript twice.
     An answer that names its effect instead ("Opening the agent dashboard.")
     keeps it, because then it is the only place the words are. */
  const captureShown = capture !== undefined
    && !(result !== undefined && capture.transcript !== undefined && result.outcome.sentence.includes(`“${capture.transcript}”`));
  const emptyState = indicator === "on" && dictating === undefined && choice === undefined && pending === undefined && problem === undefined && capture === undefined && result === undefined;
  const reporting = note !== undefined || dictating !== undefined || choice !== undefined || pending !== undefined || problem !== undefined || capture !== undefined || result !== undefined;

  return (
    /*
      One RESERVED ROW at the bottom of the window, and that is the product
      owner's own shape: *"a dedicated row at the bottom for the button and, if
      it's turned on, the text that it hears to the right next to it in the same
      row that does not overlap anything (including the overlay with the agent
      enlarged)"*.

      **What it replaces, and why the replacement is structural rather than a
      nicer position.** The trigger and the report used to be two independently
      fixed boxes floating over the screen — the report stacked *above* the
      button, growing upward as it filled. Floating means overlapping, and
      overlapping the agent pane is the case that matters: that pane is where the
      user is working, and a report describing a navigation was drawn over the
      terminal they had just enlarged to read. Moving the boxes would have bought
      a different collision, not no collision, because a floating box's height is
      its content's and no other element can reserve room for it.

      A row can. `--voice-row-height` in `styles.css` is a constant this row
      occupies and every surface that would otherwise reach the bottom edge sets
      aside: the screen content, the agent pane overlay, the reader overlay, the
      evidence drawer and the toast. The row is above the agent pane in z-order
      and outside its content box, so neither can cover the other — see the
      block comment on `.voice-row` for which surfaces reserve it and which
      deliberately do not.

      The two children are the row's two cells, left and right, and there is one
      `VOICE_PEER_PROPS` on the ROW rather than one on each of them. That is the
      same exemption narrowed, not widened: `useInertBackground` matches the
      marker on the sibling it is about to inert, and this row is that sibling
      now. The trigger and the Undo inside the report stay reachable behind an
      open pane, which is what the exemption exists for.
    */
    <div ref={rowRef} className="voice-row" data-testid="voice-row" data-voice={on ? "on" : "off"} data-choice={choice ? "open" : undefined} {...VOICE_PEER_PROPS}>
      <button
        type="button"
        className="voice-trigger"
        data-testid="voice-trigger"
        /* The state twice over, because the two audiences read different
           things: the word is what a sighted user sees on the control, and
           `aria-pressed` is what a screen reader announces. A toggle that
           showed one without the other would be on for one of them only. Four
           values rather than two — see {@link VoiceIndicator}. */
        aria-pressed={INDICATOR_PRESSED[indicator]}
        /* PRD #1260 — the mode is part of the control's state, so it is part
           of its announced name while it is on, beside the pressed state. */
        aria-label={dictatingLabel === undefined ? undefined : `${INDICATOR_LABEL[indicator]} — typing to ${dictatingLabel}`}
        /* Something is in flight and the control is not idle at the state it is
           showing. It is the announced half of the same honesty the word
           carries, and of the click below being serialised rather than racing. */
        aria-busy={indicator === "stopping" || indicator === "checking"}
        title={INDICATOR_TITLE[indicator]}
        /* Serialised rather than raced. While a release is in flight the device
           is Rust's, and a press that started a new recording over it would be
           the same lie from the other direction — or, if it landed as a second
           cancel, would race the first one's answer. Every other state is
           pressable, `unreleased` included: that press is the retry. */
        onClick={() => {
          if (phaseRef.current === "stopping") return;
          /* `unreleased` routes to the release rather than to the start, and
             not only where `on` happens to be true: the device is open, so the
             press that looks like *turn it on* has to be the retry that closes
             it. `voiceStart` would be refused by `accepts_start` anyway, which
             is the fall-through this replaces. */
          /* Through the registry entry rather than straight to `turnOff`, the
             way `DeckShell` routes its own Close through `closeAgentView`: the
             `voice_off` row names `stopVoice`, and a button that reached the
             same behaviour by a second route would be exactly the residual
             PRD #802 D10 recorded for the unspoken capabilities, which PRD
             #1195 M1 then closed. */
          if (onRef.current || phaseRef.current === "unreleased") VOICE_ACTIONS.stopVoice.run({ stopVoice });
          else void turnOn();
        }}
      >
        {/* The crossed-out microphone only where the device is known to be
            closed. Everywhere else — on, stopping, or not yet asked — it is the
            plain one, because an icon is a claim too. */}
        {indicator === "off" ? <MicOff size={16} /> : <Mic size={16} />}
        <span>{INDICATOR_LABEL[indicator]}</span>
      </button>
      {/*
        PRD #1260 — the dictation mode's non-voice exit, shown only while the
        mode is on. It leaves voice listening and sends nothing.

        Inside the row, so it carries the row's `VOICE_PEER_PROPS` exemption
        exactly as the Voice button does: clickable and tabbable behind the
        agent pane's modal fence, which is the point — a misheard "type off" is
        exactly when a user reaches for it. The keyboard route to it is Tab;
        no shortcut is bound, because on the agent screen every key belongs to
        the agent's terminal.
      */}
      {dictating && (
        <button type="button" className="button secondary compact voice-stop-typing" data-testid="voice-stop-typing" onClick={() => { endDictation(); }}>
          <SquarePen size={13} /> Stop typing
        </button>
      )}
      {/*
        The report is the row's right-hand cell, beside the button rather than in
        a dialog — which is what makes continuous voice possible at all: a dialog
        would have to be dismissed between utterances, and the press that
        dismissed it is the press that turns voice off.

        Always mounted, inside the row's exemption: the Undo in here is a
        control, and a control behind an open agent pane has to be clickable or
        the report is a picture of one.

        `role="status"` rather than `alert`: the sentences are the result of
        something the user just did, so they are announced at the next pause
        instead of interrupting. The region is in the DOM before any of them
        arrives, which is what makes the announcement reliable.

        **The last report PERSISTS until the next utterance replaces it, and that
        is now a decision rather than something inherited.** Floating over the
        screen it was a box that stayed in the way after it had been read, and
        nothing here dismissed it. In a reserved row it costs nothing to leave:
        the space is set aside whether or not it holds anything, so the sentence
        is simply what the row says until there is something newer to say. It
        also has to stay — the pipeline goes straight back to listening, so a
        sentence that faded on a timer would be gone before a user who looked
        away from the microphone and back. `forget()` is the one thing that
        clears it, and only once the next utterance turns out to have words in
        it: a segment of keyboard or room noise leaves it standing (PR #1451).
      */}
      <div
        className="voice-report"
        data-testid="voice-report"
        role="status"
        aria-live="polite"
      >
        {reporting && (
          <div className="voice-report-card">
            {note && <p className="voice-note">{note}</p>}
            {/* Not through `displayText`: this is a literal in this file, not
                free-form text from a microphone, a model or a daemon. */}
            {emptyState && <p className="voice-hint" data-testid="voice-hint">{VOICE_EMPTY_STATE}</p>}
            {/*
              PRD #1260 — the dictation mode, named for as long as it is on,
              with the reserved phrases that stay live in it. The only other
              wording this file builds around a value from elsewhere, for the
              countdown's reason below: nothing Rust-side remembers the mode.
            */}
            {dictatingLabel !== undefined && (
              <p className="voice-dictation" data-testid="voice-dictating">
                {"Typing to "}
                {dictatingLabel}
                {". Say “type off” to stop, "}
                {/* PR #1451 — highlighted after a pause with dictated words
                    unsent ({@link VOICE_SEND_NUDGE_MS}). Styling only: the
                    wording is the same whether or not it is lit. */}
                <span className="voice-send-hint" data-testid="voice-send-hint" data-nudge={nudge ? "on" : undefined}>“send it” to send</span>
                {"."}
              </p>
            )}
            {/*
              PRD #802 D6 — where the microphone is aimed, and the countdown.

              **In the reserved row, not floating.** The row is the surface's
              one piece of screen and the countdown is the most important thing
              it ever says: it is the difference between a five-second wait a
              user can talk over and a silent submission to an agent. A
              floating box would put it back over whatever is underneath, which
              is the shape `f00828f6` replaced.

              It NAMES the agent, and that is the only wording this file builds
              around a value from elsewhere. It has to: nothing Rust-side knows
              a send is pending, so no table column could carry this sentence —
              and a countdown that did not say whose prompt it was about to
              submit to would be the one question a user needs answered before
              the number reaches zero. The label is the deck's own text and
              crosses `displayText` like every other free-form string here.

              `role="timer"` rather than leaving it to the row's `status`
              region: this changes every second, and a polite live region that
              re-announced the whole report each tick would be unusable.
            */}
            {pending && (
              <p className="voice-dictation" data-testid="voice-dictation" role="timer">
                {"In "}
                {displayText(pending.label, DISPLAY_LIMITS.name)}
                {"'s prompt"}
                {sendIn === undefined
                  ? " — yours to send or edit."
                  : ` — sending in ${sendIn} s. Keep talking to cancel, or say “send it”.`}
              </p>
            )}
            {/*
              Each sentence is its own element holding nothing else, so the
              transcript inside it survives to the DOM exactly as Rust rendered
              it — punctuation, casing, inner quotes and all. Seeing what was
              heard is what turns a mis-transcription into a correction the user
              can make.

              `displayText` is the render seam every free-form string in this app
              crosses: a sentence embeds a transcript, and a transcript is
              model- or microphone-supplied text with control and bidi codepoints
              still in it. It sanitises and bounds; it never rewords.

              The `message` budget, 240 characters, is the one written for "the
              daemon's own connection message" — one sentence, once per screen,
              which is this shape exactly. It is a real bound rather than a
              formality: thirty seconds of speech is around 700 characters, so a
              rambling utterance's no-match sentence IS elided here, with the
              clamp's own marker so it never passes itself off as complete.

              `title` carries the same sanitised string, because a row is one
              line high and CSS elides what does not fit. The bound above is what
              makes that safe to put in a tooltip: it is already clamped and
              already scrubbed, and it is the same value the element renders
              rather than a second copy from somewhere upstream.
            */}
            {problem && <p className="voice-sentence" title={displayText(problem, DISPLAY_LIMITS.message)}>{displayText(problem, DISPLAY_LIMITS.message)}</p>}
            {/* PRD #1541 — a clear's Undo, beside the panel's own report of it: a
                prompt command reports in this surface's words, so no resolver
                result is shown for it to sit under. */}
            {!result && undo && <div className="voice-outcome-foot">{undoButton}</div>}
            {captureShown && <p className="voice-sentence" title={displayText(capture.sentence, DISPLAY_LIMITS.message)}>{displayText(capture.sentence, DISPLAY_LIMITS.message)}</p>}
            {result && (
              <>
                <p className="voice-sentence" title={displayText(result.outcome.sentence, DISPLAY_LIMITS.message)}>{displayText(result.outcome.sentence, DISPLAY_LIMITS.message)}</p>
                {/* Never elided and never squeezed out — `flex: 0 0 auto` in the
                    stylesheet. The Undo is a real control with a ten-second life,
                    so a long transcript beside it must lose characters before
                    this loses the button. */}
                <div className="voice-outcome-foot">
                  <span className="voice-cost">{voiceCost(result.backend, result.resolveMs)}</span>
                  {undo && undoButton}
                </div>
              </>
            )}
          </div>
        )}
      </div>
      {/*
        PRD #802 D7 — the discovery overlay, and why it is a CHILD of the row.

        It cannot be in the row: the row is one line high by contract, and a
        list of every command is not one line. So it is an overlay, which is
        what the requirement says. Being a child of `.voice-row` is what keeps
        it reachable: `useInertBackground` skips the subtree carrying
        `VOICE_PEER_PROPS`, and that marker is on the row. A sibling would be
        marked `inert` behind an open agent pane — exactly the defect the peer
        exemption exists for, reintroduced one element along — and would need a
        second exemption the hook deliberately asks to be argued for.

        The row's own "overlaps nothing" property is untouched by this, because
        this is not the report: it is modal, it is transient, and it has two
        ways out. A report that covered the screen would be a surface the user
        cannot get rid of; a list they asked for and can dismiss is the
        opposite.

        `aria-modal` is deliberately absent. Nothing here inerts the background,
        the Voice button behind it stays live on purpose — "voice off" while the
        list is up must still work — and claiming modality the DOM does not have
        is the false claim `useInertBackground`'s own note is about.
      */}
      {/*
        PRD #1261 — the numbered choice, as a dialog centred over the screen
        (PR #1451 round 3; it used to be a strip of buttons in this row). A
        CHILD of the row for the discovery overlay's reason below: the row
        carries `VOICE_PEER_PROPS`, so the choice stays reachable behind the
        agent pane's and the New agent dialog's fences. The row is raised
        above those dialogs while it is open (`data-choice` in the
        stylesheet), since a child cannot leave its parent's stacking context.
        The report above keeps saying what was heard; the dialog says what to
        choose between.
      */}
      {choice && (
        <VoiceChoiceDialog
          key={choice.deadline}
          offer={choice}
          secondsLeft={choiceIn}
          onPick={(candidate) => dispatchChoice(choice, candidate)}
          onCancel={() => { closeChoice(VOICE_CHOICE_CANCELLED); }}
        />
      )}
      {vocabulary && (
        <div className="voice-help-backdrop" data-testid="voice-help" onClick={closeVocabulary}>
          <div
            className="voice-help"
            role="dialog"
            aria-label="What you can say"
            /* The backdrop closes; the panel must not. Without this every click
               inside the list — including one that misses a row — dismisses it. */
            onClick={(event) => event.stopPropagation()}
          >
            <div className="voice-help-head">
              <h2>What you can say</h2>
              <button type="button" className="button secondary compact" data-testid="voice-help-close" onClick={closeVocabulary}>
                <X size={13} /> Close
              </button>
            </div>
            {vocabulary.problem && <p className="voice-help-problem">{displayText(vocabulary.problem, DISPLAY_LIMITS.message)}</p>}
            {vocabulary.commands && <VoiceVocabulary commands={vocabulary.commands} />}
          </div>
        </div>
      )}
    </div>
  );
}

/** PRD #1261 — what a tie is between, named for the dialog's heading, by the kind of the offered entries. */
const VOICE_CHOICE_NOUNS: Record<string, string> = {
  agent_ref: "agent",
  deck_ref: "daemon",
  dir_ref: "directory",
  mode_ref: "mode",
  agent_type_ref: "agent type",
  orchestration_ref: "orchestration",
};

/**
 * PRD #1261 — the numbered choice, as a modal dialog in `ConfirmDialog`'s
 * look (PR #1451 round 3, the maintainer's call).
 *
 * **It takes focus, which reverses the PRD's Open Question 1.** The in-row
 * strip left focus where it was, because a choice arises from speech and the
 * user may be typing into a terminal. The maintainer asked for a dialog like
 * the app's others that a number key answers, and a key reaches only the
 * element with focus — the alternative, a window listener, would also close
 * the agent pane and the New agent dialog on `Escape` and fire the Daemons
 * screen's `1`–`4`. So it moves focus to entry 1 when it opens and gives it
 * back to whatever had it when it closes, however it closes — unless the
 * close itself moved focus somewhere (a chosen agent's pane), which then
 * keeps it. Typing mode and a choice never coexist (PRD #1260's precedence),
 * so this cannot swallow dictation.
 *
 * Every key is handled HERE and stopped here: a digit 1–9 picks that entry (one
 * past the list does nothing), `Escape` cancels, and nothing else reaches the
 * window's shortcuts while the dialog has focus. Clicking the scrim cancels,
 * as `ConfirmDialog`'s does.
 */
function VoiceChoiceDialog({ offer, secondsLeft, onPick, onCancel }: {
  offer: VoiceChoiceOffer;
  secondsLeft: number | undefined;
  onPick: (candidate: VoiceResolvedParamDto) => void;
  onCancel: () => void;
}) {
  const panel = useRef<HTMLElement>(null);
  const firstEntry = useRef<HTMLButtonElement>(null);
  const titleId = `voice-choice-title-${offer.deadline}`;
  const candidates = offer.outcome.candidates;
  /* A choice across sections (round 4, D7) mixes kinds: "Which one?". */
  const kind = candidates[0]?.kind;
  const noun = candidates.every((candidate) => candidate.kind === kind) ? VOICE_CHOICE_NOUNS[kind ?? ""] : undefined;
  /* Layout effects, so the opener is read before anything else on this commit
     moves focus, and given back before the passive effects of whatever the
     close opened (a pane takes focus in one) run after it. */
  useLayoutEffect(() => {
    const opener = document.activeElement;
    firstEntry.current?.focus();
    const node = panel.current;
    return () => {
      const now = document.activeElement;
      const stillHere = now === null || now === document.body || (node?.contains(now) ?? false);
      if (stillHere && opener instanceof HTMLElement && opener !== document.body && opener.isConnected) opener.focus();
    };
  }, []);
  return (
    <div className="voice-choice-backdrop" role="presentation" onMouseDown={onCancel}>
      <section
        ref={panel}
        className="confirm-dialog voice-choice"
        data-testid="voice-choice"
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        onMouseDown={(event) => event.stopPropagation()}
        onKeyDown={(event) => {
          event.stopPropagation();
          if (event.key === "Escape") {
            event.preventDefault();
            onCancel();
            return;
          }
          if (event.ctrlKey || event.metaKey || event.altKey || !/^[0-9]$/.test(event.key)) return;
          event.preventDefault();
          const candidate = candidates[Number(event.key) - 1];
          if (candidate !== undefined) onPick(candidate);
        }}
      >
        <h2 id={titleId}>{noun ? `Which ${noun}?` : "Which one?"}</h2>
        <p>
          {`“${displayText(offer.outcome.spoken, DISPLAY_LIMITS.name)}” matches more than one${noun ? ` ${noun}` : ""}. `}
          {"Say or press its number, say its name, or click it."}
        </p>
        <ol className="voice-choice-list">
          {candidates.map((candidate, at) => (
            <li key={candidate.value}>
              <button ref={at === 0 ? firstEntry : undefined} type="button" className="button secondary" onClick={() => onPick(candidate)}>
                {`${at + 1}. ${displayText(candidate.label, DISPLAY_LIMITS.name)}`}
              </button>
            </li>
          ))}
        </ol>
        <div>
          {/* Its own `timer` rather than the row's `status` region: it changes
              every second, and a polite live region re-announcing it each tick
              would be unusable. */}
          {secondsLeft !== undefined && <span className="voice-choice-timer" role="timer">{`${secondsLeft} s`}</span>}
          <button type="button" className="button secondary" onClick={onCancel}>
            <X size={13} /> Cancel
          </button>
        </div>
      </section>
    </div>
  );
}

/**
 * The list itself, generated from the command table and from nothing else
 * (PRD #802 D7).
 *
 * **Two sections rather than one filtered list.** The requirement is a list of
 * what is callable *right now*, and that is the first section. The second is
 * the rest — because "the agent overview opens from the deck" is the single
 * most useful thing discovery can tell somebody, and dropping those rows would
 * leave a user who asked what they can say unable to learn that a command
 * exists at all. Both sections come from the same array and the same `callable`
 * flag, so neither is a maintained list.
 *
 * The two headings are this file's own words, and they are labels for a
 * boolean rather than wording derived from the table. The `unavailable_hint`
 * column is deliberately NOT rendered into a sentence here: `voice/outcome.rs`
 * already composes one from it, and a second composer is how two surfaces come
 * to phrase the same situation differently — which is the property this whole
 * pipeline is built on. The heading says what the flag means; the hint stays
 * where it is rendered once.
 *
 * `description` is printed as it stands, without `displayText`. That seam is
 * for free-form text — a transcript, a backend's detail, an agent's name — and
 * this is prose compiled into the binary from `commands.toml`, which no user
 * and no model can reach. Passing it through would also clamp it at the
 * message budget, which is shorter than several of these rows.
 */
function VoiceVocabulary({ commands }: { commands: VoiceCommandDto[] }) {
  const here = commands.filter((command) => command.callable);
  const elsewhere = commands.filter((command) => !command.callable);
  const section = (title: string, rows: VoiceCommandDto[], where: "here" | "elsewhere") => (
    rows.length > 0 && (
      <section className="voice-help-section" data-where={where}>
        <h3>{title}</h3>
        <ul>
          {rows.map((command) => (
            <li key={command.id} data-command={command.id}>
              <code>{command.id}</code>
              <p>{command.description}</p>
            </li>
          ))}
        </ul>
      </section>
    )
  );
  return (
    <div className="voice-help-body">
      {section("On this screen", here, "here")}
      {section("On another screen", elsewhere, "elsewhere")}
    </div>
  );
}
