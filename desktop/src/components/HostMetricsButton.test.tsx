import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
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

  /** Scenario: the button opens a disclosure popover with the coloured, worded values; Escape inside it closes it and returns focus to the button; a press inside keeps it open and a press outside closes it. */
  it("opens a popover that Escape and an outside press close", () => {
    render(<div><HostMetricsButton report={report({ disk: "critical" })} deckLabel="Local daemon" /><p data-testid="outside">elsewhere</p></div>);
    const button = screen.getByTestId("host-metrics-button");
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    fireEvent.click(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
    const popover = screen.getByTestId("host-metrics-popover");
    expect(popover.getAttribute("role")).toBeNull();
    expect(button.getAttribute("aria-haspopup")).toBeNull();
    expect(button.getAttribute("aria-controls")).toBe(popover.id);
    const disk = popover.querySelector(".host-metrics-row dd");
    expect(disk?.className).toBe("host-level-critical");
    expect(disk?.textContent).toBe("4 GiB free of 100 GiB total (critical)");

    fireEvent.keyDown(screen.getByTestId("host-metrics"), { key: "Escape" });
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
    expect(document.activeElement).toBe(button);

    fireEvent.click(button);
    fireEvent.pointerDown(screen.getByTestId("host-metrics"));
    expect(screen.getByTestId("host-metrics-popover")).toBeTruthy();
    fireEvent.pointerDown(screen.getByTestId("outside"));
    expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
  });

  /** Scenario: with the popover open and focus elsewhere — the palette, a terminal — an Escape raised there closes the popover but still reaches a window listener and leaves focus where it was. */
  it("lets an Escape raised outside the popover through", () => {
    render(<div><HostMetricsButton report={report({ disk: "warn" })} deckLabel="Local daemon" /><input data-testid="outside" /></div>);
    const button = screen.getByTestId("host-metrics-button");
    const outside = screen.getByTestId("outside");
    const reached: string[] = [];
    const listener = (event: KeyboardEvent) => reached.push(event.key);
    window.addEventListener("keydown", listener);
    try {
      fireEvent.click(button);
      expect(screen.getByTestId("host-metrics-popover")).toBeTruthy();
      outside.focus();
      fireEvent.keyDown(outside, { key: "Escape" });
      expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
      expect(reached).toEqual(["Escape"]);
      expect(document.activeElement).toBe(outside);
    } finally {
      window.removeEventListener("keydown", listener);
    }
  });

  /** Scenario: an Escape raised inside the open popover does not reach a window listener, so the app-wide Escape does not also act on it. */
  it("keeps an Escape raised inside the popover to itself", () => {
    render(<HostMetricsButton report={report({ disk: "warn" })} deckLabel="Local daemon" />);
    const reached: string[] = [];
    const listener = (event: KeyboardEvent) => reached.push(event.key);
    window.addEventListener("keydown", listener);
    try {
      fireEvent.click(screen.getByTestId("host-metrics-button"));
      fireEvent.keyDown(screen.getByTestId("host-metrics"), { key: "Escape" });
      expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
      expect(reached).toEqual([]);
    } finally {
      window.removeEventListener("keydown", listener);
    }
  });

  /** Scenario: a popover opened from a button near the window's right edge is shifted left by its overflow; widening the window while it is open moves it back under its button, narrowing it again shifts it further, and closing it stops it listening for resizes. */
  it("re-places its open popover when the window is resized", () => {
    const anchorLeft = 500;
    const popoverWidth = 300;
    const rect = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
      const left = this.classList.contains("host-metrics-anchor") ? anchorLeft : 0;
      const width = this.classList.contains("host-metrics-popover") ? popoverWidth : 0;
      return { left, width, x: left, y: 0, top: 0, right: left + width, bottom: 0, height: 0, toJSON: () => ({}) } as DOMRect;
    });
    const initialWidth = window.innerWidth;
    const resizeTo = (width: number) => act(() => {
      window.innerWidth = width;
      window.dispatchEvent(new Event("resize"));
    });
    const removed = vi.spyOn(window, "removeEventListener");
    try {
      window.innerWidth = 700;
      render(<HostMetricsButton report={report({ disk: "critical" })} deckLabel="Local daemon" />);
      const button = screen.getByTestId("host-metrics-button");
      fireEvent.click(button);
      const popover = () => screen.getByTestId("host-metrics-popover");
      // 500 + 300 overflows a 700-wide window less its 12px margin by 112.
      expect(popover().style.left).toBe("-112px");
      resizeTo(1200);
      expect(popover().style.left).toBe("");
      resizeTo(600);
      expect(popover().style.left).toBe("-212px");
      fireEvent.click(button);
      expect(screen.queryByTestId("host-metrics-popover")).toBeNull();
      expect(removed.mock.calls.some(([type]) => type === "resize")).toBe(true);
    } finally {
      window.innerWidth = initialWidth;
      rect.mockRestore();
      removed.mockRestore();
    }
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
