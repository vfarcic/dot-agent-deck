import { expect, test, type Locator, type Page } from "@playwright/test";

import { openOverview, showAllColumns } from "./support/overview";

/**
 * A click anywhere on an overview row opens that agent's pane, and a drag that
 * selects text in the row does not.
 *
 * The vitest tier covers the row's rules against a selection it builds itself
 * and a `click` it dispatches itself, which proves what the handler does with
 * that state and nothing about whether a real gesture produces it. Two parts of
 * the behaviour are engine questions: whether the mouse-up that ends a drag
 * across a prompt still reaches the row as a `click` (so the guard is doing
 * something rather than never being reached), and whether the selection is
 * still non-collapsed at that moment. The hover is the third, because jsdom has
 * no pointer and never matches `:hover`; the focus ring a keyboard user sees on
 * the row is the fourth.
 */

/** One agent's row, found by its accessible name (issue #2454: the row is the open route). */
function rowFor(page: Page, displayName: string): Locator {
  return page.getByRole("row", { name: `Open ${displayName} agent`, exact: true });
}

test.describe("overview row opens the agent pane", () => {
  /**
   * Scenario: open the overview and click the Planner row's status word, away
   * from its Close control. That agent's pane opens over the overview, which
   * stays mounted and terminal-free below it.
   */
  test("a click on a row's text opens that agent's pane", async ({ page }) => {
    await openOverview(page, "connected");
    const overview = page.getByTestId("overview-table-region");

    await rowFor(page, "Plan / architecture").locator(".overview-state .status-label").click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay).toHaveAttribute("aria-label", "Planner agent");
    await expect(overview).toBeAttached();
    await expect(overview.locator(".terminal-viewport")).toHaveCount(0);
  });

  /**
   * Scenario: with every column on screen, drag across the coder row's prompt
   * with a real mouse. The text is selected, the mouse-up reaches the row as a
   * click, and no pane opens; a plain click on the same row then opens it.
   */
  test("dragging across a row's prompt selects the text and opens nothing", async ({ page }) => {
    await openOverview(page, "crowded");
    await showAllColumns(page);

    const row = rowFor(page, "coder");
    const prompt = row.locator(".overview-prompt");
    await expect(prompt).not.toHaveText("");
    // Counted on the row itself, so a green run cannot be one where the engine
    // simply never delivered a click after the drag and the guard went unused.
    await row.evaluate((node) => {
      node.addEventListener("click", () => {
        node.dataset.e2eClicks = String(Number(node.dataset.e2eClicks ?? "0") + 1);
      });
    });

    const box = await prompt.boundingBox();
    expect(box, "the prompt cell has no layout box").not.toBeNull();
    const y = box!.y + box!.height / 2;
    await page.mouse.move(box!.x + 2, y);
    await page.mouse.down();
    await page.mouse.move(box!.x + box!.width - 2, y, { steps: 10 });
    await page.mouse.up();

    // The pane is checked before the selection because a pane that opens here
    // leaves the selection empty in both engines (seen with the guard removed),
    // and that regression should be reported as the pane it opened rather than
    // as a drag that seemingly selected nothing.
    await expect(row).toHaveAttribute("data-e2e-clicks", "1");
    await expect(page.getByTestId("agent-pane-overlay")).toHaveCount(0);
    const selected = await page.evaluate(() => window.getSelection()?.toString() ?? "");
    expect(selected.length, "the drag selected no text").toBeGreaterThan(3);
    expect(await prompt.textContent()).toContain(selected);

    await row.locator(".overview-state .status-label").click();
    await expect(row).toHaveAttribute("data-e2e-clicks", "2");
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay).toHaveAttribute("aria-label", /coder/i);
  });

  /**
   * Scenario: point at a failed agent's row, whose red background hides the
   * ordinary row hover, then reach the same row from the keyboard. The row and
   * its text show a pointer, and with keyboard focus the row draws a focus ring
   * over its red background (issue #2454: the row is the keyboard route, so it
   * has to show where focus is).
   */
  test("a row reads as clickable and focusable, including a failed one", async ({ page }) => {
    await openOverview(page, "crowded");

    const row = rowFor(page, "release");
    await expect(row).toHaveAttribute("data-status", "failed");
    const status = row.locator(".overview-state .status-label");
    const ring = () => row.evaluate((node) => {
      const style = getComputedStyle(node);
      return `${style.outlineStyle} ${style.outlineWidth}`;
    });

    expect(await row.evaluate((node) => getComputedStyle(node).cursor)).toBe("pointer");
    expect(await status.evaluate((node) => getComputedStyle(node).cursor)).toBe("pointer");

    const resting = await ring();
    await row.focus();
    await page.keyboard.press("Tab");
    await page.keyboard.press("Shift+Tab");
    await expect(row).toBeFocused();
    await expect.poll(ring).not.toBe(resting);
    expect(await ring()).toMatch(/^solid /);
  });
});
