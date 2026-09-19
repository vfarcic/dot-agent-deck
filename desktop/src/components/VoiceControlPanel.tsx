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
 * {@link VOICE_CAP_DISCARDED} and {@link VOICE_RELEASE_REFUSED}. The first two
 * are for situations Rust structurally cannot know about; the last three are
 * about the surface's own state machine rather than about an utterance. See
 * their own notes.
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
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Mic, MicOff, Undo2 } from "lucide-react";
import { DISPLAY_LIMITS, displayText } from "../lib/displayText";
import { VOICE_PEER_PROPS } from "../hooks/useInertBackground";
import type { VoiceOutcomeDto, VoiceResultDto, VoiceScreen, VoiceStatusDto } from "../lib/bridge";
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
 * added latency against a pipeline PRD #802 measured at 4.3-6.3 s. That is under
 * 3%, and it is an eighth of `voice::SILENCE_HOLD`, which is the term that
 * actually dominates the end of an utterance. The event path stays available if
 * the hold ever gets short enough for this to matter.
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
 * resolve, so `unavailable` means *not on that screen*. When the user walks to
 * another one during the several seconds a backend takes — PRD #802 measured
 * 4.3-6.3 s for the zero-configuration backend — that classification describes
 * a screen nobody is standing on, and running it anyway would act on the new
 * screen with the old screen's permission.
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

/** The voice half of the runtime, which a runtime may not have at all. */
type Voice = Pick<DeckRuntimeState, "declareVoiceScreen" | "resolveVoice" | "voiceStart" | "voiceStop" | "voiceStatus" | "voiceCancel">;

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
export function VoiceControlPanel({ runtime, screen, onDispatch }: VoiceControlPanelProps) {
  const { declareVoiceScreen, resolveVoice, voiceStart, voiceStop, voiceStatus, voiceCancel } = runtime;

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
   * overlay — several seconds after the user stopped it. The window is the
   * ordinary one rather than a contrived race: PRD #802 measured the
   * zero-configuration backend at 4.3-6.3 s per utterance.
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
      try {
        const status = await ask();
        if (ours() && stillHeld(status.state)) await cancelRef.current?.();
      } catch {
        // A status read or a release that failed says nothing about the device,
        // and there is no press to report it against. `turnOn` reconciles again
        // at the next one.
      }
      if (ours()) setKnown(true);
    })();
  }, [claim]);

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
    setPhase("transcribing");
    try {
      const transcription = await voiceStop();
      if (!ours()) return;
      setCapture(transcription.outcome.sentence);
      if (transcription.outcome.kind === "heard") await resolveOne(transcription.outcome.transcript, ours);
    } catch (cause) {
      if (!ours()) return;
      setProblem(sentenceOf(cause));
    }
    if (!ours()) return;
    await listen(ours);
  }, [claim, forget, listen, resolveOne, setPhase, voiceStop]);

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
    if (status.capped) {
      await discardCapped();
      return;
    }
    if (status.state === "done") await takeUtterance();
  }, [discardCapped, takeUtterance, voiceStatus]);

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
        } catch {
          // Whatever is left, the start below runs into it and reports it with
          // Rust's own sentence, which is a better answer than one invented
          // here from a release that failed.
        }
        if (!ours()) return;
      }
    }
    setOn(true);
    await listen(ours);
  }, [claim, forget, listen, setOn, setPhase, voiceStart, voiceStatus, voiceStop]);

  /**
   * After a release Rust refused: is the device gone anyway?
   *
   * A rejection says the CALL failed, not what it did — the session's own
   * `cancel` is idempotent and never refused, so a rejection is a blocking join
   * that did not complete or a command that never reached the session. Asking
   * is the only way to tell the two apart, and the unknown answers all resolve
   * to *not released*: this is the one place where guessing in the direction
   * that suits the button would put *Voice off* over a live microphone.
   */
  const releasedAfterRefusal = useCallback(async () => {
    if (!voiceStatus) return false;
    try {
      return !stillHeld((await voiceStatus()).state);
    } catch {
      return false;
    }
  }, [voiceStatus]);

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
  }, [claim, releasedAfterRefusal, setOn, setPhase, voiceCancel]);

  if (!resolveVoice) return null;

  const indicator = indicatorFor(known, on, phase);
  const note = progressNote(indicator, phase);
  const reporting = note !== undefined || problem !== undefined || capture !== undefined || result !== undefined;

  return (
    <>
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
        /* A peer of the agent pane rather than background, so `useInertBackground`
           leaves it alone while a pane is open — see that hook. Without it the
           `agent` screen's only command, `close_agent_view`, is dispatched by a
           control the browser will not even let the user click. */
        {...VOICE_PEER_PROPS}
        /* Serialised rather than raced. While a release is in flight the device
           is Rust's, and a press that started a new recording over it would be
           the same lie from the other direction — or, if it landed as a second
           cancel, would race the first one's answer. Every other state is
           pressable, `unreleased` included: that press is the retry. */
        onClick={() => {
          if (phaseRef.current === "stopping") return;
          if (onRef.current) void turnOff();
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
        The report lives beside the button rather than in a dialog, which is
        what makes continuous voice possible at all: a dialog would have to be
        dismissed between utterances, and the press that dismissed it is the
        press that turns voice off.

        Always mounted, and a peer for the trigger's reason — the Undo inside it
        is a control, and a control behind an open agent pane has to be
        clickable or the report is a picture of one.

        `role="status"` rather than `alert`: the sentences are the result of
        something the user just did, so they are announced at the next pause
        instead of interrupting. The region is in the DOM before any of them
        arrives, which is what makes the announcement reliable.
      */}
      <div
        className="voice-report"
        data-testid="voice-report"
        role="status"
        aria-live="polite"
        {...VOICE_PEER_PROPS}
      >
        {reporting && (
          <div className="voice-report-card">
            {note && <p className="voice-note">{note}</p>}
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
            */}
            {problem && <p className="voice-sentence">{displayText(problem, DISPLAY_LIMITS.message)}</p>}
            {capture && <p className="voice-sentence">{displayText(capture, DISPLAY_LIMITS.message)}</p>}
            {result && (
              <>
                <p className="voice-sentence">{displayText(result.outcome.sentence, DISPLAY_LIMITS.message)}</p>
                <div className="voice-outcome-foot">
                  <span className="voice-cost">{voiceCost(result.backend, result.resolveMs)}</span>
                  {undo && <button className="button secondary compact" onClick={() => { undo.run(); setUndo(undefined); }}><Undo2 size={13} /> Undo</button>}
                </div>
              </>
            )}
          </div>
        )}
      </div>
    </>
  );
}
