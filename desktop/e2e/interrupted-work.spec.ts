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

async function openDashboard(page: Page, interrupted = "1"): Promise<void> {
  await page.goto(`/?fixture=1&interrupted=${interrupted}`);
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

  /** Scenario: several daemons each answer with an empty list that says their journal could not be read whole. Each connected daemon's section shows a banner saying some interrupted work may not be listed; `I` opens nothing and says the daemons may have interrupted work. Tabbing to one banner's Show and pressing Enter opens that daemon's dialog, which says the list is incomplete. */
  test("offers a banner to open each daemon whose empty list is incomplete", async ({ page }) => {
    await page.goto("/?fixture=1&state=fleet&interrupted=unread");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await page.getByTestId("open-overview").click();
    const banners = page.getByTestId("interrupted-banner");
    await expect.poll(() => banners.count()).toBeGreaterThan(1);
    for (const banner of await banners.all()) await expect(banner).toContainText("some interrupted work may not be listed");

    await page.keyboard.press("I");
    await expect(page.getByTestId("overview-daemon-notice")).toContainText("Several daemons may have interrupted work");
    await expect(page.getByTestId("interrupted-dialog")).toHaveCount(0);

    const second = banners.nth(1);
    await second.getByTestId("interrupted-banner-show").focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByRole("heading", { name: "Interrupted work" })).toBeVisible();
    await expect(dialog).toContainText("The deck could not read its whole work journal, so an item may be missing or out of date.");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(0);
  });

  /* PRD #1790 stage 2, desktop review findings 3, 5 and 8. */

  /** Scenario: with keyboard focus on one of the dialog's controls, Enter does what that control does: on Dismiss all it asks the question, on Close it closes, on Reclaim it runs the dry run, and on a row other than the selected one it opens that row's details; in an item's details, Enter on Back returns to the list rather than asking for reports. */
  test("Enter on a focused control does what that control does", async ({ page }) => {
    await openDashboard(page);
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-dismiss-all-open").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-dismiss-all-question")).toBeVisible();
    await expect(dialog.getByTestId("interrupted-details-view")).toHaveCount(0);
    await dialog.getByTestId("interrupted-back").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-reclaim").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-reclaim-verdict")).toContainText("Reclaim would remove its worktree and keep its branch.");
    await dialog.getByTestId("interrupted-back").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-row").filter({ hasText: "release-team" }).focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-details-view")).toContainText("release-team");
    await dialog.getByTestId("interrupted-back").click();

    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await dialog.getByTestId("interrupted-details").click();
    await expect(dialog.getByTestId("interrupted-show-reports")).toBeVisible();
    await dialog.getByTestId("interrupted-back").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-details-view")).toHaveCount(0);
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-close").focus();
    await page.keyboard.press("Enter");
    await expect(dialog).toHaveCount(0);
  });

  /** Scenario: with thirty items, more than the dialog shows at once, pressing `j` moves the selection down past the last row on screen; the selected row is scrolled into view each time, so `d` dismisses the row the user can see selected. */
  test("keeps the selected row in view as j moves it", async ({ page }) => {
    await openDashboard(page, "many");
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(30);
    for (let at = 0; at < 25; at += 1) await page.keyboard.press("j");
    const selected = dialog.locator(".interrupted-row.is-selected");
    await expect(selected).toContainText("extra-unit-24");
    const box = await dialog.boundingBox();
    const row = await selected.boundingBox();
    expect(box && row).toBeTruthy();
    expect(row!.y).toBeGreaterThanOrEqual(box!.y);
    expect(row!.y + row!.height).toBeLessThanOrEqual(box!.y + box!.height + 1);
    await page.keyboard.press("d");
    await expect(dialog.getByTestId("interrupted-message")).toHaveText("Dismissed.");
    await expect(dialog.getByTestId("interrupted-row").filter({ hasText: "extra-unit-24" })).toHaveCount(0);
  });

  /** Scenario: on the dashboard `I` opens the Interrupted work dialog, and `q` closes it. In the dialog, `D` followed at once by `y`, typed as one burst, asks the Dismiss all question and dismisses nothing; `n` cancels it. Asked again and given time to be read, `y` dismisses every item. */
  test("opens with I, and a key burst cannot answer a question before it is presented", async ({ page }) => {
    await openDashboard(page);
    await expect(page.getByTestId("interrupted-banner")).toBeVisible();
    await page.keyboard.press("I");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await page.keyboard.press("q");
    await expect(dialog).toHaveCount(0);
    await page.keyboard.press("I");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await page.keyboard.type("Dyyy");
    await expect(dialog.getByTestId("interrupted-dismiss-all-question")).toBeVisible();
    await expect(dialog.getByTestId("interrupted-message")).toHaveCount(0);
    await page.keyboard.press("n");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await page.keyboard.press("D");
    await expect(dialog.getByTestId("interrupted-dismiss-all")).toHaveAttribute("data-presented", "true");
    await page.keyboard.press("y");
    await expect(dialog.getByTestId("interrupted-empty")).toBeVisible();
  });
  /* PRD #1790 stage 2, final reviewer 3 (CLAUDE.md rule 22): the TUI's X and its j/k scrolling. */

  /** Scenario: on the dashboard `X` hides the deck's banner, as the TUI's `X` hides its row; `I` then opens the dialog with all three items still listed, so nothing was dismissed. */
  test("X hides the banner and dismisses nothing", async ({ page }) => {
    await openDashboard(page);
    await expect(page.getByTestId("interrupted-banner")).toBeVisible();
    await page.keyboard.press("X");
    await expect(page.getByTestId("interrupted-banner")).toHaveCount(0);
    await page.keyboard.press("I");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
  });

  /** Scenario: in a window too short for the dialog, `j` scrolls an item's details down and `k` back up, the same with its report text shown and with the Dismiss all question; none of them changes the selected item, answers the question or dismisses anything. */
  test("j and k scroll the details, the report text and a question", async ({ page }) => {
    await openDashboard(page);
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await page.setViewportSize({ width: 900, height: 170 });
    const scrollTop = () => dialog.evaluate((element) => element.scrollTop);
    const overflows = () => dialog.evaluate((element) => element.scrollHeight > element.clientHeight + 4);
    const scrolls = async () => {
      await dialog.evaluate((element) => { element.scrollTop = 0; });
      expect(await overflows()).toBe(true);
      await page.keyboard.press("j");
      await expect.poll(scrollTop).toBeGreaterThan(0);
      const down = await scrollTop();
      await page.keyboard.press("k");
      await expect.poll(scrollTop).toBeLessThan(down);
    };

    await dialog.getByTestId("interrupted-details").click();
    await expect(dialog.getByTestId("interrupted-details-view")).toContainText("fix-login");
    await scrolls();
    await dialog.getByTestId("interrupted-show-reports").click();
    await expect(dialog.getByTestId("interrupted-report-body")).toBeVisible();
    await scrolls();
    await expect(dialog.getByTestId("interrupted-details-view")).toContainText("fix-login");
    await page.keyboard.press("q");
    await expect(dialog.locator(".interrupted-row.is-selected")).toContainText("fix-login");

    await page.keyboard.press("D");
    await expect(dialog.getByTestId("interrupted-dismiss-all")).toHaveAttribute("data-presented", "true");
    await page.setViewportSize({ width: 900, height: 90 });
    await scrolls();
    await expect(dialog.getByTestId("interrupted-dismiss-all-question")).toBeVisible();
    await expect(dialog.getByTestId("interrupted-message")).toHaveCount(0);
    await page.keyboard.press("n");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await expect(dialog.locator(".interrupted-row.is-selected")).toContainText("fix-login");
  });
});
