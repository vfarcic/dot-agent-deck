/**
 * Issue #1422 — keystrokes xterm.js would forward to an agent differently from
 * a native terminal and the TUI.
 *
 * xterm.js encodes keys itself (`Keyboard.ts` in `@xterm/xterm@6.0.0`), and
 * for almost every key its bytes are the TUI's and a native terminal's. This
 * module holds the exceptions, and nothing else: a key it does not name is left
 * to xterm, so an app-level shortcut or a key xterm already sends correctly is
 * untouched.
 *
 * The bytes match `keyevent_to_bytes` in `src/ui.rs`, so the same keypress
 * means the same thing to an agent whichever client it was typed into.
 */

/** The fields of a `KeyboardEvent` this reads, so it is testable without a DOM event. */
export interface TerminalKey {
  key: string;
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  isComposing?: boolean;
  /** `KeyboardEvent.getModifierState`, for AltGr, which Windows also reports as Ctrl+Alt. */
  getModifierState?(key: string): boolean;
}

const ESC = "\x1b";

/** The kitty/xterm modifier parameter: `1 + bitmask`, with Shift 1, Alt 2, Ctrl 4. */
function modifierParam(key: TerminalKey): number {
  return 1 + (key.shiftKey ? 1 : 0) + (key.altKey ? 2 : 0) + (key.ctrlKey ? 4 : 0);
}

/**
 * The bytes to send the agent for this keypress, or `undefined` to leave it to
 * xterm's own encoding.
 *
 * - **Shift+Enter / Ctrl+Enter** (Alt folded in when it accompanies either):
 *   `ESC[13;<m>u`. xterm sends a bare CR for Enter whatever Shift or Ctrl is
 *   held, which is the byte that SUBMITS — so the agent could not tell either
 *   chord from Enter. `ESC[13;2u` (Shift+Enter) is a newline in every
 *   supported agent. What `ESC[13;5u` (Ctrl+Enter) does is each agent's own
 *   binding, and they differ — `docs/develop/desktop-gui.md` has the measured
 *   table. Forwarding it faithfully is what lets each agent apply its binding,
 *   exactly as it does under the TUI. Alt+Enter alone keeps xterm's `ESC CR`,
 *   which is also what the TUI sends.
 * - **Ctrl+/**: US (0x1f), what xterm (the terminal), GNOME Terminal and the
 *   TUI send. xterm.js sends nothing at all for it.
 *
 * Never claims a Cmd/Super chord: those belong to the app and the OS. Never
 * claims a key while an input method is composing: Enter there commits the
 * composition. Never claims a key pressed with AltGr: Windows reports AltGr as
 * Ctrl+Alt, so AltGr+Enter would otherwise be sent as Ctrl+Alt+Enter.
 */
export function agentKeySequence(key: TerminalKey): string | undefined {
  if (key.metaKey || key.isComposing || key.getModifierState?.("AltGraph")) return undefined;
  if (key.key === "Enter" && (key.shiftKey || key.ctrlKey)) {
    return `${ESC}[13;${modifierParam(key)}u`;
  }
  // Shift is allowed because on some layouts `/` itself is a shifted key; Alt
  // is not, because Ctrl+Alt is how Windows reports AltGr.
  if (key.key === "/" && key.ctrlKey && !key.altKey) return "\x1f";
  return undefined;
}
