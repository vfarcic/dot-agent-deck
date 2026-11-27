import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * PR #2491 decision 2026-11-25 — the Dashboard kept uncluttered, in a real
 * browser at a phone's width and a desktop's, in Chromium and WebKit:
 *
 * - the top bar's ⋯ ("More") and each daemon's ⋯ are reached with Tab, open
 *   with Enter on their first item, move with the arrow keys and close with
 *   Escape back onto ⋯, and their menus stay inside the window (110);
 * - Needs you only, from the top bar's ⋯, shows a chip beside Filter that
 *   turns it off in one press (111);
 * - in the Table view it keeps the rows that need you and the rows above
 *   them, and hides the rest (112);
 * - the Cards toolbar row is gone, so the first daemon's section sits
 *   directly under the top bar (111);
 * - a daemon's ⋯ opens Card actions titled with that daemon's name (113).
 *
 * The fixture scene `/?fixture=1&state=docs&cards=tree` serves a daemon whose
 * card tree has three agents that need you: tester (under prd-2374, under
 * issue-queue), issue-2310 (under issue-queue) and editor (under docs-site).
 */

const WIDTHS = [375, 1440] as const;
const TOLERANCE = 0.5;
const NEEDING_YOU_WITH_PARENTS = ["docs-site", "editor", "issue-2310", "issue-queue", "prd-2374", "tester"];

async function openScene(page: Page, mode: "cards" | "table"): Promise<void> {
  await page.addInitScript((view) => {
    try {
      window.localStorage.setItem("dot-agent-deck.overview.mode", view);
      window.localStorage.removeItem("dot-agent-deck.cards.open");
    } catch {
      // A blocked storage leaves the default view, which the checks below then report.
    }
  }, mode);
  await page.goto("/?fixture=1&state=docs&cards=tree");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  if (mode === "cards") await expect(page.getByTestId("agent-cards-tree")).toBeVisible();
  else await expect(page.getByRole("row", { name: "Open issue-queue agent" })).toBeVisible();
}

/** Press Tab from `start` until `target` has focus, failing after `limit` presses. */
async function tabTo(page: Page, start: Locator, target: Locator, limit = 80): Promise<void> {
  await start.focus();
  for (let at = 0; at < limit; at += 1) {
    if (await target.evaluate((node) => node === document.activeElement)) return;
    await page.keyboard.press("Tab");
  }
  await expect(target).toBeFocused();
}

/** The menu's box is inside the window, and the page does not scroll sideways. */
async function expectInsideWindow(page: Page, menu: Locator, width: number): Promise<void> {
  const box = await menu.boundingBox();
  expect(box).not.toBeNull();
  expect(box!.x).toBeGreaterThanOrEqual(-TOLERANCE);
  expect(box!.x + box!.width).toBeLessThanOrEqual(width + TOLERANCE);
  const sideways = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(sideways).toBeLessThanOrEqual(0);
}

const tableNames = async (page: Page) => (await page.getByRole("row", { name: /^Open .* agent$/ }).evaluateAll((rows) => rows.map((row) => row.getAttribute("aria-label")!.replace(/^Open (.*) agent$/, "$1")))).sort();

for (const width of WIDTHS) {
  test.describe(`at ${width}px`, () => {
    test.use({ viewport: { width, height: 900 } });

    /// Scenario: At this width, Tab from the Filter box to the top bar's ⋯ and press Enter, then Tab to the daemon's ⋯ and press Enter. Each menu opens with its first item focused and stays inside the window; ArrowDown moves through the items; Escape closes the menu and gives focus back to its ⋯.
    test(`desktop/agent-cards/110 — the ⋯ menus are keyboard reachable and fit the window at ${width}px`, async ({ page }) => {
      await openScene(page, "cards");
      const more = page.getByRole("button", { name: "More", exact: true });
      // From the Filter box: Columns and Refresh are hidden on a phone's width, and the ⋯ is not.
      await tabTo(page, page.getByTestId("dashboard-filter-text"), more, 6);
      await expect(more).toHaveAttribute("aria-haspopup", "menu");
      await page.keyboard.press("Enter");
      const menu = page.getByRole("menu", { name: "More", exact: true });
      await expect(menu).toBeVisible();
      await expect(more).toHaveAttribute("aria-expanded", "true");
      const item = menu.getByRole("menuitemcheckbox", { name: "Needs you only" });
      await expect(item).toBeFocused();
      await expect(item).toHaveAttribute("aria-checked", "false");
      await expectInsideWindow(page, menu, width);
      await page.keyboard.press("ArrowDown");
      await expect(item, "one item: ArrowDown wraps to it").toBeFocused();
      await page.keyboard.press("Escape");
      await expect(menu).toHaveCount(0);
      await expect(more).toBeFocused();

      const daemonMore = page.getByTestId("daemon-more").first();
      await tabTo(page, more, daemonMore);
      const name = await daemonMore.getAttribute("aria-label");
      expect(name).toMatch(/^More for /);
      await page.keyboard.press("Enter");
      const daemonMenu = page.getByRole("menu", { name: name! });
      await expect(daemonMenu).toBeVisible();
      await expect(daemonMenu.getByRole("menuitem", { name: "Card actions" })).toBeFocused();
      await expectInsideWindow(page, daemonMenu, width);
      await page.keyboard.press("Escape");
      await expect(daemonMenu).toHaveCount(0);
      await expect(daemonMore).toBeFocused();

      // Enter on Card actions opens the editor bound to that daemon, titled with its name.
      await page.keyboard.press("ArrowDown");
      await expect(daemonMenu.getByRole("menuitem", { name: "Card actions" })).toBeFocused();
      await page.keyboard.press("Enter");
      const editor = page.getByRole("dialog", { name: `Card actions: ${name!.replace(/^More for /, "")}` });
      await expect(editor).toBeVisible();
      await expect(editor.getByRole("combobox", { name: "Daemon" })).toHaveCount(0);
      await expectInsideWindow(page, editor, width);
    });

    /// Scenario: At this width, on the Cards view, look under the top bar, then turn Needs you only on from the top bar's ⋯ and press the chip that appears beside Filter. No line of counts or buttons sits above the cards — the daemon's section starts right under the top bar; the chip appears only while the filter is on and stays inside the window, and one press on it turns the filter off and removes it.
    test(`desktop/agent-cards/111 — no Cards toolbar row, and the Needs you only chip turns the filter off at ${width}px`, async ({ page }) => {
      await openScene(page, "cards");
      await expect(page.locator(".agent-cards-toolbar")).toHaveCount(0);
      await expect(page.getByText(/\d+ agents? · \d+ needs? you/)).toHaveCount(0);
      const gap = await page.evaluate(() => {
        const body = document.querySelector(".overview-body")!.getBoundingClientRect();
        const first = document.querySelector("[data-testid='daemon-group']")!.getBoundingClientRect();
        return first.top - body.top;
      });
      expect(gap, "the first daemon's section is the first thing in the dashboard").toBeLessThanOrEqual(24);
      await expect(page.getByTestId("overview-needs-you-count")).toHaveText("3");
      await expect(page.getByTestId("overview-needs-you-count"), "the count on the ⋯ is drawn at every width").toBeVisible();

      await expect(page.getByTestId("needs-you-chip")).toHaveCount(0);
      await page.getByRole("button", { name: "More", exact: true }).click();
      await page.getByRole("menuitemcheckbox", { name: "Needs you only" }).click();
      const chip = page.getByTestId("needs-you-chip");
      await expect(chip).toBeVisible();
      await expect(chip).toHaveText("Needs you only");
      await expectInsideWindow(page, chip, width);
      await expect(page.locator("article[data-faded='true']").first()).toBeVisible();
      await chip.click();
      await expect(chip).toHaveCount(0);
      await expect(page.locator("article[data-faded='true']")).toHaveCount(0);
      await page.getByRole("button", { name: "More", exact: true }).click();
      await expect(page.getByRole("menuitemcheckbox", { name: "Needs you only" })).toHaveAttribute("aria-checked", "false");
    });

    /// Scenario: At this width, on the Table view, turn Needs you only on from the top bar's ⋯. The view stays the Table, and it lists only the three agents that need you and the agents above them — issue-queue, prd-2374, tester, issue-2310, docs-site and editor; pressing the chip lists every agent again.
    test(`desktop/agent-cards/112 — Needs you only in the Table keeps the rows that need you and their parents at ${width}px`, async ({ page }) => {
      await openScene(page, "table");
      const every = await tableNames(page);
      expect(every.length).toBeGreaterThan(NEEDING_YOU_WITH_PARENTS.length);
      await page.getByRole("button", { name: "More", exact: true }).click();
      await page.getByRole("menuitemcheckbox", { name: "Needs you only" }).click();
      await expect(page.getByTestId("overview-mode-table")).toHaveAttribute("aria-checked", "true");
      await expect.poll(() => tableNames(page)).toEqual(NEEDING_YOU_WITH_PARENTS);
      await page.getByTestId("needs-you-chip").click();
      await expect.poll(() => tableNames(page)).toEqual(every);
    });

    /// Scenario: At this width, open the `state=docs` scene, whose daemon reports no cards and whose agent User-path verification needs input, and turn Needs you only on from the top bar's ⋯ on the Table view. Every row stays, the agent that needs input among them; the dashboard never says "Nothing needs you", and its note says no card needs you and that the daemon's agents are shown as usual because it does not report cards.
    test(`desktop/agent-cards/112 — with a daemon that reports no cards, Needs you only does not say nothing needs you at ${width}px`, async ({ page }) => {
      await page.addInitScript(() => {
        try {
          window.localStorage.setItem("dot-agent-deck.overview.mode", "table");
        } catch {
          // A blocked storage leaves the default view, which is the Table.
        }
      });
      await page.goto("/?fixture=1&state=docs");
      await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
      await page.getByTestId("open-overview").click();
      const asking = page.getByRole("row", { name: "Open User-path verification agent" });
      await expect(asking).toBeVisible();
      const every = await tableNames(page);
      await page.getByRole("button", { name: "More", exact: true }).click();
      await page.getByRole("menuitemcheckbox", { name: "Needs you only" }).click();
      await expect(page.getByTestId("needs-you-chip")).toBeVisible();
      const note = page.getByTestId("agent-cards-none-need-you-scoped");
      await expect(note).toBeVisible();
      await expect(note).toHaveText(/^No card needs you\. Agents on .+ are shown as usual: that daemon does not report cards\.$/);
      await expect(page.getByText("Nothing needs you", { exact: false })).toHaveCount(0);
      expect(await tableNames(page), "a daemon without cards keeps its rows").toEqual(every);
      await expect(asking, "the agent that needs input is still listed").toBeVisible();
      await expect(page.getByTestId("overview-needs-you-count")).toHaveCount(0);
    });
  });
}
