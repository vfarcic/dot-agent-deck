import { describe, expect, it } from "vitest";
import { isSelectionCtrlC, isTerminalCopyChord, type CopyChordEvent } from "./terminalCopy";

/** A keydown with no modifiers, so each case names only what it is about. */
function key(overrides: Partial<CopyChordEvent> & { key: string }): CopyChordEvent {
  return { metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...overrides };
}

describe("isTerminalCopyChord", () => {
  it("claims Ctrl+Shift+C, the Linux and Windows terminal copy", () => {
    // With Shift held the key reads upper case; a layout or a driver that
    // reports it lower case is the same chord.
    expect(isTerminalCopyChord(key({ key: "C", ctrlKey: true, shiftKey: true }))).toBe(true);
    expect(isTerminalCopyChord(key({ key: "c", ctrlKey: true, shiftKey: true }))).toBe(true);
  });

  it("claims Cmd+C, the macOS copy", () => {
    expect(isTerminalCopyChord(key({ key: "c", metaKey: true }))).toBe(true);
  });

  it("leaves plain Ctrl+C to the agent as an interrupt", () => {
    expect(isTerminalCopyChord(key({ key: "c", ctrlKey: true }))).toBe(false);
  });

  it("claims nothing else", () => {
    for (const event of [
      key({ key: "c" }),
      key({ key: "C", shiftKey: true }),
      key({ key: "v", ctrlKey: true, shiftKey: true }),
      key({ key: "C", ctrlKey: true, shiftKey: true, altKey: true }),
      key({ key: "c", metaKey: true, altKey: true }),
      key({ key: "C", metaKey: true, shiftKey: true }),
      key({ key: "c", metaKey: true, ctrlKey: true }),
    ]) {
      expect(isTerminalCopyChord(event), JSON.stringify(event)).toBe(false);
    }
  });
});

describe("plain Ctrl+C over a selection (issue 1705)", () => {
  const ctrlC = key({ key: "c", ctrlKey: true });

  it("copies on Windows when text is selected, as Windows Terminal does", () => {
    expect(isTerminalCopyChord(ctrlC, "windows", true)).toBe(true);
    expect(isSelectionCtrlC(ctrlC, "windows", true)).toBe(true);
  });

  it("is the interrupt on Windows when nothing is selected", () => {
    expect(isTerminalCopyChord(ctrlC, "windows", false)).toBe(false);
  });

  it("is the interrupt on macOS and Linux even over a selection", () => {
    expect(isTerminalCopyChord(ctrlC, "mac", true)).toBe(false);
    expect(isTerminalCopyChord(ctrlC, "linux", true)).toBe(false);
  });

  it("is only the bare chord: AltGr (Ctrl+Alt) and other modifiers are not it", () => {
    for (const event of [
      key({ key: "c", ctrlKey: true, altKey: true }),
      key({ key: "c", ctrlKey: true, metaKey: true }),
      key({ key: "x", ctrlKey: true }),
    ]) {
      expect(isTerminalCopyChord(event, "windows", true), JSON.stringify(event)).toBe(false);
    }
    // Ctrl+Shift+C still copies, and is not the chord that clears the selection.
    expect(isSelectionCtrlC(key({ key: "C", ctrlKey: true, shiftKey: true }), "windows", true)).toBe(false);
  });
});
