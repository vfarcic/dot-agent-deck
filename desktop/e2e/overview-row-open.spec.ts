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
 * no pointer and never matches `:hover`.
 */

/** The row carrying one agent's open control, found by that control's name. */
function rowFor(page: Page, displayName: string): Locator {
  return page.locator(".overview-row").filter({ has: page.getByRole("button", { name: `Open ${displayName} agent`, exact: true }) });
}

test.describe("overview row opens the agent pane", () => {
  /**
   * Scenario: open the overview and click the Planner row's status word, away
   * from its open control. That agent's pane opens over the overview, which
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
   * ordinary row hover. The row shows a pointer, and hovering its text lights
   * the open control exactly as hovering the control itself does.
   */
  test("a row reads as clickable, including a failed one", async ({ page }) => {
    await openOverview(page, "crowded");

    const row = rowFor(page, "release");
    await expect(row).toHaveAttribute("data-status", "failed");
    const status = row.locator(".overview-state .status-label");
    const open = row.getByRole("button", { name: "Open release agent", exact: true });
    const look = () => open.evaluate((node) => {
      const style = getComputedStyle(node);
      return `${style.color} | ${style.backgroundColor} | ${style.borderTopColor}`;
    });

    expect(await row.evaluate((node) => getComputedStyle(node).cursor)).toBe("pointer");
    expect(await status.evaluate((node) => getComputedStyle(node).cursor)).toBe("pointer");

    await page.mouse.move(0, 0);
    const resting = await look();
    await open.hover();
    const underPointer = await look();
    expect(underPointer).not.toBe(resting);

    await page.mouse.move(0, 0);
    await expect.poll(look).toBe(resting);
    await status.hover();
    await expect.poll(look).toBe(underPointer);
  });
});
