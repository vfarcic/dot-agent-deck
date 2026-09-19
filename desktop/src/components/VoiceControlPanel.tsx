/**
 * PRD #802 M6 — the voice surface: one control, one dialog, one report.
 *
 * The pipeline is capture → transcribe → resolve → validate → execute → report.
 * Rust owns the middle four ({@link ../lib/bridge!DeckBridge.resolveVoice} and
 * the three microphone verbs beside it); this file owns the two ends — the box
 * and the button an utterance starts at, and the sentence it finishes as.
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
 * The two sentences written here are {@link NOTHING_DISPATCHED} and
 * {@link SCREEN_MOVED_ON}, both for situations Rust structurally cannot know
 * about — one about a registry this process owns, the other about a navigation
 * that happened after the question was asked. See their own notes.
 *
 * # Typed and spoken are ONE path
 *
 * A transcript from the microphone goes into {@link Voice.resolveVoice} exactly
 * as the text of this dialog's own box does — same call, same arguments, same
 * everything downstream. That is not an implementation convenience: `off` is the
 * default transcription backend and is a product statement rather than a
 * degraded mode, so the typed path is the one every user has and the spoken path
 * has to be the same path or it is untested by everybody who has no credential.
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
 * `open_overview` would unmount the panel that was explaining it.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Mic, Square, Undo2, X } from "lucide-react";
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
 * How often the surface asks what the microphone is doing while it is open.
 *
 * **Polling is the only way the surface learns the length cap fired.** The cap
 * releases the device Rust-side with nothing to notify the webview, so from the
 * user's side the microphone simply stops — and a panel that did not ask would
 * go on rendering *Listening…* over a closed device. One second is well inside
 * the 30-second bound it is watching and costs a state read that touches no
 * device.
 */
export const VOICE_STATUS_POLL_MS = 1_000;

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
 * 4.3–6.3 s for the zero-configuration backend — that classification describes
 * a screen nobody is standing on, and running it anyway would act on the new
 * screen with the old screen's permission.
 *
 * It is a sentence rather than silence for {@link NOTHING_DISPATCHED}'s reason:
 * the alternative is *Working out what that means…* vanishing with nothing in
 * its place, which reads as the surface having lost the command.
 */
export const SCREEN_MOVED_ON = "You moved to another screen while that was being worked out, so nothing ran. Say it again here.";

/** The voice half of the runtime, which a runtime may not have at all. */
type Voice = Pick<DeckRuntimeState, "declareVoiceScreen" | "resolveVoice" | "voiceStart" | "voiceStop" | "voiceStatus" | "voiceCancel">;

/** What the surface is doing, which is not the same as what the device is doing. */
type VoicePhase = "idle" | "recording" | "capped" | "transcribing" | "resolving";

interface VoiceControlPanelProps {
  runtime: Voice;
  /** The mounted screen, which is what a command's availability is judged against. */
  screen: VoiceScreen;
  /**
   * Run a resolved dispatch, and answer with how to undo it.
   *
   * The host runs it because the host owns the view: this panel knows an action
   * was dispatched and nothing about where the user was standing when it was.
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

/** Whole seconds left before the cap, never below one — see {@link VoiceControlPanel}. */
function secondsLeft(status: VoiceStatusDto): number {
  return Math.max(1, Math.ceil((status.maxMs - status.capturedMs) / 1_000));
}

/**
 * The trigger, and the dialog it opens.
 *
 * The dialog is mounted only while open, so every reopen starts from a clean
 * report rather than from the last one — the alternative is a panel that has to
 * remember to clear six pieces of state, and forgetting one shows a stale
 * sentence beside a fresh command.
 *
 * **No trigger at all when the runtime cannot resolve an utterance.** Absence is
 * a real state: a control that opened a dialog with nothing behind it would be
 * worse than its absence, and it is the same reasoning the microphone gets one
 * layer down, where `available: false` renders no mic rather than a broken one.
 */
export function VoiceControlPanel({ runtime, screen, onDispatch }: VoiceControlPanelProps) {
  const [open, setOpen] = useState(false);
  /*
    Where the keyboard came from, so it can go back there. A dialog that takes
    focus and drops it on `document.body` when it closes leaves a keyboard user
    with no position at all — the next Tab restarts from the top of the
    document rather than from the control they pressed.
  */
  const trigger = useRef<HTMLButtonElement>(null);
  if (!runtime.resolveVoice) return null;
  return (
    <>
      <button
        ref={trigger}
        className="voice-trigger"
        data-testid="voice-trigger"
        title="Voice control"
        /* A peer of the agent pane rather than background, so `useInertBackground`
           leaves it alone while a pane is open — see that hook. Without it the
           `agent` screen's only command, `close_agent_view`, is dispatched by a
           control the browser will not even let the user click. */
        {...VOICE_PEER_PROPS}
        onClick={() => setOpen(true)}
      ><Mic size={16} /><span>Voice</span></button>
      {/* Focus is restored BEFORE the unmount rather than from a cleanup
          effect: moving it while the dialog is still mounted means it never
          passes through `<body>` at all, so nothing else can claim it in
          between. */}
      {open && <VoiceDialog runtime={runtime} screen={screen} onDispatch={onDispatch} onClose={() => { trigger.current?.focus(); setOpen(false); }} />}
    </>
  );
}

function VoiceDialog({ runtime, screen, onDispatch, onClose }: VoiceControlPanelProps & { onClose: () => void }) {
  const { declareVoiceScreen, resolveVoice, voiceStart, voiceStop, voiceStatus, voiceCancel } = runtime;
  const [draft, setDraft] = useState("");
  const [status, setStatus] = useState<VoiceStatusDto>();
  const [phase, setPhase] = useState<VoicePhase>("idle");
  /** A refused start or stop, as the sentence Rust rejected with. */
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
  /**
   * The screen is read at submit time rather than closed over, so a navigation
   * before the submit cannot stale it — `runUtterance` is a `useCallback` and
   * would otherwise hold whichever screen was mounted when it was built.
   *
   * **A navigation AFTER the submit is a different problem and this does not
   * close it.** Reading it here fixes what the utterance is judged against; it
   * says nothing about the user moving during the round trip, which is what
   * {@link SCREEN_MOVED_ON} and the re-check in `runUtterance` are for.
   *
   * Written from an effect rather than during render: a ref mutated in a render
   * body is a write React is allowed to discard and re-run, and the value is
   * needed at a click, which is always after a commit.
   */
  const screenRef = useRef(screen);
  useEffect(() => { screenRef.current = screen; }, [screen]);

  /**
   * Which in-flight step the surface is still waiting for.
   *
   * The same idea as [`SessionInner::opening`][] one layer down, and for the
   * same finding: an operation that outlives the user's decision to abandon it
   * must be able to tell that it did. A dialog closed mid-resolve used to be a
   * closure still holding `onDispatch`, so the app navigated — or opened an
   * overlay — several seconds after the user dismissed Voice. The window is
   * the ordinary one rather than a contrived race: PRD #802 measured the
   * zero-configuration backend at 4.3–6.3 s per utterance.
   *
   * A counter rather than a boolean because it also has to order two live
   * requests: a superseded one must not write its report over the one that
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
  /* The unmount half. `close` abandons on the way out, which covers every
     route a user takes; this covers the rest — a host that stops offering a
     runtime, a screen teardown — with one line instead of an audit. */
  useEffect(() => abandon, [abandon]);

  /*
    The status the panel opens with, which is what decides whether a microphone
    is offered at all. One read, not a poll: with nothing recording there is
    nothing to watch, and `available` changes only when the user edits the
    settings document — which they cannot do from inside this dialog.
  */
  useEffect(() => {
    if (!voiceStatus) return;
    let live = true;
    void voiceStatus().then((next) => { if (live) setStatus(next); }).catch(() => undefined);
    return () => { live = false; };
  }, [voiceStatus]);

  /*
    The cap watch. It observes and does not act: reaching the cap must NOT
    auto-transcribe, because a voice command that ran thirty seconds is almost
    certainly a microphone somebody left open, and transcribing it would spend a
    paid call and its latency on that accident every time. So the poll stops the
    panel claiming to listen and hands the user the choice of sending it or
    discarding it.

    `capped` is the only thing it reacts to. A status that merely disagrees about
    the state is not evidence the recording ended — the reply can be a beat
    behind the start that has just been made — and treating it as such would tear
    down a live recording the user is still speaking into.
  */
  useEffect(() => {
    if (!voiceStatus || phase !== "recording") return;
    let live = true;
    const timer = window.setInterval(() => {
      void voiceStatus().then((next) => {
        if (!live) return;
        setStatus(next);
        if (next.capped) setPhase("capped");
      }).catch(() => undefined);
    }, VOICE_STATUS_POLL_MS);
    return () => { live = false; window.clearInterval(timer); };
  }, [voiceStatus, phase]);

  useEffect(() => {
    if (!undo) return;
    const timer = window.setTimeout(() => setUndo(undefined), VOICE_UNDO_WINDOW_MS);
    return () => window.clearTimeout(timer);
  }, [undo]);

  /** Everything the last utterance left behind, cleared before the next one. */
  const forget = useCallback(() => {
    setProblem(undefined);
    setResult(undefined);
    setUndo(undefined);
  }, []);

  /**
   * One utterance, whichever end of the pipeline it came from.
   *
   * The screen is stated immediately before the resolve rather than from an
   * effect: a declaration that lagged a navigation would validate this utterance
   * against the screen the user just left, which is the `unavailable` outcome
   * misfiring in the one direction nobody would notice.
   */
  const runUtterance = useCallback(async (utterance: string) => {
    if (!resolveVoice) return;
    const ours = claim();
    /* The screen this utterance was JUDGED against, held for the round trip.
       `unavailable` means "not on that screen", so an outcome is only an
       answer about the screen that was declared with it. */
    const declared = screenRef.current;
    setPhase("resolving");
    try {
      declareVoiceScreen?.(declared);
      const answer = await resolveVoice(utterance);
      // Abandoned, or replaced by a later utterance. Say nothing and run
      // nothing: the dialog is gone, or belongs to the request that replaced
      // this one.
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
      if (!ours()) return;
      setProblem(sentenceOf(cause));
    } finally {
      if (ours()) setPhase("idle");
    }
  }, [claim, declareVoiceScreen, onDispatch, resolveVoice]);

  const submitTyped = () => {
    if (!draft.trim() || phase === "resolving") return;
    forget();
    setCapture(undefined);
    const utterance = draft;
    setDraft("");
    void runUtterance(utterance);
  };

  /*
    The phase moves to `recording` BEFORE the device answers, and reverts if it
    refuses. Opening a microphone is a round trip to the OS that can prompt, so
    a control that waited for it would sit dead under the press that started it.
  */
  const startListening = async () => {
    if (!voiceStart) return;
    forget();
    setCapture(undefined);
    setPhase("recording");
    try {
      setStatus(await voiceStart());
    } catch (cause) {
      setPhase("idle");
      setProblem(sentenceOf(cause));
    }
  };

  /** Close the device, transcribe, and send a transcript on down the one path. */
  const stopListening = async () => {
    if (!voiceStop) return;
    forget();
    // The recording being closed is about to have a sentence of its own, and a
    // stop that is refused must not leave the previous one standing as if it
    // described this one.
    setCapture(undefined);
    /* Claimed here as well as inside `runUtterance`, because transcription is
       the FIRST await and a panel closed during it would otherwise reach the
       resolve through a `runUtterance` that then claims the surface afresh —
       the abandoned request laundering itself into a live one. */
    const ours = claim();
    setPhase("transcribing");
    try {
      const transcription = await voiceStop();
      if (!ours()) return;
      setCapture(transcription.outcome.sentence);
      setPhase("idle");
      if (transcription.outcome.kind === "heard") await runUtterance(transcription.outcome.transcript);
    } catch (cause) {
      if (!ours()) return;
      setPhase("idle");
      setProblem(sentenceOf(cause));
    }
  };

  const discardRecording = () => {
    setPhase("idle");
    void voiceCancel?.().then(setStatus).catch(() => undefined);
  };

  /*
    Cancel on the way out, always and unconditionally. It is idempotent and never
    refused precisely so a caller does not have to know which state it is in —
    and the state this is protecting against is the one where the panel's own
    phase disagrees with the device, which is exactly when a hidden open
    microphone would survive.
  */
  const close = () => {
    /* The webview half of the same cancel. `voiceCancel` releases the DEVICE;
       this releases the pipeline behind it, which the device has no say over —
       a transcription or a resolution already handed to a backend arrives
       whatever the microphone does. */
    abandon();
    void voiceCancel?.().catch(() => undefined);
    onClose();
  };

  const microphone = status?.available === true && voiceStart !== undefined && voiceStop !== undefined;
  const recording = phase === "recording";

  return (
    /* Marked on the BACKDROP rather than on the panel: the backdrop is the
       element the pane's walk meets as a sibling, and `inert` is inherited, so a
       marked panel inside an inerted backdrop would still be dead. */
    <div className="dialog-backdrop" role="presentation" {...VOICE_PEER_PROPS} onMouseDown={close}>
      <section
        className="voice-panel"
        role="dialog"
        /* NOT modal, stated rather than left to the default, because the
           choice is deliberate and the opposite was shipped.

           `aria-modal="true"` tells assistive technology that everything
           outside the dialog is unavailable. This dialog inerts nothing: it is
           a PEER of the agent pane by the exemption `useInertBackground`
           documents, which exists so the `agent` screen's one command,
           `close_agent_view`, is reachable by the surface that dispatches it.
           An open agent pane therefore stays interactive beside this, and so
           does the screen behind it. Claiming modality would describe an
           interface that is not the one here, and the remedy is to correct the
           claim rather than to withdraw the exemption — withdrawing it makes
           the whole voice surface inert whenever a pane is open.

           A non-modal dialog is also not obliged to trap focus, which is why
           there is none: the input takes focus on open and `Tab` leaves, the
           way it does out of any non-modal dialog. What closing it owes is the
           keyboard position back, which the trigger's `ref` above provides. */
        aria-modal="false"
        aria-label="Voice control"
        data-testid="voice-panel"
        onMouseDown={(event) => event.stopPropagation()}
        /* Escape on the dialog itself rather than at `window`: `DeckShell` binds
           a window listener for an open agent pane, and one key must not close
           two surfaces. `stopPropagation` on the synthetic event stops the
           native one, so the pane's listener never sees it. */
        onKeyDown={(event) => { if (event.key === "Escape") { event.stopPropagation(); close(); } }}
      >
        <header className="voice-head">
          <h2>Voice control</h2>
          <button className="icon-button" aria-label="Close voice control" onClick={close}><X size={16} /></button>
        </header>

        <div className="voice-entry">
          <label htmlFor="voice-command">Command</label>
          <input
            id="voice-command"
            type="text"
            autoFocus
            autoComplete="off"
            spellCheck={false}
            placeholder="Say what you want — show me every agent"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => { if (event.key === "Enter") submitTyped(); }}
          />
          <div className="voice-entry-actions">
            <button className="button primary compact" disabled={phase === "resolving"} onClick={submitTyped}>Run command</button>
            {/* The mic is absent rather than disabled when transcription is off:
                `off` is a product statement and a greyed-out microphone reads as
                a fault. The line below says what to do instead. */}
            {microphone && recording && <button className="button secondary compact" onClick={() => void stopListening()}><Square size={13} /> Stop listening</button>}
            {/* Offered from `idle` alone. While the cap block below is up it owns
                the choice, and a second control there would let the user start a
                new recording over the one they have not decided about. */}
            {microphone && phase === "idle" && <button className="button secondary compact" onClick={() => void startListening()}><Mic size={13} /> Start listening</button>}
          </div>
        </div>

        {!microphone && <p className="voice-note">Transcription is off — type a command, or pick a backend under Voice in Settings.</p>}

        {recording && status && (
          <div className="voice-listening">
            <span className="voice-pulse" aria-hidden="true" />
            <span>Listening…</span>
            <span>{secondsLeft(status)} s left</span>
          </div>
        )}

        {phase === "capped" && status && (
          <div className="voice-capped">
            {/* The cap stopped the device; it did not decide what to do with
                what it caught. Both buttons are offered because only the user
                knows whether thirty seconds was a command or a forgotten mic. */}
            <p>The microphone stopped at its {Math.round(status.maxMs / 1_000)} s limit.</p>
            <div>
              <button className="button primary compact" onClick={() => void stopListening()}>Send the recording</button>
              <button className="button secondary compact" onClick={discardRecording}>Discard it</button>
            </div>
          </div>
        )}

        {phase === "transcribing" && <p className="voice-note">Turning that into text…</p>}
        {phase === "resolving" && <p className="voice-note">Working out what that means…</p>}

        <div className="voice-report" data-testid="voice-report">
          {/*
            Each sentence is its own element holding nothing else, so the
            transcript inside it survives to the DOM exactly as Rust rendered it
            — punctuation, casing, inner quotes and all. Seeing what was heard is
            what turns a mis-transcription into a correction the user can make.

            `displayText` is the render seam every free-form string in this app
            crosses: a sentence embeds a transcript, and a transcript is
            model- or microphone-supplied text with control and bidi codepoints
            still in it. It sanitises and bounds; it never rewords.

            The `message` budget, 240 characters, is the one written for "the
            daemon's own connection message" — one sentence, once per screen,
            which is this shape exactly. It is a real bound rather than a
            formality: thirty seconds of speech is around 700 characters, so a
            rambling utterance's no-match sentence IS elided here, with the
            clamp's own marker so it never passes itself off as complete. That
            is the intended reading of a 700-character command.
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
      </section>
    </div>
  );
}
