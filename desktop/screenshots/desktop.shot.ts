import { expect, type Page } from "@playwright/test";

import { desktopScenario } from "./support";

/**
 * The desktop half of the docs screenshots (issue #1322): the production web
 * build in fixture mode — no daemon, no agent, no credential. Each scenario
 * name must also be in `xtask/screenshots/src/scenarios.rs`.
 */

/** Load a fixture state and open the agent overview through its rail control. */
async function overview(page: Page, state: "connected" | "empty"): Promise<void> {
  await page.goto(`/?fixture=1&state=${state}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
}

// The same four agents the TUI `dashboard` image shows.
desktopScenario("dashboard", async (page) => {
  await overview(page, "connected");
  await expect(page.locator(".overview-row")).toHaveCount(4);
});

desktopScenario("dashboard-empty", async (page) => {
  await overview(page, "empty");
  await expect(page.getByTestId("overview-first-run")).toBeVisible();
});
