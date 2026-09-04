import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  refitAllTerminals,
  registerRefit,
  registeredRefitCount,
  stripAnsi,
  terminalSnapshotText,
  unregisterRefit,
  type SnapshotTerminal,
} from "./terminalRegistry";

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

  it("re-fits every registered pane once", () => {
    const first = vi.fn();
    const second = vi.fn();
    registerRefit("a", first);
    registerRefit("b", second);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(first).toHaveBeenCalledTimes(1);
      expect(second).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("a", first);
      unregisterRefit("b", second);
    }
  });

  // The client-side half of the coalescing story. `fit()` reads layout, so
  // without this a held zoom key forces one reflow per pane per key repeat.
  it("collapses many requests in one frame into a single pass", () => {
    const refit = vi.fn();
    registerRefit("a", refit);
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
      unregisterRefit("a", refit);
    }
  });

  it("does not call a pane that unregistered before the frame ran", () => {
    const gone = vi.fn();
    const stays = vi.fn();
    registerRefit("gone", gone);
    registerRefit("stays", stays);
    try {
      refitAllTerminals();
      unregisterRefit("gone", gone);
      vi.advanceTimersByTime(20);
      expect(gone).not.toHaveBeenCalled();
      expect(stays).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("stays", stays);
    }
  });

  // A `fit()` can trigger a resize that unmounts a sibling, so the pass
  // iterates a snapshot and re-checks each entry. Without both, unmounting
  // during the loop would skip whatever came next.
  it("survives a pane unmounting a sibling from inside its own re-fit", () => {
    const victim = vi.fn();
    const survivor = vi.fn();
    const remover = vi.fn(() => unregisterRefit("victim", victim));
    registerRefit("remover", remover);
    registerRefit("victim", victim);
    registerRefit("survivor", survivor);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(remover).toHaveBeenCalledTimes(1);
      expect(victim).not.toHaveBeenCalled();
      expect(survivor).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("remover", remover);
      unregisterRefit("victim", victim);
      unregisterRefit("survivor", survivor);
    }
  });

  // One pane with no measurable box must not cost every other pane its resize —
  // which for a terminal means the daemon never learning the new PTY size.
  it("keeps going when one pane's re-fit throws", () => {
    const angry = vi.fn(() => { throw new Error("no measurable box"); });
    const calm = vi.fn();
    registerRefit("angry", angry);
    registerRefit("calm", calm);
    try {
      refitAllTerminals();
      expect(() => vi.advanceTimersByTime(20)).not.toThrow();
      expect(calm).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("angry", angry);
      unregisterRefit("calm", calm);
    }
  });

  // A remounting viewport registers the new pane's `fit` before the old
  // effect's cleanup runs, so an unregister keyed on the id alone would forget
  // the live pane and leave it out of every later zoom.
  it("keeps a remounted pane's re-fit when the old one unregisters", () => {
    const old = vi.fn();
    const fresh = vi.fn();
    registerRefit("a", old);
    registerRefit("a", fresh);
    unregisterRefit("a", old);
    try {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
      expect(old).not.toHaveBeenCalled();
      expect(fresh).toHaveBeenCalledTimes(1);
    } finally {
      unregisterRefit("a", fresh);
    }
  });

  it("is a no-op with nothing registered", () => {
    expect(() => {
      refitAllTerminals();
      vi.advanceTimersByTime(20);
    }).not.toThrow();
  });
});
