/**
 * PRD #1497 M5 — reading mode's webview half: which agent is being read, when
 * that ends, and what the app says about it.
 *
 * "reading on" in an agent's pane starts it for THAT agent (D11): the Rust
 * side subscribes to the agent's turn events and sends back one finished
 * sentence per event — a turn's summary, or a permission prompt or error
 * announced at once — and this hands each to the speech queue. The agent's
 * reply never reaches here; only the sentence does.
 *
 * # When it ends (D11)
 *
 * "reading off", opening a different agent's pane, closing the pane, and
 * "voice off" — and, from the Rust side, the Settings opt-in being turned off
 * (an `ended` sentence). Each ends it the same way: what the app is saying is
 * cut off and everything queued is dropped, then it says "Reading off.", then
 * the subscription is stopped. None of them carries anything over: a sentence
 * from the ended session is never queued again, and a later "reading on"
 * starts from that moment, never with a backlog (D3).
 *
 * The agent's events ending — it exited, or its deck went away — arrives as a
 * `closed` sentence, after the last summary. That ends reading too, at once:
 * the indicator goes off, nothing more from the session is accepted, and
 * "Reading off." is queued. But nothing is cut off: what is being said and
 * what is already queued — that last summary among it — are heard first.
 *
 * Until that speech has finished the session is *draining*, and everything
 * that ends live reading still cuts the drain off the same way (PR #1617's
 * review): "reading off" (answered as reading off, not as "Reading is not
 * on."), "voice off", another pane, and the Settings opt-in turned off
 * ({@link ReadingMode.consentOff}). "stop" and "quiet" silence it as they
 * silence any speech.
 *
 * # What it does NOT end
 *
 * "stop" and "quiet" silence the app's speech and leave reading on: the next
 * turn is read as usual.
 */

import type { SpeechQueue } from "./speech";

/** The agent reading is on for — the pane on screen when it started. */
export interface ReadingTarget {
  deckId: string;
  agentId: string;
  /** What the deck calls the agent; named in every sentence (D10). */
  label: string;
}

/** What `desktop_voice_reading_start` answers. */
export type ReadingStartDto =
  | { kind: "started"; session: number }
  | { kind: "not_enabled"; sentence: string }
  | { kind: "unavailable"; sentence: string };

/**
 * One sentence to speak, from `desktop_voice_reading_start`'s channel — or,
 * as `ended`, word that the Rust side ended the session (the Settings opt-in
 * was turned off), or, as `closed`, that the agent's events ended (it
 * exited); either ends reading mode here instead of being spoken.
 */
export interface ReadingSentenceDto {
  kind: "turn" | "permission" | "blocked" | "ended" | "closed";
  text: string;
}

/** Why reading ended. */
export type ReadingEnd = "asked" | "pane" | "voice_off";

/** Spoken when reading starts. */
export const READING_ON = "Reading on.";
/** Spoken when reading ends, whatever ended it (D11). */
export const READING_OFF = "Reading off.";
/** Shown when "reading off" is said with reading already off. */
export const READING_ALREADY_OFF = "Reading is not on.";
/** Shown when "reading on" is said for the agent already being read. */
export const READING_ALREADY_ON = "Reading is already on.";
/** Spoken when the start itself failed — the app could not ask, or got no answer. */
export const READING_START_FAILED = "Reading did not start. Say reading on to try again.";

/**
 * The speech queue's key for the mode's own sentences ("Reading on.", the
 * refusals), so a turn summary waiting for an agent never replaces one.
 */
export const READING_VOICE_KEY = "\u0000reading";

export interface ReadingModeDeps {
  /** Subscribe to `target`'s turn events; each finished sentence arrives on `onSentence`. */
  start: (target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => Promise<ReadingStartDto>;
  /** End the subscription `start` answered with `session`. */
  stop: (session: number) => Promise<void>;
  speech: Pick<SpeechQueue, "say" | "interrupt" | "speaking" | "subscribe">;
  /** Told whenever the agent being read changes, for the pane's indicator. */
  onChange?: (target: ReadingTarget | undefined) => void;
}

/** What {@link ReadingMode.turnOn} did, as the sentence the voice row shows. */
export type ReadingTurnOn =
  | { kind: "started"; sentence: string }
  | { kind: "refused"; sentence: string }
  /** Ended before the start answered — another pane opened, voice went off. */
  | { kind: "abandoned" };

function sameAgent(a: { deckId: string; agentId: string } | undefined, b: { deckId: string; agentId: string } | undefined): boolean {
  return a !== undefined && b !== undefined && a.deckId === b.deckId && a.agentId === b.agentId;
}

/**
 * Reading mode for one window: at most one agent at a time.
 *
 * Every start and end bumps a generation, so an answer that arrives after the
 * mode moved on — a start that resolves after the pane closed — is undone
 * rather than applied, and a sentence from a subscription that has since
 * ended is not spoken.
 */
export class ReadingMode {
  private current: { target: ReadingTarget; session: number } | undefined;
  private starting: ReadingTarget | undefined;
  /** The agent whose events closed while its last sentences are still being said. */
  private draining: ReadingTarget | undefined;
  private generation = 0;
  private alerts = 0;

  constructor(private readonly deps: ReadingModeDeps) {
    // The drain is over once the queue has nothing left to say.
    deps.speech.subscribe((speaking) => {
      if (!speaking) this.draining = undefined;
    });
  }

  /** The agent being read, if reading is on. */
  get target(): ReadingTarget | undefined {
    return this.current?.target;
  }

  /** Whether reading is on. */
  get on(): boolean {
    return this.current !== undefined;
  }

  /**
   * Whether reading is on, starting, or draining — its agent's events closed
   * and its last sentences are still being said. What "reading off" ends.
   */
  get active(): boolean {
    return this.current !== undefined || this.starting !== undefined || this.draining !== undefined;
  }

  /**
   * Start reading `target`. Ends reading for any other agent first (D11:
   * one agent at a time). A refusal — the Settings opt-in off, or reading not
   * available for this agent — is spoken, and reading stays off. So is a start
   * that failed outright ({@link READING_START_FAILED}): this never rejects.
   */
  async turnOn(target: ReadingTarget): Promise<ReadingTurnOn> {
    if (sameAgent(this.current?.target, target)) return { kind: "started", sentence: READING_ALREADY_ON };
    if (this.current !== undefined) await this.end(false);
    // A drain is left to finish: those sentences are already queued, and the
    // new session's own ending cuts them off with everything else.
    this.draining = undefined;
    const generation = ++this.generation;
    this.starting = target;
    let answer: ReadingStartDto;
    try {
      answer = await this.deps.start(target, (sentence) => {
        if (this.generation !== generation) return;
        if (sentence.kind === "ended") {
          void this.end(true);
          return;
        }
        if (sentence.kind === "closed") {
          // Nothing cut off: the last summary is already queued, and is
          // heard as a drain that everything ending reading still ends.
          void this.end(true, true);
          return;
        }
        /* A turn's summary replaces a waiting summary for the same agent
           (D6); each permission prompt or error waits under a key of its own,
           so nothing later drops what the user has to act on. */
        const agent = `${target.deckId}\u0000${target.agentId}`;
        const key = sentence.kind === "turn" ? agent : `${agent}\u0000alert\u0000${++this.alerts}`;
        this.deps.speech.say(key, sentence.text);
      });
    } catch {
      /* The start failed outright (PR #1617's review): no session was made,
         so reading stays off and nothing marks the pane. */
      if (this.generation !== generation) return { kind: "abandoned" };
      this.starting = undefined;
      this.deps.speech.say(READING_VOICE_KEY, READING_START_FAILED);
      return { kind: "refused", sentence: READING_START_FAILED };
    } finally {
      if (this.generation === generation) this.starting = undefined;
    }
    if (answer.kind !== "started") {
      if (this.generation !== generation) return { kind: "abandoned" };
      this.deps.speech.say(READING_VOICE_KEY, answer.sentence);
      return { kind: "refused", sentence: answer.sentence };
    }
    if (this.generation !== generation) {
      await this.deps.stop(answer.session).catch(() => undefined);
      return { kind: "abandoned" };
    }
    this.current = { target, session: answer.session };
    this.deps.onChange?.(target);
    this.deps.speech.say(READING_VOICE_KEY, READING_ON);
    return { kind: "started", sentence: READING_ON };
  }

  /** "reading off". Answers the sentence to show: {@link READING_OFF}, or {@link READING_ALREADY_OFF}. */
  async turnOff(): Promise<string> {
    if (!this.active) return READING_ALREADY_OFF;
    await this.end(true);
    return READING_OFF;
  }

  /** "voice off": ends reading, saying so, if it was on. */
  async voiceOff(): Promise<void> {
    if (this.active) await this.end(true);
  }

  /**
   * The Settings opt-in was turned off. The Rust side ends a live session
   * itself (an `ended` sentence); this also reaches a start in progress and a
   * drain, which have no session there to end.
   */
  async consentOff(): Promise<void> {
    if (this.active) await this.end(true);
  }

  /**
   * The pane on screen changed. Reading ends unless `pane` is still the agent
   * it was turned on for — another agent's pane and no pane both end it.
   */
  async paneChanged(pane: { deckId: string; agentId: string } | undefined): Promise<void> {
    const reading = this.current?.target ?? this.starting ?? this.draining;
    if (reading === undefined || sameAgent(reading, pane)) return;
    await this.end(true);
  }

  /**
   * The voice surface is going away: end any session and silence the app,
   * without announcing it — nothing is left on screen to say it to.
   */
  async dispose(): Promise<void> {
    await this.end(false);
  }

  /** "stop" / "quiet": silence the app now. Reading stays on. */
  quiet(): void {
    this.deps.speech.interrupt();
  }

  /**
   * End the session, in this order: bump the generation so no late sentence
   * from it is queued again; cut off what is being said and drop everything
   * queued — a drain's included — unless `drain` (the agent's events ended,
   * and what was already handed to the speech queue is heard, as a drain);
   * then say "Reading off." (when `announce`); then stop the subscription.
   */
  private async end(announce: boolean, drain = false): Promise<void> {
    this.generation += 1;
    this.starting = undefined;
    this.draining = undefined;
    const ended = this.current;
    this.current = undefined;
    if (ended !== undefined) this.deps.onChange?.(undefined);
    if (!drain) this.deps.speech.interrupt();
    if (announce) this.deps.speech.say(READING_VOICE_KEY, READING_OFF);
    if (drain && ended !== undefined && this.deps.speech.speaking) this.draining = ended.target;
    if (ended !== undefined) await this.deps.stop(ended.session);
  }
}
