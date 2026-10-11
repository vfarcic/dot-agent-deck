import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import shared from "../../../tests/fixtures/host-metrics-copy.json";
import type { HostMetrics } from "../types";
import { HostMetricsPanel } from "./HostMetricsPanel";

/*
 * PRD #1258, CLAUDE.md rule 22: the deck card's host panel uses the TUI host
 * overlay's words and formats. Both read `tests/fixtures/host-metrics-copy.json`
 * (the TUI in `tests/render_host_metrics_copy.rs`), so a word changed in one
 * client alone fails the other client's test.
 */

/** The daemon's wire shape, snake_case; every reading but the times may be absent. */
interface WireSample {
  disks: { role: string; free_bytes?: number; total_bytes?: number }[];
  load_per_cpu?: number;
  cpu_count?: number;
  memory_used_bytes?: number;
  memory_available_bytes?: number;
  sampled_at_ms: number;
  sample_age_ms: number;
}

/** The daemon's snake_case sample as the bridge hands it to the webview. */
function fromWire(wire: WireSample): HostMetrics {
  return {
    disks: wire.disks.map((disk) => ({ role: disk.role, freeBytes: disk.free_bytes, totalBytes: disk.total_bytes })),
    loadPerCpu: wire.load_per_cpu,
    cpuCount: wire.cpu_count,
    memoryUsedBytes: wire.memory_used_bytes,
    memoryAvailableBytes: wire.memory_available_bytes,
    sampledAtMs: wire.sampled_at_ms,
    sampleAgeMs: wire.sample_age_ms,
  };
}

describe("the host panel's words are the TUI overlay's (tests/fixtures/host-metrics-copy.json)", () => {
  /** Scenario: each shared sample renders exactly the shared label/value rows, in order, under the shared title and subtitle. */
  it("renders every shared sample as the shared rows", () => {
    for (const { sample, rows } of shared.samples) {
      const { container, unmount } = render(<HostMetricsPanel report={{ status: "available", metrics: fromWire(sample as WireSample) }} />);
      expect(container.querySelector("header strong")?.textContent).toBe(shared.title);
      expect(container.querySelector(".host-metrics-subtitle")?.textContent).toBe(shared.subtitle);
      const shown = Array.from(container.querySelectorAll(".host-metrics-row")).map((row) => [
        row.querySelector("dt")?.textContent,
        row.querySelector("dd")?.textContent,
      ]);
      expect(shown).toEqual(rows);
      unmount();
    }
  });

  /** Scenario: a deck without host metrics shows the shared not-available sentences. */
  it("says the shared words for a deck without host metrics", () => {
    const { container } = render(<HostMetricsPanel report={{ status: "not-available" }} />);
    expect(container.querySelector(".host-metrics-unavailable")?.textContent).toBe(shared.notAvailable.join(" "));
  });
});
