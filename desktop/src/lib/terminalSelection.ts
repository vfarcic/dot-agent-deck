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
 *   under the pointer, as xterm's own drag would have — scrolling, as xterm's
 *   does, while the pointer is above or below the terminal and still on the
 *   page. It ends on the release, on a move with the button up, or when the
 *   window loses focus.
 * - **A finished selection is put back.** One the resize clears is selected
 *   again over the same lines, held by markers the same way.
 *
 * Only a plain left-button drag is carried on, and only a plain selection is
 * put back. A double- or triple-click, Shift to extend, Alt for a column, and
 * any press an agent that reports mouse events receives, are xterm's alone, as
 * they were; so is a selection that starts or ends in a soft-wrapped line when
 * the column count changes too, because the reflow moves its text between
 * lines and a column there no longer names the same character.
 */
export function keepSelectionAcrossResize(terminal: Terminal, host: HTMLElement): IDisposable {
  let rows = terminal.rows;
  let cols = terminal.cols;
  let press: { anchor: Anchor; resumed: boolean; pointer?: Point; scroll?: number } | undefined;
  let kept: { start: Anchor; end: Anchor } | undefined;
  // Whether the latest press made a column selection, which is a rectangle and
  // cannot be put back as the linear range `select` takes.
  let column = false;
  // Set between a row-changing resize and the restore it schedules, so the
  // clear xterm makes for that resize does not count as the person's own.
  let restoring = false;

  const anchorAt = (line: number, column: number): Anchor => {
    const buffer = terminal.buffer.active;
    const wrapped = Boolean(buffer.getLine(line)?.isWrapped || buffer.getLine(line + 1)?.isWrapped);
    // Markers live on the normal buffer. The alternate one has no scrollback,
    // so a line number there stays put.
    if (buffer.type !== "normal") return { line, column, wrapped };
    return { marker: terminal.registerMarker(line - (buffer.baseY + buffer.cursorY)), line, column, wrapped };
  };

  const forget = () => {
    kept?.start.marker?.dispose();
    kept?.end.marker?.dispose();
    kept = undefined;
  };

  const keep = () => {
    const range = column ? undefined : terminal.getSelectionPosition();
    forget();
    if (range) kept = { start: anchorAt(range.start.y, range.start.x), end: anchorAt(range.end.y, range.end.x) };
  };

  const screenRect = () => {
    const rect = terminal.element?.querySelector<HTMLElement>(".xterm-screen")?.getBoundingClientRect();
    if (!rect || rect.width <= 0 || rect.height <= 0 || terminal.cols < 1 || terminal.rows < 1) return undefined;
    return rect;
  };

  /** The selection cell under the pointer, rounded the way xterm rounds one. */
  const cellAt = (pointer: Point): { line: number; column: number } | undefined => {
    const rect = screenRect();
    if (!rect) return undefined;
    const width = rect.width / terminal.cols;
    const height = rect.height / terminal.rows;
    // The left half of a cell ends a selection before it, the right half after.
    const column = clamp(Math.ceil((pointer.clientX - rect.left + width / 2) / width), 1, terminal.cols + 1) - 1;
    const row = clamp(Math.ceil((pointer.clientY - rect.top) / height), 1, terminal.rows) - 1;
    return { line: terminal.buffer.active.viewportY + row, column };
  };

  const selectBetween = (from: { line: number; column: number }, to: { line: number; column: number }) => {
    const [start, end] = from.line < to.line || (from.line === to.line && from.column <= to.column) ? [from, to] : [to, from];
    const length = (end.line - start.line) * terminal.cols + end.column - start.column;
    if (length > 0) terminal.select(start.column, start.line, length);
    else terminal.clearSelection();
  };

  const extend = (pointer: Point) => {
    const line = press && lineOf(press.anchor);
    const cell = cellAt(pointer);
    if (press && line !== undefined && cell) selectBetween({ line, column: press.anchor.column }, cell);
  };

  // While a carried-on drag's pointer is above or below the screen, scroll a
  // line at a time and extend the selection onto what scrolls in.
  const scrollTick = () => {
    const rect = screenRect();
    const pointer = press?.pointer;
    if (!rect || !pointer) return;
    const amount = pointer.clientY < rect.top ? -1 : pointer.clientY > rect.bottom ? 1 : 0;
    if (amount === 0) return;
    terminal.scrollLines(amount);
    extend(pointer);
  };

  const endPress = () => {
    press?.anchor.marker?.dispose();
    if (press?.scroll !== undefined) window.clearInterval(press.scroll);
    press = undefined;
    window.removeEventListener("mousemove", onMove, true);
    window.removeEventListener("mouseup", onRelease);
    window.removeEventListener("blur", finish);
    document.documentElement.removeEventListener("mouseleave", onLeave);
  };

  function finish() {
    endPress();
    if (terminal.hasSelection()) keep();
    else forget();
  }

  // The pointer left the page, where a release may never reach it: stop
  // scrolling until a move comes back and says the button is still down.
  function onLeave() {
    if (press) press.pointer = undefined;
  }

  function onMove(event: MouseEvent) {
    if (!press?.resumed) return;
    // The release happened where this page did not see it — over another
    // window, say. The drag is over; what it selected stands.
    if ((event.buttons & 1) === 0) {
      finish();
      return;
    }
    press.pointer = { clientX: event.clientX, clientY: event.clientY };
    extend(press.pointer);
  }

  // In the bubble phase on the window, so it runs after xterm's own release
  // handler on the document has finished the selection it is about to keep.
  function onRelease(event: MouseEvent) {
    if (press?.resumed) extend(event);
    finish();
  }

  const onPress = (event: MouseEvent) => {
    endPress();
    forget();
    column = event.altKey;
    if (event.button !== 0 || event.detail > 1 || event.shiftKey || event.altKey || event.metaKey) return;
    if (terminal.modes.mouseTrackingMode !== "none") return;
    const cell = cellAt(event);
    if (!cell) return;
    press = { anchor: anchorAt(cell.line, cell.column), resumed: false };
    window.addEventListener("mousemove", onMove, true);
    window.addEventListener("mouseup", onRelease);
    // Losing the window ends the gesture: its release will land elsewhere.
    window.addEventListener("blur", finish);
    document.documentElement.addEventListener("mouseleave", onLeave);
  };

  const resized = terminal.onResize((next) => {
    const rowsChanged = next.rows !== rows;
    const colsChanged = next.cols !== cols;
    rows = next.rows;
    cols = next.cols;
    // xterm keeps a selection across a resize that leaves the rows alone.
    if (!rowsChanged) return;
    // Runs before xterm clears the selection for this same resize: the
    // terminal forwards the buffer's resize to this event before the
    // selection service, which xterm builds later, hears it.
    if (press) {
      if (colsChanged && press.anchor.wrapped) {
        endPress();
        return;
      }
      press.resumed = true;
      press.scroll ??= window.setInterval(scrollTick, SCROLL_TICK_MS);
      return;
    }
    if (!kept || !terminal.hasSelection()) return;
    if (colsChanged && (kept.start.wrapped || kept.end.wrapped)) {
      forget();
      return;
    }
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

/** How often a carried-on drag scrolls while its pointer is past an edge. */
const SCROLL_TICK_MS = 50;

type Point = { clientX: number; clientY: number };
type Anchor = { marker?: IMarker; line: number; column: number; wrapped: boolean };

/** The anchor's current line, or undefined once its line has left the buffer. */
function lineOf(anchor: Anchor): number | undefined {
  if (!anchor.marker) return anchor.line;
  return anchor.marker.isDisposed || anchor.marker.line < 0 ? undefined : anchor.marker.line;
}

function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}
