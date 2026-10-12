import { fireEvent, render, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

/*
 * Issue #1705 — an agent that owns the mouse copies with OSC 52.
 *
 * Claude Code turns on mouse reporting, so a drag in its pane goes to Claude
 * Code rather than to xterm.js, and xterm.js holds no selection for the copy
 * key to copy. Claude Code makes the selection itself and asks the terminal to
 * put it on the clipboard with an OSC 52 escape. This file runs the REAL
 * xterm.js, as `TerminalViewport.keys.test.tsx` does, because the escape is
 * parsed by xterm.js's own parser: a fake terminal could only assert what it
 * was told. Only the two addons that need a GPU or a layout engine are stubbed.
 */
vi.mock("@xterm/addon-webgl", () => ({
  WebglAddon: class {
    onContextLoss(): void {}
    dispose(): void {}
  } as unknown as typeof import("@xterm/addon-webgl").WebglAddon,
}));
vi.mock("@xterm/addon-fit", () => ({
  FitAddon: class {
    activate(): void {}
    fit(): void {}
    dispose(): void {}
  } as unknown as typeof import("@xterm/addon-fit").FitAddon,
}));
const { writeClipboardText } = vi.hoisted(() => ({ writeClipboardText: vi.fn(async (_text: string) => undefined) }));
vi.mock("../lib/clipboard", () => ({ writeClipboardText }));

import { getTerminal } from "../lib/terminalRegistry";
import type { TerminalBuffer, TerminalFeed } from "../types";
import { TerminalViewport } from "./TerminalViewport";

// xterm's browser service watches the device pixel ratio through
// `matchMedia`, which jsdom does not implement. Nothing here depends on it.
beforeAll(() => {
  if (typeof window.matchMedia === "function") return;
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      onchange: null,
      addListener() {},
      removeListener() {},
      addEventListener() {},
      removeEventListener() {},
      dispatchEvent: () => false,
    }),
  });
});

afterEach(() => {
  writeClipboardText.mockClear();
  vi.restoreAllMocks();
});

const ESC = "\x1b";
const BEL = "\x07";
const ST = `${ESC}\\`;
/** Output already on screen when the pane opens, so what the test emits next is live. */
const READY = "agent ready\r\n";
/** What Claude Code writes when it takes the mouse: button tracking, SGR coordinates. */
const MOUSE_ON = `${ESC}[?1000h${ESC}[?1002h${ESC}[?1006h`;

function base64(text: string): string {
  let binary = "";
  for (const byte of new TextEncoder().encode(text)) binary += String.fromCharCode(byte);
  return btoa(binary);
}

/** The set-clipboard request an agent writes, `ESC ] 52 ; <targets> ; <base64> <terminator>`. */
function osc52(text: string, options: { targets?: string; terminator?: string } = {}): string {
  return `${ESC}]52;${options.targets ?? "c"};${base64(text)}${options.terminator ?? BEL}`;
}

/** A feed for one agent whose stream the test appends to, the way the bridge's rolling buffer grows. */
function streamingFeed(backlog = "") {
  const encoder = new TextEncoder();
  let buffer: TerminalBuffer | undefined = backlog ? { data: encoder.encode(backlog), baseOffset: 0, generation: 1 } : undefined;
  const listeners = new Set<(buffer: TerminalBuffer) => void>();
  const feed: TerminalFeed = {
    get: () => buffer,
    subscribe: (_deckId, _agentId, listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  /** Bytes the agent writes now, after the pane is open. */
  const emit = (text: string) => {
    const previous = buffer?.data ?? new Uint8Array();
    const next = new Uint8Array(previous.byteLength + encoder.encode(text).byteLength);
    next.set(previous);
    next.set(encoder.encode(text), previous.byteLength);
    buffer = { data: next, baseOffset: 0, generation: 1 };
    for (const listener of listeners) listener(buffer);
  };
  return { feed, emit };
}

function mountTerminal(options: { backlog?: string; transcript?: string; readOnly?: boolean } = {}) {
  const sent: string[] = [];
  const { feed, emit } = streamingFeed(options.backlog);
  const view = render(
    <TerminalViewport
      agentId="agent-1"
      deckId="deck-1"
      label="planner"
      transcript={options.transcript ?? ""}
      terminalFeed={feed}
      readOnly={options.readOnly}
      onInput={(data) => sent.push(data)}
      onResize={() => {}}
    />,
  );
  const terminal = getTerminal("deck-1", "agent-1");
  if (!terminal) throw new Error("the viewport registered no terminal");
  /** Resolves once xterm has parsed everything written to it so far. */
  const parsed = () => new Promise<void>((resolve) => terminal.write("", resolve));
  return { sent, emit, parsed, terminal, view };
}

describe("TerminalViewport honours an agent's OSC 52 copy (issue 1705)", () => {
  /**
   * Scenario: an agent in the pane turns on mouse reporting, the way Claude
   * Code does, and the user drags across its output; the agent answers with an
   * OSC 52 request to copy what it selected. The text the agent named lands on
   * the clipboard, and nothing is sent back to the agent.
   */
  it("puts the text an agent with mouse reporting copies on the clipboard", async () => {
    const { emit, parsed, sent, terminal } = mountTerminal();
    emit(MOUSE_ON);
    await parsed();
    // The precondition the report describes: the agent owns the mouse.
    expect(terminal.modes.mouseTrackingMode).not.toBe("none");

    emit(osc52("cargo test passed — 42 ✓"));
    await parsed();

    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledWith("cargo test passed — 42 ✓"));
    expect(sent).toEqual([]);
  });

  /**
   * Scenario: the agent ends its copy request with ST rather than BEL, and
   * names no clipboard (an empty target, which is how tmux writes it). The text
   * still lands on the clipboard.
   */
  it("accepts the ST terminator and an empty target list", async () => {
    const { emit, parsed } = mountTerminal({ backlog: READY });
    emit(osc52("first line\nsecond line", { targets: "", terminator: ST }));
    await parsed();
    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledWith("first line\nsecond line"));
  });

  /**
   * Scenario: an agent asks the terminal what is on the clipboard (an OSC 52
   * request whose data is `?`). The terminal never answers: nothing is sent to
   * the agent, and the clipboard is not touched.
   */
  it("never answers a clipboard read request", async () => {
    const { emit, parsed, sent } = mountTerminal();
    emit(`${ESC}]52;c;?${BEL}`);
    emit("after the query");
    await parsed();
    expect(sent).toEqual([]);
    expect(writeClipboardText).not.toHaveBeenCalled();
  });

  /**
   * Scenario: an agent sends a copy request whose data is not base64, is not
   * UTF-8 text, is empty, names a target letter OSC 52 does not define, or is
   * larger than the terminal accepts. Each is ignored: the clipboard keeps what
   * it had, rather than being cleared or truncated, and the terminal keeps
   * working.
   */
  it("ignores a malformed, empty or oversized copy request", async () => {
    const { emit, parsed, sent, terminal } = mountTerminal();
    emit(`${ESC}]52;c;not*base64!${BEL}`);
    emit(`${ESC}]52;c;${btoa("\xff\xfe")}${BEL}`);
    emit(`${ESC}]52;c;${BEL}`);
    emit(`${ESC}]52;z;${base64("bad target")}${BEL}`);
    emit(`${ESC}]52;c${BEL}`);
    emit(osc52("x".repeat(1024 * 1024 + 1)));
    emit("still-drawing");
    await parsed();
    expect(writeClipboardText).not.toHaveBeenCalled();
    expect(sent).toEqual([]);
    expect(terminal.buffer.active.getLine(0)?.translateToString(true)).toBe("still-drawing");

    // The largest request accepted is exactly the limit.
    emit(osc52("y".repeat(1024 * 1024)));
    await parsed();
    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledTimes(1));
    expect(writeClipboardText.mock.calls[0][0]).toHaveLength(1024 * 1024);
  });

  /**
   * Scenario: open the pane of an agent that copied something earlier, so the
   * copy request is in the output the pane replays when it opens. Opening the
   * pane leaves the clipboard alone; only a copy the agent makes while the pane
   * is open reaches it.
   */
  it("does not re-copy a request that is only replayed history", async () => {
    const { emit, parsed } = mountTerminal({
      transcript: osc52("from the transcript"),
      backlog: osc52("from the backlog"),
    });
    await parsed();
    expect(writeClipboardText).not.toHaveBeenCalled();

    emit(osc52("copied just now"));
    await parsed();
    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledWith("copied just now"));
    expect(writeClipboardText).toHaveBeenCalledTimes(1);
  });

  /**
   * Scenario: open the pane before its terminal has attached, so the agent's
   * earlier output, copy request included, arrives as the first chunk once the
   * attach completes. That chunk is history too, and leaves the clipboard
   * alone.
   */
  it("treats the first chunk after the pane opens as the attach's replay", async () => {
    const { emit, parsed } = mountTerminal();
    emit(`${READY}${osc52("from the attach replay")}`);
    await parsed();
    expect(writeClipboardText).not.toHaveBeenCalled();

    emit(osc52("copied after the attach"));
    await parsed();
    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledWith("copied after the attach"));
  });

  /**
   * Scenario: an agent copies twice in quick succession, and the first copy is
   * slow to reach the clipboard. The second copy starts only once the first has
   * finished, so the clipboard is left holding the newer text.
   */
  it("writes copies to the clipboard in the order the agent made them", async () => {
    const finishes: (() => void)[] = [];
    writeClipboardText.mockImplementation(() => new Promise<undefined>((resolve) => finishes.push(() => resolve(undefined))));
    try {
      const { emit, parsed } = mountTerminal({ backlog: READY });
      emit(`${osc52("older")}${osc52("overtaken")}${osc52("newer")}`);
      await parsed();
      await waitFor(() => expect(writeClipboardText).toHaveBeenCalledTimes(1));
      await new Promise((resolve) => setTimeout(resolve, 0));
      // The newer copies wait for the older one rather than racing it, and
      // only the newest of them is kept: the overtaken one would only have
      // been overwritten.
      expect(writeClipboardText.mock.calls).toEqual([["older"]]);
      finishes[0]();
      await waitFor(() => expect(writeClipboardText.mock.calls).toEqual([["older"], ["newer"]]));
      finishes[1]();
    } finally {
      writeClipboardText.mockImplementation(async () => undefined);
    }
  });

  /**
   * Scenario: the platform never answers one clipboard write. The agent's next
   * copy still reaches the clipboard once that write has had its time.
   */
  it("does not let a clipboard write that never finishes hold up the next copy", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    writeClipboardText.mockImplementationOnce(() => new Promise<undefined>(() => {}));
    try {
      const { emit, parsed } = mountTerminal({ backlog: READY });
      emit(osc52("stuck"));
      await parsed();
      await vi.waitFor(() => expect(writeClipboardText).toHaveBeenCalledTimes(1));
      emit(osc52("next"));
      await parsed();
      expect(writeClipboardText).toHaveBeenCalledTimes(1);
      await vi.advanceTimersByTimeAsync(5_000);
      await vi.waitFor(() => expect(writeClipboardText).toHaveBeenLastCalledWith("next"));
    } finally {
      vi.useRealTimers();
    }
  });

  /**
   * Scenario: the pane is read-only (another client holds its input, or the
   * agent has finished). An agent's copy still lands on the clipboard, because
   * copying out of a pane is reading, not typing into it.
   */
  it("honours the copy in a read-only pane", async () => {
    const { emit, parsed } = mountTerminal({ backlog: READY, readOnly: true });
    emit(osc52("read-only output"));
    await parsed();
    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledWith("read-only output"));
  });

  /**
   * Scenario, the control: an agent that never turns on mouse reporting, where
   * the user selects with the mouse in the terminal itself and presses the copy
   * key. The selection lands on the clipboard as it did before this change.
   */
  it("still copies a selection made in the terminal itself", async () => {
    const { emit, parsed, sent, terminal, view } = mountTerminal();
    emit("exit status 0");
    await parsed();
    expect(terminal.modes.mouseTrackingMode).toBe("none");
    terminal.select(0, 0, "exit status 0".length);
    const wrapper = view.getByTestId("terminal-agent-1");
    fireEvent.keyDown(wrapper, { key: "c", code: "KeyC", keyCode: 67, metaKey: true });
    await waitFor(() => expect(writeClipboardText).toHaveBeenCalledWith("exit status 0"));
    expect(sent).toEqual([]);
  });
});
