import type { DashboardScroll } from "./voiceActions";

/**
 * Issue #1673 — the four voice scrolls over an open agent pane.
 *
 * Measured for the issue (Claude Code 2.1.330, OpenCode 2.2.7, Pi 1.3.0,
 * Codex 0.164.0): every one of them runs on the alternate screen and keeps its
 * own transcript, scrolling it on PageUp / PageDown, idle and mid-turn. So the
 * route is decided by the pane's state, not by the agent's name, the way the
 * TUI's notice is (PRD #611):
 *
 * - **Alternate screen**: the program in the pane owns its view, so "scroll
 *   up / down" is the PageUp / PageDown a user would press in the pane. It goes
 *   through `terminal.input`, the pane's own input path, so a read-only pane
 *   and the other input gates apply to it as they do to a key. "Scroll to the
 *   top / bottom" moves nothing: the agents disagree on the key for it
 *   (Ctrl+Home for some, Home for another), and sending every candidate would
 *   type keys into some agents' prompts.
 * - **Main screen with history in this terminal** (a shell, an inline
 *   program): the terminal's own history scrolls, as Shift+PageUp does.
 * - **Neither**: the TUI's sentence for the same state.
 *
 * Answers `undefined` when something moved or was sent, or the sentence saying
 * why nothing did.
 */

/** The TUI's notice for a pane with nothing to scroll (`SCROLL_NOTICE_TEXT` in `src/ui.rs`), word for word, with a full stop. */
export const PANE_NOTHING_TO_SCROLL = "Nothing to scroll — this pane has no scrollback to move through.";
export const PANE_SCROLLS_BY_SCREEN = "An agent's pane scrolls a screen at a time: say \"scroll up\" or \"scroll down\".";
export const PANE_AT_TOP = "The pane is already at the top.";
export const PANE_AT_BOTTOM = "The pane is already at the bottom.";
export const PANE_TERMINAL_HIDDEN = "The agent's terminal is not showing, so it was not scrolled. Switch back to its terminal first.";
export const PANE_COVERED = "Something is open over the agent's pane, so it was not scrolled. Close it first.";

/** PageUp and PageDown as a terminal sends them, and as the TUI forwards them (`keyevent_to_bytes`). */
export const PAGE_UP = "\x1b[5~";
export const PAGE_DOWN = "\x1b[6~";

/** The part of an xterm `Terminal` this reads and moves. */
export type ScrollablePane = {
  buffer: { active: { type: "normal" | "alternate"; baseY: number; viewportY: number } };
  input(data: string, wasUserInput?: boolean): void;
  scrollPages(pageCount: number): void;
  scrollToTop(): void;
  scrollToBottom(): void;
};

export function scrollAgentPane(terminal: ScrollablePane | undefined, move: DashboardScroll, inputBlocked?: string): string | undefined {
  if (!terminal) return PANE_TERMINAL_HIDDEN;
  const buffer = terminal.buffer.active;
  if (buffer.type === "alternate") {
    if (move === "top" || move === "bottom") return PANE_SCROLLS_BY_SCREEN;
    if (inputBlocked) return `The pane was not scrolled: ${inputBlocked}`;
    terminal.input(move === "up" ? PAGE_UP : PAGE_DOWN, true);
    return undefined;
  }
  if (buffer.baseY <= 0) return PANE_NOTHING_TO_SCROLL;
  if ((move === "up" || move === "top") && buffer.viewportY <= 0) return PANE_AT_TOP;
  if ((move === "down" || move === "bottom") && buffer.viewportY >= buffer.baseY) return PANE_AT_BOTTOM;
  if (move === "top") terminal.scrollToTop();
  else if (move === "bottom") terminal.scrollToBottom();
  else terminal.scrollPages(move === "up" ? -1 : 1);
  return undefined;
}
