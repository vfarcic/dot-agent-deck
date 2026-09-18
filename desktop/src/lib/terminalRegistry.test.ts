import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  getTerminal,
  refitAllTerminals,
  registerRefit,
  registerTerminal,
  registeredRefitCount,
  stripAnsi,
  terminalSnapshotText,
  unregisterRefit,
  unregisterTerminal,
  type SnapshotTerminal,
} from "./terminalRegistry";

/**
 * One deck, for the call sites below that are about the re-fit MECHANISM rather
 * than about which deck a pane is on. The registry is keyed by the composite
 * `(deckId, agentId)` since PRD #1105's cross-deck pane; the two tests that are
 * about that collision name their decks themselves.
 */
const DECK = "deck-000000000000dec1";

function fakeTerminal(rows: { text: string; wrapped?: boolean }[]): SnapshotTerminal {
  return {
    buffer: {
      active: {
        length: rows.length,
        getLine: (index: number) => {
          const row = rows[index];
          if (!row) return undefined;
          return { isWrapped: row.wrapped ?? false, translateToString: () => row.text };
        },
      },
    },
  };
}

describe("terminalSnapshotText", () => {
  it("joins soft-wrapped rows back into one logical line", () => {
    const terminal = fakeTerminal([
      { text: "a long line that the terminal " },
      { text: "wrapped onto a second row", wrapped: true },
      { text: "next line" },
    ]);
    expect(terminalSnapshotText(terminal)).toBe(
      "a long line that the terminal wrapped onto a second row\nnext line",
    );
  });

  it("drops trailing blank rows below the cursor but keeps interior blanks", () => {
    const terminal = fakeTerminal([
      { text: "first" },
      { text: "" },
      { text: "last" },
      { text: "" },
      { text: "   " },
    ]);
    expect(terminalSnapshotText(terminal)).toBe("first\n\nlast");
  });

  it("returns an empty string for an empty buffer", () => {
    expect(terminalSnapshotText(fakeTerminal([]))).toBe("");
  });
});

describe("stripAnsi", () => {
  it("removes color and cursor sequences but keeps the text", () => {
    expect(stripAnsi("\x1b[32mPASS\x1b[0m plan accepted\x1b[2K")).toBe("PASS plan accepted");
  });

  it("normalizes carriage returns to newlines", () => {
    expect(stripAnsi("progress 1\rprogress 2\r\ndone")).toBe("progress 1\nprogress 2\ndone");
  });

  it("removes OSC title sequences", () => {
    expect(stripAnsi("\x1b]0;window title\x07real output")).toBe("real output");
  });
});

describe("terminals from colliding agent ids", () => {
  /**
   * Scenario: a deck tile and a cross-deck overview pane both mount an xterm
   * for an agent named `planner`. Looking either one up by its composite deck
   * identity returns that exact instance, and unmounting one leaves the other
   * registered.
   */
  it("registers concurrent same-id terminals by deck and agent", () => {
    const first = { name: "local-planner" };
    const second = { name: "remote-planner" };
    const registerComposite = registerTerminal as unknown as (deckId: string, agentId: string, terminal: unknown) => void;
    const unregisterComposite = unregisterTerminal as unknown as (deckId: string, agentId: string, terminal: unknown) => void;
    const getComposite = getTerminal as unknown as (deckId: string, agentId: string) => unknown;

    registerComposite("deck-a", "planner", first);
    registerComposite("deck-b", "planner", second);
    try {
      expect(getComposite("deck-a", "planner")).toBe(first);
      expect(getComposite("deck-b", "planner")).toBe(second);

      unregisterComposite("deck-a", "planner", first);
      expect(getComposite("deck-a", "planner")).toBeUndefined();
      expect(getComposite("deck-b", "planner")).toBe(second);
    } finally {
      unregisterComposite("deck-a", "planner", first);
      unregisterComposite("deck-b", "planner", second);
    }
  });
});

/**
 * The re-fit seam PRD #744 added, and the two properties it exists to hold: a
 * zoom change reaches every mounted pane, and a burst of them measures layout
 * once rather than once per keystroke per pane.
 */
describe("refitAllTerminals", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.runOnlyPendingTimers();
    vi.useRealTimers();
    expect(registeredRefitCount()).toBe(0);
  });

  /**
   * Scenario: two decks mount same-id terminals concurrently and the window is
   * re-fitted. Both callbacks run once, and removing deck A's callback does not
   * remove deck B's independently keyed pane.
   */
  it("re-fits concurrent same-id panes independently by deck", () => {
    const first = vi.fn();
    const second = vi.fn();
    const registerComposite = registerRefit as unknown as (deckId: string, agentId: string, refit: () => void) => void;
    const unregisterComposite = unregisterRefit as unknown as (deckId: string, agentId: string, refit: () => void) => void;

    registerComposite("deck-a", "planner", first);
    registerComposite("deck-b", "planner", second);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(first).toHaveBeenCalledTimes(1);
      expect(second).toHaveBeenCalledTimes(1);

      unregisterComposite("deck-a", "planner", first);
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(first).toHaveBeenCalledTimes(1);
      expect(second).toHaveBeenCalledTimes(2);
    } finally {
      unregisterComposite("deck-a", "planner", first);
      unregisterComposite("deck-b", "planner", second);
    }
  });

  it("re-fits every registered pane once", () => {
    const first = vi.fn();
    const second = vi.fn();
    registerRefit(DECK, "a", first);
    registerRefit(DECK, "b", second);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(first).toHaveBeenCalledTimes(1);
      expect(second).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit(DECK, "a", first);
      unregisterRefit(DECK, "b", second);
    }
  });

  // The client-side half of the coalescing story. `fit()` reads layout, so
  // without this a held zoom key forces one reflow per pane per key repeat.
  it("collapses many requests in one frame into a single pass", () => {
    const refit = vi.fn();
    registerRefit(DECK, "a", refit);
    try {
      for (let i = 0; i < 10; i += 1) refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(refit).toHaveBeenCalledTimes(1);

      // …and the next frame is schedulable again, so the coalescing is a
      // throttle rather than a one-shot latch.
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(refit).toHaveBeenCalledTimes(2);
    } finally {
      unregisterRefit(DECK, "a", refit);
    }
  });

  it("does not call a pane that unregistered before the frame ran", () => {
    const gone = vi.fn();
    const stays = vi.fn();
    registerRefit(DECK, "gone", gone);
    registerRefit(DECK, "stays", stays);
    try {
      refitAllTerminals();
      unregisterRefit(DECK, "gone", gone);
      vi.advanceTimersByTime(20);
      expect(gone).not.toHaveBeenCalled();
      expect(stays).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit(DECK, "stays", stays);
    }
  });

  // A `fit()` can trigger a resize that unmounts a sibling, so the pass
  // iterates a snapshot and re-checks each entry. Without both, unmounting
  // during the loop would skip whatever came next.
  it("survives a pane unmounting a sibling from inside its own re-fit", () => {
    const victim = vi.fn();
    const survivor = vi.fn();
    const remover = vi.fn(() => unregisterRefit(DECK, "victim", victim));
    registerRefit(DECK, "remover", remover);
    registerRefit(DECK, "victim", victim);
    registerRefit(DECK, "survivor", survivor);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(remover).toHaveBeenCalledTimes(1);
      expect(victim).not.toHaveBeenCalled();
      expect(survivor).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit(DECK, "remover", remover);
      unregisterRefit(DECK, "victim", victim);
      unregisterRefit(DECK, "survivor", survivor);
    }
  });

  // One pane with no measurable box must not cost every other pane its resize —
  // which for a terminal means the daemon never learning the new PTY size.
  it("keeps going when one pane's re-fit throws", () => {
    const angry = vi.fn(() => { throw new Error("no measurable box"); });
    const calm = vi.fn();
    registerRefit(DECK, "angry", angry);
    registerRefit(DECK, "calm", calm);
    try {
      refitAllTerminals();
      expect(() => vi.advanceTimersByTime(20)).not.toThrow();
      expect(calm).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit(DECK, "angry", angry);
      unregisterRefit(DECK, "calm", calm);
    }
  });

  // A remounting viewport registers the new pane's `fit` before the old
  // effect's cleanup runs, so an unregister keyed on the id alone would forget
  // the live pane and leave it out of every later zoom.
  it("keeps a remounted pane's re-fit when the old one unregisters", () => {
    const old = vi.fn();
    const fresh = vi.fn();
    registerRefit(DECK, "a", old);
    registerRefit(DECK, "a", fresh);
    unregisterRefit(DECK, "a", old);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(old).not.toHaveBeenCalled();
      expect(fresh).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit(DECK, "a", fresh);
    }
  });

  it("is a no-op with nothing registered", () => {
    expect(() => {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
    }).not.toThrow();
  });
});
