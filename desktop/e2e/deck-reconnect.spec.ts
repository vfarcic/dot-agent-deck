import { expect, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * Issue #1994 — Reconnect on one deck reconnects THAT deck and leaves the rest
 * of the fleet alone, which is what the TUI's reconnect does with the one
 * daemon it is attached to (CLAUDE.md rule 22).
 *
 * Driven through the real live bridge over a mocked Tauri IPC, like
 * `host-metrics.spec.ts`: the bundle cannot tell this from the desktop crate,
 * so the bridge's own `connect()` and fleet bookkeeping run as they ship. The
 * mock answers `desktop_bootstrap` the way the crate does, with the SELECTED
 * deck's snapshot, and takes its time over a second one, as a real
 * handshake does: a whole-fleet reconnect is then on screen for long enough
 * to be seen.
 */

const LOCAL = "deck-0000000000001994";
const REMOTE = "deck-0000000000001995";
const HOST = "deploy@build-box:2222";

function snapshot(deckId: string, connected: boolean) {
  const local = deckId === LOCAL;
  return {
    connection: {
      status: connected ? "connected" : "disconnected",
      deckId,
      deckKind: local ? "local" : "remote",
      socketPath: local ? "/var/tmp/deck-reconnect-browser.sock" : HOST,
      clientProtocolVersion: 10,
      clientBuildVersion: "0.45.0",
      ...(connected
        ? { serverProtocolVersion: 10, daemonBuildVersion: "0.45.0" }
        : { error: `Could not reach ${HOST}.`, disconnectedReason: { kind: "unreachable", action: "reconnect", message: `The app could not reach ${HOST}.`, host: HOST } }),
    },
    agents: local
      ? [{ id: "1", paneId: "pane-1", displayName: "planner", agentType: "claude_code", status: "working", toolCount: 0, rows: 32, cols: 120, cwd: "/work/planner", incarnation: "process-a", tab: { kind: "dashboard" } }]
      : [],
    protocolVersion: 10,
    source: "daemon",
    fleet: [LOCAL, REMOTE],
    observed: [
      { deckId: LOCAL, label: "/var/tmp/deck-reconnect-browser.sock", deckKind: "local" },
      { deckId: REMOTE, label: HOST, deckKind: "remote" },
    ],
  };
}

type Snapshot = ReturnType<typeof snapshot>;
type MockWindow = Window & {
  __dadCalls: { command: string; deckId?: unknown }[];
  __dadEmit: (payload: Snapshot) => void;
};

async function twoDecks(page: Page) {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.addInitScript(({ local, remote, remoteUp }) => {
    const callbacks = new Map<number, (event: unknown) => void>();
    const listeners = new Map<string, number[]>();
    const calls: { command: string; deckId?: unknown }[] = [];
    let next = 0;
    let bootstraps = 0;
    const emit = (payload: unknown) => {
      for (const id of listeners.get("desktop://snapshot") ?? []) callbacks.get(id)?.({ event: "desktop://snapshot", id, payload });
    };
    Object.defineProperty(window, "__dadCalls", { value: calls });
    Object.defineProperty(window, "__dadEmit", { value: emit });
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {
      transformCallback: (callback: (event: unknown) => void) => {
        const id = ++next;
        callbacks.set(id, callback);
        return id;
      },
      unregisterCallback: (id: number) => callbacks.delete(id),
      invoke: async (command: string, args: Record<string, unknown> = {}) => {
        if (command === "plugin:event|listen") {
          const event = String(args.event);
          listeners.set(event, [...(listeners.get(event) ?? []), Number(args.handler)]);
          return args.handler;
        }
        if (command === "plugin:event|unlisten") {
          const event = String(args.event);
          listeners.set(event, (listeners.get(event) ?? []).filter((id) => id !== args.eventId));
          return;
        }
        if (command === "desktop_bootstrap") {
          calls.push({ command });
          // The crate's `desktop_bootstrap` answers the selected deck alone.
          // A second one is a reconnect, and a reconnect handshakes.
          if (++bootstraps > 1) await new Promise((resolve) => setTimeout(resolve, 2_000));
          return local;
        }
        if (command === "desktop_reconnect_deck") {
          calls.push({ command, deckId: args.deckId });
          if (args.deckId !== remote.connection.deckId) throw new Error("Reconnect must name the deck whose card it is on.");
          emit(remoteUp);
          return remoteUp;
        }
        if (command === "desktop_features") return {};
        if (command === "desktop_get_settings") return { settings: { version: 1, appearance: { mode: "light" }, zoom: { level: 1 } } };
        if (command === "desktop_set_zoom") return args.level;
        return { ok: true };
      },
    } });
    Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", {
      value: { unregisterListener: (_event: string, id: number) => callbacks.delete(id) },
    });
  }, { local: snapshot(LOCAL, true), remote: snapshot(REMOTE, false), remoteUp: snapshot(REMOTE, true) });
  await page.goto("/?live=1");
  const localCard = page.locator(`[data-testid="daemon-group"][data-daemon-id="${LOCAL}"]`);
  await expect(localCard).toHaveAttribute("data-deck-connected", "yes");
  // The remote deck's watcher reports after the bootstrap has folded, which
  // is the order the crate's watchers produce.
  await page.evaluate((payload) => (window as unknown as MockWindow).__dadEmit(payload), snapshot(REMOTE, false));
  const remoteCard = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE}"]`);
  await expect(remoteCard.getByTestId("overview-disconnected")).toBeVisible();
  await expect(localCard.locator(".overview-row")).toHaveCount(1);
  return { localCard, remoteCard };
}

test.describe("Reconnect on one deck (issue #1994)", () => {
  /// Scenario: The dashboard shows a connected local deck with one agent and a disconnected remote deck.
  /// Pressing Reconnect on the remote deck's section reconnects that deck, and the local deck's section keeps its agent row and never shows a reconnect of its own.
  test("reconnects the deck whose section it is on and leaves the other deck alone", async ({ page }) => {
    const { localCard, remoteCard } = await twoDecks(page);
    // Record whether the local deck's section ever stops showing its agent,
    // however briefly: a whole-fleet reconnect replaces it with the
    // "Establishing control channel" note while it runs.
    await localCard.evaluate((node) => {
      const seen = { lost: false };
      Object.defineProperty(window, "__dadLocalLost", { value: seen });
      new MutationObserver(() => {
        if (node.querySelector("[data-testid='overview-loading']") || !node.querySelector(".overview-row")) seen.lost = true;
      }).observe(node, { subtree: true, childList: true, attributes: true });
    });

    await remoteCard.getByRole("button", { name: "Reconnect", exact: true }).click();

    // What the click reached: this deck's link, by name, and no fleet-wide
    // bootstrap beside the one the app started with.
    await expect.poll(async () => (await page.evaluate(() => (window as unknown as MockWindow).__dadCalls)).slice(1)).toEqual([
      { command: "desktop_reconnect_deck", deckId: REMOTE },
    ]);
    await expect(remoteCard).toHaveAttribute("data-deck-connected", "yes");
    await expect(remoteCard.getByTestId("overview-disconnected")).toHaveCount(0);
    await expect(localCard).toHaveAttribute("data-deck-connected", "yes");
    await expect(localCard.getByTestId("overview-loading")).toHaveCount(0);
    await expect(localCard.locator(".overview-row")).toHaveCount(1);
    expect(await page.evaluate(() => (window as unknown as { __dadLocalLost: { lost: boolean } }).__dadLocalLost.lost), "the local deck's section was reconnected by the remote deck's Reconnect").toBe(false);
  });

  /// Scenario: The same two decks, and the dashboard's Refresh button at the top.
  /// Refresh still reconnects every daemon, so the whole-fleet reconnect keeps a button of its own.
  test("Refresh still reconnects the whole fleet", async ({ page }) => {
    await twoDecks(page);
    await page.getByTestId("overview-refresh").click();
    await expect.poll(async () => (await page.evaluate(() => (window as unknown as MockWindow).__dadCalls)).map((call) => call.command)).toEqual(["desktop_bootstrap", "desktop_bootstrap"]);
  });
});
