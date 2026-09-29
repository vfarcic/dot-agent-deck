import { describe, expect, it } from "vitest";
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
    expect(agentKeySequence(key({ key: "Enter" }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Enter", altKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "c", ctrlKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "/" }), "linux")).toBeUndefined();
  });

  it("never claims a Cmd/Super chord", () => {
    expect(agentKeySequence(key({ key: "Enter", metaKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Enter", metaKey: true, ctrlKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "/", metaKey: true, ctrlKey: true }), "linux")).toBeUndefined();
  });

  it("never claims a key while an input method is composing", () => {
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, isComposing: true }), "linux")).toBeUndefined();
  });

  it("does not read AltGr (Ctrl+Alt) as Ctrl+/", () => {
    expect(agentKeySequence(key({ key: "/", ctrlKey: true, altKey: true }), "linux")).toBeUndefined();
  });

  it("leaves AltGr, which Windows reports as Ctrl+Alt, to xterm", () => {
    const altGraph = (name: string) => name === "AltGraph";
    expect(agentKeySequence(key({ key: "Enter", ctrlKey: true, altKey: true, getModifierState: altGraph }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Enter", ctrlKey: true, altKey: true, shiftKey: true, getModifierState: altGraph }), "linux")).toBeUndefined();
    // A genuine Ctrl+Alt+Enter, with no AltGr reported, is still translated.
    expect(agentKeySequence(key({ key: "Enter", ctrlKey: true, altKey: true, getModifierState: () => false }), "linux")).toBe("\x1b[13;7u");
  });

  it("accepts Ctrl+/ where the layout puts / on a shifted key", () => {
    expect(agentKeySequence(key({ key: "/", ctrlKey: true, shiftKey: true }), "linux")).toBe("\x1f");
  });

  it("carries Shift, Alt and Ctrl in the CSI u modifier parameter", () => {
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, altKey: true }), "linux")).toBe("\x1b[13;4u");
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, altKey: true, ctrlKey: true }), "linux")).toBe("\x1b[13;8u");
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

describe("agentKeySequence: platform editing shortcuts (issue #1422)", () => {
  it("maps macOS's Cmd line keys and Option+Delete to bytes every agent's line editor acts on", () => {
    expect(agentKeySequence(key({ key: "ArrowLeft", metaKey: true }), "mac")).toBe("\x01");
    expect(agentKeySequence(key({ key: "ArrowRight", metaKey: true }), "mac")).toBe("\x05");
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true }), "mac")).toBe("\x15");
    expect(agentKeySequence(key({ key: "Delete", altKey: true }), "mac")).toBe("\x1bd");
  });

  it("maps Ctrl+Backspace and Ctrl+Delete to word deletes on Windows and Linux", () => {
    for (const platform of ["windows", "linux"] as const) {
      expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true }), platform)).toBe("\x17");
      expect(agentKeySequence(key({ key: "Delete", ctrlKey: true }), platform)).toBe("\x1bd");
    }
  });

  it("leaves the keys xterm already sends correctly to xterm", () => {
    expect(agentKeySequence(key({ key: "ArrowLeft", altKey: true }), "mac")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", altKey: true }), "mac")).toBeUndefined();
    expect(agentKeySequence(key({ key: "ArrowLeft", ctrlKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Home" }), "windows")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace" }), "mac")).toBeUndefined();
  });

  it("keeps each platform's chords to that platform", () => {
    // Super+Left on Linux and Windows is the window manager's, not Cmd+Left.
    expect(agentKeySequence(key({ key: "ArrowLeft", metaKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true }), "windows")).toBeUndefined();
    // Ctrl+Backspace is not a macOS editing key; xterm's BS stays.
    expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true }), "mac")).toBeUndefined();
    // Alt+Delete is not a Windows or Linux editing key.
    expect(agentKeySequence(key({ key: "Delete", altKey: true }), "linux")).toBeUndefined();
  });

  it("claims only the bare chord, not one with extra modifiers", () => {
    expect(agentKeySequence(key({ key: "ArrowLeft", metaKey: true, shiftKey: true }), "mac")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true, altKey: true }), "mac")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true, shiftKey: true }), "linux")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Delete", ctrlKey: true, altKey: true }), "windows")).toBeUndefined();
  });

  it("never claims a key while an input method is composing", () => {
    expect(agentKeySequence(key({ key: "Backspace", metaKey: true, isComposing: true }), "mac")).toBeUndefined();
    expect(agentKeySequence(key({ key: "Backspace", ctrlKey: true, isComposing: true }), "linux")).toBeUndefined();
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
        expect(agentKeySequence(chord, platform)).toBeUndefined();
        expect(leavesPasteToWebview(chord, platform)).toBe(false);
      }
    }
  });
});
