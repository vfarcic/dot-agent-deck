import { fireEvent, render } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";

/*
 * Issue #1422 — what a keystroke in an agent's terminal actually sends to the
 * agent's PTY.
 *
 * Unlike `TerminalViewport.test.tsx`, this file runs the REAL xterm.js: the
 * defect lives inside xterm's own key evaluation (`Keyboard.ts` in
 * `@xterm/xterm@6.0.0` encodes Enter as a bare CR whatever Shift or Ctrl is
 * held), so a fake terminal would only be asserting what the fake was told to
 * do. Only the two addons that need a GPU or a layout engine are stubbed; key
 * handling needs neither.
 *
 * The expected bytes are the TUI's (`keyevent_to_bytes` in `src/ui.rs`). A bare
 * CR submits in every supported agent; `ESC[13;2u` inserts a newline in every
 * one; `ESC[13;5u` is whatever each agent binds Ctrl+Enter to, which differs
 * (`docs/develop/desktop-gui.md` has the measured table).
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

const ESC = "\x1b";

interface Key {
  key: string;
  code: string;
  keyCode: number;
  shiftKey?: boolean;
  ctrlKey?: boolean;
  altKey?: boolean;
  metaKey?: boolean;
}

const enter = (mods: Partial<Key> = {}): Key => ({ key: "Enter", code: "Enter", keyCode: 13, ...mods });

function mountTerminal(options: { readOnly?: boolean } = {}) {
  const sent: string[] = [];
  const view = render(
    <TerminalViewport
      agentId="agent-1"
      label="planner"
      transcript=""
      readOnly={options.readOnly}
      onInput={(data) => sent.push(data)}
      onResize={() => {}}
    />,
  );
  const textarea = view.container.querySelector<HTMLTextAreaElement>(".xterm-helper-textarea");
  if (!textarea) throw new Error("xterm did not mount its helper textarea");
  /** Press one key the way a browser delivers it: keydown, then keyup. */
  const press = (key: Key) => {
    const accepted = fireEvent.keyDown(textarea, key);
    fireEvent.keyUp(textarea, key);
    return accepted;
  };
  return { sent, press, textarea };
}

afterEach(() => vi.restoreAllMocks());

/**
 * Make the webview report `platform` the way each engine does, before the
 * terminal mounts: WebKit on macOS says `MacIntel`, WebView2 says `Win32`, and
 * WebKitGTK says `Linux x86_64`. The terminal decides the platform's shortcuts
 * from this, never from the daemon's host.
 */
function onPlatform(platform: "mac" | "windows" | "linux") {
  const reported = { mac: "MacIntel", windows: "Win32", linux: "Linux x86_64" }[platform];
  vi.spyOn(navigator, "platform", "get").mockReturnValue(reported);
}

const arrowLeft = (mods: Partial<Key> = {}): Key => ({ key: "ArrowLeft", code: "ArrowLeft", keyCode: 37, ...mods });
const arrowRight = (mods: Partial<Key> = {}): Key => ({ key: "ArrowRight", code: "ArrowRight", keyCode: 39, ...mods });
const backspace = (mods: Partial<Key> = {}): Key => ({ key: "Backspace", code: "Backspace", keyCode: 8, ...mods });
const forwardDelete = (mods: Partial<Key> = {}): Key => ({ key: "Delete", code: "Delete", keyCode: 46, ...mods });
const home: Key = { key: "Home", code: "Home", keyCode: 36 };
const end: Key = { key: "End", code: "End", keyCode: 35 };
const keyV = (mods: Partial<Key> = {}): Key => ({ key: "v", code: "KeyV", keyCode: 86, ...mods });

describe("TerminalViewport keystrokes reach the agent as the TUI sends them (issue #1422)", () => {
  /**
   * Scenario: open an agent's terminal, press each common agent key, and
   * record exactly what reaches the agent. Shift+Enter and Ctrl+Enter must
   * arrive as the sequences the TUI sends for them, which an agent can tell
   * apart from Enter, not as the same carriage return that submits the draft.
   */
  it.each<[string, Key, string]>([
    ["Enter submits", enter(), "\r"],
    ["Shift+Enter inserts a newline", enter({ shiftKey: true }), `${ESC}[13;2u`],
    ["Ctrl+Enter arrives as Ctrl+Enter, not Enter", enter({ ctrlKey: true }), `${ESC}[13;5u`],
    ["Ctrl+Shift+Enter keeps both modifiers", enter({ ctrlKey: true, shiftKey: true }), `${ESC}[13;6u`],
    ["Ctrl+Alt+Enter keeps both modifiers", enter({ ctrlKey: true, altKey: true }), `${ESC}[13;7u`],
    ["Alt+Enter keeps its ESC-prefixed form", enter({ altKey: true }), `${ESC}\r`],
    ["Ctrl+J sends a line feed", { key: "j", code: "KeyJ", keyCode: 74, ctrlKey: true }, "\n"],
    ["Ctrl+C interrupts", { key: "c", code: "KeyC", keyCode: 67, ctrlKey: true }, "\x03"],
    ["Escape", { key: "Escape", code: "Escape", keyCode: 27 }, ESC],
    ["Shift+Tab", { key: "Tab", code: "Tab", keyCode: 9, shiftKey: true }, `${ESC}[Z`],
    ["Shift+Up keeps its modifier", { key: "ArrowUp", code: "ArrowUp", keyCode: 38, shiftKey: true }, `${ESC}[1;2A`],
    ["Ctrl+/ sends the unit separator", { key: "/", code: "Slash", keyCode: 191, ctrlKey: true }, "\x1f"],
  ])("%s", (_name, key, bytes) => {
    const { sent, press } = mountTerminal();
    press(key);
    expect(sent).toEqual([bytes]);
  });

  /**
   * Scenario: with an agent's terminal focused, press Ctrl+Enter. The key must
   * reach the agent once and stop there, exactly as xterm treats every key it
   * sends: it must not also bubble to the app's own window-level shortcuts or
   * type a newline into xterm's hidden input box.
   */
  it("claims a translated key the way xterm claims a key it sends", () => {
    const { sent, press } = mountTerminal();
    const bubbled = vi.fn();
    window.addEventListener("keydown", bubbled);
    try {
      const accepted = press(enter({ ctrlKey: true }));
      expect(accepted).toBe(false); // default prevented
      expect(bubbled).not.toHaveBeenCalled();
      expect(sent).toEqual([`${ESC}[13;5u`]);
    } finally {
      window.removeEventListener("keydown", bubbled);
    }
  });

  /**
   * Scenario: with an agent's terminal focused, press a Cmd/Super chord such
   * as Cmd+K or Cmd+Enter. Those belong to the app and the operating system,
   * so nothing reaches the agent and the app's own shortcut still sees it.
   */
  it("leaves Cmd/Super chords to the app", () => {
    const { sent, press } = mountTerminal();
    const bubbled = vi.fn();
    window.addEventListener("keydown", bubbled);
    try {
      press({ key: "k", code: "KeyK", keyCode: 75, metaKey: true });
      expect(bubbled).toHaveBeenCalledTimes(1);
      expect(sent).toEqual([]);
    } finally {
      window.removeEventListener("keydown", bubbled);
    }
  });

  /**
   * Scenario: on a Windows keyboard layout, hold AltGr (reported as Ctrl+Alt)
   * and press Enter. The key is not mistaken for Ctrl+Alt+Enter: it reaches
   * the agent as xterm encodes it on its own.
   */
  it("does not read AltGr+Enter as Ctrl+Alt+Enter", () => {
    const { sent, textarea } = mountTerminal();
    fireEvent.keyDown(textarea, { ...enter({ ctrlKey: true, altKey: true }), modifierAltGraph: true });
    expect(sent).toEqual([`${ESC}\r`]);
  });

  /**
   * Scenario: open a terminal whose input is disabled (another client holds
   * the write lease) and press Ctrl+Enter. Nothing reaches the agent: the
   * translated key goes through the same input gate as every other keystroke.
   */
  it("sends nothing from a read-only terminal", () => {
    const { sent, textarea } = mountTerminal({ readOnly: true });
    fireEvent.keyDown(textarea, enter({ ctrlKey: true }));
    expect(sent).toEqual([]);
  });
});

describe("TerminalViewport editing shortcuts follow the platform the webview reports (issue #1422)", () => {
  /**
   * Scenario: on each platform, open an agent's terminal, type into the
   * agent's prompt and use that platform's own editing shortcuts — jump to the
   * start or end of the line, move by word, delete a word or the whole line.
   * Each reaches the agent as bytes every supported agent's input box acts on
   * (the per-agent measurements are in `docs/develop/desktop-gui.md`), rather
   * than nothing at all or a single-character delete.
   */
  it.each<[string, "mac" | "windows" | "linux", Key, string]>([
    ["macOS: Cmd+Left goes to the start of the line", "mac", arrowLeft({ metaKey: true }), "\x01"],
    ["macOS: Cmd+Right goes to the end of the line", "mac", arrowRight({ metaKey: true }), "\x05"],
    ["macOS: Option+Left moves a word left", "mac", arrowLeft({ altKey: true }), `${ESC}[1;3D`],
    ["macOS: Option+Right moves a word right", "mac", arrowRight({ altKey: true }), `${ESC}[1;3C`],
    ["macOS: Cmd+Backspace deletes to the start of the line", "mac", backspace({ metaKey: true }), "\x15"],
    ["macOS: Option+Backspace deletes the previous word", "mac", backspace({ altKey: true }), `${ESC}\x7f`],
    ["macOS: Option+Delete deletes the next word", "mac", forwardDelete({ altKey: true }), `${ESC}d`],
    ["macOS: Ctrl+V still reaches the agent (its image paste)", "mac", keyV({ ctrlKey: true }), "\x16"],
    ["Windows: Home goes to the start of the line", "windows", home, `${ESC}[H`],
    ["Windows: End goes to the end of the line", "windows", end, `${ESC}[F`],
    ["Windows: Ctrl+Left moves a word left", "windows", arrowLeft({ ctrlKey: true }), `${ESC}[1;5D`],
    ["Windows: Ctrl+Right moves a word right", "windows", arrowRight({ ctrlKey: true }), `${ESC}[1;5C`],
    ["Windows: Ctrl+Backspace deletes the previous word", "windows", backspace({ ctrlKey: true }), "\x17"],
    ["Windows: Ctrl+Delete deletes the next word", "windows", forwardDelete({ ctrlKey: true }), `${ESC}d`],
    ["Linux: Home goes to the start of the line", "linux", home, `${ESC}[H`],
    ["Linux: End goes to the end of the line", "linux", end, `${ESC}[F`],
    ["Linux: Ctrl+Left moves a word left", "linux", arrowLeft({ ctrlKey: true }), `${ESC}[1;5D`],
    ["Linux: Ctrl+Right moves a word right", "linux", arrowRight({ ctrlKey: true }), `${ESC}[1;5C`],
    ["Linux: Ctrl+Backspace deletes the previous word", "linux", backspace({ ctrlKey: true }), "\x17"],
    ["Linux: Ctrl+Delete deletes the next word", "linux", forwardDelete({ ctrlKey: true }), `${ESC}d`],
    ["Linux: Ctrl+V still reaches the agent (its image paste)", "linux", keyV({ ctrlKey: true }), "\x16"],
    ["Backspace deletes one character", "linux", backspace(), "\x7f"],
  ])("%s", (_name, platform, key, bytes) => {
    onPlatform(platform);
    const { sent, press } = mountTerminal();
    press(key);
    expect(sent).toEqual([bytes]);
  });

  /**
   * Scenario: on Linux or Windows, where the Windows key is the operating
   * system's, press Win+Left. The desktop app does not read it as macOS's
   * Cmd+Left, so nothing reaches the agent.
   */
  it("leaves Super+Left alone outside macOS", () => {
    onPlatform("linux");
    const { sent, press } = mountTerminal();
    press(arrowLeft({ metaKey: true }));
    expect(sent).toEqual([]);
  });

  /**
   * Scenario: press the platform's paste shortcut in an agent's terminal —
   * Cmd+V on macOS, Ctrl+V on Windows, Ctrl+Shift+V on Linux. The key itself
   * sends nothing to the agent and is not cancelled, so the webview performs
   * its own paste, which the terminal then hands to the agent as pasted text.
   */
  it.each<[string, "mac" | "windows" | "linux", Key]>([
    ["macOS: Cmd+V", "mac", keyV({ metaKey: true })],
    ["Windows: Ctrl+V", "windows", keyV({ ctrlKey: true })],
    ["Windows: Ctrl+Shift+V", "windows", keyV({ key: "V", ctrlKey: true, shiftKey: true })],
    ["Linux: Ctrl+Shift+V", "linux", keyV({ key: "V", ctrlKey: true, shiftKey: true })],
  ])("%s leaves the paste to the webview", (_name, platform, key) => {
    onPlatform(platform);
    const { sent, press } = mountTerminal();
    const accepted = press(key);
    expect(sent).toEqual([]);
    expect(accepted).toBe(true); // default not prevented: the webview pastes
  });

  /**
   * Scenario: paste text into an agent's terminal. The text reaches the agent
   * as it was on the clipboard.
   */
  it("hands a webview paste to the agent", () => {
    const { sent, textarea } = mountTerminal();
    const paste = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(paste, "clipboardData", { value: { getData: () => "pasted text" } });
    textarea.dispatchEvent(paste);
    expect(sent).toEqual(["pasted text"]);
  });
});

describe("TerminalViewport copy and editing keys share one terminal (issues 1403 and 1422)", () => {
  /**
   * Scenario: on each platform, open an agent's terminal, press the copy chord
   * (Cmd+C on macOS, Ctrl+Shift+C elsewhere), then that platform's "start of
   * line" or "delete the previous word" shortcut. The copy chord sends nothing
   * to the agent, and the editing shortcut that follows still reaches it, so
   * the copy listener and the editing-key handler do not swallow each other's
   * keys.
   */
  it.each<[string, "mac" | "windows" | "linux", Key, Key, string]>([
    ["macOS", "mac", { key: "c", code: "KeyC", keyCode: 67, metaKey: true }, arrowLeft({ metaKey: true }), "\x01"],
    [
      "Windows",
      "windows",
      { key: "C", code: "KeyC", keyCode: 67, ctrlKey: true, shiftKey: true },
      backspace({ ctrlKey: true }),
      "\x17",
    ],
    [
      "Linux",
      "linux",
      { key: "C", code: "KeyC", keyCode: 67, ctrlKey: true, shiftKey: true },
      backspace({ ctrlKey: true }),
      "\x17",
    ],
  ])("%s", (_name, platform, copyChord, editKey, bytes) => {
    onPlatform(platform);
    const { sent, press } = mountTerminal();
    press(copyChord);
    expect(sent).toEqual([]);
    press(editKey);
    expect(sent).toEqual([bytes]);
  });
});
