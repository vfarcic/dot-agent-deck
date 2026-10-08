import { describe, expect, it, vi } from "vitest";
import { READING_ALREADY_OFF, READING_OFF, READING_ON, READING_VOICE_KEY, ReadingMode, type ReadingSentenceDto, type ReadingStartDto, type ReadingTarget } from "./reading";

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
    const sentence = "Reading is not available: this daemon does not report when an agent finishes a turn yet.";
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
});
