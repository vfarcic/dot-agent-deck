import { describe, expect, it, vi } from "vitest";
import type { SpeechPlanDto } from "./bridge";
import { READING_ALREADY_OFF, READING_OFF, READING_ON, READING_VOICE_KEY, ReadingMode, type ReadingSentenceDto, type ReadingStartDto, type ReadingTarget } from "./reading";
import { SpeechQueue, type SpeechVoice } from "./speech";

const TESTER: ReadingTarget = { deckId: "deck-local", agentId: "tester", label: "tester" };
const CODER: ReadingTarget = { deckId: "deck-local", agentId: "coder", label: "coder" };

/** A reading mode over recorded fakes; `answer` is what the next start answers. */
function harness(answer: ReadingStartDto = { kind: "started", session: 7 }) {
  const said: [string, string][] = [];
  const speech = { say: vi.fn((agent: string, text: string) => { said.push([agent, text]); }), interrupt: vi.fn() };
  const sinks: ((sentence: ReadingSentenceDto) => void)[] = [];
  let next: ReadingStartDto = answer;
  let gate: Promise<void> | undefined;
  const start = vi.fn(async (_target: ReadingTarget, onSentence: (sentence: ReadingSentenceDto) => void) => {
    sinks.push(onSentence);
    if (gate) await gate;
    return next;
  });
  const stop = vi.fn(async () => undefined);
  const changes: (ReadingTarget | undefined)[] = [];
  const mode = new ReadingMode({ start, stop, speech, onChange: (target) => changes.push(target) });
  return {
    mode, said, speech, start, stop, changes, sinks,
    answer: (value: ReadingStartDto) => { next = value; },
    hold: () => {
      let release!: () => void;
      gate = new Promise((resolve) => { release = resolve; });
      return () => { gate = undefined; release(); };
    },
  };
}

describe("ReadingMode (PRD #1497 M5)", () => {
  /** Scenario: "reading on" starts reading for the pane's agent, marks it, and says "Reading on."; turn sentences that arrive afterwards are spoken, naming nothing from before (no backlog). */
  it("starts for the agent on screen and speaks only what arrives after", async () => {
    const h = harness();
    const result = await h.mode.turnOn(TESTER);
    expect(result).toEqual({ kind: "started", sentence: READING_ON });
    expect(h.mode.target).toEqual(TESTER);
    expect(h.changes).toEqual([TESTER]);
    expect(h.said).toEqual([[READING_VOICE_KEY, READING_ON]]);
    // The subscription is opened at "reading on" and nothing is asked for before it.
    expect(h.start).toHaveBeenCalledTimes(1);
    h.sinks[0]({ kind: "turn", text: "The tester finished: all tests pass." });
    expect(h.said.at(-1)).toEqual(["deck-local\u0000tester", "The tester finished: all tests pass."]);
  });

  /** Scenario: with the Settings opt-in off, "reading on" refuses in spoken words and reading stays off. */
  it("refuses in spoken words when the opt-in is off", async () => {
    const sentence = "Reading is turned off in Settings. Turn on Read turns aloud in Settings, Voice, first.";
    const h = harness({ kind: "not_enabled", sentence });
    expect(await h.mode.turnOn(TESTER)).toEqual({ kind: "refused", sentence });
    expect(h.mode.on).toBe(false);
    expect(h.changes).toEqual([]);
    expect(h.said).toEqual([[READING_VOICE_KEY, sentence]]);
  });

  /** Scenario: when reading is not available for the agent (the daemon reports no turn ends), "reading on" says so plainly and reading stays off. */
  it("says plainly when reading is unavailable", async () => {
    const sentence = "Reading is not available: this deck's daemon is too old to report finished turns. Update dot-agent-deck on that machine.";
    const h = harness({ kind: "unavailable", sentence });
    expect(await h.mode.turnOn(TESTER)).toEqual({ kind: "refused", sentence });
    expect(h.mode.on).toBe(false);
    expect(h.said).toEqual([[READING_VOICE_KEY, sentence]]);
  });

  /** Scenario (D11): "reading off" ends reading, stops the subscription, and says "Reading off."; said again, it reports reading is not on. */
  it("ends on reading off", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    expect(await h.mode.turnOff()).toBe(READING_OFF);
    expect(h.mode.on).toBe(false);
    expect(h.stop).toHaveBeenCalledWith(7);
    expect(h.changes).toEqual([TESTER, undefined]);
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
    expect(await h.mode.turnOff()).toBe(READING_ALREADY_OFF);
    expect(h.stop).toHaveBeenCalledTimes(1);
  });

  /** Scenario (D11): opening a different agent's pane ends reading, and it says "Reading off."; it never follows the user to that pane. */
  it("ends when a different agent's pane opens", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    await h.mode.paneChanged({ deckId: TESTER.deckId, agentId: TESTER.agentId });
    expect(h.mode.on).toBe(true);
    await h.mode.paneChanged({ deckId: CODER.deckId, agentId: CODER.agentId });
    expect(h.mode.on).toBe(false);
    expect(h.stop).toHaveBeenCalledWith(7);
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
  });

  /** Scenario (D11): the same agent id on another deck is a different agent, so its pane ends reading too. */
  it("ends when the same agent id on another deck opens", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    await h.mode.paneChanged({ deckId: "deck-remote", agentId: TESTER.agentId });
    expect(h.mode.on).toBe(false);
  });

  /** Scenario (D11): closing the pane ends reading and says "Reading off." */
  it("ends when the pane closes", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    await h.mode.paneChanged(undefined);
    expect(h.mode.on).toBe(false);
    expect(h.stop).toHaveBeenCalledWith(7);
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
  });

  /** Scenario (D11): "voice off" ends reading and says "Reading off."; with reading off it says nothing. */
  it("ends on voice off", async () => {
    const h = harness();
    await h.mode.voiceOff();
    expect(h.said).toEqual([]);
    await h.mode.turnOn(TESTER);
    await h.mode.voiceOff();
    expect(h.mode.on).toBe(false);
    expect(h.stop).toHaveBeenCalledWith(7);
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
  });

  /** Scenario (D3): after reading ends, a sentence from the ended subscription is not spoken, and turning it on again opens a fresh subscription from that moment. */
  it("speaks nothing from an ended session and starts afresh", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    await h.mode.turnOff();
    const before = h.said.length;
    h.sinks[0]({ kind: "turn", text: "late" });
    expect(h.said.length).toBe(before);
    h.answer({ kind: "started", session: 8 });
    await h.mode.turnOn(TESTER);
    expect(h.start).toHaveBeenCalledTimes(2);
    h.sinks[1]({ kind: "turn", text: "fresh" });
    expect(h.said.at(-1)?.[1]).toBe("fresh");
  });

  /** Scenario: the pane closes while the start is still being answered; the session it then answers with is stopped at once and reading never turns on. */
  it("undoes a start the pane outlived", async () => {
    const h = harness();
    const release = h.hold();
    const pending = h.mode.turnOn(TESTER);
    await Promise.resolve();
    expect(h.mode.active).toBe(true);
    await h.mode.paneChanged(undefined);
    release();
    expect(await pending).toEqual({ kind: "abandoned" });
    expect(h.mode.on).toBe(false);
    expect(h.stop).toHaveBeenCalledWith(7);
    expect(h.changes).toEqual([]);
  });

  /** Scenario (D6): "stop" / "quiet" silence the app and leave reading on. */
  it("keeps reading on through quiet", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    h.mode.quiet();
    expect(h.speech.interrupt).toHaveBeenCalledTimes(1);
    expect(h.mode.on).toBe(true);
    expect(h.stop).not.toHaveBeenCalled();
  });

  /** Scenario (D10): each permission prompt and error is spoken under a key of its own, so nothing later replaces one still waiting, while a turn summary keeps replacing the agent's waiting summary. */
  it("keeps permission prompts and errors apart from summaries", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    h.sinks[0]({ kind: "permission", text: "The tester is asking for permission: run cargo publish." });
    h.sinks[0]({ kind: "blocked", text: "The tester hit a usage limit and stopped." });
    h.sinks[0]({ kind: "turn", text: "The tester finished: done." });
    const [permission, blocked, turn] = h.said.slice(1);
    expect(permission[1]).toBe("The tester is asking for permission: run cargo publish.");
    expect(blocked[1]).toBe("The tester hit a usage limit and stopped.");
    expect(turn).toEqual(["deck-local\u0000tester", "The tester finished: done."]);
    expect(new Set([permission[0], blocked[0], turn[0]]).size).toBe(3);
  });

  /** Scenario: the voice surface unmounting ends the session and silences the app without saying anything. */
  it("disposes quietly", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    const before = h.said.length;
    await h.mode.dispose();
    expect(h.mode.on).toBe(false);
    expect(h.stop).toHaveBeenCalledWith(7);
    expect(h.speech.interrupt).toHaveBeenCalled();
    expect(h.said.length).toBe(before);
  });

  /** Scenario (audit A-B1): the Rust side ends the session because the Settings opt-in was turned off; reading mode ends here too — indicator cleared, speech interrupted, "Reading off." said, subscription stopped — and the ended sentence itself is never spoken. */
  it("ends when the Rust side says the session ended", async () => {
    const h = harness();
    await h.mode.turnOn(TESTER);
    h.sinks[0]({ kind: "ended", text: "Reading off." });
    await Promise.resolve();
    expect(h.mode.on).toBe(false);
    expect(h.changes).toEqual([TESTER, undefined]);
    expect(h.speech.interrupt).toHaveBeenCalledTimes(1);
    expect(h.said.at(-1)).toEqual([READING_VOICE_KEY, READING_OFF]);
    expect(h.said.filter(([key]) => key !== READING_VOICE_KEY)).toEqual([]);
    expect(h.stop).toHaveBeenCalledWith(7);
  });
});

/** A provider voice that records every sentence it is asked to fetch and play, and finishes only when the test says so. */
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

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

/** Reading mode over a real speech queue whose provider voice is recorded. */
function speaking(plan: SpeechPlanDto = { kind: "provider", fallbackToSystem: false }) {
  const provider = new FetchingVoice();
  const system = new FetchingVoice();
  const queue = new SpeechQueue({ plan: () => Promise.resolve(plan), provider, system });
  const sinks: ((sentence: ReadingSentenceDto) => void)[] = [];
  const stop = vi.fn(async () => undefined);
  const mode = new ReadingMode({
    start: async (_target, onSentence) => {
      sinks.push(onSentence);
      return { kind: "started", session: 7 };
    },
    stop,
    speech: queue,
  });
  return { mode, queue, provider, system, sinks, stop };
}

describe("ReadingMode ending with speech in flight (audit A-B3)", () => {
  const ends: [string, (mode: ReadingMode) => Promise<unknown>][] = [
    ["reading off", (mode) => mode.turnOff()],
    ["another agent's pane", (mode) => mode.paneChanged({ deckId: CODER.deckId, agentId: CODER.agentId })],
    ["the pane closing", (mode) => mode.paneChanged(undefined)],
    ["voice off", (mode) => mode.voiceOff()],
  ];
  for (const [name, endIt] of ends) {
    /** Scenario: a summary is being spoken by the provider and a private permission sentence is queued behind it when reading ends; the summary is aborted, the queued sentence is removed and never fetched, "Reading off." is the only thing left, and a late sentence from the ended session never re-enters the queue. */
    it(`interrupts and clears the queue before saying Reading off on ${name}`, async () => {
      const h = speaking();
      await h.mode.turnOn(TESTER);
      await flush();
      h.provider.finish(); // "Reading on."
      await flush();
      h.sinks[0]({ kind: "turn", text: "The tester finished: a private summary." });
      await flush();
      h.sinks[0]({ kind: "permission", text: "The tester is asking for permission: read secrets.env." });
      expect(h.provider.fetched.at(-1)).toBe("The tester finished: a private summary.");
      expect(h.queue.pending.map((entry) => entry.text)).toEqual(["The tester is asking for permission: read secrets.env."]);

      await endIt(h.mode);
      expect(h.provider.aborted).toEqual(["The tester finished: a private summary."]);
      expect(h.queue.pending.map((entry) => entry.text)).not.toContain("The tester is asking for permission: read secrets.env.");
      expect(h.stop).toHaveBeenCalledWith(7);

      // A sentence from the ended generation arrives late.
      h.sinks[0]({ kind: "turn", text: "The tester finished: late news." });
      await flush();
      h.provider.finish();
      await flush();
      expect(h.provider.fetched).toEqual(["Reading on.", "The tester finished: a private summary.", READING_OFF]);
      expect(h.queue.speaking).toBe(false);
    });
  }

  /** Scenario (audit A-B2): under a provider speech source, a permission sentence — the agent's name and up to 120 characters of what it wants — is sent to the provider's speech like every other sentence, which is what Settings → Voice discloses. */
  it("sends a permission sentence to the provider's speech when it is the source", async () => {
    const h = speaking({ kind: "provider", fallbackToSystem: true });
    await h.mode.turnOn(TESTER);
    await flush();
    h.provider.finish();
    await flush();
    const permission = `The tester is asking for permission: Bash: ${"x".repeat(100)}.`;
    h.sinks[0]({ kind: "permission", text: permission });
    await flush();
    expect(h.provider.fetched).toEqual(["Reading on.", permission]);
    expect(h.system.fetched).toEqual([]);
  });
});
