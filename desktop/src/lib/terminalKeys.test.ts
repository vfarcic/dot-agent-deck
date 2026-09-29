import { describe, expect, it } from "vitest";
import { agentKeySequence, type TerminalKey } from "./terminalKeys";

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

  it("accepts Ctrl+/ where the layout puts / on a shifted key", () => {
    expect(agentKeySequence(key({ key: "/", ctrlKey: true, shiftKey: true }))).toBe("\x1f");
  });

  it("carries Shift, Alt and Ctrl in the CSI u modifier parameter", () => {
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, altKey: true }))).toBe("\x1b[13;4u");
    expect(agentKeySequence(key({ key: "Enter", shiftKey: true, altKey: true, ctrlKey: true }))).toBe("\x1b[13;8u");
  });
});
