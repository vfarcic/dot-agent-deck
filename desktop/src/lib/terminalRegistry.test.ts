import { describe, expect, it } from "vitest";
import { stripAnsi, terminalSnapshotText, type SnapshotTerminal } from "./terminalRegistry";

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
