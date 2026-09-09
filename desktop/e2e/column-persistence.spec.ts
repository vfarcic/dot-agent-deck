import { expect, test, type Page } from "@playwright/test";

import {
  ALL_COLUMNS,
  DEFAULT_COLUMNS,
  chooseColumns,
  legendFor,
  legendLabels,
  openOverview,
  showAllColumns,
  type OverviewColumn,
} from "./support/overview";

/**
 * The column choice outlives the visit that made it — issue #836's second item,
 * and the one the jsdom suite is structurally unable to reach.
 *
 * jsdom can assert that the app CALLS `localStorage.setItem`. It cannot reload
 * a document, because there is no navigation to survive: the suite mounts a
 * component into a tree it already owns and unmounts it again. What is under
 * test here is the whole round trip — a real engine writing to real per-origin
 * storage, a real navigation tearing the document down, and a fresh mount
 * reading the value back — which is the only sequence in which the feature can
 * actually fail for a user.
 *
 * The second half is the SCOPING. Every persisted key on this app is suffixed
 * with the runtime mode (`modeScopedKey`), so a fixture visit cannot hand live
 * mode a layout and a live layout cannot follow anyone into the demo data. That
 * is a claim about two literal key names, so it is asserted against the literal
 * key names.
 */

/**
 * The base key, copied deliberately — this test exists to pin the two
 * suffixed names, and reading them off the app would make it pass by
 * construction whatever the app wrote.
 */
const COLUMNS_KEY_BASE = "dot-agent-deck.desktop.overview-columns.v1";
const FIXTURE_KEY = `${COLUMNS_KEY_BASE}.fixture`;
const LIVE_KEY = `${COLUMNS_KEY_BASE}.live`;

/**
 * A set that is neither the defaults nor everything: it drops one default
 * column and adds two that are off by default. A choice that happened not to be
 * saved would come back as the defaults, and this is far enough from them that
 * "it persisted" and "it fell back" cannot be confused.
 */
const CHOSEN: readonly OverviewColumn[] = ["status", "spawnedAtMs", "toolCount", "lastUserPrompt"];

/** Every stored column choice, keyed by its full storage key. */
async function storedColumnChoices(page: Page, base: string): Promise<Record<string, unknown>> {
  return page.evaluate((prefix) => {
    const found: Record<string, unknown> = {};
    for (let index = 0; index < window.localStorage.length; index += 1) {
      const key = window.localStorage.key(index);
      if (key === null || !key.startsWith(prefix)) continue;
      const raw = window.localStorage.getItem(key);
      try {
        found[key] = raw === null ? null : JSON.parse(raw);
      } catch {
        found[key] = raw;
      }
    }
    return found;
  }, base);
}

/** Land back on the overview after a navigation, which always starts on the deck. */
async function reopenOverview(page: Page): Promise<void> {
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("overview-table-region")).toBeVisible();
}

test.describe("the overview's remembered columns", () => {
  test("a chosen set survives a reload", async ({ page }) => {
    await openOverview(page, "crowded");
    // The starting point, so "it persisted" is not being read off a screen that
    // was already showing the chosen set.
    await expect(legendLabels(page)).toHaveText(legendFor(DEFAULT_COLUMNS));

    await chooseColumns(page, CHOSEN);
    await expect(legendLabels(page)).toHaveText(legendFor(CHOSEN));

    await page.reload();
    await reopenOverview(page);

    await expect(legendLabels(page), "the reload came back on a different column set").toHaveText(legendFor(CHOSEN));

    /*
      And the LAYOUT took it, not just the legend text. This reads the engine's
      resolved `grid-template-columns` — a list of used pixel widths, computed
      after layout — rather than the template string the app wrote, so a screen
      that printed five labels over the four-track grid it woke up with would
      fail here.
    */
    const tracks = await page.locator(".overview-legend").evaluate((node) =>
      getComputedStyle(node).gridTemplateColumns.trim().split(/\s+/),
    );
    expect(tracks).toHaveLength(legendFor(CHOSEN).length);
    for (const track of tracks) expect(Number.parseFloat(track), `track "${track}" did not resolve to a width`).toBeGreaterThan(0);
  });

  test("fixture and live mode remember separate sets", async ({ page }) => {
    await openOverview(page, "crowded");
    await showAllColumns(page);

    const afterFixture = await storedColumnChoices(page, COLUMNS_KEY_BASE);
    expect(Object.keys(afterFixture), "a fixture visit wrote a key that is not fixture-scoped").toEqual([FIXTURE_KEY]);
    expect(afterFixture[FIXTURE_KEY]).toEqual({ columns: [...ALL_COLUMNS] });

    /*
      Live mode in a plain browser has no Tauri bridge, so no fleet ever
      arrives and the body shows a connection note instead of a table. The
      overview SCREEN still mounts, and mounting is what writes the column
      choice — so this is exactly the question the item asks: whose choice does
      live mode wake up with?
    */
    await page.goto("/?live=1");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("overview-columns-toggle")).toBeVisible();

    const afterLive = await storedColumnChoices(page, COLUMNS_KEY_BASE);
    expect(
      afterLive[LIVE_KEY],
      "live mode woke up on the layout a fixture visit chose — the keys are not scoped by mode",
    ).toEqual({ columns: [...DEFAULT_COLUMNS] });
    expect(afterLive[FIXTURE_KEY], "the live visit overwrote what fixture mode remembers").toEqual({ columns: [...ALL_COLUMNS] });

    // The other direction, on the screen rather than in storage: fixture mode
    // comes back to its own nine columns after the detour through live.
    await page.goto("/?fixture=1&state=crowded");
    await reopenOverview(page);
    await expect(legendLabels(page)).toHaveText(legendFor(ALL_COLUMNS));
  });

  test("a stored value that cannot be read leaves the screen on its defaults", async ({ page }) => {
    // Open once so the origin exists and its storage is writable, and so the
    // value written below is the one a reload reads.
    await openOverview(page, "crowded");
    await chooseColumns(page, CHOSEN);

    await page.evaluate((key) => window.localStorage.setItem(key, '{"columns": ['), FIXTURE_KEY);
    await page.reload();

    /*
      The failure this rules out is a white screen. `readStoredColumns` parses on
      mount, and an unguarded `JSON.parse` throwing there takes the whole app
      down before anything renders — which no jsdom test can observe, because
      the value it would parse never went through a real reload. What a user
      must get instead is the opening four.
    */
    await reopenOverview(page);
    await expect(legendLabels(page)).toHaveText(legendFor(DEFAULT_COLUMNS));
    await expect(page.locator(".overview-row")).toHaveCount(15);
  });
});
