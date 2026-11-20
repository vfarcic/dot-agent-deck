import { expect, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * PRD #1790 stage 2 — a deck's interrupted work, as a user sees it: the
 * banner on the deck's dashboard section counting what a restart of its
 * daemon left behind, and the "Interrupted work" dialog it opens.
 *
 * `interrupted=1` makes the preview's daemon list three items — a dispatched
 * unit, an orchestration and a plain agent — and act on dismiss and reclaim
 * as a daemon would: the item is gone from the next list.
 */

async function openDashboard(page: Page): Promise<void> {
  await page.goto("/?fixture=1&interrupted=1");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
}

test.describe("Interrupted work", () => {
  /** Scenario: open the dashboard; the deck's banner reads "3 interrupted items from before the restart". Show opens the "Interrupted work" dialog; selecting the plain agent under "Other agents" and pressing Dismiss says "Dismissed.", and the banner then counts 2. */
  test("opens from the banner, and a dismissal drops the banner's count", async ({ page }) => {
    await openDashboard(page);
    const banner = page.getByTestId("interrupted-banner");
    await expect(banner).toContainText("3 interrupted items from before the restart");
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByRole("heading", { name: "Interrupted work" })).toBeVisible();
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await expect(dialog.getByTestId("interrupted-other-agents")).toHaveText("Other agents");

    await dialog.getByTestId("interrupted-row").filter({ hasText: "scratch-shell" }).click();
    await expect(dialog.getByTestId("interrupted-reclaim")).toHaveCount(0);
    await dialog.getByTestId("interrupted-dismiss").click();
    await expect(dialog.getByTestId("interrupted-message")).toHaveText("Dismissed.");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(2);
    await expect(banner).toContainText("2 interrupted items from before the restart");
  });

  /** Scenario: in the dialog, Reclaim on the unit shows the dry run's verdict "Reclaim would remove its worktree and keep its branch." and a Reclaim confirm; pressing it says "Reclaimed: its worktree was removed and its branch kept." and the unit leaves the list. Back from a dry run sends nothing. */
  test("reclaims only after the dry run's confirm", async ({ page }) => {
    await openDashboard(page);
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await dialog.getByTestId("interrupted-reclaim").click();
    await expect(dialog.getByTestId("interrupted-reclaim-verdict")).toContainText("Reclaim would remove its worktree and keep its branch.");
    await dialog.getByTestId("interrupted-back").click();
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-reclaim").click();
    await dialog.getByTestId("interrupted-reclaim-confirm").click();
    await expect(dialog.getByTestId("interrupted-message")).toHaveText("Reclaimed: its worktree was removed and its branch kept.");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(2);
    await expect(dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" })).toHaveCount(0);
    await expect(page.getByTestId("interrupted-banner")).toContainText("2 interrupted items from before the restart");
  });

  /** Scenario: without `interrupted=1` the preview's daemon does not support interrupted work, and the dashboard shows no banner. */
  test("shows nothing for a daemon without interrupted work", async ({ page }) => {
    await page.goto("/?fixture=1");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group").first()).toBeVisible();
    await expect(page.getByTestId("interrupted-banner")).toHaveCount(0);
  });
});
