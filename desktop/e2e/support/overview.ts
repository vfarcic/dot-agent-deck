import { expect, type Locator, type Page } from "@playwright/test";

/**
 * Helpers shared by the browser specs. Everything here waits on STATE — a
 * locator becoming visible, a count reaching a number — and nothing waits on a
 * timer. Issue #807 records a load-sensitive race in the Rust e2e tier on a
 * 4-vCPU runner; a browser suite has the same exposure and the cheapest time to
 * avoid it is before the first `waitForTimeout` is written.
 */

/**
 * The fixture scenarios PRD #745 built for daemon-free inspection, reachable by
 * query string. They need no daemon, no credentials and no real agent, so a
 * browser driver can use them exactly as a human reviewer does.
 */
export type FixtureScenario = "connected" | "crowded" | "empty" | "disconnected" | "error";

/**
 * Every column the overview can show, in the picker's own order.
 *
 * Duplicated from `src/components/AgentOverview.tsx` rather than imported: the
 * import would pull React, `lucide-react` and the whole component graph into
 * the Playwright runner for a list of nine strings. `assertsEveryColumnExists`
 * below is what stops the copy drifting — a renamed id fails the run rather
 * than quietly testing eight columns.
 */
export const ALL_COLUMNS = [
  "status",
  "displayName",
  "lastActivityMs",
  "spawnedAtMs",
  "cli",
  "activeTool",
  "toolCount",
  "cwd",
  "lastUserPrompt",
] as const;

/** The one column the picker refuses to untick, so `check()` must skip it. */
const PERMANENT_COLUMN = "displayName";

/**
 * Load the fixture and click through to the agent overview.
 *
 * There is no URL that lands on the overview — `DeckShell` starts on the deck
 * and the rail button is the only way across — so this is a real click on the
 * real production bundle, not a router shortcut.
 */
export async function openOverview(page: Page, scenario: FixtureScenario = "crowded"): Promise<void> {
  await page.goto(`/?fixture=1&state=${scenario}`);
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("overview-table-region")).toBeVisible();
}

/**
 * Tick every column in the picker, which is how the table is made wide enough
 * to overflow. The overflow is the precondition for the alignment contract:
 * cards can only fall out of step once there is somewhere to scroll to.
 */
export async function showAllColumns(page: Page): Promise<void> {
  await page.getByTestId("overview-columns-toggle").click();
  const menu = page.getByTestId("overview-columns-menu");
  await expect(menu).toBeVisible();

  for (const column of ALL_COLUMNS) {
    const checkbox = page.getByTestId(`overview-column-${column}`);
    await expect(checkbox, `column "${column}" has no checkbox — the id list here has drifted from ALL_OVERVIEW_COLUMNS`).toBeAttached();
    if (column === PERMANENT_COLUMN) {
      // Permanent means `disabled`, and `check()` on a disabled input waits
      // until it times out. Assert the invariant instead of fighting it.
      await expect(checkbox).toBeChecked();
      continue;
    }
    if (!(await checkbox.isChecked())) await checkbox.check();
  }

  await page.keyboard.press("Escape");
  await expect(menu).toBeHidden();
  // The layout has actually taken the new template once the legend prints one
  // label per column. This is the state wait that replaces "give React a
  // moment" — without it the first `boundingBox()` can read the four-column
  // grid.
  await expect(page.locator(".overview-legend span")).toHaveCount(ALL_COLUMNS.length);
}

/** The single scroll region the legend and every group card live inside. */
export function tableRegion(page: Page): Locator {
  return page.getByTestId("overview-table-region");
}

/** Every group card currently on screen, in document order. */
export function groupCards(page: Page): Locator {
  return page.locator(".overview-group");
}

/**
 * `getBoundingClientRect()` for each element a locator matches, read in one
 * round trip. Playwright's own `boundingBox()` is per-element and returns
 * `null` for anything invisible; these assertions want the raw numbers for a
 * whole set, measured at one instant, so a scroll cannot land between two
 * reads.
 */
export async function rects(locator: Locator): Promise<DOMRect[]> {
  return locator.evaluateAll((nodes) => nodes.map((node) => node.getBoundingClientRect().toJSON() as DOMRect));
}

/**
 * Sub-pixel layout is legitimate — a fractional container width divided by
 * fractional tracks lands on fractional edges, and the two engines round
 * differently — so equality across cards is asserted to within half a CSS
 * pixel rather than exactly. Anything larger than that is a card at a different
 * horizontal offset, which is the failure this tier exists to catch.
 */
export const ALIGNMENT_TOLERANCE_PX = 0.5;
