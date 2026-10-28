import { expect, test } from "@playwright/test";
import { openOverview } from "./support/overview";

/**
 * Issue #2001 made the tile's title the agent's name, which the daemon accepts
 * up to 128 bytes long. jsdom lays nothing out, so whether a long name pushes
 * the status out of a narrow header is a question only an engine can answer.
 */
test.describe("agent pane header with a long name at 400x780", () => {
  test.use({ viewport: { width: 400, height: 780 } });

  /**
   * Scenario: open Planner's pane on a phone-width window and give its title a
   * long unbroken name. The title is cut with an ellipsis and the status beside
   * it stays inside the header.
   *
   * The fixture offers no rename (only a live deck does), so the name is
   * written into the rendered title in place. What is under test is the
   * stylesheet, which lays out whatever text the title holds.
   */
  test("cuts a long name and keeps the status in view", async ({ page }) => {
    const longName = "an-agent-whose-name-is-far-too-long-to-fit-beside-its-status-on-a-phone-width-pane";
    await openOverview(page, "connected");
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();

    const title = overlay.getByRole("heading", { level: 2 });
    await expect(title).toHaveText("Plan / architecture");
    await title.evaluate((el, name) => { el.textContent = name; }, longName);
    expect(await title.evaluate((el) => el.scrollWidth > el.clientWidth), "the long name is cut rather than shown whole").toBe(true);

    const header = await overlay.locator(".agent-header").boundingBox();
    const status = await overlay.locator(".agent-title-line .status-label").boundingBox();
    expect(header, "the header has no layout box").not.toBeNull();
    expect(status, "the status has no layout box").not.toBeNull();
    expect(status!.x + status!.width).toBeLessThanOrEqual(header!.x + header!.width);
    expect(status!.width).toBeGreaterThan(0);
  });
});
