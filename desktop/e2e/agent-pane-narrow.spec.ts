import { expect, test } from "@playwright/test";
import { openOverview } from "./support/overview";

/**
 * The narrow-window trap, in the one engine pair that evaluates media queries.
 *
 * `@media (max-width: 680px)` sets `.agent-panel, .agent-tabs { display: none }`
 * for the deck's tiles — a reasonable thing to do to a half-width tile on a
 * phone, and the wrong thing entirely to do to a pane that IS the window. A
 * pane that inherited it would open full-screen with a header, an assignment,
 * a footer and no terminal at all, which is the whole feature missing rather
 * than a layout blemish. jsdom evaluates no media query, so this claim is not
 * assertable in the vitest tier at any effort.
 *
 * The overview is the origin under test because it is the one that is actually
 * reachable here: the deck's own `.agent-tile:not(.is-selected) { display:
 * none }` hides every tile at this width until one is selected, so there is no
 * Open control on screen to press.
 */
test.describe("agent pane overlay at 400x780 phone", () => {
  test.use({ viewport: { width: 400, height: 780 } });

  /**
   * Scenario: open Planner from the overview on a phone-width window. The pane
   * covers the window and carries a live terminal and its tab strip, both of
   * which the tile's own responsive rules would have hidden.
   */
  test("keeps the terminal and the tab strip the tile's phone rules hide", async ({ page }) => {
    await openOverview(page, "connected");
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay.locator(".terminal-viewport")).toBeVisible();
    await expect(overlay.locator(".agent-tabs")).toBeVisible();
    // The pane is the window, not a tile-sized box that happens to be on top.
    const box = await overlay.boundingBox();
    expect(box, "the agent pane has no browser layout box").not.toBeNull();
    expect(Math.abs(box!.width - 400)).toBeLessThanOrEqual(1);
    expect(Math.abs(box!.height - 780)).toBeLessThanOrEqual(1);
    // And the terminal has real height inside it rather than being collapsed
    // to nothing by an inherited `display: none` on an ancestor.
    const terminal = await overlay.locator(".terminal-viewport").boundingBox();
    expect(terminal!.height).toBeGreaterThan(200);

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
  });
});
