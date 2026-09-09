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

/** One of the nine ids above. */
export type OverviewColumn = (typeof ALL_COLUMNS)[number];

/** The one column the picker refuses to untick, so `check()` must skip it. */
export const PERMANENT_COLUMN = "displayName";

/**
 * The four columns the screen shows before anyone chooses, in grid order.
 *
 * A second deliberate copy, of `DEFAULT_OVERVIEW_COLUMNS`. Reversing it — a
 * spec that read the defaults off the screen it is about to assert — would make
 * "Restore defaults restored the defaults" true by construction, which is the
 * one thing that test must not be.
 */
export const DEFAULT_COLUMNS = ["status", "displayName", "spawnedAtMs", "cwd"] as const satisfies readonly OverviewColumn[];

/**
 * Each column's own legend text, as the visible legend strip prints it.
 *
 * Copied from `OVERVIEW_COLUMNS` for the reason `ALL_COLUMNS` is, and guarded
 * the same way: `showAllColumns` asserts the rendered legend equals
 * `legendFor(ALL_COLUMNS)` exactly and in order, so a renamed or reordered
 * legend fails the run instead of being quietly accepted by a count.
 */
export const COLUMN_LEGEND: Record<OverviewColumn, string> = {
  status: "STATUS",
  displayName: "AGENT",
  lastActivityMs: "LAST ACTIVITY",
  spawnedAtMs: "UPTIME",
  cli: "CLI",
  activeTool: "ACTIVE TOOL",
  toolCount: "TOOLS",
  cwd: "WORKING DIRECTORY",
  lastUserPrompt: "LAST PROMPT",
};

/**
 * The legend a chosen set should produce: grid order, never without the
 * permanent column. It mirrors the app's `orderedColumns` rather than calling
 * it — the selection is a SET and the layout is a property of the screen, so a
 * spec that ticked columns in some order and expected them back in that order
 * would be asserting the wrong contract.
 */
export function legendFor(columns: Iterable<OverviewColumn>): string[] {
  const chosen = new Set<string>([...columns, PERMANENT_COLUMN]);
  return ALL_COLUMNS.filter((column) => chosen.has(column)).map((column) => COLUMN_LEGEND[column]);
}

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
  await chooseColumns(page, ALL_COLUMNS);
}

/**
 * Leave the picker showing exactly `wanted` — plus the permanent column, which
 * has no route out — then close it and wait for the layout to take the new
 * template.
 *
 * Every tick and untick is a real click on a real checkbox, so the picker's own
 * event handling is exercised rather than bypassed by writing state.
 */
export async function chooseColumns(page: Page, wanted: Iterable<OverviewColumn>): Promise<void> {
  const chosen = [...wanted];
  const target = new Set<string>(chosen);
  const menu = await openColumnPicker(page);

  for (const column of ALL_COLUMNS) {
    const checkbox = columnCheckbox(page, column);
    await expect(checkbox, `column "${column}" has no checkbox — the id list here has drifted from ALL_OVERVIEW_COLUMNS`).toBeAttached();
    if (column === PERMANENT_COLUMN) {
      // Permanent means `disabled`, and `check()` on a disabled input waits
      // until it times out. Assert the invariant instead of fighting it.
      await expect(checkbox).toBeChecked();
      continue;
    }
    const ticked = await checkbox.isChecked();
    if (ticked === target.has(column)) continue;
    if (ticked) await checkbox.uncheck();
    else await checkbox.check();
  }

  await page.keyboard.press("Escape");
  await expect(menu).toBeHidden();
  // The layout has actually taken the new template once the legend prints the
  // labels for the chosen set, in grid order. This is the state wait that
  // replaces "give React a moment" — without it the first `boundingBox()` can
  // read the previous grid. Asserting the TEXTS rather than a count is what
  // keeps `COLUMN_LEGEND` honest.
  await expect(legendLabels(page)).toHaveText(legendFor(chosen));
}

/** The picker's checkbox for one column. */
export function columnCheckbox(page: Page, column: OverviewColumn): Locator {
  return page.getByTestId(`overview-column-${column}`);
}

/** The picker's menu, whether or not it is currently mounted. */
export function columnsMenu(page: Page): Locator {
  return page.getByTestId("overview-columns-menu");
}

/** Click the picker's trigger and wait for the menu to be on screen. */
export async function openColumnPicker(page: Page): Promise<Locator> {
  await page.getByTestId("overview-columns-toggle").click();
  const menu = columnsMenu(page);
  await expect(menu).toBeVisible();
  return menu;
}

/** The visible legend strip — one label per column currently on screen. */
export function legendLabels(page: Page): Locator {
  return page.locator(".overview-legend span");
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
