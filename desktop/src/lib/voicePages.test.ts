import { describe, expect, it } from "vitest";
import { gridFit, usedColumns } from "./voicePages";

describe("usedColumns (issue #1494)", () => {
  /* A wide, tall box: five columns of ten rows. */
  const fit = { columns: 5, rows: 10 };

  /** Scenario: a few directories, filled top to bottom, fit in one column's height, so they take one column — the whole width — instead of one narrow column each. */
  it("gives a few cells filled top to bottom one column", () => {
    expect(usedColumns(fit, 3, "column")).toBe(1);
    expect(usedColumns(fit, 1, "column")).toBe(1);
  });

  /** Scenario: exactly one column's worth still takes one column, and one more cell starts a second. */
  it("starts a second column only when the first is full", () => {
    expect(usedColumns(fit, 10, "column")).toBe(1);
    expect(usedColumns(fit, 11, "column")).toBe(2);
  });

  /** Scenario: a list several columns long takes the columns it fills. */
  it("takes as many columns as the cells fill", () => {
    expect(usedColumns(fit, 25, "column")).toBe(3);
    expect(usedColumns(fit, 50, "column")).toBe(5);
  });

  /** Scenario: a list longer than a page — which pages — never takes more columns than fit. */
  it("never takes more columns than fit", () => {
    expect(usedColumns(fit, 60, "column")).toBe(5);
    expect(usedColumns(fit, 9, "row")).toBe(5);
  });

  /** Scenario: Mode chips fill left to right, so each needs a column of its own until the row is full. */
  it("gives cells filled left to right a column each, up to what fits", () => {
    expect(usedColumns(fit, 3, "row")).toBe(3);
    expect(usedColumns(fit, 5, "row")).toBe(5);
  });

  /** Scenario: an empty list still lays out one column. */
  it("uses at least one column", () => {
    expect(usedColumns(fit, 0, "column")).toBe(1);
    expect(usedColumns(fit, 0, "row")).toBe(1);
  });

  /** Scenario: the fit it narrows is the measured one — a 852px-wide, 300px-tall directory list holds five 160px columns of nine 30px rows, and three directories in it take one column. */
  it("narrows a measured fit", () => {
    const measured = gridFit(852, 300, { rowHeight: 30, minColumnWidth: 160, gap: 2 });
    expect(measured).toEqual({ columns: 5, rows: 9 });
    expect(usedColumns(measured, 3, "column")).toBe(1);
  });
});
