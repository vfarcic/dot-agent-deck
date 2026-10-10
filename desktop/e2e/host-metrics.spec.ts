import { expect, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

// Required contract: the existing snapshot IPC carries one daemon's facts at
// a time; the Rust bridge capability-gates queries and supplies hostMetrics.
const LOCAL = "deck-0000000000001258";
const REMOTE = "deck-0000000000001259";
const GiB = 1024 ** 3;

interface HostMetrics {
  disks: { role: string; freeBytes?: number; totalBytes?: number }[];
  loadPerCpu?: number;
  cpuCount?: number;
  memoryUsedBytes?: number;
  memoryAvailableBytes?: number;
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

/** Scenario: Two connected decks report distinct utilisation through bootstrap and snapshot events. The overview shows both hosts' disk roles, load, cores, memory and sample age at once, and a new remote sample changes only its own deck. */
test("each connected deck shows its own host utilisation and sample age", async ({ page }) => {
  const { localCard, remoteCard } = await overview(page, available(), available(true));
  for (const [card, free, worktreeFree, tempFree, load, cores, used, memory, age] of [
    [localCard, 128, 64, 8, "0.75", 8, 12, 20, 1500],
    [remoteCard, 96, 48, 6, "1.25", 16, 24, 40, 2500],
  ] as const) {
    const metrics = card.getByTestId("host-metrics");
    await expect(metrics).toBeVisible();
    await expect(metrics).toContainText("Host of this deck");
    await expect(metrics).toContainText("free");
    await expect(metrics).toContainText("total");
    await expect(metrics).toContainText(new RegExp(`Working root[^\\n]*${free}(?:\\.0+)? GiB[^\\n]*512(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Worktree parent[^\\n]*${worktreeFree}(?:\\.0+)? GiB[^\\n]*256(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Temp root[^\\n]*${tempFree}(?:\\.0+)? GiB[^\\n]*16(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Load per core[^\\n]*${load.replace(".", "\\.")}[^\\n]*${cores} cores`));
    await expect(metrics).toContainText(new RegExp(`Memory used[^\\n]*${used}(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Memory available[^\\n]*${memory}(?:\\.0+)? GiB`));
    await expect(metrics).toContainText(new RegExp(`Sample age[^\\n]*${age} ms`));
  }
  await expect(localCard.getByTestId("daemon-identity")).toContainText("Local daemon");
  await expect(remoteCard.getByTestId("daemon-identity")).toContainText("build@remote-host");
  const updated = available(true);
  if (updated.status !== "available") throw new Error("available fixture");
  updated.metrics.disks[0].freeBytes = 80 * GiB;
  updated.metrics.sampleAgeMs = 3500;
  await page.evaluate((payload) => (window as unknown as MockWindow).__dadHostMetricsSnapshot(payload), snapshot(REMOTE, updated));
  await expect(remoteCard.getByTestId("host-metrics")).toContainText(/Working root[^\n]*80(?:\.0+)? GiB/);
  await expect(remoteCard.getByTestId("host-metrics")).toContainText(/Sample age[^\n]*3500 ms/);
  await expect(localCard.getByTestId("host-metrics")).toContainText(/Working root[^\n]*128(?:\.0+)? GiB/);
  await expect(localCard.getByTestId("host-metrics")).toContainText(/Sample age[^\n]*1500 ms/);
});

/** Scenario: A connected old daemon lacks host-metrics while another deck reports valid numbers. Its host section says “not available from this deck” without displaying zero readings, and the capable deck keeps its own numbers. */
test("an old daemon says metrics are not available from this deck without zeros", async ({ page }) => {
  const { localCard, remoteCard } = await overview(page, available(), { status: "not-available" });
  const unavailable = remoteCard.getByTestId("host-metrics");
  await expect(unavailable).toBeVisible();
  await expect(unavailable).toContainText("Host of this deck");
  await expect(unavailable).toContainText("not available from this deck");
  await expect(unavailable).not.toContainText(/\b0(?:\.0+)?\s*(?:GiB|cores|ms)\b/);
  await expect(unavailable).not.toContainText("0.00");
  await expect(localCard.getByTestId("host-metrics")).toContainText(/Working root[^\n]*128(?:\.0+)? GiB/);
  await expect(localCard.getByTestId("host-metrics")).not.toContainText("not available from this deck");
});

/** Scenario: A capable daemon returns unreadable individual fields with a known sample age. Every missing disk, load, core-count and memory value is visibly unknown instead of zero while the other deck remains measurable. */
test("absent host fields render unknown independently", async ({ page }) => {
  const unknown: HostMetricsReport = { status: "available", metrics: {
    disks: [{ role: "working_root" }, { role: "worktree_parent" }, { role: "temp_root" }],
    sampledAtMs: 1_700_000_000_000, sampleAgeMs: 1500,
  } };
  const { localCard } = await overview(page, unknown, available(true));
  const metrics = localCard.getByTestId("host-metrics");
  await expect(metrics).toBeVisible();
  for (const label of ["Working root", "Worktree parent", "Temp root", "Load per core", "Memory used", "Memory available"]) {
    await expect(metrics).toContainText(new RegExp(`${label}[^\\n]*unknown`));
  }
  await expect(metrics).toContainText(/Load per core[^\n]*unknown[^\n]*unknown/);
  await expect(metrics).toContainText(/Sample age[^\n]*1500 ms/);
  await expect(metrics).not.toContainText(/\b0(?:\.0+)?\s*(?:GiB|cores)\b/);
});
