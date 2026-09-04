import type { Terminal } from "@xterm/xterm";

/**
 * Live xterm instances by agent id. The Reader overlay pulls readable text out
 * of the terminal's own screen buffer — xterm has already resolved every TUI
 * repaint, cursor jump, and spinner frame, so this is the one place clean
 * output exists without re-parsing ANSI.
 */
const terminals = new Map<string, Terminal>();

export function registerTerminal(agentId: string, terminal: Terminal): void {
  terminals.set(agentId, terminal);
}

export function unregisterTerminal(agentId: string, terminal: Terminal): void {
  // Only forget the exact instance we registered; a remounting viewport may
  // have already replaced it.
  if (terminals.get(agentId) === terminal) terminals.delete(agentId);
}

export function getTerminal(agentId: string): Terminal | undefined {
  return terminals.get(agentId);
}

/**
 * Every mounted pane's re-fit, by agent id (PRD #744).
 *
 * A second map rather than a method on `Terminal`, because the thing that has
 * to be called is the pane's `FitAddon`, which `TerminalViewport` creates and
 * never exposes — and deliberately keeps unexposed, since a `fit()` called from
 * anywhere but that component's own observer would measure a box it does not
 * own.
 *
 * The registration is separate from `registerTerminal` even though both happen
 * in the same effect, because the two have different consumers and one of them
 * is allowed to be absent: the Reader overlay needs the terminal and does not
 * care about fitting, and a future read-only pane could register a terminal
 * with no fit at all.
 */
const refits = new Map<string, () => void>();

export function registerRefit(agentId: string, refit: () => void): void {
  refits.set(agentId, refit);
}

export function unregisterRefit(agentId: string, refit: () => void): void {
  // Same identity check as `unregisterTerminal`, and for the same reason: a
  // remounting viewport has already replaced this entry by the time the old
  // effect's cleanup runs, and forgetting the new pane's refit would leave it
  // out of every subsequent zoom.
  if (refits.get(agentId) === refit) refits.delete(agentId);
}

/** Test seam: how many panes would be re-fitted right now. */
export function registeredRefitCount(): number {
  return refits.size;
}

let refitFrame: number | undefined;

/**
 * Re-fit every mounted pane, at most once per frame.
 *
 * **Why this is coalesced and the daemon resize is not.** Two different costs.
 * `fit()` reads layout, so calling it per keystroke across every mounted pane
 * forces one reflow per pane per key repeat — that is this function's problem
 * and `requestAnimationFrame` is the fix. What each `fit()` then reports flows
 * into `TauriDeckBridge.resizeTerminal`, which already coalesces per agent on
 * its own frame with a single-in-flight gate, so the daemon cannot see a
 * resize storm however often this is called. Adding a second layer there would
 * be reinventing something that works.
 *
 * A frame is also the right unit for a different reason: a zoom that has just
 * been handed to the native webview has not necessarily been laid out yet, so
 * measuring on the next frame is more likely to measure the new geometry than
 * measuring synchronously would be. The pane's own `ResizeObserver` remains the
 * backstop that catches it if even that is too early.
 */
export function refitAllTerminals(): void {
  if (refitFrame !== undefined) return;
  // Called through `window.` rather than extracted into a local, because an
  // unbound `requestAnimationFrame` throws `Illegal invocation` in some
  // engines. The `setTimeout` arm is for a host with no rAF at all, which is
  // not hypothetical here: `terminalRegistry` is imported by tests that never
  // touch a DOM.
  const hasRaf = typeof window !== "undefined" && typeof window.requestAnimationFrame === "function";
  refitFrame = hasRaf
    ? window.requestAnimationFrame(() => runRefits())
    : (setTimeout(() => runRefits(), 0) as unknown as number);
}

function runRefits(): void {
  refitFrame = undefined;
  // Snapshotted before iterating: a `fit()` can trigger a resize that unmounts
  // a pane, and mutating the map mid-iteration would skip a sibling. A pane
  // that has gone away between the snapshot and its turn is re-checked rather
  // than called.
  for (const [agentId, refit] of [...refits]) {
    if (refits.get(agentId) !== refit) continue;
    try {
      refit();
    } catch {
      // A hidden or mid-teardown pane can have no measurable box. One bad pane
      // must not stop the rest from being told about the zoom.
    }
  }
}

/**
 * The subset of the xterm buffer API the snapshot needs — kept minimal so
 * tests can hand in a fake without a DOM-mounted terminal.
 */
export interface SnapshotBufferLine {
  isWrapped: boolean;
  translateToString(trimRight?: boolean): string;
}
export interface SnapshotTerminal {
  buffer: { active: { length: number; getLine(index: number): SnapshotBufferLine | undefined } };
}

/**
 * Flatten the terminal's resolved buffer (scrollback + viewport) into plain
 * text. Soft-wrapped rows are re-joined into their logical line so the reader
 * can reflow them at its own width; trailing blank rows below the cursor are
 * dropped.
 */
export function terminalSnapshotText(terminal: SnapshotTerminal): string {
  const buffer = terminal.buffer.active;
  const lines: string[] = [];
  for (let index = 0; index < buffer.length; index += 1) {
    const line = buffer.getLine(index);
    if (!line) continue;
    const text = line.translateToString(true);
    if (line.isWrapped && lines.length > 0) lines[lines.length - 1] += text;
    else lines.push(text);
  }
  while (lines.length > 0 && lines[lines.length - 1].trim() === "") lines.pop();
  return lines.join("\n");
}

// Control Sequence Introducer + OSC + single-char escapes — enough to make a
// raw transcript readable when no live terminal exists to snapshot.
// eslint-disable-next-line no-control-regex
const ANSI_PATTERN = new RegExp(
  [
    "\\x1b\\[[0-9;?]*[ -/]*[@-~]", // CSI sequences (colors, cursor movement, erase)
    "\\x1b\\][^\\x07\\x1b]*(?:\\x07|\\x1b\\\\)?", // OSC (window title etc.)
    "\\x1b[@-_]", // single-char escapes
    "[\\x00-\\x08\\x0b\\x0c\\x0e-\\x1f\\x7f]", // stray control bytes (\\n and \\t survive)
  ].join("|"),
  "g",
);

/** Fallback for agents whose terminal is not mounted: strip ANSI from the raw transcript. */
export function stripAnsi(raw: string): string {
  return raw.replace(ANSI_PATTERN, "").replace(/\r\n/g, "\n").replace(/\r/g, "\n");
}
