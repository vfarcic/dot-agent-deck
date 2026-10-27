import { render } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

// The colour-sequence boundaries of the replay filter (inbox/probe/038), on the
// same real browser xterm as TerminalViewport.replay.test.tsx: several colour
// sequences back to back, stacked slots in one sequence, and a set and a query
// in one sequence. Only the GPU and layout addons are fake.
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: class { onContextLoss(): void {} dispose(): void {} },
}));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class { activate(): void {} fit(): void {} dispose(): void {} },
}));
vi.mock("../lib/clipboard", () => ({ writeClipboardText: vi.fn(async () => undefined) }));

import { getTerminal } from "../lib/terminalRegistry";
import type { TerminalBuffer, TerminalFeed } from "../types";
import { TerminalViewport } from "./TerminalViewport";

beforeAll(() => {
  if (typeof window.matchMedia === "function") return;
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (media: string) => ({ matches: false, media, onchange: null, addListener() {}, removeListener() {}, addEventListener() {}, removeEventListener() {}, dispatchEvent: () => false }),
  });
});
afterEach(() => vi.restoreAllMocks());

const E = "\x1b";
const ST = `${E}\\`;
const osc = (body: string) => `${E}]${body}${ST}`;
const FG_SET = "rgb:11/22/33";
const BG_SET = "rgb:44/55/66";
const FG_REPORT = osc("10;rgb:1111/2222/3333");
const BG_REPORT = osc("11;rgb:4444/5555/6666");

/** Mounts a viewport whose backlog is `backlog`; `emit` appends live output. */
function mountTerminal(backlog: string) {
  const sent: string[] = [];
  const encoder = new TextEncoder();
  let buffer: TerminalBuffer = { data: encoder.encode(backlog), baseOffset: 0, generation: 1 };
  const listeners = new Set<(buffer: TerminalBuffer) => void>();
  const feed: TerminalFeed = {
    get: () => buffer,
    subscribe: (_deck, _agent, _incarnation, listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  render(
    <TerminalViewport
      deckId="replay-colour-deck" agentId="replay-colour-agent" label="planner"
      transcript="" terminalFeed={feed} onInput={(data) => sent.push(data)} onResize={() => {}}
    />,
  );
  const terminal = getTerminal("replay-colour-deck", "replay-colour-agent", undefined);
  if (!terminal) throw new Error("viewport registered no terminal");
  const parsed = () => new Promise<void>((resolve) => terminal.write("", resolve));
  const emit = async (text: string) => {
    buffer = { ...buffer, data: encoder.encode(new TextDecoder().decode(buffer.data) + text) };
    for (const listener of listeners) listener(buffer);
    await parsed();
  };
  /** Live queries for the foreground and background, returning their replies. */
  const liveColours = async () => {
    sent.length = 0;
    await emit(osc("10;?") + osc("11;?"));
    return [...sent];
  };
  return { sent, parsed, emit, liveColours };
}

describe("inbox/probe/038: colour sequence boundaries in a replayed history", () => {
  /** Scenario: replay a foreground set, a background set and a query for each, back to back in one buffer. Nothing is sent during the replay, and live queries afterwards report both changed colours. */
  it("inbox/probe/038: back-to-back colour sets and queries apply the sets and answer nothing", async () => {
    const { sent, parsed, liveColours } = mountTerminal(osc(`10;${FG_SET}`) + osc(`11;${BG_SET}`) + osc("10;?") + osc("11;?"));
    await parsed();
    expect.soft([...sent], "historical queries").toEqual([]);
    expect(await liveColours()).toEqual([FG_REPORT, BG_REPORT]);
  });

  /** Scenario: replay a query followed by a set of the same colour in one buffer. The query sends nothing and the set still applies. */
  it("inbox/probe/038: a query before a set in one buffer leaves the set applied", async () => {
    const { sent, parsed, liveColours } = mountTerminal(osc("10;?") + osc(`10;${FG_SET}`));
    await parsed();
    expect.soft([...sent], "historical query").toEqual([]);
    expect((await liveColours())[0]).toBe(FG_REPORT);
  });

  /** Scenario: replay stacked queries (foreground and background in one sequence, the background alone through an empty foreground slot, and two indexed colours with a BEL terminator). Nothing is sent; the same sequences live are answered once per asked slot. */
  it("inbox/probe/038: stacked query slots answer nothing in history and every slot live", async () => {
    const stacked = osc("10;?;?") + osc("10;;?") + `${E}]4;1;?;2;?\x07`;
    const { sent, parsed, emit } = mountTerminal(stacked);
    await parsed();
    expect.soft([...sent], "historical stacked queries").toEqual([]);
    sent.length = 0;
    await emit(stacked);
    expect(sent).toHaveLength(5);
    expect(sent[0]).toMatch(/^\x1b\]10;rgb:[0-9a-f/]+\x1b\\$/);
    expect(sent[1]).toMatch(/^\x1b\]11;rgb:[0-9a-f/]+\x1b\\$/);
    expect(sent[2]).toMatch(/^\x1b\]11;rgb:[0-9a-f/]+\x1b\\$/);
    expect(sent[3]).toMatch(/^\x1b\]4;1;rgb:[0-9a-f/]+\x1b\\$/);
    expect(sent[4]).toMatch(/^\x1b\]4;2;rgb:[0-9a-f/]+\x1b\\$/);
  });

  /** Scenario: replay one sequence that sets both the foreground and the background through stacked slots, with no query in it. Both colours change, as a set is never claimed. */
  it("inbox/probe/038: a stacked set with no query applies every slot", async () => {
    const { sent, parsed, liveColours } = mountTerminal(osc(`10;${FG_SET};${BG_SET}`));
    await parsed();
    expect.soft([...sent]).toEqual([]);
    expect(await liveColours()).toEqual([FG_REPORT, BG_REPORT]);
  });

  /**
   * Scenario: replay one sequence that sets the foreground and asks for the background, and one that sets indexed colour 1 and asks for indexed colour 2. Nothing is sent, and, as TerminalViewport.tsx discloses, the whole sequence is skipped, so the set it carries is not applied either.
   *
   * This pins the disclosed limit rather than endorsing it: a change that keeps the set while still answering nothing should update these expectations.
   */
  it("inbox/probe/038: a set and a query in one sequence is skipped whole in history", async () => {
    const { sent, parsed, emit, liveColours } = mountTerminal(osc(`10;${FG_SET};?`) + osc(`4;1;${FG_SET};2;?`));
    await parsed();
    expect.soft([...sent], "historical mixed sequences").toEqual([]);
    const [foreground] = await liveColours();
    expect(foreground).toMatch(/^\x1b\]10;rgb:[0-9a-f/]+\x1b\\$/);
    expect(foreground, "the mixed sequence's set is skipped").not.toBe(FG_REPORT);
    sent.length = 0;
    await emit(osc("4;1;?"));
    expect(sent, "the mixed indexed sequence's set is skipped").not.toEqual([osc("4;1;rgb:1111/2222/3333")]);
  });

  /** Scenario: after the replay, the agent sends one sequence that sets the foreground and asks for the background. The set applies and the background is answered, as without the replay filter. */
  it("inbox/probe/038: a live set and query in one sequence applies the set and answers", async () => {
    const { sent, parsed, emit, liveColours } = mountTerminal("");
    await parsed();
    await emit(osc(`10;${FG_SET};?`));
    expect(sent).toHaveLength(1);
    expect(sent[0]).toMatch(/^\x1b\]11;rgb:[0-9a-f/]+\x1b\\$/);
    expect((await liveColours())[0]).toBe(FG_REPORT);
  });
});
