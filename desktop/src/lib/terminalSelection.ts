import type { IDisposable, IMarker, Terminal } from "@xterm/xterm";

/**
 * Issue #1457 — keep a terminal's selection when the daemon reshapes its grid.
 *
 * xterm 6 clears the selection whenever the row count changes, and a drag in
 * progress goes with it: the selection service drops its move and release
 * listeners, so the rest of the gesture selects nothing. The row count changes
 * under the person's hand in an ordinary case. Pressing into a window that is
 * not focused is also that window's focus-in, the app claims the agent's size
 * on its daemon (PRD #1105), and when a TUI was sizing the agent until then the
 * daemon resizes it to this pane a few milliseconds after the press.
 *
 * The resize itself is right and stays: the agent has redrawn for the new grid,
 * and xterm has to parse that redraw at it. What this keeps is the person's
 * selection across it:
 *
 * - **A drag the resize interrupts carries on.** The cell the press landed on
 *   is held by a buffer marker, which follows its line through the resize, and
 *   until the button comes up every move selects from that cell to the one
 *   under the pointer, as xterm's own drag would have.
 * - **A finished selection is put back.** One the resize clears is selected
 *   again over the same lines, held by markers the same way.
 *
 * Only a plain left-button drag is carried on. A double- or triple-click,
 * Shift to extend, Alt for a column, and any press an agent that reports mouse
 * events receives, are xterm's alone, as they were.
 */
export function keepSelectionAcrossResize(terminal: Terminal, host: HTMLElement): IDisposable {
  let rows = terminal.rows;
  let press: { anchor: Anchor; resumed: boolean } | undefined;
  let kept: { start: Anchor; end: Anchor } | undefined;
  // Set between a row-changing resize and the restore it schedules, so the
  // clear xterm makes for that resize does not count as the person's own.
  let restoring = false;

  const anchorAt = (line: number, column: number): Anchor => {
    const buffer = terminal.buffer.active;
    // Markers live on the normal buffer. The alternate one has no scrollback,
    // so a line number there stays put.
    if (buffer.type !== "normal") return { line, column };
    return { marker: terminal.registerMarker(line - (buffer.baseY + buffer.cursorY)), line, column };
  };

  const forget = () => {
    kept?.start.marker?.dispose();
    kept?.end.marker?.dispose();
    kept = undefined;
  };

  const keep = () => {
    const range = terminal.getSelectionPosition();
    forget();
    if (range) kept = { start: anchorAt(range.start.y, range.start.x), end: anchorAt(range.end.y, range.end.x) };
  };

  /** The selection cell under the pointer, rounded the way xterm rounds one. */
  const cellAt = (event: MouseEvent): { line: number; column: number } | undefined => {
    const screen = terminal.element?.querySelector<HTMLElement>(".xterm-screen");
    const rect = screen?.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0 || terminal.cols < 1 || terminal.rows < 1) return undefined;
    const width = rect.width / terminal.cols;
    const height = rect.height / terminal.rows;
    // The left half of a cell ends a selection before it, the right half after.
    const column = clamp(Math.ceil((event.clientX - rect.left + width / 2) / width), 1, terminal.cols + 1) - 1;
    const row = clamp(Math.ceil((event.clientY - rect.top) / height), 1, terminal.rows) - 1;
    return { line: terminal.buffer.active.viewportY + row, column };
  };

  const selectBetween = (from: { line: number; column: number }, to: { line: number; column: number }) => {
    const [start, end] = from.line < to.line || (from.line === to.line && from.column <= to.column) ? [from, to] : [to, from];
    const length = (end.line - start.line) * terminal.cols + end.column - start.column;
    if (length > 0) terminal.select(start.column, start.line, length);
    else terminal.clearSelection();
  };

  const extend = (event: MouseEvent) => {
    const line = press && lineOf(press.anchor);
    const cell = cellAt(event);
    if (press && line !== undefined && cell) selectBetween({ line, column: press.anchor.column }, cell);
  };

  const onMove = (event: MouseEvent) => {
    if (press?.resumed) extend(event);
  };

  const endPress = () => {
    press?.anchor.marker?.dispose();
    press = undefined;
    window.removeEventListener("mousemove", onMove, true);
    window.removeEventListener("mouseup", onRelease);
  };

  // In the bubble phase on the window, so it runs after xterm's own release
  // handler on the document has finished the selection it is about to keep.
  function onRelease(event: MouseEvent) {
    if (press?.resumed) extend(event);
    endPress();
    if (terminal.hasSelection()) keep();
    else forget();
  }

  const onPress = (event: MouseEvent) => {
    endPress();
    forget();
    if (event.button !== 0 || event.detail > 1 || event.shiftKey || event.altKey || event.metaKey) return;
    if (terminal.modes.mouseTrackingMode !== "none") return;
    const cell = cellAt(event);
    if (!cell) return;
    press = { anchor: anchorAt(cell.line, cell.column), resumed: false };
    window.addEventListener("mousemove", onMove, true);
    window.addEventListener("mouseup", onRelease);
  };

  const resized = terminal.onResize(({ rows: next }) => {
    const rowsChanged = next !== rows;
    rows = next;
    // xterm keeps a selection across a resize that leaves the rows alone.
    if (!rowsChanged) return;
    // Runs before xterm clears the selection for this same resize: the
    // terminal forwards the buffer's resize to this event before the
    // selection service, which xterm builds later, hears it.
    if (press) {
      press.resumed = true;
      return;
    }
    if (!kept || !terminal.hasSelection()) return;
    restoring = true;
    queueMicrotask(() => {
      restoring = false;
      const start = kept && lineOf(kept.start);
      const end = kept && lineOf(kept.end);
      if (!kept || start === undefined || end === undefined || terminal.hasSelection()) return;
      selectBetween({ line: start, column: kept.start.column }, { line: end, column: kept.end.column });
    });
  });

  const selectionChanged = terminal.onSelectionChange(() => {
    // A drag keeps what it selects when it ends; a resize's own clear is not
    // the person clearing the selection.
    if (press || restoring) return;
    if (terminal.hasSelection()) keep();
    else forget();
  });

  host.addEventListener("mousedown", onPress, true);

  return {
    dispose: () => {
      host.removeEventListener("mousedown", onPress, true);
      resized.dispose();
      selectionChanged.dispose();
      endPress();
      forget();
    },
  };
}

type Anchor = { marker?: IMarker; line: number; column: number };

/** The anchor's current line, or undefined once its line has left the buffer. */
function lineOf(anchor: Anchor): number | undefined {
  if (!anchor.marker) return anchor.line;
  return anchor.marker.isDisposed || anchor.marker.line < 0 ? undefined : anchor.marker.line;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}
