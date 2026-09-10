import { expect, test, type Page } from "@playwright/test";

import { openOverview, showAllColumns, tableRegion } from "./support/overview";

/**
 * Nothing pushes the page itself sideways — issue #836's "no horizontal
 * overflow of the page body at a stated viewport".
 *
 * The overview is the sharp case: it deliberately DOES overflow, inside
 * `.overview-table-region`. The contract is that the overflow stops there, and
 * `body { overflow-x: hidden }` is what makes getting this wrong invisible to
 * the eye — content past the right edge is clipped rather than reachable, so a
 * reader sees a truncated screen and no scrollbar telling them why.
 * `scrollWidth` still reports it, and that is what these assertions read.
 *
 * Two viewports, both stated rather than inherited from the Playwright device
 * preset: the ordinary desktop width, and the narrow one where the table is
 * known to overflow.
 */

const VIEWPORTS = [
  { name: "1280x800 desktop", width: 1280, height: 800, tableOverflows: false },
  { name: "900x800 narrow", width: 900, height: 800, tableOverflows: true },
] as const;

/** Every metric read in one round trip, so nothing can reflow between two reads. */
async function pageMetrics(page: Page) {
  return page.evaluate(() => ({
    rootScroll: document.documentElement.scrollWidth,
    rootClient: document.documentElement.clientWidth,
    bodyScroll: document.body.scrollWidth,
    bodyClient: document.body.clientWidth,
    viewport: window.innerWidth,
  }));
}

/**
 * The page-overflow contract, asserted identically by both tests below.
 *
 * It is a shared function rather than a copied block because the two tests
 * disagreed: the overview one checked `body`, `documentElement` and the
 * viewport, and the deck one checked `body` alone (found by Greptile on PR
 * #958). `body` carries `overflow-x: hidden`, so it can be within its own
 * client width while the ROOT scrolls — which is the case a reader would see as
 * a page that slides sideways. Both halves are needed, and now neither test can
 * be more rigorous than the other by accident.
 */
function expectNoPageOverflow(metrics: Awaited<ReturnType<typeof pageMetrics>>) {
  expect(metrics.bodyScroll, "content extends past the body, clipped by overflow-x: hidden").toBeLessThanOrEqual(
    metrics.bodyClient,
  );
  expect(metrics.rootScroll, "the root scrolls sideways, so the whole page slides").toBeLessThanOrEqual(
    metrics.rootClient,
  );
  expect(metrics.rootClient, "the root is wider than the viewport").toBeLessThanOrEqual(metrics.viewport);
}

for (const viewport of VIEWPORTS) {
  test.describe(`at ${viewport.name}`, () => {
    test.use({ viewport: { width: viewport.width, height: viewport.height } });

    test("the overview overflows only inside its own scroll region", async ({ page }) => {
      await openOverview(page, "crowded");
      await showAllColumns(page);

      expectNoPageOverflow(await pageMetrics(page));

      // The counterpart, so this is not passing because the screen collapsed to
      // nothing. At the narrow viewport the table region really is overflowing,
      // and the containment above is what keeps that off the page; at the wide
      // one nine columns fit, so there is nothing to contain and asserting an
      // overflow would be asserting a coincidence.
      const region = await tableRegion(page).evaluate((node) => ({
        scrollWidth: node.scrollWidth,
        clientWidth: node.clientWidth,
      }));
      if (viewport.tableOverflows) expect(region.scrollWidth).toBeGreaterThan(region.clientWidth);
      else expect(region.scrollWidth).toBe(region.clientWidth);
    });

    test("the deck does not overflow the page either", async ({ page }) => {
      await page.goto("/?fixture=1&state=crowded");
      // State, not a timer: the rail is rendered once the shell has mounted.
      await expect(page.getByTestId("open-overview")).toBeVisible();

      expectNoPageOverflow(await pageMetrics(page));
    });
  });
}
