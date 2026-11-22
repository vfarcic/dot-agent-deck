import { describe, expect, it, vi } from "vitest";
import { PAGE_DOWN, PAGE_UP, PANE_AT_BOTTOM, PANE_AT_TOP, PANE_NOTHING_TO_SCROLL, PANE_SCROLLS_BY_SCREEN, PANE_TERMINAL_HIDDEN, scrollAgentPane, type ScrollablePane } from "./paneScroll";

function pane(type: "normal" | "alternate", baseY = 0, viewportY = baseY) {
  return {
    buffer: { active: { type, baseY, viewportY } },
    scrollPages: vi.fn(),
    scrollToTop: vi.fn(),
    scrollToBottom: vi.fn(),
  } satisfies ScrollablePane;
}

describe("issue #1673 — scrolling an agent's pane by voice", () => {
  const send = () => vi.fn<(key: string) => void>();
  const idle = vi.fn<(key: string) => void>();
  /** Scenario: an agent on the alternate screen (Claude Code, Codex, OpenCode and Pi all are) gets the PageUp / PageDown a user would press in its pane, written straight to the agent. */
  it("sends PageUp and PageDown to a program on the alternate screen", () => {
    const terminal = pane("alternate");
    const sendKey = send();
    expect(scrollAgentPane(terminal, "up", sendKey)).toBeUndefined();
    expect(scrollAgentPane(terminal, "down", sendKey)).toBeUndefined();
    expect(sendKey.mock.calls).toEqual([[PAGE_UP], [PAGE_DOWN]]);
    expect(PAGE_UP).toBe("\x1b[5~");
    expect(PAGE_DOWN).toBe("\x1b[6~");
    expect(terminal.scrollPages).not.toHaveBeenCalled();
  });

  /** Scenario: "scroll to the top" or "to the bottom" over such a pane sends nothing, since the agents use different keys for it, and says to scroll a screen at a time. */
  it("moves nothing for top and bottom on the alternate screen, and says why", () => {
    const terminal = pane("alternate");
    const sendKey = send();
    expect(scrollAgentPane(terminal, "top", sendKey)).toBe(PANE_SCROLLS_BY_SCREEN);
    expect(scrollAgentPane(terminal, "bottom", sendKey)).toBe(PANE_SCROLLS_BY_SCREEN);
    expect(sendKey).not.toHaveBeenCalled();
  });

  /** Scenario: a pane whose terminal cannot take input sends no key and says why. */
  it("sends no key to a pane that cannot take input", () => {
    const terminal = pane("alternate");
    const sendKey = send();
    expect(scrollAgentPane(terminal, "up", sendKey, "its daemon is not answering.")).toBe("The pane was not scrolled: its daemon is not answering.");
    expect(sendKey).not.toHaveBeenCalled();
  });

  /** Scenario: a pane on the main screen with history above it scrolls that history a page at a time, or to either end, and says when it is already there. */
  it("scrolls the terminal's own history on the main screen", () => {
    const middle = pane("normal", 100, 50);
    expect(scrollAgentPane(middle, "up", idle)).toBeUndefined();
    expect(scrollAgentPane(middle, "down", idle)).toBeUndefined();
    expect(scrollAgentPane(middle, "top", idle)).toBeUndefined();
    expect(scrollAgentPane(middle, "bottom", idle)).toBeUndefined();
    expect(middle.scrollPages.mock.calls).toEqual([[-1], [1]]);
    expect(middle.scrollToTop).toHaveBeenCalledTimes(1);
    expect(middle.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(idle).not.toHaveBeenCalled();

    const atBottom = pane("normal", 100, 100);
    expect(scrollAgentPane(atBottom, "down", idle)).toBe(PANE_AT_BOTTOM);
    expect(scrollAgentPane(atBottom, "bottom", idle)).toBe(PANE_AT_BOTTOM);
    const atTop = pane("normal", 100, 0);
    expect(scrollAgentPane(atTop, "up", idle)).toBe(PANE_AT_TOP);
    expect(scrollAgentPane(atTop, "top", idle)).toBe(PANE_AT_TOP);
    expect(atTop.scrollPages).not.toHaveBeenCalled();
  });

  /** Scenario: a pane on the main screen with no history gets the TUI's sentence, and nothing is sent to the agent. */
  it("answers the TUI's notice when there is nothing to scroll", () => {
    const terminal = pane("normal", 0, 0);
    for (const move of ["up", "down", "top", "bottom"] as const) expect(scrollAgentPane(terminal, move, idle)).toBe(PANE_NOTHING_TO_SCROLL);
    expect(PANE_NOTHING_TO_SCROLL).toBe("Nothing to scroll — this pane has no scrollback to move through.");
    expect(idle).not.toHaveBeenCalled();
    expect(terminal.scrollPages).not.toHaveBeenCalled();
  });

  /** Scenario: no terminal mounted for the pane. */
  it("says the terminal is not showing when the pane has none", () => {
    expect(scrollAgentPane(undefined, "up", idle)).toBe(PANE_TERMINAL_HIDDEN);
  });
});
