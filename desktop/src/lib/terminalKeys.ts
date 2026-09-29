/**
 * Issue #1422 — keystrokes xterm.js would forward to an agent differently from
 * a native terminal and the TUI, or not in a form the agents act on.
 *
 * xterm.js encodes keys itself (`Keyboard.ts` in `@xterm/xterm@6.0.0`), and
 * for almost every key its bytes are the TUI's and a native terminal's. This
 * module holds the exceptions, and nothing else: a key it does not name is left
 * to xterm, so an app-level shortcut or a key xterm already sends correctly is
 * untouched.
 *
 * Two kinds of exception. The modified Enters and Ctrl+/ match
 * `keyevent_to_bytes` in `src/ui.rs`, so the same keypress means the same thing
 * to an agent whichever client it was typed into. The platform editing
 * shortcuts (Cmd+Left on macOS, Ctrl+Backspace on Windows and Linux, …) are
 * bytes each supported agent's input box was measured to act on;
 * `docs/develop/desktop-gui.md` has the per-agent table.
 */

import type { BrowserPlatformHints } from "./platform";

/**
 * Which platform's editing shortcuts apply. Read from what the webview reports
 * (`navigator`), never from the daemon's host: the keyboard in front of the
 * user is the webview's, and a Mac driving a Linux daemon still types Cmd+Left.
 *
 * The most specific report wins — `userAgentData.platform`, then `platform`,
 * and the user agent only when neither is there — rather than any report that
 * matches, because a user-agent string is the one a webview may dress up as
 * another browser's.
 */
export type KeyPlatform = "mac" | "windows" | "linux";

export function keyPlatform(
  hints: BrowserPlatformHints | undefined = typeof navigator === "undefined" ? undefined : navigator,
): KeyPlatform {
  const reported = hints?.userAgentData?.platform || hints?.platform || hints?.userAgent || "";
  if (/windows|^win(?:32|64|ce)/i.test(reported)) return "windows";
  if (/^mac|macintosh/i.test(reported)) return "mac";
  return "linux";
}

/** The fields of a `KeyboardEvent` this reads, so it is testable without a DOM event. */
export interface TerminalKey {
  key: string;
  /** The physical key, for finding V on a non-Latin layout. */
  code?: string;
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

type Modifier = "shift" | "ctrl" | "alt" | "meta";

/** Exactly these modifiers are held, and no others. */
function only(key: TerminalKey, ...held: Modifier[]): boolean {
  return (
    key.shiftKey === held.includes("shift") &&
    key.ctrlKey === held.includes("ctrl") &&
    key.altKey === held.includes("alt") &&
    key.metaKey === held.includes("meta")
  );
}

/**
 * The platform's line-editing chords that xterm.js sends nothing for, or sends
 * as a key the agents read differently: Cmd+Left/Right as nothing at all,
 * Cmd+Backspace as a one-character DEL, Ctrl+Backspace as BS (also a
 * one-character delete in every agent), and Ctrl+Delete / Option+Delete as
 * `ESC[3;<m>~`, which Claude Code and Pi do not read as a word delete. The
 * replacements are readline's: Ctrl+A, Ctrl+E, Ctrl+U, Ctrl+W and `ESC d`.
 *
 * Home/End, Ctrl/Option+Left/Right and Option+Backspace are not here: xterm's
 * own bytes for them already work in every agent.
 */
function editingSequence(key: TerminalKey, platform: KeyPlatform): string | undefined {
  if (platform === "mac") {
    if (only(key, "meta")) {
      if (key.key === "ArrowLeft") return "\x01"; // start of line
      if (key.key === "ArrowRight") return "\x05"; // end of line
      if (key.key === "Backspace") return "\x15"; // delete to start of line
    }
    if (only(key, "alt") && key.key === "Delete") return `${ESC}d`; // delete next word
    return undefined;
  }
  if (only(key, "ctrl")) {
    if (key.key === "Backspace") return "\x17"; // delete previous word
    if (key.key === "Delete") return `${ESC}d`; // delete next word
  }
  return undefined;
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
 * - **The platform's line-editing chords** (`editingSequence` above).
 *
 * Claims no Cmd/Super chord other than macOS's Cmd+Left, Cmd+Right and
 * Cmd+Backspace: the rest belong to the app and the OS. Never claims a key
 * while an input method is composing: Enter there commits the composition.
 * Never claims a key pressed with AltGr: Windows reports AltGr as Ctrl+Alt, so
 * AltGr+Enter would otherwise be sent as Ctrl+Alt+Enter.
 */
export function agentKeySequence(key: TerminalKey, platform: KeyPlatform): string | undefined {
  if (key.isComposing || key.getModifierState?.("AltGraph")) return undefined;
  const editing = editingSequence(key, platform);
  if (editing !== undefined) return editing;
  if (key.metaKey) return undefined;
  if (key.key === "Enter" && (key.shiftKey || key.ctrlKey)) {
    return `${ESC}[13;${modifierParam(key)}u`;
  }
  // Shift is allowed because on some layouts `/` itself is a shifted key; Alt
  // is not, because Ctrl+Alt is how Windows reports AltGr.
  if (key.key === "/" && key.ctrlKey && !key.altKey) return "\x1f";
  return undefined;
}

/**
 * Whether this keypress is the platform's paste key: Cmd+V on macOS, Ctrl+V or
 * Ctrl+Shift+V on Windows, Ctrl+Shift+V on Linux — what Terminal.app, Windows
 * Terminal and GNOME Terminal paste with.
 *
 * The caller keeps xterm from encoding it and does NOT cancel it, so the
 * webview performs its own paste into xterm's textarea and xterm hands the
 * clipboard's text to the agent (bracketed, when the agent asked for that).
 * Only Ctrl+V on Windows needs this — xterm would otherwise send it as ^V and
 * cancel the paste; the others xterm already leaves alone — but naming all of
 * them keeps the paste keys in one place. Ctrl+V on macOS and Linux stays the
 * agent's, where Claude Code, for one, reads it as "paste an image".
 */
export function leavesPasteToWebview(key: TerminalKey, platform: KeyPlatform): boolean {
  if (key.isComposing) return false;
  // The character wins on a Latin layout (Dvorak's V is where V is printed);
  // the physical key stands in on a layout whose letters are not Latin.
  const isV = /^[a-z]$/i.test(key.key) ? key.key.toLowerCase() === "v" : key.code === "KeyV";
  if (!isV) return false;
  if (platform === "mac") return only(key, "meta");
  if (platform === "windows" && only(key, "ctrl")) return true;
  return only(key, "ctrl", "shift");
}
