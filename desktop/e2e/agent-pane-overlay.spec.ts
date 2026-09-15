import { expect, test, type Locator, type Page } from "@playwright/test";
import { openOverview } from "./support/overview";

/** Assert the pane covers the browser viewport rather than merely being larger than a tile. */
async function expectToOccupyWindow(page: Page, overlay: Locator): Promise<void> {
  const box = await overlay.boundingBox();
  const viewport = await page.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }));
  expect(box, "the agent pane has no browser layout box").not.toBeNull();
  expect(Math.abs(box!.x)).toBeLessThanOrEqual(1);
  expect(Math.abs(box!.y)).toBeLessThanOrEqual(1);
  expect(Math.abs(box!.width - viewport.width)).toBeLessThanOrEqual(1);
  expect(Math.abs(box!.height - viewport.height)).toBeLessThanOrEqual(1);
}

test.describe("agent pane overlay", () => {
  /**
   * Scenario: open Planner from the deck in the built browser bundle. Its
   * dialog covers the viewport while the grid and another tile remain mounted;
   * Escape removes the dialog and exposes the same deck again.
   */
  test("occupies the window over a still-mounted deck and closes on Escape", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");

    const grid = page.locator(".agent-grid");
    const otherTile = page.getByTestId("agent-tile-builder");
    await expect(grid).toBeVisible();
    const originalGrid = await grid.elementHandle();
    expect(originalGrid, "the deck has no grid node to preserve").not.toBeNull();
    await page.getByRole("button", { name: "Open Planner agent" }).click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toHaveAttribute("role", "dialog");
    await expect(overlay).toHaveAttribute("aria-label", "Planner agent");
    await expect(overlay).toHaveAttribute("aria-modal", "true");
    await expect(overlay.getByRole("heading", { name: "Planner" })).toBeVisible();
    await expect(grid).toBeAttached();
    await expect(otherTile).toBeAttached();
    expect(await originalGrid!.evaluate((node) => node.isConnected)).toBe(true);
    expect(await page.evaluate((node) => document.querySelector(".agent-grid") === node, originalGrid)).toBe(true);
    await expectToOccupyWindow(page, overlay);

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    await expect(grid).toBeVisible();
    await expect(page.getByTestId("agent-tile-planner")).toBeVisible();
  });

  /**
   * Scenario: open Planner from its terminal-free overview card in the built
   * browser bundle. The overview remains mounted below the full-window pane,
   * and Escape returns to that overview rather than to the deck.
   */
  test("keeps the overview mounted below the pane and returns there on Escape", async ({ page }) => {
    await openOverview(page, "connected");

    const overview = page.getByTestId("overview-table-region");
    await expect(page.locator(".terminal-viewport")).toHaveCount(0);
    const originalOverview = await overview.elementHandle();
    expect(originalOverview, "the overview has no table-region node to preserve").not.toBeNull();
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overview).toBeAttached();
    expect(await originalOverview!.evaluate((node) => node.isConnected)).toBe(true);
    expect(await page.evaluate((node) => document.querySelector('[data-testid="overview-table-region"]') === node, originalOverview)).toBe(true);
    await expectToOccupyWindow(page, overlay);

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    await expect(overview).toBeVisible();
    await expect(page.getByRole("button", { name: "Open Plan / architecture agent" })).toBeVisible();
  });
});
