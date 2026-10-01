import { test as setup } from "@playwright/test";

/**
 * Pays WebKit's first-launch cost once, before any spec's 30-second budget is
 * running.
 *
 * **What it absorbs.** The first WebKit test of a CI run is consistently slow.
 * Across 21 green `desktop-browser` runs read on
 * 2026-10-01, the first WebKit test (`agent-pane-modal.spec.ts`, by
 * sort order) took 5.1–19.0s, while the WebKit tests after it took 0.9–2.6s
 * and the same test took ~2s in Chromium. Twice the tail went past 30s and
 * reddened a test that had nothing wrong with it. In one of those two runs,
 * creating the page alone took 25.1s, and every step of the test then passed
 * in 4.5s before the budget expired. In the other, the page stopped answering
 * for more than 20s just after the agent pane opened its full-window
 * terminal. After each failure Playwright started a fresh browser, and the
 * next test ran in 1.1s. So the cost belongs to the runner's first use of
 * WebKit rather than to one browser process, and a separate warm-up browser
 * can pay it. Which first-use state holds that cost was not measured.
 *
 * **What it does.** It drives the flow the slow test drives: load the fixture,
 * enter the deck and open an agent's pane. That way the first page creation
 * and the first full-window WebGL terminal both happen here, under a
 * deliberately generous budget, and are logged. It asserts nothing about the
 * app. A run where this fails has a real problem, because three minutes is
 * not a cold start.
 *
 * **Why a setup project and not `globalSetup`.** The `webkit` project depends
 * on this one, so `--project=chromium` never launches WebKit, and a machine
 * without WebKit installed can still run the Chromium half.
 */
setup.setTimeout(180_000);

setup("warm WebKit before the first spec", async ({ page }) => {
  const started = Date.now();
  await page.goto("/?fixture=1&state=fleet&experimental=1");
  await page.getByTestId("open-deck").click({ timeout: 120_000 });
  await page.locator(".agent-grid").waitFor({ timeout: 120_000 });
  await page.getByRole("button", { name: "Open Planner agent" }).click({ timeout: 120_000 });
  await page.getByTestId("agent-pane-overlay").waitFor({ timeout: 120_000 });
  console.log(`webkit warm-up: ${Date.now() - started}ms from first navigation to an open agent pane`);
});
