import { describe, expect, it, vi } from "vitest";
import type { SpeechPlanDto } from "./bridge";
import {
  BUSY_RETRY_MS,
  CAPACITY_FREED_RETRY_MS,
  DeckReader,
  EXITED_DRAIN_MS,
  isThisMachine,
  NOT_ENABLED_RETRY_MS,
  READING_OFF,
  READING_ON,
  READING_VOICE_KEY,
  readingKey,
  readingNotice,
  readingStartFailed,
  type ReadingAgent,
  type ReadingSentenceDto,
  type ReadingStartDto,
  type ReadingTarget,
} from "./reading";
import noticeCases from "./readingNoticeCases.json";
import { SpeechQueue, SpeechRefusedError, wordedNow, type SpeechText, type SpeechVoice } from "./speech";

const TESTER: ReadingAgent = { deckId: "deck-local", agentId: "tester", label: "tester", incarnation: 1 };
const CODER: ReadingAgent = { deckId: "deck-local", agentId: "coder", label: "coder", incarnation: 2 };
const BUILDER: ReadingAgent = { deckId: "deck-build", agentId: "builder", label: "builder", incarnation: 3 };

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/** The words a queued sentence will be said with, asked now. */
const words = (text: SpeechText) => wordedNow(text);

/**
 * A reader over recorded fakes. Each start answers `answer(target)` (started,
 * with a session per call, by default) and is recorded with its sink.
 */
function harness(answer?: (target: ReadingTarget) => ReadingStartDto | Promise<ReadingStartDto>) {
  const said: [string, string][] = [];
  const speech = {
    say: vi.fn((key: string, text: SpeechText) => { said.push([key, words(text)]); }),
    interrupt: vi.fn(),
    drop: vi.fn((_match: (key: string) => boolean) => undefined),
  };
  /* Retries the reader schedules, run only when the test says so. */
  const timers: { ms: number; run: () => void; cancelled: boolean }[] = [];
  const schedule = (run: () => void, ms: number) => {
    const timer = { ms, run, cancelled: false };
    timers.push(timer);
    return () => { timer.cancelled = true; };
  };
  const sinks = new Map<string, (sentence: ReadingSentenceDto) => void>();
  let sessions = 0;
  const start = vi.fn(async (target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => {
    sinks.set(target.agentId, onSentence);
    return answer ? answer(target) : { kind: "started" as const, session: ++sessions };
  });
  const stop = vi.fn(async (_session: number) => undefined);
  const problems: string[] = [];
  let open: { deckId: string; agentId: string } | undefined;
  const reader = new DeckReader({ start, stop, speech, openPane: () => open, onProblem: (sentence) => problems.push(sentence), schedule });
  return {
    reader, said, speech, start, stop, sinks, problems,
    /** The retries waiting to run, by delay. */
    waiting: () => timers.filter((timer) => !timer.cancelled).map((timer) => timer.ms),
    /** Run every retry waiting now. */
    runTimers: () => {
      const due = timers.splice(0).filter((timer) => !timer.cancelled);
      for (const timer of due) timer.run();
    },
    open: (agent: ReadingTarget | undefined) => { open = agent; },
    started: () => start.mock.calls.map(([target]) => target.agentId),
  };
}

describe("DeckReader (PRD #1497, decisions 1–3 of 2026-10-09)", () => {
  /** Scenario: Reading is turned on with two agents on the deck; both are subscribed, the app says "Reading on.", and each agent's sentence is queued under that agent. */
  it("reads every agent on the deck once the switch is on", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    expect(h.started()).toEqual(["tester", "coder"]);
    expect(h.said).toEqual([[READING_VOICE_KEY, READING_ON]]);
    expect(h.reader.on).toBe(true);
    h.sinks.get("coder")!({ kind: "turn", text: "The coder finished: done.", bare: "Finished: done." });
    expect(h.said.at(-1)).toEqual([readingKey(CODER), "The coder finished: done."]);
  });

  /** Scenario: the app opens with Reading already on; the agents are read, and nothing is announced — the switch did not change. */
  it("says nothing for the switch it starts with", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false);
    await flush();
    expect(h.started()).toEqual(["tester"]);
    expect(h.said).toEqual([]);
  });

  /** Scenario: with Reading off nothing is subscribed, whatever agents the deck has. */
  it("reads nothing while the switch is off", async () => {
    const h = harness();
    h.reader.update(false, [TESTER, CODER]);
    await flush();
    expect(h.start).not.toHaveBeenCalled();
    expect(h.reader.on).toBe(false);
  });

  /** Scenario: while Reading is on an agent appears and is read; another goes and its session is stopped; nothing is announced for either. */
  it("adds agents as they appear and drops them as they go", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false);
    await flush();
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    expect(h.started()).toEqual(["tester", "coder"]);
    h.reader.update(true, [CODER]);
    await flush();
    expect(h.stop).toHaveBeenCalledWith(1);
    expect(h.stop).toHaveBeenCalledTimes(1);
    expect(h.said).toEqual([]);
  });

  /** Scenario: the selected deck changes; the old deck's agents are stopped and the new deck's agents are read, and reading stays on. */
  it("follows a change of deck", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.reader.update(true, [BUILDER]);
    await flush();
    expect(h.stop.mock.calls.map(([session]) => session).sort()).toEqual([1, 2]);
    expect(h.started()).toEqual(["tester", "coder", "builder"]);
    expect(h.reader.on).toBe(true);
  });

  /** Scenario: an agent is replaced under the same id (a new incarnation); the old session is stopped and the new one read. */
  it("reads a replaced agent afresh", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false);
    await flush();
    h.reader.update(true, [{ ...TESTER, incarnation: 9 }]);
    await flush();
    expect(h.stop).toHaveBeenCalledWith(1);
    expect(h.started()).toEqual(["tester", "tester"]);
  });

  /** Scenario: Reading is turned off; what is being said is cut off, the queue dropped, "Reading off." said, and every session stopped. A late sentence from a stopped session is never spoken. */
  it("ends every session when the switch turns off", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    const late = h.sinks.get("tester")!;
    h.reader.update(false, [TESTER, CODER]);
    expect(h.speech.interrupt).toHaveBeenCalledTimes(1);
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
    expect(h.stop.mock.calls.map(([session]) => session).sort()).toEqual([1, 2]);
    late({ kind: "turn", text: "The tester finished: late news." });
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
    expect(h.reader.on).toBe(false);
  });

  /** Scenario: Reading is turned off while an agent's start is still being answered; when it answers, its session is stopped at once and nothing from it is spoken. */
  it("undoes a start that answers after reading ended", async () => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const h = harness(async () => {
      await gate;
      return { kind: "started", session: 5 };
    });
    h.reader.update(true, [TESTER]);
    await flush();
    h.reader.update(false, [TESTER]);
    release();
    await flush();
    expect(h.stop).toHaveBeenCalledWith(5);
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: late." });
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
  });

  /** Scenario: opening a pane, closing it or changing screens changes neither the switch nor the deck's agents, so reading is not ended and "Reading off." is never said. */
  it("does not end when only the pane on screen changes", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.open(TESTER);
    h.reader.update(true, [TESTER, CODER]);
    h.open(undefined);
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    expect(h.stop).not.toHaveBeenCalled();
    expect(h.start).toHaveBeenCalledTimes(2);
    expect(h.said).toEqual([]);
  });

  /** Scenario: an agent exits — its events close after its last summary; that agent's session ends without cutting off speech, it is not subscribed again while still listed, and the other agent is read on. */
  it("drops an agent whose events close, without cutting speech off", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: the last turn." });
    h.sinks.get("tester")!({ kind: "closed", text: "Reading off." });
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    expect(h.speech.interrupt).not.toHaveBeenCalled();
    expect(h.said).toEqual([[readingKey(TESTER), "The tester finished: the last turn."]]);
    expect(h.start).toHaveBeenCalledTimes(2);
    expect(h.reader.on).toBe(true);
  });

  /** Scenario: the Rust side ends every session because a save turned the switch off; reading ends here, saying "Reading off." once however many sessions said so. */
  it("ends once when the Rust side ends its sessions", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.sinks.get("tester")!({ kind: "ended", text: "Reading off." });
    h.sinks.get("coder")!({ kind: "ended", text: "Reading off." });
    h.reader.consentOff();
    expect(h.said).toEqual([[READING_VOICE_KEY, READING_OFF]]);
    expect(h.reader.on).toBe(false);
  });

  /** Scenario: a start reads the settings before the save that turned Reading on reached the disk and is refused as not enabled; the save's consent-on report starts it again, saying nothing more. */
  it("retries a start refused before its save reached the disk", async () => {
    let enabled = false;
    let session = 0;
    const h = harness(() => (enabled ? { kind: "started", session: ++session } : { kind: "not_enabled", sentence: "Reading is off." }));
    h.reader.update(true, [TESTER]);
    await flush();
    expect(h.started()).toEqual(["tester"]);
    h.reader.update(true, [TESTER]);
    await flush();
    expect(h.start).toHaveBeenCalledTimes(1);
    enabled = true;
    h.reader.consentOn();
    await flush();
    expect(h.started()).toEqual(["tester", "tester"]);
    expect(h.said).toEqual([[READING_VOICE_KEY, READING_ON]]);
    expect(h.problems).toEqual([]);
  });

  /** Scenario (audit A1): Reading is turned on and a start reads the settings before the save reached the disk; the save's consent-on report arrives while that start is still being answered, and the start then answers that Reading is off. It is asked again at once, rather than waiting for a report that already came. */
  it("retries a refusal answered after the save already reported Reading on", async () => {
    let answerFirst!: (answer: ReadingStartDto) => void;
    let calls = 0;
    const h = harness(() => {
      calls += 1;
      if (calls === 1) return new Promise<ReadingStartDto>((resolve) => { answerFirst = resolve; });
      return { kind: "started", session: 9 };
    });
    h.reader.update(true, [TESTER], false);
    await flush();
    h.reader.consentOn();
    await flush();
    expect(h.start).toHaveBeenCalledTimes(1);
    answerFirst({ kind: "not_enabled", sentence: "Reading is off." });
    await flush();
    expect(h.started()).toEqual(["tester", "tester"]);
    expect(h.problems).toEqual([]);
    // A refusal with no report since it began waits for the next one.
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    expect(h.start).toHaveBeenCalledTimes(3);
  });

  /** Scenario (PR #1617's Qodo review): this window could not install the consent-on listener, so no save can report Reading on to it. A start refused as not enabled is asked again on a schedule rather than waiting, and is read once the save has reached the disk; a waiting retry is not kept for an agent that has gone. */
  it("asks again for a not-enabled refusal when no consent-on report can arrive", async () => {
    let enabled = false;
    let session = 0;
    const h = harness(() => (enabled ? { kind: "started", session: ++session } : { kind: "not_enabled", sentence: "Reading is off." }));
    h.reader.consentOnUnheard();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    expect(h.started()).toEqual(["tester", "coder"]);
    expect(h.waiting()).toEqual([NOT_ENABLED_RETRY_MS[0], NOT_ENABLED_RETRY_MS[0]]);
    h.reader.update(true, [TESTER], false);
    expect(h.waiting()).toEqual([NOT_ENABLED_RETRY_MS[0]]);
    enabled = true;
    h.runTimers();
    await flush();
    expect(h.started()).toEqual(["tester", "coder", "tester"]);
    expect(h.waiting()).toEqual([]);
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: green.", bare: "Finished: green." });
    expect(h.said.at(-1)).toEqual([readingKey(TESTER), "The tester finished: green."]);
    expect(h.problems).toEqual([]);
  });

  /** Scenario (PR #1617's Qodo review): no consent-on report can reach this window and the switch stays off on disk; the reader asks again on a growing delay, and once those run out it says the refusal once, so reading does not wait silently. Turning Reading off and on again asks afresh. */
  it("says a not-enabled refusal once its retries run out", async () => {
    let enabled = false;
    const h = harness(() => (enabled ? { kind: "started", session: 1 } : { kind: "not_enabled", sentence: "Reading is off." }));
    h.reader.consentOnUnheard();
    h.reader.update(true, [TESTER], false);
    await flush();
    for (const delay of NOT_ENABLED_RETRY_MS) {
      expect(h.waiting()).toEqual([delay]);
      expect(h.problems).toEqual([]);
      h.runTimers();
      await flush();
    }
    expect(h.start).toHaveBeenCalledTimes(NOT_ENABLED_RETRY_MS.length + 1);
    expect(h.waiting()).toEqual([]);
    expect(h.problems).toEqual(["Reading is off."]);
    h.reader.update(false, [TESTER], false);
    enabled = true;
    h.reader.update(true, [TESTER], false);
    await flush();
    expect(h.start).toHaveBeenCalledTimes(NOT_ENABLED_RETRY_MS.length + 2);
    expect(h.reader.reading).toEqual([readingKey(TESTER)]);
  });

  /** Scenario: a window that hears consent-on reports schedules nothing for a not-enabled refusal; it waits for the report instead. */
  it("schedules no retry for a not-enabled refusal while consent-on reports can arrive", async () => {
    const h = harness(() => ({ kind: "not_enabled", sentence: "Reading is off." }));
    h.reader.update(true, [TESTER], false);
    await flush();
    expect(h.waiting()).toEqual([]);
    expect(h.problems).toEqual([]);
  });

  /** Scenario (audit A4): the deck already reports as many agents' turns as it can, so it refuses two of this window's agents; the limit is said once for the deck. Each refused agent is asked again on a growing delay while the deck stays full, and as soon as one of this window's sessions on that deck ends it is asked again shortly and is read. */
  it("asks again for an agent refused while its deck was full", async () => {
    const full = "Reading is not available: this deck is already reporting as many agents' turns as it can.";
    let deckFull = true;
    let session = 0;
    const reviewer: ReadingAgent = { ...TESTER, agentId: "reviewer", label: "reviewer", incarnation: 4 };
    const h = harness((target) => (deckFull && target.agentId !== "tester" ? { kind: "busy", sentence: full } : { kind: "started", session: ++session }));
    h.reader.update(true, [TESTER, CODER, reviewer], false);
    await flush();
    expect(h.problems).toEqual([full]);
    expect(h.waiting()).toEqual([BUSY_RETRY_MS[0], BUSY_RETRY_MS[0]]);
    // Still full: asked again, on the next delay, and not said again.
    h.runTimers();
    await flush();
    expect(h.started().filter((agent) => agent === "coder")).toHaveLength(2);
    expect(h.waiting()).toEqual([BUSY_RETRY_MS[1], BUSY_RETRY_MS[1]]);
    expect(h.problems).toEqual([full]);
    // An update does not ask again early.
    h.reader.update(true, [TESTER, CODER, reviewer]);
    await flush();
    expect(h.started().filter((agent) => agent === "coder")).toHaveLength(2);
    // The tester exits: its session ends, which frees a reader on the deck.
    deckFull = false;
    h.sinks.get("tester")!({ kind: "closed", text: "" });
    expect(h.waiting()).toEqual([CAPACITY_FREED_RETRY_MS, CAPACITY_FREED_RETRY_MS]);
    h.runTimers();
    await flush();
    expect(h.started().filter((agent) => agent === "coder")).toHaveLength(3);
    expect(h.reader.reading).toEqual(expect.arrayContaining([readingKey(CODER), readingKey(reviewer)]));
    expect(h.waiting()).toEqual([]);
  });

  /** Scenario (audit A4): a deck that stays full is asked a bounded number of times; after the last delay the agent is asked again only when one of this window's sessions on that deck ends. Turning Reading off cancels what is waiting. */
  it("bounds the retries of a deck that stays full", async () => {
    const h = harness((target) => (target.agentId === "coder" ? { kind: "busy", sentence: "full" } : { kind: "started", session: 1 }));
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    for (let attempt = 0; attempt < BUSY_RETRY_MS.length; attempt += 1) {
      h.runTimers();
      await flush();
    }
    expect(h.started().filter((agent) => agent === "coder")).toHaveLength(BUSY_RETRY_MS.length + 1);
    expect(h.waiting()).toEqual([]);
    h.reader.update(true, [CODER]);
    await flush();
    expect(h.stop).toHaveBeenCalledWith(1);
    expect(h.waiting()).toEqual([CAPACITY_FREED_RETRY_MS]);
    h.reader.update(false, [CODER]);
    h.runTimers();
    await flush();
    expect(h.started().filter((agent) => agent === "coder")).toHaveLength(BUSY_RETRY_MS.length + 1);
  });

  /** Scenario: the deck's daemon is too old to report finished turns; with three agents on it the reason is said and shown once, not once per agent, and no agent of that deck is subscribed again until its agents change. */
  it("says an older daemon's reason once for the deck", async () => {
    const reason = "Reading is not available: this deck's daemon is too old to report finished turns.";
    const h = harness((target) => (target.deckId === "deck-local" ? { kind: "unavailable", sentence: reason, scope: "deck" } : { kind: "started", session: 1 }));
    const third: ReadingAgent = { ...TESTER, agentId: "reviewer", label: "reviewer" };
    h.reader.update(true, [TESTER, CODER, third, BUILDER], false);
    await flush();
    expect(h.problems).toEqual([reason]);
    expect(h.said.filter(([, text]) => text === reason)).toHaveLength(1);
    const asked = h.start.mock.calls.length;
    h.reader.update(true, [TESTER, CODER, third, BUILDER]);
    await flush();
    expect(h.start.mock.calls.length).toBe(asked);
    // The deck's agents change: it is asked again, and the same reason is not said twice.
    h.reader.update(true, [TESTER, CODER, BUILDER]);
    await flush();
    expect(h.start.mock.calls.length).toBeGreaterThan(asked);
    expect(h.problems).toEqual([reason]);
  });

  /** Scenario: one agent cannot be read (agent-scoped), and another's start fails outright; each is said once, neither is retried while listed, and the rest are read. */
  it("says an agent's own problem once and reads the rest", async () => {
    const h = harness((target) => {
      if (target.agentId === "tester") return { kind: "unavailable", sentence: "Reading is not available: the tester reports no turns.", scope: "agent" };
      if (target.agentId === "coder") throw new Error("ipc down");
      return { kind: "started", session: 4 };
    });
    h.reader.update(true, [TESTER, CODER, BUILDER], false);
    await flush();
    h.reader.update(true, [TESTER, CODER, BUILDER]);
    await flush();
    expect(h.problems).toEqual(["Reading is not available: the tester reports no turns.", readingStartFailed("coder")]);
    expect(h.start).toHaveBeenCalledTimes(3);
  });

  /** Scenario: an agent exits between being listed and being subscribed; its start answers that it is gone, nothing is said, and it is not asked again while it is still listed. */
  it("says nothing for an agent that left before it was read", async () => {
    const h = harness((target) => (target.agentId === "tester" ? { kind: "gone" } : { kind: "started", session: 1 }));
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.reader.update(true, [TESTER, CODER]);
    await flush();
    expect(h.problems).toEqual([]);
    expect(h.said).toEqual([]);
    expect(h.start).toHaveBeenCalledTimes(2);
  });

  /** Scenario: "quiet" silences the app and leaves reading on. */
  it("keeps reading on through quiet", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false);
    await flush();
    h.reader.quiet();
    expect(h.speech.interrupt).toHaveBeenCalledTimes(1);
    expect(h.reader.on).toBe(true);
    expect(h.stop).not.toHaveBeenCalled();
  });

  /** Scenario: permission prompts and errors each wait under a key of their own, apart from the agent's turn summaries. */
  it("keeps permission prompts and errors apart from summaries", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false);
    await flush();
    h.sinks.get("tester")!({ kind: "permission", text: "The tester is asking for permission." });
    h.sinks.get("tester")!({ kind: "blocked", text: "The tester stopped with an error." });
    const keys = h.said.map(([key]) => key);
    expect(new Set(keys).size).toBe(2);
    expect(keys).not.toContain(readingKey(TESTER));
  });

  /** Scenario: the voice surface goes away; every session stops and the app falls silent without a sentence. */
  it("disposes quietly", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false);
    await flush();
    h.reader.dispose();
    expect(h.stop).toHaveBeenCalledWith(1);
    expect(h.speech.interrupt).toHaveBeenCalled();
    expect(h.said).toEqual([]);
  });
});

/** A provider voice that records every sentence it is asked to say, and finishes only when the test says so. */
class FetchingVoice implements SpeechVoice {
  readonly fetched: string[] = [];
  readonly aborted: string[] = [];
  private finishers: (() => void)[] = [];

  speak(text: string, signal: AbortSignal): Promise<void> {
    this.fetched.push(text);
    return new Promise((resolve) => {
      signal.addEventListener("abort", () => {
        this.aborted.push(text);
        this.finishers = this.finishers.filter((finisher) => finisher !== resolve);
        resolve();
      });
      this.finishers.push(resolve);
    });
  }

  finish(): void {
    this.finishers.shift()?.();
  }
}

/** A reader over a real speech queue whose provider voice is recorded. */
function speaking(plan: SpeechPlanDto = { kind: "provider", fallbackToSystem: false }) {
  const provider = new FetchingVoice();
  const system = new FetchingVoice();
  const queue = new SpeechQueue({ plan: () => Promise.resolve(plan), provider, system });
  const sinks = new Map<string, (sentence: ReadingSentenceDto) => void>();
  let sessions = 0;
  const stop = vi.fn(async () => undefined);
  let open: { deckId: string; agentId: string } | undefined;
  const reader = new DeckReader({
    start: async (target, onSentence) => {
      sinks.set(target.agentId, onSentence);
      return { kind: "started", session: ++sessions };
    },
    stop,
    speech: queue,
    openPane: () => open,
  });
  return { reader, queue, provider, system, sinks, stop, open: (agent: ReadingTarget | undefined) => { open = agent; } };
}

describe("DeckReader with speech in flight", () => {
  /** Scenario (decision 4): the tester's summary is queued while the tester's pane is open, and the user opens the coder's pane before it is said; it is said naming the tester. Queued while the coder's pane is open and said after the tester's opens, it drops the name. */
  it("decides the name when the sentence is said", async () => {
    const h = speaking();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.sinks.get("coder")!({ kind: "turn", text: "The coder finished: first.", bare: "Finished: first." });
    await flush();
    h.open(TESTER);
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: all tests pass.", bare: "Finished: all tests pass." });
    h.open(CODER);
    h.provider.finish();
    await flush();
    expect(h.provider.fetched).toEqual(["The coder finished: first.", "The tester finished: all tests pass."]);

    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: second.", bare: "Finished: second." });
    h.open(TESTER);
    h.provider.finish();
    await flush();
    expect(h.provider.fetched.at(-1)).toBe("Finished: second.");
  });

  /** Scenario (D6, shared across agents): while one sentence is being said, the tester finishes two turns and the coder one; the tester's newer summary replaces its older one, the coder's is kept, and they are heard in order. */
  it("replaces an agent's waiting summary and never another agent's", async () => {
    const h = speaking();
    h.reader.update(true, [TESTER, CODER], false);
    await flush();
    h.sinks.get("coder")!({ kind: "permission", text: "The coder is asking for permission." });
    await flush();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: one." });
    h.sinks.get("coder")!({ kind: "turn", text: "The coder finished: theirs." });
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: two." });
    expect(h.queue.pending.map((entry) => words(entry.text))).toEqual(["The tester finished: two.", "The coder finished: theirs."]);
  });

  /** Scenario: a summary is being said and a permission prompt is queued when Reading is turned off; the summary is aborted, the prompt dropped unsaid, and "Reading off." is all that is left. */
  it("cuts speech off when the switch turns off", async () => {
    const h = speaking();
    h.reader.update(true, [TESTER], false);
    await flush();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: a private summary." });
    await flush();
    h.sinks.get("tester")!({ kind: "permission", text: "The tester is asking for permission: read secrets.env." });
    h.reader.update(false, [TESTER]);
    await flush();
    expect(h.provider.aborted).toEqual(["The tester finished: a private summary."]);
    expect(h.provider.fetched).toEqual(["The tester finished: a private summary.", READING_OFF]);
  });

  /** Scenario: the agent exits while its last summary is being said; the summary is heard to the end. */
  it("lets an exiting agent's last summary finish", async () => {
    const h = speaking();
    h.reader.update(true, [TESTER], false, ["deck-local"]);
    await flush();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: the last turn." });
    await flush();
    h.sinks.get("tester")!({ kind: "closed", text: "" });
    h.reader.update(true, [], true, ["deck-local"]);
    await flush();
    expect(h.provider.aborted).toEqual([]);
    expect(h.queue.speaking).toBe(true);
  });

  /** Scenario (re-audit R2): the tester exits while its last turn is still being summarised, so the deck stops listing it BEFORE the summary and its `closed` arrive; the session is not stopped, the summary is said, and the `closed` ends it with nothing left waiting. */
  it("still reads an exited agent's last summary that arrives after it left the deck", async () => {
    const h = harness();
    h.reader.update(true, [TESTER, CODER], false, ["deck-local"]);
    await flush();
    h.reader.update(true, [CODER], true, ["deck-local"]);
    await flush();
    expect(h.stop).not.toHaveBeenCalled();
    expect(h.waiting()).toEqual([EXITED_DRAIN_MS]);
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: all 42 tests pass." });
    expect(h.said.at(-1)).toEqual([readingKey(TESTER), "The tester finished: all 42 tests pass."]);
    h.sinks.get("tester")!({ kind: "closed", text: "" });
    expect(h.reader.reading).toEqual([readingKey(CODER)]);
    expect(h.waiting()).toEqual([]);
    expect(h.stop).not.toHaveBeenCalled();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: late." });
    expect(h.said.map(([, text]) => text)).not.toContain("The tester finished: late.");
  });

  /** Scenario (re-audit R2): an exited agent's `closed` never comes; its session is stopped once EXITED_DRAIN_MS passes, and a sentence after that is not said. */
  it("stops an exited agent's session whose close never comes", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false, ["deck-local"]);
    await flush();
    h.reader.update(true, [], true, ["deck-local"]);
    await flush();
    expect(h.stop).not.toHaveBeenCalled();
    h.runTimers();
    await flush();
    expect(h.stop).toHaveBeenCalledWith(1);
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: too late." });
    expect(h.said).toEqual([]);
  });

  /** Scenario (re-audit R2): an agent exits, and before its last summary arrives the user switches deck, or turns Reading off; either stops its session at once, and the summary is not said. */
  it("stops an exited agent's session at once on a deck change or Reading off", async () => {
    const moved = harness();
    moved.reader.update(true, [TESTER], false, ["deck-local"]);
    await flush();
    moved.reader.update(true, [], true, ["deck-local"]);
    moved.reader.update(true, [BUILDER], true, ["deck-build"]);
    await flush();
    expect(moved.stop).toHaveBeenCalledWith(1);
    expect(moved.waiting()).toEqual([]);
    moved.sinks.get("tester")!({ kind: "turn", text: "The tester finished: old deck." });
    expect(moved.said.map(([, text]) => text)).not.toContain("The tester finished: old deck.");

    const off = harness();
    off.reader.update(true, [TESTER], false, ["deck-local"]);
    await flush();
    off.reader.update(true, [], true, ["deck-local"]);
    off.reader.update(false, [], true, ["deck-local"]);
    await flush();
    expect(off.stop).toHaveBeenCalledWith(1);
    expect(off.waiting()).toEqual([]);
    off.sinks.get("tester")!({ kind: "turn", text: "The tester finished: after off." });
    expect(off.said).toEqual([[READING_VOICE_KEY, READING_OFF]]);
  });

  /** Scenario (re-audit R2): an agent drops out of the deck's list and comes back before its session closed; it goes on being read in the same session, and no drain is left waiting. A replaced agent (a new incarnation under the same id) is not drained: its old session stops at once. */
  it("keeps a session for an agent listed again, and does not drain a replaced one", async () => {
    const h = harness();
    h.reader.update(true, [TESTER], false, ["deck-local"]);
    await flush();
    h.reader.update(true, [], true, ["deck-local"]);
    h.reader.update(true, [TESTER], true, ["deck-local"]);
    await flush();
    expect(h.waiting()).toEqual([]);
    expect(h.started()).toEqual(["tester"]);
    h.runTimers();
    expect(h.stop).not.toHaveBeenCalled();
    h.reader.update(true, [{ ...TESTER, incarnation: 9 }], true, ["deck-local"]);
    await flush();
    expect(h.stop).toHaveBeenCalledWith(1);
    expect(h.started()).toEqual(["tester", "tester"]);
  });

  /** Scenario (audit A7): the tester's summary is being said and the coder's waits when the user switches to another deck; the tester's is cut off, the coder's dropped unsaid, and the new deck's agent is read. A sentence about reading itself is kept. */
  it("drops the old deck's speech when the deck changes", async () => {
    const h = speaking();
    h.reader.update(true, [TESTER, CODER], false, ["deck-local"]);
    await flush();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: old deck." });
    await flush();
    h.sinks.get("coder")!({ kind: "permission", text: "The coder is asking for permission." });
    h.queue.say(READING_VOICE_KEY, "Reading is not available: the other deck did not answer.");
    h.reader.update(true, [BUILDER], true, ["deck-build"]);
    await flush();
    expect(h.provider.aborted).toEqual(["The tester finished: old deck."]);
    expect(h.queue.pending.map((entry) => words(entry.text))).toEqual([]);
    expect(h.provider.fetched).toEqual(["The tester finished: old deck.", "Reading is not available: the other deck did not answer."]);
    h.provider.finish();
    await flush();
    h.sinks.get("builder")!({ kind: "turn", text: "The builder finished: new deck." });
    await flush();
    expect(h.provider.fetched.at(-1)).toBe("The builder finished: new deck.");
  });

  /** Scenario (PR #1617 round 3): under Auto, the provider's speech is refused because the switch was turned off or the connection changed; the sentence is not said with the system voice instead. A provider failure still falls back. */
  it("never answers a provider refusal with the system voice", async () => {
    const plan: SpeechPlanDto = { kind: "provider", fallbackToSystem: true };
    const system = new FetchingVoice();
    const problems: string[] = [];
    let refuse = true;
    const queue = new SpeechQueue({
      plan: () => Promise.resolve(plan),
      provider: { speak: () => Promise.reject(refuse ? new SpeechRefusedError("not permitted") : new Error("outage")) },
      system,
      onProblem: (reason) => problems.push(reason),
    });
    queue.say("tester", "The tester finished: refused.");
    await flush();
    await flush();
    expect(system.fetched).toEqual([]);
    expect(problems).toEqual(["not permitted"]);
    expect(queue.speaking).toBe(false);

    refuse = false;
    queue.say("tester", "The tester finished: fell back.");
    await flush();
    await flush();
    expect(system.fetched).toEqual(["The tester finished: fell back."]);
  });
});

describe("the one-time notice (decision 5 of 2026-10-09)", () => {
  /** Scenario: the notice names the Commands connection's host, or says replies stay on this machine for a local endpoint. */
  it("names where replies go", () => {
    expect(readingNotice("https://api.openai.com/v1/chat/completions")).toBe("Each finished turn's reply is sent to api.openai.com to be summarised.");
    expect(readingNotice("http://127.0.0.1:11434/v1/chat/completions")).toBe("Each finished turn's reply stays on this machine: the Commands service that summarises it runs here.");
    expect(readingNotice("http://localhost:8080/v1")).toContain("stays on this machine");
    expect(readingNotice("http://[::1]:8080/v1")).toContain("stays on this machine");
    expect(readingNotice("not a url")).toContain("Commands service");
    expect(isThisMachine("api.localhost")).toBe(true);
    expect(isThisMachine("127.0.0.1.example.com")).toBe(false);
  });
  /**
   * Scenario (re-audit R1): the notice is word for word the one the Rust side
   * lets reach the provider's speech before it is recorded as shown, for every
   * endpoint in the table both sides are tested against.
   */
  it("matches the wording the speech command recognises", () => {
    expect(noticeCases.length).toBeGreaterThan(0);
    for (const { endpoint, notice } of noticeCases) expect(readingNotice(endpoint), endpoint).toBe(notice);
  });
});
