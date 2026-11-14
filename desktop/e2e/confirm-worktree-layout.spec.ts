/// <reference path="../src/vite-env.d.ts" />

import { expect } from "@playwright/test";
import type { DesktopSnapshotDto } from "../src/lib/bridge";
import { test } from "./support/load-budget";

const LONG_PATH = `/home/dev/${"nested-project-directory/".repeat(90)}dispatch-kept-tail`;

/** [name, the dirty kept-worktree note's path, the worktree the keep choice is offered for]. */
const CASES = [
  ["no warning", null, null],
  ["short path", "/home/dev/dispatch-kept", null],
  ["long path", LONG_PATH, null],
  ["keep worktree and no warning", null, "/home/dev/dispatch-clean"],
  ["short path and keep worktree", "/home/dev/dispatch-kept", "/home/dev/dispatch-kept"],
  ["long path and keep worktree", LONG_PATH, LONG_PATH],
] as const;

for (const [name, path, keepPath] of CASES) {
  const offerKeep = keepPath !== null;
  /** Scenario: close a dispatched agent in a short window, reading either no warning or a short/long kept-worktree path, with or without the "Close agent, keep worktree" choice. Every button remains fully on screen; the long warning can be scrolled to its identifying tail, Cancel leaves the agent alone, and Close (or the keep choice) sends its stop request. */
  test(`close confirmation stays reachable with ${name}`, async ({ page }) => {
    await page.setViewportSize({ width: 640, height: 320 });
    // Exercise the live preview through Tauri's IPC seam, with no real daemon.
    await page.addInitScript(({ path, keepPath }) => {
      const callbacks = new Map<number, (event: unknown) => void>();
      let notifySnapshot: ((event: unknown) => void) | undefined;
      let next = 0;
      const snapshot: DesktopSnapshotDto = {
        connection: {
          status: "connected", deckKind: "local", deckId: "deck-0000000000001674", socketPath: "/tmp/worktree-layout.sock",
          clientProtocolVersion: 10, serverProtocolVersion: 10, clientBuildVersion: "0.46.0", daemonBuildVersion: "0.46.0",
          runningAgentCount: 1, incarnation: "layout-process",
        },
        agents: [{ id: "agent-1", paneId: "pane-1", displayName: "Builder", agentType: "codex", status: "working", toolCount: 0, rows: 32, cols: 120, incarnation: "layout-process", tab: { kind: "dashboard" } }],
        protocolVersion: 10, source: "daemon", fleet: ["deck-0000000000001674"],
      };
      Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {
        transformCallback: (callback: (event: unknown) => void) => { const id = ++next; callbacks.set(id, callback); return id; },
        unregisterCallback: (id: number) => callbacks.delete(id),
        invoke: async (command: string, args: Record<string, unknown> = {}) => {
          if (command === "plugin:event|listen") {
            if (args.event === "desktop://snapshot") notifySnapshot = callbacks.get(Number(args.handler));
            return args.handler;
          }
          if (command === "plugin:event|unlisten") return;
          if (command === "desktop_bootstrap" || command === "desktop_get_snapshot") return snapshot;
          if (command === "desktop_features") return {};
          if (command === "desktop_get_settings") return { settings: { version: 1, appearance: { mode: "system" }, zoom: { level: 1 } } };
          if (command === "desktop_set_zoom") return args.level;
          // The report shape `previewWorktreeClose` resolves with (`WorktreeCloseReport`, issue #1958).
          if (command === "desktop_worktree_close_preview") return { kept: path ? { path, confirmed_dirty: true } : null, keepWorktree: keepPath };
          if (command === "desktop_run_action" && (args.action as { type?: string })?.type === "stop_agent") {
            (window as unknown as { __stops: unknown[] }).__stops = [...((window as unknown as { __stops?: unknown[] }).__stops ?? []), args.action];
            snapshot.agents = [];
            snapshot.connection.runningAgentCount = 0;
            notifySnapshot?.({ payload: snapshot });
          }
          return { ok: true };
        },
      } });
      Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", { value: { unregisterListener: (_event: string, id: number) => callbacks.delete(id) } });
    }, { path, keepPath });
    await page.goto("/?live=1");
    const stop = page.getByRole("button", { name: "Close Builder agent", exact: true });
    await stop.click();
    const confirmation = page.getByRole("alertdialog");
    const cancel = confirmation.getByRole("button", { name: "Cancel", exact: true });
    const close = confirmation.getByRole("button", { name: "Close agent", exact: true });
    const keep = confirmation.getByRole("button", { name: "Close agent, keep worktree", exact: true });
    await expect(close).toBeEnabled();
    if (offerKeep) await expect(keep).toBeEnabled();
    else await expect(keep).toHaveCount(0);
    if (path) await expect(confirmation.locator("code")).toHaveText(path);
    else await expect(confirmation.locator(".kept-worktree-text")).toHaveCount(0);

    // toBeVisible alone accepts controls below the viewport; measure before
    // Playwright's click has a chance to scroll anything into view.
    for (const control of offerKeep ? [cancel, close, keep] : [cancel, close]) await expect(control).toBeInViewport({ ratio: 1 });
    const box = await confirmation.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.y).toBeGreaterThanOrEqual(0);
    expect(box!.y + box!.height).toBeLessThanOrEqual(320);
    if (path === LONG_PATH) {
      const warning = confirmation.locator("p").filter({ has: page.locator(".kept-worktree-text") });
      const scrolled = await warning.evaluate((node) => {
        node.scrollTop = node.scrollHeight;
        return { top: node.scrollTop, bottom: node.scrollTop + node.clientHeight, height: node.scrollHeight };
      });
      expect(scrolled.top, "the full path must remain readable by scrolling").toBeGreaterThan(0);
      expect(scrolled.bottom).toBeCloseTo(scrolled.height, 0);
      await expect(close).toBeInViewport({ ratio: 1 });
      if (offerKeep) await expect(keep).toBeInViewport({ ratio: 1 });
    }
    await cancel.click();
    await expect(confirmation).toHaveCount(0);
    await expect(stop).toBeVisible();
    await stop.click();
    await (offerKeep ? keep : close).click();
    await expect(confirmation).toHaveCount(0);
    await expect(stop).toHaveCount(0);
    const stops = await page.evaluate(() => (window as unknown as { __stops?: { keepWorktree?: boolean }[] }).__stops ?? []);
    expect(stops).toHaveLength(1);
    expect(stops[0].keepWorktree ?? false).toBe(offerKeep);
  });
}
