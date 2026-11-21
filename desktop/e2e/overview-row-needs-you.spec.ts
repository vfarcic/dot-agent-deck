import { expect, test, type Locator, type Page } from "@playwright/test";

import { selectOverview } from "./support/overview";

/**
 * Issue #2454 — a dashboard row's Needs you badge sits beside the agent's name,
 * and the row itself is the keyboard route into the agent's pane.
 *
 * The vitest tier proves where the badge is in the DOM and that Enter on the
 * row opens the pane. Whether the controls at the end of a row stay put when
 * the badge comes and goes is geometry, which jsdom does not compute, and
 * whether a real Tab lands on the row and a real Enter opens it is the
 * engine's focus and key handling — so both are asked here.
 */

/** The docs scene with cards: two of its rows need you and the others do not. */
async function openCardsScene(page: Page): Promise<void> {
  await page.goto("/?fixture=1&state=docs&cards=1");
  await selectOverview(page);
  await expect(page.getByTestId("overview-needs-you").first()).toBeVisible();
}

function rows(page: Page): Locator {
  return page.locator(".overview-row");
}

async function box(locator: Locator): Promise<{ x: number; y: number; width: number; height: number }> {
  const found = await locator.boundingBox();
  expect(found, "the element has a box on screen").not.toBeNull();
  return found!;
}

test.describe("a dashboard row's Needs you and keyboard open (issue #2454)", () => {
  /**
   * Scenario: open the Dashboard's table on the cards scene, where some rows
   * need you and some do not. Every row's Close and ⋯ controls start at the
   * same x and are the same width whether or not the row shows Needs you, and
   * each Needs you badge sits inside the name cell, left of those controls.
   */
  test("the row's controls line up with and without the badge", async ({ page }) => {
    await openCardsScene(page);
    const all = rows(page);
    const count = await all.count();
    const withBadge: Locator[] = [];
    const without: Locator[] = [];
    for (let at = 0; at < count; at += 1) {
      const row = all.nth(at);
      if (await row.getByTestId("overview-needs-you").count()) withBadge.push(row);
      else without.push(row);
    }
    expect(withBadge.length, "the scene has rows that need you").toBeGreaterThan(0);
    expect(without.length, "the scene has rows that do not").toBeGreaterThan(0);

    const reference = await box(without[0].locator(".overview-row-actions"));
    const referenceClose = await box(without[0].getByTestId("overview-stop-agent"));
    for (const row of [...withBadge, ...without]) {
      const actions = await box(row.locator(".overview-row-actions"));
      expect(actions.x).toBeCloseTo(reference.x, 0);
      expect(actions.width).toBeCloseTo(reference.width, 0);
      expect((await box(row.getByTestId("overview-stop-agent"))).x).toBeCloseTo(referenceClose.x, 0);
    }
    for (const row of withBadge) {
      const badge = row.getByTestId("overview-needs-you");
      await expect(badge).toHaveText(/needs you/i);
      await expect(badge).toHaveAttribute("title", /^Needs you: /);
      const name = await box(row.locator(".overview-agent-name"));
      const badgeBox = await box(badge);
      const actions = await box(row.locator(".overview-row-actions"));
      expect(badgeBox.x).toBeGreaterThanOrEqual(name.x);
      expect(badgeBox.x + badgeBox.width).toBeLessThanOrEqual(actions.x);
      await expect(row.locator(".overview-row-actions").getByTestId("overview-needs-you")).toHaveCount(0);
    }
    await expect(page.locator(".overview-open-agent, [data-testid='overview-generate-config']")).toHaveCount(0);
  });

  /**
   * Scenario: click the page above the table, press Tab until a row has focus,
   * and press Enter. The focused row is announced as "Open <name> agent", the
   * next Tab goes to that row's Close rather than to a second stop on the same
   * row, and Enter on the row opens that agent's pane.
   */
  test("Tab reaches each row once and Enter opens its pane", async ({ page }) => {
    await openCardsScene(page);
    const first = rows(page).first();
    await first.focus();
    await expect(first).toBeFocused();
    const name = await first.getAttribute("aria-label");
    expect(name).toMatch(/^Open .+ agent$/);
    await page.keyboard.press("Tab");
    await expect(first.getByTestId("overview-stop-agent")).toBeFocused();
    await page.keyboard.press("Shift+Tab");
    await expect(first).toBeFocused();

    await page.keyboard.press("Enter");
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);

    const second = rows(page).nth(1);
    await second.focus();
    await page.keyboard.press("Space");
    await expect(overlay).toBeVisible();
  });
});
