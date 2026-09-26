import type { Terminal } from "@xterm/xterm";
import { agentKey } from "./agentKey";

/**
 * Live xterm instances by the composite `(deckId, agentId)`. The Reader overlay
 * pulls readable text out of the terminal's own screen buffer — xterm has
 * already resolved every TUI repaint, cursor jump, and spinner frame, so this
 * is the one place clean output exists without re-parsing ANSI.
 *
 * # Why this is no longer keyed by the bare agent id
 *
 * PRD #1105's first security audit cleared these two maps explicitly, on a
 * premise it named: *"the intended paths do not mount two decks' same-id xterms
 * concurrently"*. That premise died when the agent pane learnt to attach a
 * terminal on a deck that is **not** the selected one. A deck-screen tile and an
 * overview pane for another deck are now live in the same commit, and agent ids
 * are per-daemon monotonic integers — so both can claim `planner`.
 *
 * Under a bare-id map the second mount's `registerTerminal` overwrote the
 * first's entry, the Reader then snapshotted whichever mounted last under either
 * pane's heading, and the first pane's unmount cleaned up nothing because the
 * identity check below no longer matched. `registerRefit` had the same shape, so
 * a zoom re-fitted one of the two panes and left the other measuring at the old
 * scale.
 */
const terminals = new Map<string, Terminal>();

export function registerTerminal(deckId: string | undefined, agentId: string, terminal: Terminal): void {
  terminals.set(agentKey(deckId, agentId), terminal);
}

export function unregisterTerminal(deckId: string | undefined, agentId: string, terminal: Terminal): void {
  // Only forget the exact instance we registered; a remounting viewport may
  // have already replaced it.
  const key = agentKey(deckId, agentId);
  if (terminals.get(key) === terminal) terminals.delete(key);
}

export function getTerminal(deckId: string | undefined, agentId: string): Terminal | undefined {
  return terminals.get(agentKey(deckId, agentId));
}

/**
 * Every mounted pane's re-fit, by the composite `(deckId, agentId)` (PRD #744;
 * the deck is PRD #1105's cross-deck pane — see {@link terminals} above).
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

export function registerRefit(deckId: string | undefined, agentId: string, refit: () => void): void {
  refits.set(agentKey(deckId, agentId), refit);
}

export function unregisterRefit(deckId: string | undefined, agentId: string, refit: () => void): void {
  // Same identity check as `unregisterTerminal`, and for the same reason: a
  // remounting viewport has already replaced this entry by the time the old
  // effect's cleanup runs, and forgetting the new pane's refit would leave it
  // out of every subsequent zoom.
  const key = agentKey(deckId, agentId);
  if (refits.get(key) === refit) refits.delete(key);
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
  for (const [key, refit] of [...refits]) {
    if (refits.get(key) !== refit) continue;
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

// Issue #953 — the driver tier's one read into the app, and it is compiled out
// of every other build.
//
// The WebGL renderer draws the terminal into a canvas and leaves no row text in
// the DOM, and a classic WebDriver session has no pre-load script to reach this
// module's private map the way the browser tier's `addInitScript` does. So the
// driver tier asks for this at BUILD time: `VITE_DAD_DRIVER_SEAM=1` in the
// environment of the `tauri build` that produces the binary under test. Vite
// replaces `import.meta.env.VITE_DAD_DRIVER_SEAM` with a literal, the branch is
// then constant-false in any other build and the minifier drops it, and
// `desktop-web` greps its own `pnpm build` output for `__dadDriver` so a bundle
// built the ordinary way cannot quietly start carrying it. It exposes the same
// text the Reader overlay already shows a user, and it writes nothing.
if (import.meta.env.VITE_DAD_DRIVER_SEAM === "1") {
  (window as Window & { __dadDriver?: unknown }).__dadDriver = {
    terminalTexts: () =>
      [...terminals.entries()].map(([key, terminal]) => ({ key, text: terminalSnapshotText(terminal) })),
  };
}
