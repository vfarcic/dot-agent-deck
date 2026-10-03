import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * PRD #1487 M5 — the desktop's Upgrade action, as a user drives it.
 *
 * The `upgrade` fixture fleet plays three daemons: this machine's, at the
 * app's own release (nothing to offer); `dev@build-box`, connected on an older
 * release with agents running, so its upgrade asks before stopping them; and
 * `ci@runner-7`, on an older release across a compatibility break, so it is
 * refused and its note carries Upgrade beside the other remedies. The fixture
 * bridge walks the same stages the live crate reports, and waits for the
 * dialog's answer the way the daemon's policy does.
 *
 * Every wait is on state — a stage turning active, the question appearing, an
 * outcome rendering — never on a timer.
 */

const BUILD_BOX = "dev@build-box";
const RUNNER = "ci@runner-7";

async function openUpgradeFleet(page: Page): Promise<void> {
  await page.goto("/?fixture=1&state=upgrade");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("daemon-group")).toHaveCount(3);
}

/** A deck's card on the dashboard, by the name its header shows. */
function card(page: Page, name: string): Locator {
  return page.getByTestId("daemon-group").filter({ has: page.getByTestId("daemon-identity").getByText(name, { exact: true }) });
}

test.describe("Upgrade a daemon from the dashboard", () => {
  test("is offered on an older daemon's card and nowhere else", async ({ page }) => {
    await openUpgradeFleet(page);

    await expect(card(page, BUILD_BOX).getByTestId("daemon-upgrade")).toBeVisible();
    await expect(card(page, "Local daemon").getByTestId("daemon-upgrade")).toHaveCount(0);
    // The refused deck carries it in its note, with the sentence that explains it.
    await expect(card(page, RUNNER).getByTestId("daemon-upgrade")).toHaveCount(0);
    await expect(card(page, RUNNER).getByTestId("overview-upgrade")).toBeVisible();
    await expect(card(page, RUNNER).getByTestId("overview-incompatible")).toContainText("Upgrade installs this app's version on that machine");
  });

  test("asks before stopping running agents, and Restart now upgrades", async ({ page }) => {
    await openUpgradeFleet(page);
    await card(page, BUILD_BOX).getByTestId("daemon-upgrade").click();

    const dialog = page.getByTestId("upgrade-dialog");
    await expect(dialog).toContainText(`Upgrade the daemon on ${BUILD_BOX}?`);
    await expect(page.getByTestId("upgrade-confirm-body")).toContainText("installs 0.45.0");
    await page.getByTestId("upgrade-start").click();

    // Progress, stage by stage.
    await expect(page.getByTestId("upgrade-stage-installing")).toHaveAttribute("data-state", /active|done/);
    // Agents are running there, so the daemon's policy asks first and names them.
    const question = page.getByTestId("upgrade-decision");
    await expect(question).toBeVisible();
    await expect(page.getByTestId("upgrade-stage-installing")).toHaveAttribute("data-state", "done");
    await expect(question).toContainText(`Restarting the daemon on ${BUILD_BOX} stops`);
    const atStake = page.getByTestId("upgrade-at-stake").getByRole("listitem");
    expect(await atStake.count()).toBeGreaterThan(0);

    await page.getByTestId("upgrade-restart-now").click();
    await expect(question).toHaveCount(0);

    const outcome = page.getByTestId("upgrade-outcome");
    await expect(outcome).toHaveAttribute("data-tone", "success");
    await expect(dialog).toContainText("Daemon upgraded");
    await expect(outcome).toContainText(`The daemon on ${BUILD_BOX} now runs 0.45.0 (it was 0.44.0).`);
    await expect(outcome).toContainText("These were stopped by the restart:");
    await page.getByTestId("upgrade-close").click();
    await expect(dialog).toHaveCount(0);

    // The deck is on the app's release now: nothing left to offer, nothing running.
    await expect(card(page, BUILD_BOX).getByTestId("daemon-upgrade")).toHaveCount(0);
    await expect(card(page, BUILD_BOX).getByTestId("overview-first-run")).toBeVisible();
  });

  test("Keep current daemon stops nothing and says so", async ({ page }) => {
    await openUpgradeFleet(page);
    await card(page, BUILD_BOX).getByTestId("daemon-upgrade").click();
    await page.getByTestId("upgrade-start").click();

    await expect(page.getByTestId("upgrade-decision")).toBeVisible();
    await page.getByTestId("upgrade-keep-current").click();

    const outcome = page.getByTestId("upgrade-outcome");
    await expect(outcome).toHaveAttribute("data-tone", "neutral");
    await expect(page.getByTestId("upgrade-dialog")).toContainText("Daemon kept running");
    await expect(outcome).toContainText("The daemon keeps running 0.44.0, as you chose");
    await page.getByTestId("upgrade-close").click();

    // Still older, still running its agents, still offered.
    await expect(card(page, BUILD_BOX).getByTestId("daemon-upgrade")).toBeVisible();
    await expect(card(page, BUILD_BOX).getByTestId("overview-first-run")).toHaveCount(0);
  });

  test("Upgrade in a refused daemon's note restarts an idle daemon without asking", async ({ page }) => {
    await openUpgradeFleet(page);
    await card(page, RUNNER).getByTestId("overview-upgrade").click();
    await page.getByTestId("upgrade-start").click();

    const outcome = page.getByTestId("upgrade-outcome");
    await expect(outcome).toContainText(`The daemon on ${RUNNER} now runs 0.45.0`);
    await expect(outcome).toContainText("Nothing was running, so nothing was stopped.");
    await expect(page.getByTestId("upgrade-decision")).toHaveCount(0);
    await page.getByTestId("upgrade-close").click();

    // Refused before, connected now.
    await expect(card(page, RUNNER).getByTestId("overview-incompatible")).toHaveCount(0);
    await expect(card(page, RUNNER)).toHaveAttribute("data-deck-connected", "yes");
  });
});

test.describe("Upgrade from the Daemons screen's banner", () => {
  test("offers Upgrade beside the other remedies and runs it", async ({ page }) => {
    await page.goto("/?fixture=1&state=upgrade-error&experimental=1");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await page.getByTestId("open-deck").click();

    const banner = page.locator(".connection-banner");
    await expect(banner).toContainText("Incompatible daemon");
    await expect(page.getByTestId("connection-banner-remedy")).toContainText("Upgrade installs this app's version on that machine");
    await page.getByTestId("upgrade-daemon-banner").click();
    await page.getByTestId("upgrade-start").click();

    await expect(page.getByTestId("upgrade-outcome")).toContainText(`The daemon on ${RUNNER} now runs 0.45.0`);
    await page.getByTestId("upgrade-close").click();
    await expect(page.getByTestId("upgrade-daemon-banner")).toHaveCount(0);
  });
});
