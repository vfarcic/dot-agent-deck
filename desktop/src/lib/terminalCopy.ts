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
 */
export interface CopyChordEvent {
  key: string;
  ctrlKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}

export function isTerminalCopyChord(event: CopyChordEvent): boolean {
  if (event.altKey || event.key.toLowerCase() !== "c") return false;
  const ctrlShift = event.ctrlKey && event.shiftKey && !event.metaKey;
  const cmd = event.metaKey && !event.ctrlKey && !event.shiftKey;
  return ctrlShift || cmd;
}
