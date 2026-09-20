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
 * {@link SCREEN_MOVED_ON}, {@link VOICE_UNAVAILABLE},
 * {@link VOICE_CAP_DISCARDED}, {@link VOICE_RELEASE_REFUSED} and
 * {@link VOICE_EMPTY_STATE}. The first two are for situations Rust
 * structurally cannot know about; the rest are about the surface's own state
 * — a release it cannot vouch for, a microphone it has nothing to open, a row
 * with nothing in it yet — rather than about an utterance. See their own
 * notes.
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
import { useCallback, useEffect, useRef, useState } from "react";
import { Mic, MicOff, Undo2, X } from "lucide-react";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_PEER_PROPS } from "../hooks/useInertBackground";
import { VOICE_ACTIONS, type VoiceDispatchTarget, type VoicePanelChannel, type VoicePanelContext } from "../lib/voiceActions";
import type { VoiceCommandDto, VoiceOutcomeDto, VoiceResultDto, VoiceScreen, VoiceStatusDto } from "../lib/bridge";
import type { DeckRuntimeState } from "../types";

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
 * safe to tune rather than the number itself.
 */
export const VOICE_DICTATION_SEND_MS = 5_000;

/** How often the countdown redraws, and the resolution it is shown at. */
export const VOICE_DICTATION_TICK_MS = 1_000;

/**
 * What ends dictation, said out loud.
 *
 * **Matched EXACTLY, against the whole normalised transcript, and never as a
 * substring** — which is the entire defence against the false positive PRD #802
 * D6 names: *"we should stop dictation of the log"* is a sentence somebody may
 * really want typed, and a substring rule would truncate them mid-thought and
 * silently stop listening to them. Whole-utterance equality means only an
 * utterance that IS the phrase ends anything.
 *
 * **Matched here rather than by the intent backend, and that is a deliberate
 * trade rather than a shortcut.** Running the resolver over every dictated
 * sentence would cost a model call per sentence — 0.62-1.03 s on the measured
 * backends, and money — and would reintroduce the exact failure the phrase's
 * distinctiveness is meant to remove: a model asked *"was that the exit?"* can
 * be wrong, where `===` cannot. The cost of matching here is that these phrases
 * are a list in this file rather than a row in the table, and so are the only
 * spoken words in the product the command table does not own. They are written
 * down in one place, they are tested by value, and the row's own `report`
 * sentence teaches the phrase at the moment dictation starts.
 *
 * Three spellings rather than one, because a transcriber picks among them
 * freely and a user who said the right thing must not be told they did not.
 */
export const VOICE_DICTATION_EXIT_PHRASES = ["stop dictation", "end dictation", "stop dictating"] as const;

/**
 * What ends EVERYTHING while dictating — the `voice_off` row's phrases, matched
 * the same way and checked first.
 *
 * **The precedence is deliberate and it is the safe direction.** The exit
 * phrase ends dictation and leaves the microphone open; this closes the
 * microphone as well. The two lists share no phrase, so the order decides
 * nothing today — but if one utterance ever satisfied both, treating it as the
 * bigger stop is the only reading that cannot leave a live microphone after a
 * user asked for it to stop. A user who meant the smaller one says four words
 * and presses a button; a user who meant the bigger one and got the smaller has
 * a microphone they believe is off.
 *
 * It is a second copy of wording that also lives in `commands.toml`, and the
 * duplication is the price of the trade above: a dictated sentence must not
 * cost a model call. The row is what answers *"voice off"* when NOT dictating,
 * and the two must be kept saying the same thing — which the phrase fixtures
 * check from one side and this file's tests from the other.
 */
export const VOICE_OFF_PHRASES = ["voice off", "turn off the voice", "turn voice off", "stop voice", "stop voice control", "stop listening"] as const;

/**
 * One transcript, reduced to the form the phrase lists are compared against.
 *
 * Case, punctuation and spacing are all things a transcriber decides for
 * itself — *"Stop dictation."*, *"stop dictation"* and *"stop, dictation"* are
 * the same four syllables — so comparing raw text would make the exit phrase a
 * lottery on the backend's punctuation model. Everything that is not a letter
 * or a digit becomes a space, runs of space collapse, and the ends are trimmed.
 */
export function spokenPhrase(transcript: string): string {
  return transcript.toLowerCase().replace(/[^\p{L}\p{N}]+/gu, " ").trim();
}

/**
 * What actually gets typed into the agent, for one utterance.
 *
 * **Every control and format character becomes a space, and a carriage return
 * is the one that matters**: a transcript carrying `\r` or `\n` would SUBMIT
 * the agent's prompt the moment it was written, which is precisely the silent
 * auto-submit this whole countdown exists to prevent. Format characters go with
 * them because a bidi override in a prompt is text that reads as one thing and
 * says another.
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

/** What the report says when dictation ended because the user said so. */
export const VOICE_DICTATION_ENDED = "Dictation off. Anything still in the prompt is yours to send or edit.";

/** The voice half of the runtime, which a runtime may not have at all. */
type Voice = Pick<DeckRuntimeState, "declareVoiceScreen" | "resolveVoice" | "voiceCommands" | "voiceStart" | "voiceStop" | "voiceStatus" | "voiceCancel" | "sendTerminalInput">;

/**
 * Which agent the microphone is aimed at while dictating (PRD #802 D6).
 *
 * The composite identity, never the bare agent id: ids collide across decks,
 * and typing a user's words into the wrong machine's namesake is the worst
 * version of that collision this app has.
 */
type Dictation = { deckId: string; agentId: string; label: string };

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
  onDispatch: (outcome: Extract<VoiceOutcomeDto, { kind: "dispatch" }>) => { undo?: () => void } | undefined;
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
export function VoiceControlPanel({ runtime, screen, onDispatch, channel }: VoiceControlPanelProps) {
  const { declareVoiceScreen, resolveVoice, voiceCommands, voiceStart, voiceStop, voiceStatus, voiceCancel, sendTerminalInput } = runtime;

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
  /** The transcription stage's own sentence — what was heard, or why nothing was. */
  const [capture, setCapture] = useState<string>();
  const [result, setResult] = useState<VoiceResultDto>();
  /*
    Wrapped in an object rather than held bare: `useState` treats a function
    argument as an updater, so `setUndo(fn)` would call `fn` instead of storing
    it — and `fn` here is a navigation.
  */
  const [undo, setUndo] = useState<{ run: () => void }>();

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
  const phaseRef = useRef<VoicePhase>("idle");
  const setPhase = useCallback((next: VoicePhase) => { phaseRef.current = next; setPhaseState(next); }, []);

  /**
   * Which agent the microphone is aimed at, or `undefined` for none
   * (PRD #802 D6).
   *
   * Mirrored into a ref and written through a setter for the reason the two
   * toggles above are: the cycle reads it BETWEEN its own awaits — after a
   * transcription comes back, to decide whether this utterance is a command or
   * something to type — and a `useState` value captured in that closure is
   * whatever it was when the closure was built.
   */
  const [dictation, setDictationState] = useState<Dictation>();
  const dictationRef = useRef<Dictation | undefined>(undefined);
  const setDictation = useCallback((next?: Dictation) => { dictationRef.current = next; setDictationState(next); }, []);
  /** Seconds left before the typed text is sent, or `undefined` for no pending send. */
  const [sendIn, setSendIn] = useState<number>();
  const sendTimer = useRef<number | undefined>(undefined);

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
  const cancelPendingSend = useCallback(() => {
    if (sendTimer.current !== undefined) window.clearInterval(sendTimer.current);
    sendTimer.current = undefined;
    setSendIn(undefined);
  }, []);

  /**
   * Press Enter in the agent's prompt.
   *
   * Through `sendTerminalInput`, which is the path the user's own keystrokes
   * take — dictation gets no send verb of its own, and in particular not
   * `submit_text`, which types AND submits in one guarded call and would make
   * the text invisible until it was already gone.
   */
  const submitDictation = useCallback(async (aim: Dictation) => {
    try {
      await sendTerminalInput({ deckId: aim.deckId, agentId: aim.agentId }, VOICE_DICTATION_SUBMIT);
    } catch (cause) {
      setProblem(sentenceOf(cause));
    }
  }, [sendTerminalInput]);

  /**
   * Start the visible countdown to a send, replacing any already running.
   *
   * An interval rather than one timeout, because the number on screen is the
   * whole of what makes this safe: a silent five-second wait and a five-second
   * wait a user can watch and talk over are different features. The interval
   * both redraws and decides, so the two cannot disagree about how long is
   * left.
   */
  const armSend = useCallback((aim: Dictation) => {
    cancelPendingSend();
    let left = Math.max(1, Math.round(VOICE_DICTATION_SEND_MS / VOICE_DICTATION_TICK_MS));
    setSendIn(left);
    sendTimer.current = window.setInterval(() => {
      left -= 1;
      if (left > 0) {
        setSendIn(left);
        return;
      }
      cancelPendingSend();
      void submitDictation(aim);
    }, VOICE_DICTATION_TICK_MS);
  }, [cancelPendingSend, submitDictation]);

  /* A pending send must not survive this panel. The timer is a window timer and
     would otherwise keep running with nothing behind it. */
  useEffect(() => cancelPendingSend, [cancelPendingSend]);

  /** Everything the last utterance left behind, cleared before the next one. */
  const forget = useCallback(() => {
    setProblem(undefined);
    setCapture(undefined);
    setResult(undefined);
    setUndo(undefined);
  }, []);

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
      setPhase("listening");
    } catch (cause) {
      if (!ours()) return;
      setPhase("idle");
      setOn(false);
      setProblem(sentenceOf(cause));
    }
  }, [setOn, setPhase, voiceStart]);

  /**
   * One transcript, resolved and — if it is a command that runs here — run.
   *
   * The screen is stated immediately before the resolve rather than from an
   * effect: a declaration that lagged a navigation would validate this utterance
   * against the screen the user just left, which is the `unavailable` outcome
   * misfiring in the one direction nobody would notice.
   */
  const resolveOne = useCallback(async (utterance: string, ours: () => boolean) => {
    if (!resolveVoice) return;
    /* The screen this utterance was JUDGED against, held for the round trip.
       `unavailable` means "not on that screen", so an outcome is only an
       answer about the screen that was declared with it. */
    const declared = screenRef.current;
    setPhase("resolving");
    try {
      declareVoiceScreen?.(declared);
      const answer = await resolveVoice(utterance);
      // Abandoned, or replaced by a later utterance. Say nothing and run
      // nothing: voice is off, or this belongs to the cycle that replaced it.
      if (!ours()) return;
      if (screenRef.current !== declared) {
        setProblem(SCREEN_MOVED_ON);
        return;
      }
      setResult(answer);
      if (answer.outcome.kind === "dispatch") {
        const dispatched = onDispatch(answer.outcome);
        if (!dispatched) setProblem(NOTHING_DISPATCHED);
        else if (dispatched.undo) setUndo({ run: dispatched.undo });
      }
    } catch (cause) {
      if (ours()) setProblem(sentenceOf(cause));
    }
  }, [declareVoiceScreen, onDispatch, resolveVoice, setPhase]);

  /**
   * One transcript, while the microphone is AIMED at an agent (PRD #802 D6).
   *
   * Three outcomes, in this order, and the order is the precedence decision:
   *
   * 1. **`voice off`** ends everything — dictation and the microphone. Checked
   *    first because stopping must never be the thing that loses a race; see
   *    {@link VOICE_OFF_PHRASES}.
   * 2. **the exit phrase** ends dictation and leaves voice listening for
   *    commands. It does NOT send what is already typed: *"never auto-submits
   *    on exit"* is D6's own requirement, so the words stay in the prompt for
   *    the user to send or edit.
   * 3. **anything else** is typed into the agent's visible prompt, and the
   *    countdown to a send is armed — or re-armed, which is what makes a second
   *    sentence extend the first rather than race it.
   *
   * **The missed exit — the transcriber hearing something else, so the phrase
   * lands in the prompt — is handled by what this does NOT do.** Nothing is
   * submitted for {@link VOICE_DICTATION_SEND_MS}, the countdown says so, and
   * the words are in an input the user is looking at. They see *stop dictation*
   * arrive in the prompt and press the Voice button, which cancels the send and
   * leaves the text there to fix. That is the whole reason the text goes
   * somewhere visible instead of into a buffer.
   */
  const dictateOne = useCallback(async (transcript: string, aim: Dictation, ours: () => boolean) => {
    const phrase = spokenPhrase(transcript);
    if ((VOICE_OFF_PHRASES as readonly string[]).includes(phrase)) {
      cancelPendingSend();
      setDictation(undefined);
      /* Through the registry entry, exactly as the button and the row do.
         Saying it while dictating must not be a third way to stop. */
      VOICE_ACTIONS.stopVoice.run({ stopVoice: stopVoiceRef.current });
      return;
    }
    if ((VOICE_DICTATION_EXIT_PHRASES as readonly string[]).includes(phrase)) {
      cancelPendingSend();
      setDictation(undefined);
      setProblem(VOICE_DICTATION_ENDED);
      return;
    }
    const typed = dictationText(transcript);
    // Nothing to type. The transcription stage already refuses silence, so this
    // is the residual — a transcript that was nothing but control characters —
    // and typing an empty string would arm a send for no reason.
    if (typed === "") return;
    try {
      await sendTerminalInput({ deckId: aim.deckId, agentId: aim.agentId }, typed);
    } catch (cause) {
      if (!ours()) return;
      /* The words did not reach the agent. Dictation ends rather than
         continuing to aim at a pane that is not accepting them — an aimed
         microphone whose words go nowhere is the silent failure this surface
         must not have. */
      cancelPendingSend();
      setDictation(undefined);
      setProblem(sentenceOf(cause));
      return;
    }
    if (!ours()) return;
    armSend(aim);
  }, [armSend, cancelPendingSend, sendTerminalInput, setDictation]);

  /**
   * One whole utterance: close the device, transcribe, resolve, listen again.
   *
   * Serial on purpose — the microphone is shut while the backends are working
   * rather than recording over them. Overlapping would let a second command
   * resolve against a screen the first one is still changing, and the
   * {@link SCREEN_MOVED_ON} guard would then be refusing the user's own
   * sentences. The cost is stated plainly: speech during those seconds is not
   * captured, which is why the report says what it is doing.
   */
  const takeUtterance = useCallback(async () => {
    if (!voiceStop) return;
    const ours = claim();
    forget();
    /* A new utterance has arrived, so whatever was about to be sent is no
       longer the whole of what the user said. The poll below cancels on SPEECH,
       which covers the sentence still being spoken; this covers the gap between
       that sentence ending and its text being appended, during which the poll
       returns early because the phase is no longer `listening`. */
    if (dictationRef.current) cancelPendingSend();
    setPhase("transcribing");
    try {
      const transcription = await voiceStop();
      if (!ours()) return;
      setCapture(transcription.outcome.sentence);
      if (transcription.outcome.kind === "heard") {
        /* The fork this whole mode is: aimed at an agent, an utterance is
           something to type; otherwise it is something to resolve. Read from
           the ref rather than the state for the reason the ref exists — this
           is after an await. */
        const aim = dictationRef.current;
        if (aim) await dictateOne(transcription.outcome.transcript, aim, ours);
        else await resolveOne(transcription.outcome.transcript, ours);
      }
    } catch (cause) {
      if (!ours()) return;
      setProblem(sentenceOf(cause));
    }
    if (!ours()) return;
    await listen(ours);
  }, [cancelPendingSend, claim, dictateOne, forget, listen, resolveOne, setPhase, voiceStop]);

  /** The capped utterance: thrown away unheard, and said so. See {@link VOICE_CAP_DISCARDED}. */
  const discardCapped = useCallback(async () => {
    const ours = claim();
    forget();
    setPhase("opening");
    setProblem(VOICE_CAP_DISCARDED);
    // `voiceCancel` rather than `voiceStop`: stopping would transcribe it,
    // which is the whole of what this path exists to avoid.
    try { await voiceCancel?.(); } catch { /* idempotent and never refused */ }
    if (!ours()) return;
    await listen(ours);
  }, [claim, forget, listen, setPhase, voiceCancel]);

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
    if (dictationRef.current && status.speech && sendTimer.current !== undefined) cancelPendingSend();
    if (status.capped) {
      await discardCapped();
      return;
    }
    if (status.state === "done") await takeUtterance();
  }, [cancelPendingSend, discardCapped, takeUtterance, voiceStatus]);

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
    /* The non-voice escape from dictation, and the one that works when nothing
       is being heard correctly. Cleared before the release rather than after,
       so a pending send cannot fire during it. */
    cancelPendingSend();
    setDictation(undefined);
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
  }, [cancelPendingSend, claim, releasedAfterRefusal, setDictation, setOn, setPhase, voiceCancel]);

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
    void voiceCommands(screenRef.current).then(
      (commands) => { if (vocabularyRequest.current === mine) setVocabulary({ commands }); },
      (cause) => { if (vocabularyRequest.current === mine) setVocabulary({ problem: sentenceOf(cause) }); },
    );
  }, [voiceCommands]);

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
  /*
    `dictateOne` says "voice off" through the registry, and it is defined ABOVE
    `turnOff` — so it reaches the current `stopVoice` through a ref rather than
    through a closure it could not have. Written from an effect for the reason
    `screenRef` is: a ref mutated during render is a write React may discard.
  */
  const stopVoiceRef = useRef(stopVoice);
  useEffect(() => { stopVoiceRef.current = stopVoice; }, [stopVoice]);

  /**
   * Aim the microphone at one agent (PRD #802 D6).
   *
   * The label falls back to the agent id, which is what the surface has when a
   * dispatch carried no resolved label. Naming it badly is better than naming
   * it nothing: the countdown line has to say WHOSE prompt is about to be sent
   * to, and an unnamed one is the case where a user most needs to check.
   */
  const startDictation = useCallback((target: VoiceDispatchTarget) => {
    cancelPendingSend();
    setDictation({ deckId: target.deckId, agentId: target.agentId, label: target.agentLabel ?? target.agentId });
  }, [cancelPendingSend, setDictation]);

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
    const published: Partial<VoicePanelContext> = voiceCommands
      ? { stopVoice, showVoiceCommands, startDictation }
      : { stopVoice, startDictation };
    channel.current = published;
    return () => { channel.current = undefined; };
  });

  if (!resolveVoice) return null;

  const indicator = indicatorFor(known, on, phase);
  const note = progressNote(indicator, phase);
  /*
    The empty state: voice is on and this session has nothing to report yet.

    Keyed on the three report slots rather than on a "have we spoken" flag,
    because that is exactly the question — the hint is what stands in the row
    while none of them holds anything, and it goes the moment one does.
    `forget()` clears all three at the start of each cycle, so it reappears
    between utterances too, which is right: it is a label for an empty row
    rather than a first-run tutorial.
  */
  const emptyState = indicator === "on" && dictation === undefined && problem === undefined && capture === undefined && result === undefined;
  const reporting = note !== undefined || dictation !== undefined || problem !== undefined || capture !== undefined || result !== undefined;

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
    <div className="voice-row" data-testid="voice-row" {...VOICE_PEER_PROPS}>
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
             PRD #802 D10 records for the five unspoken capabilities. */
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
        away from the microphone and back. `forget()` at the start of the next
        cycle is the one thing that clears it.
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
            {dictation && (
              <p className="voice-dictation" data-testid="voice-dictation" role="timer">
                {"Typing to "}
                {displayText(dictation.label, DISPLAY_LIMITS.name)}
                {sendIn === undefined
                  ? " — keep talking."
                  : ` — sending in ${sendIn} s. Keep talking to cancel.`}
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
            {capture && <p className="voice-sentence" title={displayText(capture, DISPLAY_LIMITS.message)}>{displayText(capture, DISPLAY_LIMITS.message)}</p>}
            {result && (
              <>
                <p className="voice-sentence" title={displayText(result.outcome.sentence, DISPLAY_LIMITS.message)}>{displayText(result.outcome.sentence, DISPLAY_LIMITS.message)}</p>
                {/* Never elided and never squeezed out — `flex: 0 0 auto` in the
                    stylesheet. The Undo is a real control with a ten-second life,
                    so a long transcript beside it must lose characters before
                    this loses the button. */}
                <div className="voice-outcome-foot">
                  <span className="voice-cost">{voiceCost(result.backend, result.resolveMs)}</span>
                  {undo && <button className="button secondary compact" onClick={() => { undo.run(); setUndo(undefined); }}><Undo2 size={13} /> Undo</button>}
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
