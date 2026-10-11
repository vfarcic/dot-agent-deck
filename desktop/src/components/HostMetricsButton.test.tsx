import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { HostMetricsReport } from "../types";
import { HostMetricsButton } from "./HostMetricsPanel";

/*
 * PRD #1258 — the deck header's compact host button: coloured by the worst
 * level the deck's daemon served, naming that state in words, opening the
 * numbers in a popover that Escape and an outside press close.
 */

const GiB = 1024 ** 3;

function report(levels: { disk?: "ok" | "warn" | "critical"; load?: "ok" | "warn" | "critical" }): HostMetricsReport {
  return {
    status: "available",
    metrics: {
      disks: [{ role: "working_root", freeBytes: 4 * GiB, totalBytes: 100 * GiB, level: levels.disk }],
      loadPerCpu: 1,
      cpuCount: 4,
      loadLevel: levels.load,
      sampledAtMs: 1,
      sampleAgeMs: 0,
    },
  };
}

describe("HostMetricsButton", () => {
  /** Scenario: a deck with a critical disk and a warning load gets a critical button that says so in its text, tooltip and accessible name; one with only ok readings an ok button; one without levels a neutral button whose name states nothing. */
  it("is coloured by the worst level and names it in words", () => {
    const { rerender } = render(<HostMetricsButton report={report({ disk: "critical", load: "warn" })} deckLabel="Local daemon" />);
    const button = screen.getByTestId("host-metrics-button");
    expect(button.className).toContain("host-button-critical");
    expect(button.getAttribute("aria-label")).toBe("Host of this deck, Local daemon: critical");
    expect(button.getAttribute("title")).toBe("Host of this deck: critical");
    expect(button.textContent).toBe("Host: critical");

    rerender(<HostMetricsButton report={report({ disk: "ok", load: "ok" })} deckLabel="Local daemon" />);
    expect(button.className).toContain("host-button-ok");
    expect(button.getAttribute("aria-label")).toBe("Host of this deck, Local daemon: ok");
    expect(button.textContent).toBe("Host");

    rerender(<HostMetricsButton report={report({})} deckLabel="Local daemon" />);
    expect(button.className).toContain("host-button-none");
    expect(button.getAttribute("aria-label")).toBe("Host of this deck, Local daemon");
  });

  /** Scenario: the button opens the popover with the coloured, worded values; Escape closes it and returns focus to the button; a press inside keeps it open and a press outside closes it. */
  it("opens a popover that Escape and an outside press close", () => {
    render(<div><HostMetricsButton report={report({ disk: "critical" })} deckLabel="Local daemon" /><p data-testid="outside">elsewhere</p></div>);
    const button = screen.getByTestId("host-metrics-button");
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    fireEvent.click(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
    const popover = screen.getByTestId("host-metrics-popover");
    expect(popover.getAttribute("role")).toBe("dialog");
    const disk = popover.querySelector(".host-metrics-row dd");
    expect(disk?.className).toBe("host-level-critical");
    expect(disk?.textContent).toBe("4 GiB free of 100 GiB total (critical)");

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    expect(document.activeElement).toBe(button);

    fireEvent.click(button);
    fireEvent.pointerDown(screen.getByTestId("host-metrics"));
    expect(screen.getByTestId("host-metrics-popover")).toBeTruthy();
    fireEvent.pointerDown(screen.getByTestId("outside"));
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
  });

  /** Scenario: a deck whose daemon does not report its host gets a neutral button that says so, and its popover gives the same not-available sentences as the TUI. */
  it("says a deck without host metrics does not report them", () => {
    render(<HostMetricsButton report={{ status: "not-available" }} deckLabel="build@remote-host" />);
    const button = screen.getByTestId("host-metrics-button");
    expect(button.className).toContain("host-button-none");
    expect(button.getAttribute("aria-label")).toBe("Host of this deck, build@remote-host: not available from this deck");
    fireEvent.click(button);
    expect(screen.getByTestId("host-metrics").textContent).toContain("Host metrics are not available from this deck.");
  });
});
