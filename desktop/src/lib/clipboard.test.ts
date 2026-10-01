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
