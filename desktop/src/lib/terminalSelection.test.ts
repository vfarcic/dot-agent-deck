import { describe, expect, it } from "vitest";
import type { Terminal } from "@xterm/xterm";
import { keepSelectionAcrossResize } from "./terminalSelection";

/**
 * Issue #1457 — the part of xterm 6 the selection keeper meets, reduced to
 * what decides the outcome, as read from xterm's own source:
 *
 * - its drag: a press on the terminal starts a selection at the cell under the
 *   pointer, document moves extend it, and the release ends it;
 * - `resize`: the public `onResize` fires first, then a change of row count
 *   clears the selection and abandons a drag in progress
 *   (`SelectionService`'s `onResize` listener);
 * - growing the grid pulls lines down from the scrollback, so a line keeps its
 *   buffer index while its row on screen moves — and a marker keeps its line.
 *
 * Cells are 10x20 px, and the screen starts at the host's top-left corner.
 */
const CELL = { width: 10, height: 20 };

class FakeXterm {
  cols: number;
  rows: number;
  element: HTMLElement;
  screen: HTMLElement;
  buffer = { active: { type: "normal" as "normal" | "alternate", baseY: 100, cursorY: 0, viewportY: 100 } };
  modes = { mouseTrackingMode: "none" };
  selection: { column: number; line: number; length: number } | undefined;
  private resizeListeners = new Set<(size: { cols: number; rows: number }) => void>();
  private selectionListeners = new Set<() => void>();
  private drag: { line: number; column: number } | undefined;

  constructor(host: HTMLElement, cols: number, rows: number) {
    this.cols = cols;
    this.rows = rows;
    this.element = document.createElement("div");
    this.screen = document.createElement("div");
    this.screen.className = "xterm-screen";
    this.element.appendChild(this.screen);
    host.appendChild(this.element);
    this.screen.getBoundingClientRect = () =>
      ({ left: 0, top: 0, width: this.cols * CELL.width, height: this.rows * CELL.height }) as DOMRect;
    this.element.addEventListener("mousedown", (event) => {
      if (event.button !== 0) return;
      this.drag = this.cellAt(event);
      this.setSelection(undefined);
      document.addEventListener("mousemove", this.onDragMove);
      document.addEventListener("mouseup", this.onDragEnd);
    });
  }

  /** xterm's own selection rounding, as `Mouse.ts` computes it. */
  cellAt(event: MouseEvent): { line: number; column: number } {
    const column = Math.min(Math.max(Math.ceil((event.clientX + CELL.width / 2) / CELL.width), 1), this.cols + 1) - 1;
    const row = Math.min(Math.max(Math.ceil(event.clientY / CELL.height), 1), this.rows) - 1;
    return { line: this.buffer.active.viewportY + row, column };
  }

  private onDragMove = (event: MouseEvent) => {
    if (!this.drag) return;
    const end = this.cellAt(event);
    const length = (end.line - this.drag.line) * this.cols + end.column - this.drag.column;
    this.setSelection(length > 0 ? { column: this.drag.column, line: this.drag.line, length } : undefined);
  };

  private onDragEnd = (event: MouseEvent) => {
    this.onDragMove(event);
    this.stopDrag();
  };

  private stopDrag() {
    this.drag = undefined;
    document.removeEventListener("mousemove", this.onDragMove);
    document.removeEventListener("mouseup", this.onDragEnd);
  }

  private setSelection(selection: FakeXterm["selection"]) {
    this.selection = selection;
    this.selectionListeners.forEach((listener) => listener());
  }

  resize(cols: number, rows: number) {
    const rowsChanged = rows !== this.rows;
    // More rows show more of the scrollback above the same lines.
    this.buffer.active.viewportY -= rows - this.rows;
    this.buffer.active.baseY -= rows - this.rows;
    this.cols = cols;
    this.rows = rows;
    this.resizeListeners.forEach((listener) => listener({ cols, rows }));
    if (rowsChanged) {
      this.stopDrag();
      this.setSelection(undefined);
    }
  }

  onResize(listener: (size: { cols: number; rows: number }) => void) {
    this.resizeListeners.add(listener);
    return { dispose: () => this.resizeListeners.delete(listener) };
  }

  onSelectionChange(listener: () => void) {
    this.selectionListeners.add(listener);
    return { dispose: () => this.selectionListeners.delete(listener) };
  }

  registerMarker(offset: number) {
    const marker = { line: this.buffer.active.baseY + this.buffer.active.cursorY + offset, isDisposed: false, dispose: () => {
      marker.isDisposed = true;
      marker.line = -1;
    } };
    return marker;
  }

  hasSelection() { return this.selection !== undefined; }
  clearSelection() { this.setSelection(undefined); }
  select(column: number, line: number, length: number) { this.setSelection({ column, line, length }); }
  getSelectionPosition() {
    if (!this.selection) return undefined;
    const { column, line, length } = this.selection;
    const end = column + length;
    return { start: { x: column, y: line }, end: { x: end % this.cols, y: line + Math.floor(end / this.cols) } };
  }
}

/** A deck tile's terminal, with the keeper on it unless `kept` is false. */
function mount(kept = true) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const xterm = new FakeXterm(host, 80, 12);
  const keeper = kept ? keepSelectionAcrossResize(xterm as unknown as Terminal, host) : undefined;
  return { host, xterm, keeper };
}

/** Viewport point of a cell's centre. */
const at = (column: number, row: number) => ({ clientX: (column + 0.5) * CELL.width, clientY: (row + 0.5) * CELL.height });

function press(xterm: FakeXterm, point: { clientX: number; clientY: number }, init: MouseEventInit = {}) {
  xterm.screen.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, button: 0, detail: 1, ...point, ...init }));
}
const move = (point: { clientX: number; clientY: number }) =>
  document.dispatchEvent(new MouseEvent("mousemove", { bubbles: true, buttons: 1, ...point }));
const release = (point: { clientX: number; clientY: number }) =>
  document.dispatchEvent(new MouseEvent("mouseup", { bubbles: true, ...point }));

describe("keepSelectionAcrossResize", () => {
  it("carries on a drag whose grid gains rows while the button is down", async () => {
    for (const kept of [false, true]) {
      const { xterm, keeper } = mount(kept);
      // Press on line 103 (screen row 3), the first cell of a 14-character word.
      press(xterm, at(0, 3));
      move(at(5, 3));
      // The focus claim: twelve rows become twenty, so line 103 is now row 11.
      xterm.resize(80, 20);
      move(at(10, 11));
      release({ clientX: 14 * CELL.width, clientY: 11.5 * CELL.height });
      // Without the keeper this is the reported symptom: nothing is selected.
      expect(xterm.selection).toEqual(kept ? { column: 0, line: 103, length: 14 } : undefined);
      keeper?.dispose();
    }
  });

  it("leaves a drag with no row change to xterm", () => {
    const { xterm } = mount();
    press(xterm, at(0, 3));
    xterm.resize(100, 12);
    release({ clientX: 14 * CELL.width, clientY: 3.5 * CELL.height });
    expect(xterm.selection).toEqual({ column: 0, line: 103, length: 14 });
  });

  it("selects a finished selection again after a resize clears it", async () => {
    const { xterm } = mount();
    press(xterm, at(2, 4));
    release({ clientX: 9 * CELL.width, clientY: 4.5 * CELL.height });
    expect(xterm.selection).toEqual({ column: 2, line: 104, length: 7 });
    xterm.resize(80, 20);
    expect(xterm.selection, "xterm clears it as it resizes").toBeUndefined();
    await Promise.resolve();
    expect(xterm.selection).toEqual({ column: 2, line: 104, length: 7 });
  });

  it("does not bring back a selection the person cleared", async () => {
    const { xterm } = mount();
    press(xterm, at(2, 4));
    release({ clientX: 9 * CELL.width, clientY: 4.5 * CELL.height });
    xterm.clearSelection();
    xterm.resize(80, 20);
    await Promise.resolve();
    expect(xterm.selection).toBeUndefined();
  });

  it("leaves a double-click, Shift, Alt and an agent's mouse reporting to xterm", () => {
    const cases: [string, MouseEventInit, string][] = [
      ["double-click", { detail: 2 }, "none"],
      ["shift", { shiftKey: true }, "none"],
      ["alt", { altKey: true }, "none"],
      ["mouse reporting", {}, "any"],
    ];
    for (const [name, init, mode] of cases) {
      const { xterm, keeper } = mount();
      xterm.modes.mouseTrackingMode = mode;
      press(xterm, at(0, 3), init);
      xterm.resize(80, 20);
      release({ clientX: 14 * CELL.width, clientY: 11.5 * CELL.height });
      expect(xterm.selection, name).toBeUndefined();
      keeper?.dispose();
    }
  });

  it("stops listening once disposed", () => {
    const { xterm, keeper } = mount();
    keeper?.dispose();
    press(xterm, at(0, 3));
    xterm.resize(80, 20);
    release({ clientX: 14 * CELL.width, clientY: 11.5 * CELL.height });
    expect(xterm.selection).toBeUndefined();
  });
});
