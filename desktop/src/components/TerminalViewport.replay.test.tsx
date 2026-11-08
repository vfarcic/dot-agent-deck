import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fireEvent, render } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

// Real browser xterm, parser and WriteBuffer. Only GPU/layout addons are fake,
// as in TerminalViewport.clipboard.test.tsx; no immediate-write terminal mock.
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
const READY = "ready\r\n";
const REPORT_OPTIONS = { getWinSizePixels: true, getCellSizePixels: true, getWinSizeChars: true, pushTitle: true, popTitle: true };

// Every supported parser-reply family in installed xterm 6.0.0, including
// replies dispatched indirectly through the browser core (colour/focus/size).
const queries = [
  { name: "DA1", query: `${E}[c`, reply: `${E}[?1;2c` },
  { name: "DA2", query: `${E}[>c`, reply: `${E}[>0;276;0c` },
  { name: "DSR", query: `${E}[5n`, reply: `${E}[0n` },
  { name: "CPR", query: `${E}[6n`, reply: `${E}[2;1R` },
  { name: "private CPR", query: `${E}[?6n`, reply: `${E}[?2;1R` },
  { name: "ANSI DECRQM", query: `${E}[4$p`, reply: `${E}[4;2$y` },
  { name: "private DECRQM", query: `${E}[?2004$p`, reply: `${E}[?2004;2$y` },
  { name: "DECRQSS", query: `${E}P$qm${ST}`, reply: `${E}P1$r0m${ST}` },
  { name: "window pixels", query: `${E}[14t`, reply: /^\x1b\[4;\d+;\d+t$/ },
  { name: "cell pixels", query: `${E}[16t`, reply: /^\x1b\[6;\d+;\d+t$/ },
  { name: "window characters", query: `${E}[18t`, reply: `${E}[8;24;80t` },
  { name: "indexed colour", query: `${E}]4;1;?${ST}`, reply: /^\x1b\]4;1;rgb:[0-9a-f/]+\x1b\\$/ },
  { name: "foreground colour", query: `${E}]10;?${ST}`, reply: /^\x1b\]10;rgb:[0-9a-f/]+\x1b\\$/ },
  { name: "background colour", query: `${E}]11;?${ST}`, reply: /^\x1b\]11;rgb:[0-9a-f/]+\x1b\\$/ },
  { name: "cursor colour", query: `${E}]12;?${ST}`, reply: /^\x1b\]12;rgb:[0-9a-f/]+\x1b\\$/ },
  { name: "focus", query: `${E}[?1004h`, reply: `${E}[O` },
];

function mountTerminal(options: { transcript?: string; backlog?: string } = {}) {
  const sent: string[] = [];
  const encoder = new TextEncoder();
  let buffer: TerminalBuffer | undefined = options.backlog === undefined ? undefined : { data: encoder.encode(options.backlog), baseOffset: 0, generation: 1 };
  const listeners = new Set<(buffer: TerminalBuffer) => void>();
  const feed: TerminalFeed = {
    get: () => buffer,
    subscribe: (_deck, _agent, _incarnation, listener) => {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  const props = {
    deckId: "replay-deck", agentId: "replay-agent", label: "planner",
    transcript: options.transcript ?? "", terminalFeed: feed,
    onInput: (data: string) => sent.push(data), onResize: () => {},
  };
  const view = render(<TerminalViewport {...props} />);
  const terminal = getTerminal(props.deckId, props.agentId, undefined);
  if (!terminal) throw new Error("viewport registered no terminal");
  // xterm disables report-window-ops by default. Enable them so suppression
  // must work even if a client opts into a normally disabled report family.
  terminal.options.windowOptions = REPORT_OPTIONS;
  const parsed = () => new Promise<void>((resolve) => terminal.write("", resolve));
  const emit = (text: string, replace = false) => {
    buffer = {
      data: encoder.encode((replace ? "" : new TextDecoder().decode(buffer?.data)) + text),
      baseOffset: 0, generation: replace ? (buffer?.generation ?? 0) + 1 : buffer?.generation ?? 1,
    };
    for (const listener of listeners) listener(buffer);
  };
  const textarea = view.container.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
  if (!textarea) throw new Error("xterm did not mount its input");
  return { terminal, sent, emit, parsed, view, props, textarea };
}

describe("inbox/probe/038: desktop history never answers an old terminal query", () => {
  /** Scenario: open a pane whose backlog contains an old query. Its real xterm renders the following text but sends no historical answer into the agent's input. */
  it.each(queries)("inbox/probe/038: replayed $name has no reply", async ({ query }) => {
    const { sent, parsed, terminal } = mountTerminal({ backlog: READY + query + "after-query" });
    await parsed();
    expect(terminal.buffer.active.getLine(1)?.translateToString(true)).toBe("after-query");
    expect(sent).toEqual([]);
  });

  /** Scenario: after the pane finishes replaying, the agent sends the same query live. Xterm sends its normal answer back exactly once. */
  it.each(queries)("inbox/probe/038: live $name still replies", async ({ query, reply }) => {
    const { sent, emit, parsed } = mountTerminal({ backlog: READY });
    await parsed();
    emit(query);
    await parsed();
    expect(sent).toHaveLength(1);
    if (typeof reply === "string") expect(sent[0]).toBe(reply);
    else expect(sent[0]).toMatch(reply);
  });

  /** Scenario: history enables focus and paste reporting together, then enables focus again. No old focus report is sent, both modes take effect, and real focus changes after replay still reach the agent. */
  it("inbox/probe/038: mixed and repeated focus enables preserve modes and later focus", async () => {
    const { terminal, sent, parsed, textarea } = mountTerminal({ backlog: `${E}[?1004;2004h${E}[?1004h` });
    await parsed();
    expect(terminal.modes.bracketedPasteMode).toBe(true);
    expect.soft([...sent], "historical enables").toEqual([]);
    sent.length = 0;
    textarea.focus();
    textarea.blur();
    expect(sent, "real focus changes after replay").toEqual([`${E}[I`, `${E}[O`]);
  });

  /** Scenario: pause a large replay in the real asynchronous parser while the user presses a key, pastes, and uses Shift+Enter. Those bytes arrive in order during the yield; focus-shaped human bytes in the same parse turn and input after replay arrive unchanged too. */
  it("inbox/probe/038: a WriteBuffer yield never filters typing, paste or shortcuts", async () => {
    const { terminal, sent, emit, parsed, textarea } = mountTerminal();
    await parsed();
    let resume!: (handled: boolean) => void;
    let reached!: () => void;
    const paused = new Promise<void>((resolve) => { reached = resolve; });
    const gate = new Promise<boolean>((resolve) => { resume = resolve; });
    const pause = terminal.parser.registerOscHandler(777, () => { reached(); return gate; });
    const human = terminal.parser.registerOscHandler(778, () => {
      // A genuine input API call in the same synchronous parse after ?1004h
      // must survive, even if its bytes look exactly like a focus reply.
      terminal.input(`${E}[I`, true);
      terminal.input(`${E}[O`, true);
      return true;
    });
    try {
      // The promise makes the real WriteBuffer yield deterministically rather
      // than betting on how many milliseconds this machine takes to parse.
      emit(`${E}[?2004h${"history\r\n".repeat(8192)}${E}]777;pause${ST}${E}[?1004h${E}]778;human${ST}${E}[6n`);
      await paused;
      fireEvent.keyDown(textarea, { key: "x", code: "KeyX", keyCode: 88 });
      fireEvent.keyUp(textarea, { key: "x", code: "KeyX", keyCode: 88 });
      fireEvent.paste(textarea, { clipboardData: { getData: () => "pasted\ntext" } });
      fireEvent.keyDown(textarea, { key: "Enter", code: "Enter", keyCode: 13, shiftKey: true });
      fireEvent.keyUp(textarea, { key: "Enter", code: "Enter", keyCode: 13, shiftKey: true });
      expect([...sent], "human input while replay is paused").toEqual(["x", `${E}[200~pasted\rtext${E}[201~`, `${E}[13;2u`]);
    } finally {
      resume(true);
      await parsed();
      pause.dispose();
      human.dispose();
    }
    expect.soft([...sent], "human bytes including focus-shaped input, without historical replies").toEqual(["x", `${E}[200~pasted\rtext${E}[201~`, `${E}[13;2u`, `${E}[I`, `${E}[O`]);
    terminal.input("after replay", true);
    expect(sent.at(-1)).toBe("after replay");
  });

  /** Scenario: live output is queued immediately before a generation rebuild and immediately after its two consecutive history replays. Only the live queries answer, including when the pane is disposed and opened again. */
  it("inbox/probe/038: deferred callbacks bracket consecutive rebuilds and remounts", async () => {
    const t = mountTerminal({ transcript: `${E}[5n`, backlog: READY });
    await t.parsed();
    t.sent.length = 0;
    t.emit(`${E}[5n`);
    t.emit(`${E}[5n`, true);
    t.emit(`${E}[5n`);
    await t.parsed();
    expect.soft([...t.sent]).toEqual([`${E}[0n`, `${E}[0n`]);
    t.view.unmount();
    const next = mountTerminal({ backlog: READY + `${E}[5n` });
    await next.parsed();
    expect.soft([...next.sent]).toEqual([]);
    next.sent.length = 0;
    next.emit(`${E}[5n`);
    await next.parsed();
    expect(next.sent).toEqual([`${E}[0n`]);
  });

  /** Scenario: a replay changes and saves the window title, changes it again, and restores the saved title. Report suppression must let these manipulation parameters reach xterm, while an adjacent size report sends nothing. */
  it("inbox/probe/038: window report suppression leaves title push and pop working", async () => {
    const { terminal, sent, emit, parsed } = mountTerminal();
    await parsed();
    const titles: string[] = [];
    const listener = terminal.onTitleChange((title) => titles.push(title));
    try {
      emit(`${E}]2;original${ST}${E}[22;2t${E}]2;temporary${ST}${E}[23;2t${E}[18t`);
      await parsed();
      expect(titles).toEqual(["original", "temporary", "original"]);
      expect(sent).toEqual([]);
    } finally {
      listener.dispose();
    }
  });

  /** Scenario: replay a colour change followed by a historical colour query. The change is rendered normally without a stale reply, and the next live query reports the changed colour. */
  it("inbox/probe/038: suppressing colour queries preserves colour changes", async () => {
    const { sent, emit, parsed } = mountTerminal({ backlog: `${E}]10;rgb:11/22/33${ST}${E}]10;?${ST}` });
    await parsed();
    expect.soft([...sent]).toEqual([]);
    sent.length = 0;
    emit(`${E}]10;?${ST}`);
    await parsed();
    expect(sent).toEqual([`${E}]10;rgb:1111/2222/3333${ST}`]);
  });

  /** Scenario: count the named data-emission and dispatch sites per method in the installed browser xterm source against the query families exercised above and the known human-input sites. A change to the per-method counts fails until reviewed; a reply added elsewhere is not detected. */
  it("inbox/probe/038: installed xterm's counted reply sites are unchanged", () => {
    const require = createRequire(import.meta.url);
    const root = dirname(require.resolve("@xterm/xterm/package.json"));
    const sites = (relative: string, event: string) => {
      const source = readFileSync(join(root, relative), "utf8");
      let method = "<unowned>";
      const found: Record<string, number> = {};
      for (const line of source.split("\n")) {
        const declaration = /^  (?:public|private|protected) (?:override )?(\w+)\(/.exec(line);
        if (declaration) method = declaration[1];
        const count = [...line.matchAll(new RegExp(`\\.${event.replaceAll(".", "\\.")}\\(`, "g"))].length;
        if (count) found[method] = (found[method] ?? 0) + count;
      }
      return found;
    };
    // Counts are per site, not just per method: adding a reply to an already
    // known method must also force this inventory and the behaviour table to
    // be reviewed. These are installed dependency sources, never repo copies.
    expect(sites("src/common/InputHandler.ts", "triggerDataEvent")).toEqual({
      sendDeviceAttributesPrimary: 2, sendDeviceAttributesSecondary: 4,
      requestMode: 1, deviceStatus: 2, deviceStatusPrivate: 1,
      windowOptions: 1, requestStatusString: 1,
    });
    expect(sites("src/browser/CoreBrowserTerminal.ts", "triggerDataEvent")).toEqual({
      _handleColorEvent: 1, _reportFocus: 2, _reportWindowsOptions: 2,
      _handleTextAreaFocus: 1, _handleTextAreaBlur: 1,
      bindMouse: 1, _keyDown: 1, _keyPress: 1, _inputEvent: 1,
    });
    expect(sites("src/common/InputHandler.ts", "_onRequestSendFocus.fire")).toEqual({ setModePrivate: 1 });
    // A new query dispatch through an existing browser emitter is a new site
    // too, even when it adds no direct triggerDataEvent call in InputHandler.
    expect(sites("src/common/InputHandler.ts", "_onRequestWindowsOptionsReport.fire")).toEqual({ windowOptions: 2 });
    expect(sites("src/common/InputHandler.ts", "_onColor.fire")).toEqual({
      setOrReportIndexedColor: 1, _setOrReportSpecialColor: 2,
      restoreIndexedColor: 2, restoreFgColor: 1, restoreBgColor: 1, restoreCursorColor: 1,
    });
    expect(queries.map(({ name }) => name)).toEqual([
      "DA1", "DA2", "DSR", "CPR", "private CPR", "ANSI DECRQM", "private DECRQM", "DECRQSS",
      "window pixels", "cell pixels", "window characters", "indexed colour", "foreground colour", "background colour", "cursor colour", "focus",
    ]);
  });
});
