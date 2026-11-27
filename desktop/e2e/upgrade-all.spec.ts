import { expect, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * Issue #1600 — the Dashboard's Upgrade all, as a user drives it.
 *
 * The `upgrade` fixture fleet plays this machine's daemon at the app's own
 * release and two remote daemons on an older one: `dev@build-box`, connected
 * with agents running, and `ci@runner-7`, refused across a compatibility break
 * with nothing running. Upgrade all checks the two remote decks and upgrades
 * them one after another: the busy one asks about its own agents before
 * stopping any, and the idle one restarts without a question.
 * The fixture bridge plays the crate's batch; every wait is on state.
 */

const BUILD_BOX = "dev@build-box";
const RUNNER = "ci@runner-7";

async function openUpgradeFleet(page: Page): Promise<void> {
  await page.goto("/?fixture=1&state=upgrade");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("daemon-group")).toHaveCount(3);
}

test.describe("Upgrade all daemons from the dashboard", () => {
  /** Scenario: press Upgrade all, read the plan for both older remote decks, start it, keep the busy daemon when it asks, let the idle one restart without a question, and read one line per deck at the end. */
  test("asks only the busy daemon and reports one line per deck", async ({ page }) => {
    await openUpgradeFleet(page);
    await page.getByTestId("overview-upgrade-all").click();

    const dialog = page.getByTestId("upgrade-all-dialog");
    await expect(dialog).toContainText("Upgrade all daemons");
    await expect(page.getByTestId("upgrade-all-headline")).toHaveText("Checked 2 remote decks: 2 to upgrade.");
    await expect(page.getByTestId("upgrade-all-deck")).toHaveText([
      `${BUILD_BOX}: Will be upgraded from 0.44.0 to 0.45.0.`,
      `${RUNNER}: Will be upgraded from 0.44.0 to 0.45.0.`,
    ]);
    await expect(page.getByTestId("upgrade-all-cancel")).toBeFocused();
    await page.getByTestId("upgrade-all-start").click();

    // The first deck asks about its own agents.
    const decision = page.getByTestId("upgrade-all-decision");
    await expect(decision).toContainText(`Restarting the daemon on ${BUILD_BOX} stops`);
    await expect(page.getByTestId("upgrade-all-deck").nth(1)).toHaveText(`${RUNNER}: Waiting to be upgraded.`);
    await page.getByTestId("upgrade-all-keep-current").click();

    // The idle second deck restarts without asking.
    const results = page.getByTestId("upgrade-all-results");
    await expect(results).toBeVisible();
    await expect(page.getByTestId("upgrade-all-headline")).toHaveText("Upgrade all: 1 upgraded, 1 kept running of 2 remote decks.");
    await expect(page.getByTestId("upgrade-all-deck").first()).toHaveAttribute("data-status", "kept");
    await expect(page.getByTestId("upgrade-all-deck").nth(1)).toHaveAttribute("data-status", "upgraded");
    await expect(page.getByTestId("upgrade-all-deck").nth(1)).toContainText(`${RUNNER}: Upgraded: now runs 0.45.0 (was 0.44.0).`);
    await page.getByTestId("upgrade-all-close").click();
    await expect(dialog).toHaveCount(0);
  });

  /** Scenario: start Upgrade all and press Stop after this daemon while the first daemon asks; keeping it ends the batch with the second deck not started. */
  test("Stop after this daemon starts no further deck", async ({ page }) => {
    await openUpgradeFleet(page);
    await page.getByTestId("overview-upgrade-all").click();
    await page.getByTestId("upgrade-all-start").click();
    // Stop is offered while the first deck installs, before its question.
    await page.getByTestId("upgrade-all-stop").click();
    await expect(page.getByTestId("upgrade-all-headline")).toHaveText("Stopping after this daemon…");
    await expect(page.getByTestId("upgrade-all-decision")).toContainText(`Restarting the daemon on ${BUILD_BOX} stops`);
    await page.getByTestId("upgrade-all-keep-current").click();

    await expect(page.getByTestId("upgrade-all-results")).toBeVisible();
    await expect(page.getByTestId("upgrade-all-deck").nth(1)).toHaveText(`${RUNNER}: Not started: you stopped the upgrade before it reached this deck.`);
  });
});
