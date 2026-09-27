import { expect, test, type Page } from "@playwright/test";
import { enterDeck, selectOverview } from "./support/overview";

/** The navigation rail as a user sees it in the built browser app. */
function rail(page: Page) {
  // The background becomes inert while an agent dialog is open, but its rail
  // remains mounted in the DOM and must keep the same entries.
  return page.locator('aside.rail[aria-label="Primary navigation"]');
}

test.describe("desktop navigation rail", () => {
  /**
   * Scenario: open the app with fixture data and no prior navigation. The
   * overview is the landing screen, and its Dashboard entry leads the rail.
   */
  test("lands on the overview with Dashboard first", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
    await expect(rail(page)).toHaveCount(1);
    await expect(rail(page).locator("nav button")).toHaveText(["Dashboard", "Settings"]);
    await expect(rail(page).locator("[aria-current='page']")).toHaveText("Dashboard");
    await expect(page.getByTestId("overview-open-deck")).toHaveCount(0);
  });

  /**
   * Scenario: visit the deck, return to the overview, and open Planner's pane.
   * Each state keeps one rail with the same entries and the correct current
   * screen, including the overview below the pane.
   */
  test("keeps one rail and its active screen through navigation and agent view", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    await enterDeck(page);
    await expect(page.getByTestId("agent-tile-planner")).toBeVisible();
    await expect(rail(page)).toHaveCount(1);
    const entries = await rail(page).locator("nav button").allTextContents();
    expect(entries).toEqual(["Dashboard", "Daemons", "Projects", "Prompts", "Orchestrations", "Agent Profiles", "Settings"]);
    await expect(rail(page).locator("[aria-current='page']")).toHaveText("Daemons");

    await page.getByTestId("open-overview").click();
    await expect(rail(page)).toHaveCount(1);
    await expect(rail(page).locator("nav button")).toHaveText(entries);
    await expect(rail(page).locator("[aria-current='page']")).toHaveText("Dashboard");

    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();
    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
    await expect(rail(page)).toHaveCount(1);
    await expect(rail(page).locator("nav button")).toHaveText(entries);
    await expect(rail(page).locator("[aria-current='page']")).toHaveText("Dashboard");
  });

  /**
   * Scenario: on the overview, click the Settings entry in the rail. The
   * Settings sheet opens directly over the overview.
   */
  test("opens Settings directly from the overview", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    await selectOverview(page);
    const settings = rail(page).getByRole("button", { name: "Settings" });
    await expect(settings).toHaveCount(1);
    await settings.click();
    await expect(page.getByRole("dialog", { name: "Settings" })).toBeVisible();
  });

  /**
   * Scenario: on the shipped overview, open Planner's terminal pane and close
   * it with Escape. The overview returns without exposing the gated deck.
   */
  test("returns from an agent pane to the overview with experimental off", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();
    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByTestId("agent-pane-overlay")).toHaveCount(0);
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
    await expect(rail(page).locator("nav button")).toHaveText(["Dashboard", "Settings"]);
  });
});
