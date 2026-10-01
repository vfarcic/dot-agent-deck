import { describe, expect, it } from "vitest";
import shared from "../../../tests/fixtures/editing-shortcuts.json";
import { agentKeySequence, keyPlatform, leavesPasteToWebview, type TerminalKey } from "./terminalKeys";

const key = (fields: Partial<TerminalKey> & { key: string }): TerminalKey => ({
  shiftKey: false,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  ...fields,
});

describe("agentKeySequence (issue #1422)", () => {
  it("leaves every key xterm already encodes like the TUI to xterm", () => {
    expect(agentKeySequence(key({ key: "Enter" }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Enter", altKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "c", ctrlKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "/" }))).toBeUndefined();
  });

  it("never claims a Cmd/Super chord", () => {
    expect(agentKeySequence(key({ key: "Enter", metaKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Enter", metaKey: true, ctrlKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "/", metaKey: true, ctrlKey: true }))).toBeUndefined();
  });

  it("never claims a key while an input method is composing", () => {
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, isComposing: true }))).toBeUndefined();
  });

  it("does not read AltGr (Ctrl+Alt) as Ctrl+/", () => {
    expect(agentKeySequence(key({ key: "/", ctrlKey: true, altKey: true }))).toBeUndefined();
  });

  it("leaves AltGr, which Windows reports as Ctrl+Alt, to xterm", () => {
    const altGraph = (name: string) => name === "AltGraph";
    expect(agentKeySequence(key({ key: "Enter", ctrlKey: true, altKey: true, getModifierState: altGraph }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Enter", ctrlKey: true, altKey: true, shiftKey: true, getModifierState: altGraph }))).toBeUndefined();
    // A genuine Ctrl+Alt+Enter, with no AltGr reported, is still translated.
    expect(agentKeySequence(key({ key: "Enter", ctrlKey: true, altKey: true, getModifierState: () => false }))).toBe("\x1b[13;7u");
  });

  it("accepts Ctrl+/ where the layout puts / on a shifted key", () => {
    expect(agentKeySequence(key({ key: "/", ctrlKey: true, shiftKey: true }))).toBe("\x1f");
  });

  it("carries Shift, Alt and Ctrl in the CSI u modifier parameter", () => {
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, altKey: true }))).toBe("\x1b[13;4u");
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, altKey: true, ctrlKey: true }))).toBe("\x1b[13;8u");
  });
});

describe("keyPlatform: what the webview reports, not the daemon's host", () => {
  it("reads macOS from WKWebView and Chromium's reports", () => {
    expect(keyPlatform({ platform: "MacIntel" })).toBe("mac");
    expect(keyPlatform({ userAgentData: { platform: "macOS" } })).toBe("mac");
    expect(keyPlatform({ userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15" })).toBe("mac");
  });

  it("reads Windows from WebView2's report", () => {
    expect(keyPlatform({ platform: "Win32" })).toBe("windows");
    expect(keyPlatform({ userAgentData: { platform: "Windows" } })).toBe("windows");
  });

  it("treats everything else, WebKitGTK included, as Linux", () => {
    expect(keyPlatform({ platform: "Linux x86_64" })).toBe("linux");
    expect(keyPlatform({})).toBe("linux");
    expect(keyPlatform(undefined)).toBe("linux");
  });

  it("trusts the platform report over a user agent dressed as another browser's", () => {
    // Playwright's WebKit on Linux: a Linux platform behind a Safari-on-Mac user agent.
    expect(keyPlatform({ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)" })).toBe("linux");
    expect(keyPlatform({ platform: "MacIntel", userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" })).toBe("mac");
    expect(keyPlatform({ userAgentData: { platform: "Windows" }, platform: "MacIntel" })).toBe("windows");
  });
});

describe("agentKeySequence: editing shortcuts (issue #1422)", () => {
  it("maps the Cmd/Super line keys to bytes every agent's line editor acts on", () => {
    expect(agentKeySequence(key({ key: "ArrowLeft", metaKey: true }))).toBe("\x01");
    expect(agentKeySequence(key({ key: "ArrowRight", metaKey: true }))).toBe("\x05");
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true }))).toBe("\x15");
  });

  it("maps Ctrl+Backspace, Ctrl+Delete and Alt+Delete to word deletes", () => {
    expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true }))).toBe("\x17");
    expect(agentKeySequence(key({ key: "Delete", ctrlKey: true }))).toBe("\x1bd");
    expect(agentKeySequence(key({ key: "Delete", altKey: true }))).toBe("\x1bd");
  });

  it("leaves the keys xterm already sends correctly to xterm", () => {
    expect(agentKeySequence(key({ key: "ArrowLeft", altKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", altKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "ArrowLeft", ctrlKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Home" }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace" }))).toBeUndefined();
  });

  it("claims only the bare chord, not one with extra modifiers", () => {
    expect(agentKeySequence(key({ key: "ArrowLeft", metaKey: true, shiftKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true, altKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true, shiftKey: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Delete", ctrlKey: true, altKey: true }))).toBeUndefined();
  });

  it("never claims a key while an input method is composing", () => {
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true, isComposing: true }))).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true, isComposing: true }))).toBeUndefined();
  });
});

describe("leavesPasteToWebview: each platform's paste key", () => {
  it("is Cmd+V on macOS, where Ctrl+V stays the agent's", () => {
    expect(leavesPasteToWebview(key({ key: "v", metaKey: true }), "mac")).toBe(true);
    expect(leavesPasteToWebview(key({ key: "v", ctrlKey: true }), "mac")).toBe(false);
  });

  it("is Ctrl+V and Ctrl+Shift+V on Windows", () => {
    expect(leavesPasteToWebview(key({ key: "v", ctrlKey: true }), "windows")).toBe(true);
    expect(leavesPasteToWebview(key({ key: "V", ctrlKey: true, shiftKey: true }), "windows")).toBe(true);
  });

  it("is Ctrl+Shift+V on Linux, where Ctrl+V stays the agent's", () => {
    expect(leavesPasteToWebview(key({ key: "V", ctrlKey: true, shiftKey: true }), "linux")).toBe(true);
    expect(leavesPasteToWebview(key({ key: "v", ctrlKey: true }), "linux")).toBe(false);
  });

  it("finds V by its physical key on a non-Latin layout", () => {
    expect(leavesPasteToWebview(key({ key: "м", code: "KeyV", ctrlKey: true }), "windows")).toBe(true);
    // On a Latin layout the character wins, so Dvorak's V is where V is printed.
    expect(leavesPasteToWebview(key({ key: "k", code: "KeyV", ctrlKey: true }), "windows")).toBe(false);
  });

  it("does not take Ctrl+Alt+V, which Windows may report for AltGr+V", () => {
    expect(leavesPasteToWebview(key({ key: "v", ctrlKey: true, altKey: true }), "windows")).toBe(false);
  });
});

describe("leaves the copy chords to issue #1403's listener", () => {
  it("claims neither Ctrl+Shift+C nor Cmd+C on any platform", () => {
    for (const platform of ["mac", "windows", "linux"] as const) {
      for (const chord of [key({ key: "C", ctrlKey: true, shiftKey: true }), key({ key: "c", metaKey: true })]) {
        expect(agentKeySequence(chord)).toBeUndefined();
        expect(leavesPasteToWebview(chord, platform)).toBe(false);
      }
    }
  });
});

describe("the editing shortcuts are the TUI's table (tests/fixtures/editing-shortcuts.json)", () => {
  type Row = { key: string; modifiers: string[]; bytes: string };
  const held = (row: Pick<Row, "key" | "modifiers">): TerminalKey =>
    key({
      key: row.key,
      shiftKey: row.modifiers.includes("shift"),
      ctrlKey: row.modifiers.includes("ctrl"),
      altKey: row.modifiers.includes("alt"),
      metaKey: row.modifiers.includes("super"),
    });
  const names = ["shift", "ctrl", "alt", "super"];
  const keys = ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Backspace", "Delete", "Home", "End"];

  // `agentKeySequence` takes no platform: these hold on every one, as in the TUI.
  it("translates every row the TUI translates, to the same bytes", () => {
    for (const row of shared.translated as Row[]) {
      expect(agentKeySequence(held(row)), `${row.modifiers.join("+")}+${row.key}`).toBe(row.bytes);
    }
  });

  it("translates no editing chord the TUI leaves alone", () => {
    for (const name of keys) {
      for (let mask = 0; mask < 1 << names.length; mask++) {
        const modifiers = names.filter((_, i) => mask & (1 << i));
        const row = (shared.translated as Row[]).find(
          (candidate) => candidate.key === name && [...candidate.modifiers].sort().join() === [...modifiers].sort().join(),
        );
        expect(agentKeySequence(held({ key: name, modifiers })), `${modifiers.join("+")}+${name}`).toBe(row?.bytes);
      }
    }
  });
});
