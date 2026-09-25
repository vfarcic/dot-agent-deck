import { expect, test } from "@playwright/test";
import { enterDeck } from "./support/overview";

/**
 * The deck's terminal-input contract, exercised against the crowded fixture so
 * the rendered page includes coordinator, writable, history-only, and
 * no-live-target tiles without a daemon or an agent credential.
 */
test.describe("agent terminal input", () => {
  /**
   * Scenario: open the crowded deck and wait for all fifteen xterm inputs to
   * mount. Each tile has exactly that one textarea, including the coordinator;
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
    await expect(tiles.getByText("Message coordinator", { exact: true })).toHaveCount(0);
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
});
