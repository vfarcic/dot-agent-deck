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
