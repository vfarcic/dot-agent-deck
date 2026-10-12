import { afterEach, describe, expect, it, vi } from "vitest";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn(async () => undefined) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke }));

import { writeClipboardText } from "./clipboard";

describe("writeClipboardText", () => {
  afterEach(() => {
    delete window.__TAURI_INTERNALS__;
    invoke.mockClear();
    vi.unstubAllGlobals();
  });

  it("writes through the clipboard plugin inside the app, never the webview's clipboard", async () => {
    window.__TAURI_INTERNALS__ = {};
    const webWrite = vi.fn(async () => undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText: webWrite } });

    await writeClipboardText("exit status 0");

    // The one command `capabilities/default.json` grants; anything else is
    // refused by the app at runtime rather than here.
    expect(invoke).toHaveBeenCalledWith("plugin:clipboard-manager|write_text", { text: "exit status 0" });
    expect(webWrite).not.toHaveBeenCalled();
  });

  it("falls back to the web clipboard in a plain browser", async () => {
    const webWrite = vi.fn(async () => undefined);
    vi.stubGlobal("navigator", { clipboard: { writeText: webWrite } });

    await writeClipboardText("exit status 0");

    expect(webWrite).toHaveBeenCalledWith("exit status 0");
    expect(invoke).not.toHaveBeenCalled();
  });
});

describe("writeClipboardText when writes finish out of order (issue 1705)", () => {
  afterEach(() => {
    delete window.__TAURI_INTERNALS__;
    invoke.mockReset();
    invoke.mockImplementation(async () => undefined);
  });

  /** Make each platform write wait until the test finishes it, in any order. */
  function deferredWrites() {
    const finishes = new Map<string, () => void>();
    window.__TAURI_INTERNALS__ = {};
    invoke.mockImplementation(((_cmd: string, args: { text: string }) =>
      new Promise<undefined>((resolve) => finishes.set(args.text, () => resolve(undefined)))) as never);
    const texts = () => invoke.mock.calls.map((call) => (call as unknown as [string, { text: string }])[1].text);
    return { finishes, texts };
  }

  /**
   * Scenario: an agent's copy is slow to reach the clipboard, and the user
   * copies something else in the app (the Reader's Copy all) that finishes
   * first. When the slow write lands after it, the user's newer copy is
   * written again, so the clipboard ends on it.
   */
  it("writes the newest text again when an older write lands after it", async () => {
    const { finishes, texts } = deferredWrites();
    const older = writeClipboardText("agent copy");
    const newer = writeClipboardText("reader copy");
    await vi.waitFor(() => expect(texts()).toEqual(["agent copy", "reader copy"]));
    finishes.get("reader copy")!();
    await newer;
    finishes.get("agent copy")!();
    await older;
    await vi.waitFor(() => expect(texts()).toEqual(["agent copy", "reader copy", "reader copy"]));
    finishes.get("reader copy")!();
  });

  /**
   * Scenario: an older write lands while the newest is still on its way. The
   * newest lands after it anyway, so nothing is written again — and in
   * particular nothing older than a copy still waiting its turn.
   */
  it("writes nothing again while the newest write is still in flight", async () => {
    const { finishes, texts } = deferredWrites();
    const first = writeClipboardText("A");
    const second = writeClipboardText("B");
    const third = writeClipboardText("C");
    await vi.waitFor(() => expect(texts()).toEqual(["A", "B", "C"]));
    finishes.get("B")!();
    await second;
    finishes.get("A")!();
    await first;
    finishes.get("C")!();
    await third;
    await new Promise((resolve) => setTimeout(resolve, 0));
    expect(texts()).toEqual(["A", "B", "C"]);
  });
});