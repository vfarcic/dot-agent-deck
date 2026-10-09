import { describe, expect, it, vi } from "vitest";
import type { SpeechPlanDto } from "./bridge";
import {
  DeckReader,
  isThisMachine,
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
import { SpeechQueue, SpeechRefusedError, type SpeechText, type SpeechVoice } from "./speech";

const TESTER: ReadingAgent = { deckId: "deck-local", agentId: "tester", label: "tester", incarnation: 1 };
const CODER: ReadingAgent = { deckId: "deck-local", agentId: "coder", label: "coder", incarnation: 2 };
const BUILDER: ReadingAgent = { deckId: "deck-build", agentId: "builder", label: "builder", incarnation: 3 };

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/** The words a queued sentence will be said with, asked now. */
const words = (text: SpeechText) => (typeof text === "string" ? text : text());

/**
 * A reader over recorded fakes. Each start answers `answer(target)` (started,
 * with a session per call, by default) and is recorded with its sink.
 */
function harness(answer?: (target: ReadingTarget) => ReadingStartDto | Promise<ReadingStartDto>) {
  const said: [string, string][] = [];
  const speech = { say: vi.fn((key: string, text: SpeechText) => { said.push([key, words(text)]); }), interrupt: vi.fn() };
  const sinks = new Map<string, (sentence: ReadingSentenceDto) => void>();
  let sessions = 0;
  const start = vi.fn(async (target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => {
    sinks.set(target.agentId, onSentence);
    return answer ? answer(target) : { kind: "started" as const, session: ++sessions };
  });
  const stop = vi.fn(async (_session: number) => undefined);
  const problems: string[] = [];
  let open: { deckId: string; agentId: string } | undefined;
  const reader = new DeckReader({ start, stop, speech, openPane: () => open, onProblem: (sentence) => problems.push(sentence) });
  return {
    reader, said, speech, start, stop, sinks, problems,
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
    h.reader.update(true, [TESTER], false);
    await flush();
    h.sinks.get("tester")!({ kind: "turn", text: "The tester finished: the last turn." });
    await flush();
    h.sinks.get("tester")!({ kind: "closed", text: "Reading off." });
    h.reader.update(true, []);
    await flush();
    expect(h.provider.aborted).toEqual([]);
    expect(h.queue.speaking).toBe(true);
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
});
