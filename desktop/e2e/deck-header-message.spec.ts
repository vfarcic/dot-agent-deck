import { expect, type Locator, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * Issue #2406 — a deck's connection message on the dashboard, as a reader sees
 * it in an engine that lays out.
 *
 * The message used to sit inline in the deck's header row, beside the lamp,
 * the name, the Host button, the status pips, Upgrade, Stop daemon and New
 * agent. A full sentence there — the connected-anyway caveat issue #801 keeps
 * on screen for the whole session — took the row's width from everything else:
 * the Host button overlapped the name, the pips stacked, and button labels
 * wrapped. jsdom computes no geometry, so this is the tier that can see it.
 */

const DECK = "deck-0000000000002406";
/** The connected-anyway message, as `daemon_bridge.rs` words it for a session allowance. */
const CONNECTED_ANYWAY = "This daemon is older than this app. You chose to connect anyway for this session, so some of what this daemon shows may be wrong. Update the daemon to this app's version.";
const GiB = 1024 ** 3;

function snapshot(message: { text: string; mismatch: boolean } | undefined) {
  // Four distinct pips: running, needs input, failed, idle.
  const statuses = ["working", "idle", "waiting_for_input", "error"];
  return {
    connection: {
      status: "connected", deckKind: "remote", deckId: DECK, socketPath: "deploy@build-box",
      clientProtocolVersion: 10, serverProtocolVersion: 10,
      clientBuildVersion: "0.51.0", daemonBuildVersion: message?.mismatch ? "0.50.0" : "0.51.0",
      daemonDetected: true, runningAgentCount: statuses.length,
      upgradeOffer: { kind: "offered", from: "0.50.0", to: "0.51.0" },
      // The crate carries the connected-anyway sentence in `error` (the bridge maps it to the message).
      ...(message ? { error: message.text, buildStampMismatchOnly: message.mismatch } : {}),
    },
    agents: statuses.map((status, index) => ({
      id: `agent-${index}`, paneId: `pane-${index}`, displayName: `Agent ${index}`, agentType: "claude", status,
      toolCount: 0, rows: 32, cols: 120, incarnation: `header-${index}`, tab: { kind: "dashboard" },
    })),
    protocolVersion: 10, source: "daemon", fleet: [DECK],
    hostMetrics: { status: "available", metrics: {
      disks: [{ role: "working_root", freeBytes: 128 * GiB, totalBytes: 512 * GiB }],
      loadPerCpu: 0.5, cpuCount: 8, memoryUsedBytes: 8 * GiB, memoryAvailableBytes: 24 * GiB,
      sampledAtMs: 1_700_000_000_000, sampleAgeMs: 1000,
    } },
  };
}

/**
 * Load the dashboard against one live deck described by `initial`, with Tauri's
 * IPC mocked. The deck is a remote one so its Upgrade is the daemon's own and
 * the mock needs no answer for this machine's self-upgrade check; the header
 * row carries the same controls either way.
 */
async function openDeck(page: Page, initial: ReturnType<typeof snapshot>): Promise<Locator> {
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.addInitScript((initial) => {
    const callbacks = new Map<number, (event: unknown) => void>();
    let next = 0;
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {
      transformCallback: (callback: (event: unknown) => void) => { const id = ++next; callbacks.set(id, callback); return id; },
      unregisterCallback: (id: number) => callbacks.delete(id),
      invoke: async (command: string, args: Record<string, unknown> = {}) => {
        if (command === "plugin:event|listen") return args.handler;
        if (command === "plugin:event|unlisten") return;
        if (command === "desktop_bootstrap" || command === "desktop_get_snapshot") return initial;
        if (command === "desktop_features") return {};
        if (command === "desktop_get_settings") return { settings: { version: 1, appearance: { mode: "light" }, zoom: { level: 1 } } };
        if (command === "desktop_set_zoom") return args.level;
        return { ok: true };
      },
    } });
    Object.defineProperty(window, "__TAURI_EVENT_PLUGIN_INTERNALS__", {
      value: { unregisterListener: (_event: string, id: number) => callbacks.delete(id) },
    });
  }, initial);
  await page.goto("/?live=1");
  const card = page.locator(`[data-testid="daemon-group"][data-daemon-id="${DECK}"]`);
  await expect(card).toHaveAttribute("data-deck-connected", "yes");
  // Every control the row carries for this deck, so a missing one fails here
  // rather than letting the geometry below pass on a shorter row.
  for (const id of ["daemon-identity", "host-metrics-button", "daemon-upgrade", "daemon-stop", "daemon-new-agent"]) {
    await expect(card.getByTestId(id)).toBeVisible();
  }
  await expect(card.locator(".daemon-pips .status-label")).toHaveCount(4);
  return card;
}

type Box = { left: number; right: number; top: number; bottom: number };

/**
 * The header row's geometry: the name and Host button boxes, each pip's top,
 * and how many lines each button label's text lays out on. A label's lines are
 * read from a Range over its text, which reports one rectangle per line box —
 * the element's own rect would be one box however many lines it holds.
 */
async function headerGeometry(card: Locator) {
  return card.locator("header.daemon-group-header").evaluate((header) => {
    const box = (element: Element | null): Box => {
      if (!element) throw new Error("missing header element");
      const rect = element.getBoundingClientRect();
      return { left: rect.left, right: rect.right, top: rect.top, bottom: rect.bottom };
    };
    const lines = (element: Element): number => {
      const range = document.createRange();
      range.selectNodeContents(element);
      return new Set([...range.getClientRects()].filter((rect) => rect.width > 0).map((rect) => Math.round(rect.top))).size;
    };
    const controls = ["daemon-identity", "host-metrics-button", "daemon-upgrade", "daemon-stop", "daemon-new-agent"]
      .map((id) => header.querySelector(`[data-testid="${id}"]`));
    return {
      name: box(header.querySelector('[data-testid="daemon-identity"]')),
      host: box(header.querySelector('[data-testid="host-metrics-button"]')),
      pipTops: [...header.querySelectorAll(".daemon-pips .status-label")].map((pip) => Math.round(pip.getBoundingClientRect().top)),
      pipLines: [...header.querySelectorAll(".daemon-pips .status-label")].map(lines),
      labelLines: [...header.querySelectorAll("button > span")].map((label) => ({ label: label.textContent, lines: lines(label) })),
      controls: controls.map(box),
      message: header.querySelector('[data-testid="daemon-state"]') ? box(header.querySelector('[data-testid="daemon-state"]')) : undefined,
      header: box(header),
    };
  });
}

/** The assertions both cases share: the row's controls keep their normal layout. */
function expectRowIntact(geometry: Awaited<ReturnType<typeof headerGeometry>>) {
  // The name and the Host button beside it do not overlap.
  expect(geometry.name.right).toBeLessThanOrEqual(geometry.host.left);
  // The pips sit on one line, each on one line.
  expect(new Set(geometry.pipTops).size).toBe(1);
  expect(geometry.pipLines.every((count) => count === 1)).toBe(true);
  // Every button's label lays out on one line.
  expect(geometry.labelLines.filter((entry) => entry.lines !== 1)).toEqual([]);
  // The row's controls share one line: no control starts below another's bottom.
  const lowestTop = Math.max(...geometry.controls.map((control) => control.top));
  const highestBottom = Math.min(...geometry.controls.map((control) => control.bottom));
  expect(lowestTop).toBeLessThan(highestBottom);
}

test.describe("a deck's connection message (issue #2406)", () => {
  /**
   * Scenario: open the dashboard on a connected-anyway deck whose header also
   * carries the Host button, four status pips, Upgrade, Stop daemon and New
   * agent. The caveat reads in full on its own line below the row, and the
   * row's controls keep their layout: no overlap, pips on one line, button
   * labels on one line.
   */
  test("goes on its own line below the header row and leaves the row's controls alone", async ({ page }) => {
    const card = await openDeck(page, snapshot({ text: CONNECTED_ANYWAY, mismatch: true }));
    const message = card.getByTestId("daemon-state");
    await expect(message).toBeVisible();
    await expect(message).toHaveText(CONNECTED_ANYWAY);

    const geometry = await headerGeometry(card);
    expectRowIntact(geometry);
    // Below every control in the row, not beside them.
    const rowBottom = Math.max(...geometry.controls.map((control) => control.bottom));
    expect(geometry.message!.top).toBeGreaterThanOrEqual(rowBottom);
    // Inside the header band, and not cut off: every character of the caveat
    // issue #801 keeps on screen is laid out, not ellipsised away.
    expect(geometry.message!.bottom).toBeLessThanOrEqual(geometry.header.bottom);
    expect(await message.evaluate((node) => node.scrollWidth <= node.clientWidth)).toBe(true);
  });

  /**
   * Scenario: open the dashboard on a healthy connected deck with the same
   * controls. There is no message, so there is no message line and the header
   * is one row tall.
   */
  test("adds no row when the deck has nothing to say", async ({ page }) => {
    const card = await openDeck(page, snapshot(undefined));
    await expect(card.getByTestId("daemon-state")).toHaveCount(0);

    const geometry = await headerGeometry(card);
    expectRowIntact(geometry);
    expect(geometry.message).toBeUndefined();
    // One row: the header is no taller than its 50px floor, which the row's
    // controls fit inside.
    expect(geometry.header.bottom - geometry.header.top).toBeLessThanOrEqual(50);
  });
});
