/// <reference path="../src/vite-env.d.ts" />

import { expect } from "@playwright/test";
import type { DesktopSnapshotDto } from "../src/lib/bridge";
import { test } from "./support/load-budget";

/**
 * Issue #2436 — an orchestration role's Close dialog offers closing the whole
 * orchestration, with a "Keep the worktree" checkbox instead of a separate keep
 * button, and lays its three buttons out in one row on a normal window. jsdom
 * computes no geometry, so the row and the narrow-window fit are measured here.
 */
const WINDOWS = [
  ["wide", 1280, 800],
  ["narrow", 360, 720],
] as const;

for (const [name, width, height] of WINDOWS) {
  /** Scenario: in a {name} window, press Close on the orchestrator's row of a dispatched two-role orchestration. The confirmation offers Cancel, Close agent and Close orchestration, with Close orchestration leading, and a "Keep the worktree" checkbox; in the wide window the three buttons share one row, and in the narrow one the dialog fits the window without horizontal scrolling. Ticking the box and pressing Close orchestration sends one stop for both roles that keeps the worktree. */
  test(`closes the whole orchestration from an agent's dialog in a ${name} window`, async ({ page }) => {
    await page.setViewportSize({ width, height });
    // Through Tauri's IPC seam, with no real daemon.
    await page.addInitScript(() => {
      const callbacks = new Map<number, (event: unknown) => void>();
      let notifySnapshot: ((event: unknown) => void) | undefined;
      let next = 0;
      const role = (id: string, roleName: string, roleIndex: number, isStartRole: boolean) => ({
        id, paneId: `pane-${id}`, displayName: roleName, agentType: "claude", status: "working", toolCount: 0, rows: 32, cols: 120, incarnation: "close-process",
        tab: { kind: "orchestration", orchestrationId: "orc-2436", name: "review-run", roleName, roleIndex, isStartRole },
      });
      const snapshot = {
        connection: {
          status: "connected", deckKind: "local", deckId: "deck-0000000000002436", socketPath: "/tmp/close-orchestration.sock",
          clientProtocolVersion: 10, serverProtocolVersion: 10, clientBuildVersion: "0.46.0", daemonBuildVersion: "0.46.0",
          runningAgentCount: 2, incarnation: "close-process",
        },
        agents: [role("agent-1", "lead", 0, true), role("agent-2", "critic", 1, false)],
        protocolVersion: 10, source: "daemon", fleet: ["deck-0000000000002436"],
      } as unknown as DesktopSnapshotDto;
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
          if (command === "desktop_worktree_close_preview") return { kept: null, keepWorktree: "/home/dev/review-run-worktree" };
          if (command === "desktop_run_action") {
            const action = args.action as { type?: string };
            (window as unknown as { __stops: unknown[] }).__stops = [...((window as unknown as { __stops?: unknown[] }).__stops ?? []), action];
            if (action.type === "stop_orchestration") {
              snapshot.agents = [];
              snapshot.connection.runningAgentCount = 0;
              notifySnapshot?.({ payload: snapshot });
            }
          }
          return { ok: true };
        },
      } });
      Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", { value: { unregisterListener: (_event: string, id: number) => callbacks.delete(id) } });
    });
    await page.goto("/?live=1");
    const stop = page.getByRole("button", { name: "Close lead agent", exact: true });
    await stop.click();
    const confirmation = page.getByRole("alertdialog");
    const buttons = [
      confirmation.getByRole("button", { name: "Cancel", exact: true }),
      confirmation.getByRole("button", { name: "Close agent", exact: true }),
      confirmation.getByRole("button", { name: "Close orchestration", exact: true }),
    ];
    const keep = confirmation.getByRole("checkbox", { name: "Keep the worktree", exact: true });
    await expect(buttons[1]).toBeEnabled();
    await expect(keep).not.toBeChecked();
    await expect(buttons[2]).toHaveClass(/\bleads\b/);
    await expect(confirmation).toContainText("all 2 of its roles: lead, critic");

    for (const control of [...buttons, keep]) await expect(control).toBeInViewport({ ratio: 1 });
    const box = await confirmation.boundingBox();
    expect(box).not.toBeNull();
    expect(box!.x).toBeGreaterThanOrEqual(0);
    expect(box!.x + box!.width).toBeLessThanOrEqual(width);
    const overflow = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    expect(overflow, "the dialog must not make the window scroll sideways").toBeLessThanOrEqual(0);
    const tops = await Promise.all(buttons.map(async (button) => (await button.boundingBox())!.y));
    if (name === "wide") {
      expect(new Set(tops.map(Math.round)).size, "three buttons in one row").toBe(1);
      expect(box!.width).toBeLessThanOrEqual(760);
    }

    await keep.check();
    await buttons[2].click();
    await expect(confirmation).toHaveCount(0);
    await expect(stop).toHaveCount(0);
    const stops = await page.evaluate(() => (window as unknown as { __stops?: unknown[] }).__stops ?? []);
    expect(stops).toEqual([{
      type: "stop_orchestration", deckId: "deck-0000000000002436", keepWorktree: true,
      roles: [{ agentId: "agent-1", name: "lead", incarnation: "close-process" }, { agentId: "agent-2", name: "critic", incarnation: "close-process" }],
    }]);
  });
}
