import type { KeyPlatform } from "./terminalKeys";

/**
 * Issue #1403 — the keystroke that copies an agent terminal's selection.
 *
 * In a terminal the ordinary copy key is taken: plain `Ctrl+C` is the agent's
 * interrupt, and xterm hands it to the PTY as ETX. So the chord is the one
 * terminal emulators already use, `Ctrl+Shift+C` on Linux and Windows, and the
 * ordinary `Cmd+C` on macOS, where `Cmd` never reaches a PTY. Neither is
 * narrowed to its own platform: xterm sends nothing to the agent for either
 * chord on any platform, so accepting both everywhere costs nothing and needs
 * no platform guess.
 *
 * Issue #1705 — and on Windows, plain `Ctrl+C` while text is selected, which is
 * how Windows Terminal and conhost copy: the selection is copied and cleared,
 * and with nothing selected `Ctrl+C` is the interrupt as before. That is the
 * one plain `Ctrl+C` claimed anywhere; on macOS and Linux it is not the copy
 * key, so it reaches the agent even over a selection. The platform is the
 * webview's (`keyPlatform()`), never the daemon's host.
 */
export interface CopyChordEvent {
  key: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

export function isTerminalCopyChord(event: CopyChordEvent, platform?: KeyPlatform, hasSelection = false): boolean {
  if (event.altKey || event.key.toLowerCase() !== "c") return false;
  const ctrlShift = event.ctrlKey && event.shiftKey && !event.metaKey;
  const cmd = event.metaKey && !event.ctrlKey && !event.shiftKey;
  return ctrlShift || cmd || isSelectionCtrlC(event, platform, hasSelection);
}

/**
 * Issue #1705 — Windows' plain `Ctrl+C` over a selection, the chord that
 * copies AND clears it, so the next `Ctrl+C` interrupts.
 */
export function isSelectionCtrlC(event: CopyChordEvent, platform: KeyPlatform | undefined, hasSelection: boolean): boolean {
  return (
    platform === "windows"
    && hasSelection
    && event.ctrlKey
    && !event.shiftKey
    && !event.metaKey
    && !event.altKey
    && event.key.toLowerCase() === "c"
  );
}
