import { describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());
const listeners = vi.hoisted(() => [] as ((event: { payload: number }) => void)[]);

vi.mock("@tauri-apps/api/core", () => ({ invoke }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_event: string, listener: (event: { payload: number }) => void) => {
    listeners.push(listener);
    return () => undefined;
  }),
}));

import { PR_BROWSER_CLOSED_EVENT, tauriPrBrowser } from "./prBrowser";

const BOUNDS = { x: 0, y: 40, width: 800, height: 600, viewportWidth: 800, viewportHeight: 640 };

describe("PRD #1401 — the in-app browser's close event", () => {
  /** Scenario: a close the page asked for under an earlier open arrives after another pull request opened; the app ignores it and still answers a close of the page on screen. */
  it("ignores a close of an open the app has since replaced", async () => {
    let generation = 0;
    invoke.mockImplementation(async (command: string) => (command === "desktop_pr_browser_open" ? ++generation : undefined));
    const host = tauriPrBrowser();
    const closed = vi.fn();
    host.onClosed(closed);
    await vi.waitFor(() => expect(listeners).toHaveLength(1));
    expect(PR_BROWSER_CLOSED_EVENT).toBe("pr-browser://closed");

    await host.open("https://github.com/o/r/pull/1", BOUNDS);
    await host.open("https://github.com/o/r/pull/2", BOUNDS);
    listeners[0]({ payload: 1 });
    expect(closed).not.toHaveBeenCalled();

    listeners[0]({ payload: 2 });
    expect(closed).toHaveBeenCalledTimes(1);
  });
});
