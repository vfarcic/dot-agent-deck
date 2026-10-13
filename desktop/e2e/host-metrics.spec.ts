import { expect, type Locator, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

// Required contract: the existing snapshot IPC carries one daemon's facts at
// a time; the Rust bridge capability-gates queries and supplies hostMetrics.
const LOCAL = "deck-0000000000001258";
const REMOTE = "deck-0000000000001259";
const GiB = 1024 ** 3;

type Level = "ok" | "warn" | "critical";

interface HostMetrics {
  disks: { role: string; freeBytes?: number; totalBytes?: number; level?: Level }[];
  loadPerCpu?: number;
  cpuCount?: number;
  memoryUsedBytes?: number;
  memoryAvailableBytes?: number;
  loadLevel?: Level;
  memoryLevel?: Level;
  sampledAtMs: number;
  sampleAgeMs: number;
}

type HostMetricsReport = { status: "available"; metrics: HostMetrics } | { status: "not-available" };

function available(remote = false): HostMetricsReport {
  return { status: "available", metrics: {
    disks: [
      { role: "working_root", freeBytes: (remote ? 96 : 128) * GiB, totalBytes: 512 * GiB },
      { role: "worktree_parent", freeBytes: (remote ? 48 : 64) * GiB, totalBytes: 256 * GiB },
      { role: "temp_root", freeBytes: (remote ? 6 : 8) * GiB, totalBytes: 16 * GiB },
    ],
    loadPerCpu: remote ? 1.25 : 0.75,
    cpuCount: remote ? 16 : 8,
    memoryUsedBytes: (remote ? 24 : 12) * GiB,
    memoryAvailableBytes: (remote ? 40 : 20) * GiB,
    sampledAtMs: 1_700_000_000_000,
    sampleAgeMs: remote ? 2500 : 1500,
  } };
}

function snapshot(deckId: string, hostMetrics: HostMetricsReport) {
  return {
    connection: {
      status: "connected", deckId,
      deckKind: deckId === LOCAL ? "local" : "remote",
      socketPath: deckId === LOCAL ? "/var/tmp/host-metrics-browser.sock" : "build@remote-host",
      clientProtocolVersion: 10, serverProtocolVersion: 10,
      clientBuildVersion: "0.45.0", daemonBuildVersion: "0.45.0",
    },
    agents: [], protocolVersion: 10, source: "daemon",
    fleet: [LOCAL, REMOTE], allDecks: true,
    observed: [
      { deckId: LOCAL, label: "/var/tmp/host-metrics-browser.sock", deckKind: "local" },
      { deckId: REMOTE, label: "build@remote-host", deckKind: "remote" },
    ],
    hostMetrics,
  };
}

type Snapshot = ReturnType<typeof snapshot>;
type MockWindow = Window & { __dadHostMetricsSnapshot: (payload: Snapshot) => void };

async function overview(page: Page, local: HostMetricsReport, remote: HostMetricsReport) {
  await page.setViewportSize({ width: 1440, height: 1000 });
  await page.addInitScript((initial) => {
    const callbacks = new Map<number, (event: unknown) => void>();
    const listeners = new Map<string, number[]>();
    let next = 0;
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
    Object.defineProperty(window, "__dadHostMetricsSnapshot", { value: (payload: Snapshot) => {
      const event = "desktop://snapshot";
      const handlers = listeners.get(event) ?? [];
      if (!handlers.length) throw new Error("snapshot listener is not installed");
      for (const id of handlers) callbacks.get(id)?.({ event, id, payload });
    } });
  }, snapshot(LOCAL, local));
  await page.goto("/?live=1");
  const localCard = page.locator(`[data-testid="daemon-group"][data-daemon-id="${LOCAL}"]`);
  await expect(localCard).toHaveAttribute("data-deck-connected", "yes");
  // Bootstrap has folded before this second daemon arrives, so it cannot
  // erase the remote sample by clearing the map after our event.
  await page.evaluate((payload) => (window as unknown as MockWindow).__dadHostMetricsSnapshot(payload), snapshot(REMOTE, remote));
  const remoteCard = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE}"]`);
  await expect(remoteCard).toHaveAttribute("data-deck-connected", "yes");
  await expect(page.getByTestId("daemon-group")).toHaveCount(2);
  return { localCard, remoteCard };
}

/** Open a deck's host popover from the button beside its name, and return its numbers. */
async function openHost(card: Locator): Promise<Locator> {
  await card.getByTestId("host-metrics-button").click();
  const metrics = card.getByTestId("host-metrics");
  await expect(metrics).toBeVisible();
  return metrics;
}

/** Scenario: Two connected decks report distinct utilisation through bootstrap and snapshot events. Each deck has its own host button beside its name; opening one shows that host's disk roles, load, cores and memory but no sample age, and a new remote sample changes only its own deck. */
test("each connected deck shows its own host utilisation", async ({ page }) => {
  const { localCard, remoteCard } = await overview(page, available(), available(true));
  for (const [card, free, worktreeFree, tempFree, load, cores, used, memory] of [
    [localCard, 128, 64, 8, "0.75", 8, 12, 20],
    [remoteCard, 96, 48, 6, "1.25", 16, 24, 40],
  ] as const) {
    const metrics = await openHost(card);
    await expect(metrics).toContainText("Host of this deck");
    await expect(metrics).toContainText("free");
    await expect(metrics).toContainText("total");
    await expect(metrics).toContainText(new RegExp(`Working root[^\\n]*${free}(?:\\.0+)? GiB[^\\n]*512(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Worktree parent[^\\n]*${worktreeFree}(?:\\.0+)? GiB[^\\n]*256(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Temp root[^\\n]*${tempFree}(?:\\.0+)? GiB[^\\n]*16(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Load per core[^\\n]*${load.replace(".", "\\.")}[^\\n]*${cores} cores`));
    await expect(metrics).toContainText(new RegExp(`Memory used[^\\n]*${used}(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Memory available[^\\n]*${memory}(?:\\.0+)? GiB`));
    // The daemon's sample age is on the wire but not shown (PR #1672).
    await expect(metrics).not.toContainText("Sample age");
  }
  await expect(localCard.getByTestId("daemon-identity")).toContainText("Local daemon");
  await expect(remoteCard.getByTestId("daemon-identity")).toContainText("build@remote-host");
  const updated = available(true);
  if (updated.status !== "available") throw new Error("available fixture");
  updated.metrics.disks[0].freeBytes = 80 * GiB;
  updated.metrics.sampleAgeMs = 3500;
  await page.evaluate((payload) => (window as unknown as MockWindow).__dadHostMetricsSnapshot(payload), snapshot(REMOTE, updated));
  // The remote popover is still the open one, and it shows the new sample.
  await expect(remoteCard.getByTestId("host-metrics")).toContainText(/Working root[^\n]*80(?:\.0+)? GiB/);
  const local = await openHost(localCard);
  await expect(remoteCard.getByTestId("host-metrics")).toHaveCount(0);
  await expect(local).toContainText(/Working root[^\n]*128(?:\.0+)? GiB/);
});

/** Scenario: A connected old daemon lacks host-metrics while another deck reports valid numbers. Its host button names that state, and opening it says “not available from this deck” without displaying zero readings, while the capable deck keeps its own numbers. */
test("an old daemon says metrics are not available from this deck without zeros", async ({ page }) => {
  const { localCard, remoteCard } = await overview(page, available(), { status: "not-available" });
  const button = remoteCard.getByTestId("host-metrics-button");
  await expect(button).toHaveAttribute("aria-label", /not available from this deck/);
  await expect(button).toHaveAttribute("data-level", "");
  const unavailable = await openHost(remoteCard);
  await expect(unavailable).toContainText("Host of this deck");
  await expect(unavailable).toContainText("not available from this deck");
  await expect(unavailable).not.toContainText(/\b0(?:\.0+)?\s*(?:GiB|cores|ms)\b/);
  await expect(unavailable).not.toContainText("0.00");
  const local = await openHost(localCard);
  await expect(local).toContainText(/Working root[^\n]*128(?:\.0+)? GiB/);
  await expect(local).not.toContainText("not available from this deck");
});

/** Scenario: A capable daemon returns unreadable individual fields. Every missing disk, load, core-count and memory value is visibly unknown instead of zero while the other deck remains measurable. */
test("absent host fields render unknown independently", async ({ page }) => {
  const unknown: HostMetricsReport = { status: "available", metrics: {
    disks: [{ role: "working_root" }, { role: "worktree_parent" }, { role: "temp_root" }],
    sampledAtMs: 1_700_000_000_000, sampleAgeMs: 1500,
  } };
  const { localCard } = await overview(page, unknown, available(true));
  const metrics = await openHost(localCard);
  for (const label of ["Working root", "Worktree parent", "Temp root", "Load per core", "Memory used", "Memory available"]) {
    await expect(metrics).toContainText(new RegExp(`${label}[^\\n]*unknown`));
  }
  await expect(metrics).toContainText(/Load per core[^\n]*unknown[^\n]*unknown/);
  await expect(metrics).not.toContainText(/\b0(?:\.0+)?\s*(?:GiB|cores)\b/);
});

/** A sample whose readings the daemon judged: one disk critical, one a warning, the load a warning and memory ok. */
function judged(): HostMetricsReport {
  return { status: "available", metrics: {
    disks: [
      { role: "working_root", freeBytes: 4 * GiB, totalBytes: 100 * GiB, level: "critical" },
      { role: "worktree_parent", freeBytes: 10 * GiB, totalBytes: 100 * GiB, level: "warn" },
      { role: "temp_root", freeBytes: 50 * GiB, totalBytes: 100 * GiB, level: "ok" },
    ],
    loadPerCpu: 1, cpuCount: 4, loadLevel: "warn",
    memoryUsedBytes: 8 * GiB, memoryAvailableBytes: 8 * GiB, memoryLevel: "ok",
    sampledAtMs: 1_700_000_000_000, sampleAgeMs: 10,
  } };
}

/** Scenario: One deck reports a critical disk and a warning load, the other only ok readings. Each deck's host button is coloured by its worst level and says that level in words; in the open popover every non-ok value carries its colour class and its word, and an ok value carries its class and no word. */
test("each deck's host button and values are coloured by the daemon's levels, with words", async ({ page }) => {
  const okOnly = available(true);
  if (okOnly.status !== "available") throw new Error("available fixture");
  okOnly.metrics.disks.forEach((disk) => { disk.level = "ok"; });
  okOnly.metrics.loadLevel = "ok";
  okOnly.metrics.memoryLevel = "ok";
  const { localCard, remoteCard } = await overview(page, judged(), okOnly);

  const critical = localCard.getByTestId("host-metrics-button");
  await expect(critical).toHaveAttribute("data-level", "critical");
  await expect(critical).toHaveClass(/host-button-critical/);
  await expect(critical).toHaveAttribute("aria-label", /: critical$/);
  await expect(critical).toContainText("critical");
  const ok = remoteCard.getByTestId("host-metrics-button");
  await expect(ok).toHaveAttribute("data-level", "ok");
  await expect(ok).toHaveClass(/host-button-ok/);
  await expect(ok).toHaveAttribute("aria-label", /: ok$/);
  // The two buttons are told apart by colour, and by more than colour.
  const colour = (locator: Locator) => locator.evaluate((element) => getComputedStyle(element).backgroundColor);
  expect(await colour(critical)).not.toBe(await colour(ok));

  const metrics = await openHost(localCard);
  const value = (label: string) => metrics.locator(".host-metrics-row", { hasText: label }).locator("dd");
  await expect(value("Working root")).toHaveClass(/host-level-critical/);
  await expect(value("Working root")).toHaveText("4 GiB free of 100 GiB total (critical)");
  await expect(value("Worktree parent")).toHaveClass(/host-level-warn/);
  await expect(value("Worktree parent")).toHaveText("10 GiB free of 100 GiB total (warning)");
  await expect(value("Temp root")).toHaveClass(/host-level-ok/);
  await expect(value("Temp root")).toHaveText("50 GiB free of 100 GiB total");
  await expect(value("Load per core")).toHaveText("1.00 across 4 cores (warning)");
  await expect(value("Memory available")).toHaveClass(/host-level-ok/);
  await expect(value("Memory used")).not.toHaveClass(/host-level/);
});

/** Scenario: A deck whose daemon sends numbers but no levels — an older build — shows its button neutral, with no state in its name, and its values uncoloured rather than as ok. */
test("a sample without levels leaves the button neutral and the values uncoloured", async ({ page }) => {
  const { localCard } = await overview(page, available(), available(true));
  const button = localCard.getByTestId("host-metrics-button");
  await expect(button).toHaveAttribute("data-level", "");
  await expect(button).toHaveClass(/host-button-none/);
  await expect(button).toHaveAttribute("aria-label", "Host of this deck, Local daemon");
  const metrics = await openHost(localCard);
  await expect(metrics.locator("dd[class*='host-level']")).toHaveCount(0);
});

/** Scenario: Reach a deck's host button from the keyboard, open it with Enter and close it with Escape, which returns focus to the button; open it again and close it by pressing elsewhere on the page. */
test("the host popover opens from the keyboard and closes on Escape and on an outside press", async ({ page }) => {
  const { localCard } = await overview(page, judged(), available(true));
  const button = localCard.getByTestId("host-metrics-button");
  await button.focus();
  await page.keyboard.press("Enter");
  await expect(button).toHaveAttribute("aria-expanded", "true");
  await expect(localCard.getByTestId("host-metrics-popover")).toBeVisible();
  await page.keyboard.press("Escape");
  await expect(localCard.getByTestId("host-metrics-popover")).toHaveCount(0);
  await expect(button).toBeFocused();
  await expect(button).toHaveAttribute("aria-expanded", "false");

  await button.click();
  await expect(localCard.getByTestId("host-metrics-popover")).toBeVisible();
  // A press inside the popover keeps it open.
  await localCard.getByTestId("host-metrics").click();
  await expect(localCard.getByTestId("host-metrics-popover")).toBeVisible();
  await page.mouse.click(5, 990);
  await expect(localCard.getByTestId("host-metrics-popover")).toHaveCount(0);
});

/** Scenario: Open a deck's host popover and press its X, named "Close host", in the popover's top-right corner; the popover closes and focus returns to the Host button. */
test("the host popover closes from its X button", async ({ page }) => {
  const { localCard } = await overview(page, judged(), available(true));
  const button = localCard.getByTestId("host-metrics-button");
  await button.click();
  const popover = localCard.getByTestId("host-metrics-popover");
  const close = popover.getByRole("button", { name: "Close host" });
  await expect(close).toBeVisible();
  const [closeBox, popoverBox] = [await close.boundingBox(), await popover.boundingBox()];
  if (!closeBox || !popoverBox) throw new Error("the popover or its X has no box");
  // In the top-right corner, and clear of the first row of figures.
  expect(closeBox.x + closeBox.width).toBeGreaterThan(popoverBox.x + popoverBox.width - 12);
  expect(closeBox.y).toBeLessThan(popoverBox.y + 12);
  const firstRow = await popover.locator(".host-metrics-row").first().boundingBox();
  if (!firstRow) throw new Error("the first row has no box");
  expect(closeBox.y + closeBox.height).toBeLessThanOrEqual(firstRow.y);
  await close.click();
  await expect(popover).toHaveCount(0);
  await expect(button).toBeFocused();
  await expect(button).toHaveAttribute("aria-expanded", "false");
});

/** Scenario: In a narrow window, open each deck's host popover; it stays inside the window on both sides and opening it adds no horizontal scrolling to the page. */
test("the host popover stays inside a narrow window", async ({ page }) => {
  const { localCard, remoteCard } = await overview(page, judged(), available(true));
  await page.setViewportSize({ width: 480, height: 900 });
  const scrollWidth = () => page.evaluate(() => document.documentElement.scrollWidth);
  for (const card of [localCard, remoteCard]) {
    const before = await scrollWidth();
    await openHost(card);
    const box = await card.getByTestId("host-metrics-popover").boundingBox();
    if (!box) throw new Error("the popover has no box");
    expect(box.x).toBeGreaterThanOrEqual(0);
    expect(box.x + box.width).toBeLessThanOrEqual(480);
    expect(await scrollWidth()).toBeLessThanOrEqual(before);
    await page.keyboard.press("Escape");
    await expect(card.getByTestId("host-metrics-popover")).toHaveCount(0);
  }
});

/** Scenario: Open each deck's host popover in a wide window, then narrow the window while it stays open; the popover moves to stay inside the window and the page gains no horizontal scrolling. Widen the window again and the popover returns under its button. */
test("the host popover follows a window resized while it is open", async ({ page }) => {
  const { localCard, remoteCard } = await overview(page, judged(), available(true));
  const wide = page.viewportSize() ?? { width: 1280, height: 900 };
  const scrollWidth = () => page.evaluate(() => document.documentElement.scrollWidth);
  for (const card of [localCard, remoteCard]) {
    await page.setViewportSize(wide);
    await openHost(card);
    const popover = card.getByTestId("host-metrics-popover");
    await page.setViewportSize({ width: 480, height: wide.height });
    await expect(popover).toBeVisible();
    await expect.poll(async () => {
      const box = await popover.boundingBox();
      return box ? box.x >= 0 && box.x + box.width <= 480 : false;
    }).toBe(true);
    expect(await scrollWidth()).toBeLessThanOrEqual(480);
    await page.setViewportSize(wide);
    await expect(popover).toBeVisible();
    const button = await card.getByTestId("host-metrics-button").boundingBox();
    if (!button) throw new Error("the host button has no box");
    await expect.poll(async () => (await popover.boundingBox())?.x).toBeCloseTo(button.x, 0);
    await page.keyboard.press("Escape");
    await expect(popover).toHaveCount(0);
  }
});
