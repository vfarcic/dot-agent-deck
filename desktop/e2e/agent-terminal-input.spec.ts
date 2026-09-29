import { expect, test, type Page } from "@playwright/test";
import { enterDeck } from "./support/overview";

interface RecordingWindow {
  __dadE2eTerminals?: { element?: HTMLElement; onData(listener: (data: string) => void): unknown }[];
  __dadE2eSent?: string[];
}

/**
 * Capture every xterm instance as the app registers it, the same way
 * `agent-pane-overlay.spec.ts` does, so a spec can listen to what a terminal
 * hands the app for the agent's PTY.
 */
async function captureTerminals(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const originalSet = Map.prototype.set;
    const terminals: unknown[] = [];
    Object.defineProperty(window, "__dadE2eTerminals", { value: terminals });
    Object.defineProperty(Map.prototype, "set", {
      configurable: true,
      writable: true,
      value(this: Map<unknown, unknown>, key: unknown, value: unknown) {
        if (value && typeof value === "object") {
          const candidate = value as { element?: unknown; onData?: unknown };
          if (candidate.element instanceof HTMLElement && typeof candidate.onData === "function") terminals.push(value);
        }
        return Reflect.apply(originalSet, this, [key, value]);
      },
    });
  });
}

/**
 * The deck's terminal-input contract, exercised against the crowded fixture so
 * the rendered page includes coordinator, writable, history-only, and
 * no-live-target tiles without a daemon or an agent credential.
 */
test.describe("agent terminal input", () => {
  /**
   * Scenario: open the crowded deck and wait for all fifteen xterm inputs to
   * mount. Each tile has exactly that one textarea, including the orchestrator;
   * no role-labelled composer or Send control is rendered beside it.
   */
  test("renders xterm as each tile's only text input", async ({ page }) => {
    await page.goto("/?fixture=1&state=crowded");
    await enterDeck(page);

    const tiles = page.locator(".agent-tile");
    const terminalInputs = tiles.locator("textarea.xterm-helper-textarea");
    await expect(tiles).toHaveCount(15);
    await expect(terminalInputs).toHaveCount(15);
    await expect(tiles.locator('[data-testid^="composer-"]')).toHaveCount(0);
    await expect(tiles.locator("textarea:not(.xterm-helper-textarea)")).toHaveCount(0);
    await expect(tiles.getByText("Message orchestrator", { exact: true })).toHaveCount(0);
    await expect(tiles.getByRole("button", { name: "Send", exact: true })).toHaveCount(0);
  });

  /**
   * Scenario: open the crowded deck, which carries one live writer, one
   * history-only pane, and one pane with no live target. The writer stays
   * enabled while both non-live terminal surfaces are disabled, marked with
   * their exact verdict, and accompanied by a human-readable status.
   */
  test("marks non-live fixture panes instead of accepting terminal input", async ({ page }) => {
    await page.goto("/?fixture=1&state=crowded");
    await enterDeck(page);

    const historyOnly = page.getByTestId("terminal-11");
    await expect(historyOnly).toHaveAttribute("data-input-state", "history-only");
    await expect(historyOnly).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByTestId("terminal-input-status-11")).toHaveAttribute("role", "status");
    await expect(page.getByTestId("terminal-input-status-11")).toHaveText(
      "Terminal input unavailable — the agent has no live pane — only its history remains.",
    );

    const noLiveTarget = page.getByTestId("terminal-15");
    await expect(noLiveTarget).toHaveAttribute("data-input-state", "no-live-target");
    await expect(noLiveTarget).toHaveAttribute("aria-disabled", "true");
    await expect(page.getByTestId("terminal-input-status-15")).toHaveAttribute("role", "status");
    await expect(page.getByTestId("terminal-input-status-15")).toHaveText(
      "Terminal input unavailable — there is nothing live to write to.",
    );

    const applied = page.getByTestId("terminal-2");
    await expect(applied).toHaveAttribute("data-input-state", "applied");
    await expect(applied).toHaveAttribute("aria-disabled", "false");
    await expect(page.getByTestId("terminal-input-status-2")).toHaveCount(0);
  });

  /**
   * Scenario: open the crowded deck, click into the writable agent's terminal
   * and press Enter, Shift+Enter, Ctrl+Enter and Ctrl+/ with the browser's own
   * key events. Each reaches the agent as the TUI sends it: Enter as the
   * carriage return that submits, Shift+Enter and Ctrl+Enter as sequences the
   * agent can tell apart from it rather than that same carriage return, and
   * Escape to the agent (issue #1422).
   */
  test("forwards modified Enter as the TUI does, not as Enter", async ({ page }) => {
    await captureTerminals(page);
    await page.goto("/?fixture=1&state=crowded");
    await enterDeck(page);

    const viewport = page.getByTestId("terminal-2");
    await expect(viewport).toHaveAttribute("aria-disabled", "false");
    await viewport.locator("textarea.xterm-helper-textarea").focus();
    await viewport.evaluate((root) => {
      const recording = window as Window & RecordingWindow;
      const terminal = recording.__dadE2eTerminals?.find((candidate) => candidate.element && root.contains(candidate.element));
      if (!terminal) throw new Error("the writable tile's xterm was not captured");
      recording.__dadE2eSent = [];
      terminal.onData((data) => recording.__dadE2eSent?.push(data));
    });

    const sentFor = async (chord: string) => {
      await page.evaluate(() => { (window as Window & RecordingWindow).__dadE2eSent = []; });
      await page.keyboard.press(chord);
      return page.evaluate(() => (window as Window & RecordingWindow).__dadE2eSent ?? []);
    };

    expect(await sentFor("Enter")).toEqual(["\r"]);
    expect(await sentFor("Shift+Enter")).toEqual(["\x1b[13;2u"]);
    expect(await sentFor("Control+Enter")).toEqual(["\x1b[13;5u"]);
    expect(await sentFor("Control+Shift+Enter")).toEqual(["\x1b[13;6u"]);
    expect(await sentFor("Alt+Enter")).toEqual(["\x1b\r"]);
    expect(await sentFor("Control+/")).toEqual(["\x1f"]);
    expect(await sentFor("Escape")).toEqual(["\x1b"]);
  });
});
