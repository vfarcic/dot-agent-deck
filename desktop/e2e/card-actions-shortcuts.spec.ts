import { expect, test, type Locator, type Page } from "@playwright/test";

/**
 * PR #2491 (the maintainer, 2026-11-25) — the shortcuts to the Card actions
 * editor, in a real browser at a phone's width and a desktop's, in Chromium
 * and WebKit:
 *
 * - the selection bar's Edit card actions, after the batch actions and apart
 *   from them, opens the editor of the one daemon the selected cards are on
 *   (116), and with the selection across daemons opens a menu of them (117);
 * - the Card tab's Edit, beside its Actions heading, opens the editor of
 *   the agent's daemon over the pane (118).
 *
 * The fixture scene `/?fixture=1&state=docs-fleet&cards=fleet` shows two
 * daemons whose cards do batches: the local one's four agents and the build
 * box's two. Both advertise the editor.
 */

const WIDTHS = [375, 1440] as const;
const TOLERANCE = 0.5;

async function openScene(page: Page): Promise<void> {
  await page.addInitScript(() => {
    try {
      window.localStorage.setItem("dot-agent-deck.overview.mode", "cards");
    } catch {
      // A blocked storage leaves the default view, which the checks below then report.
    }
  });
  await page.goto("/?fixture=1&state=docs-fleet&cards=fleet");
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.getByRole("checkbox", { name: "Select API review" })).toBeVisible();
}

/** The names the dashboard gives its daemons, in its order. */
async function daemonNames(page: Page): Promise<string[]> {
  const labels = await page.getByTestId("daemon-more").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("aria-label") ?? ""));
  return labels.map((label) => label.replace(/^More for /, ""));
}

/** The box is inside the window, and the page does not scroll sideways. */
async function expectInsideWindow(page: Page, node: Locator, width: number): Promise<void> {
  const box = await node.boundingBox();
  expect(box).not.toBeNull();
  expect(box!.x).toBeGreaterThanOrEqual(-TOLERANCE);
  expect(box!.x + box!.width).toBeLessThanOrEqual(width + TOLERANCE);
  const sideways = await page.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  expect(sideways).toBeLessThanOrEqual(0);
}

/** Press Tab from `start` until `target` has focus, failing after `limit` presses. */
async function tabTo(page: Page, start: Locator, target: Locator, limit = 12): Promise<void> {
  await start.focus();
  for (let at = 0; at < limit; at += 1) {
    if (await target.evaluate((node) => node === document.activeElement)) return;
    await page.keyboard.press("Tab");
  }
  await expect(target).toBeFocused();
}

for (const width of WIDTHS) {
  test.describe(`at ${width}px`, () => {
    test.use({ viewport: { width, height: 900 } });

    /// Scenario: At this width, tick two cards on the local daemon. The selection bar shows Edit card actions after its action icons, set apart from them and inside the window; Tab reaches it from the last action, Enter opens the local daemon's Card actions editor inside the window, and Escape closes it.
    test(`desktop/agent-cards/116 — a one-daemon selection opens that daemon's editor at ${width}px`, async ({ page }) => {
      await openScene(page);
      const [local] = await daemonNames(page);
      await page.getByRole("checkbox", { name: "Select Plan / architecture" }).check();
      await page.getByRole("checkbox", { name: "Select User-path verification" }).check();
      const bar = page.getByTestId("agent-cards-selection-bar");
      const edit = bar.getByRole("button", { name: "Edit card actions" });
      await expect(edit).toBeVisible();
      await expect(edit).toHaveAttribute("title", "Edit card actions");
      await expect(edit).not.toHaveAttribute("aria-haspopup", /.*/);
      await expectInsideWindow(page, edit, width);
      // Apart from the action group: a visible gap wider than the gap between two actions.
      const group = bar.getByTestId("action-icon-group");
      const [groupBox, editBox] = [await group.boundingBox(), await edit.boundingBox()];
      const sameLine = Math.abs(groupBox!.y + groupBox!.height / 2 - (editBox!.y + editBox!.height / 2)) < 4;
      if (sameLine) expect(editBox!.x - (groupBox!.x + groupBox!.width)).toBeGreaterThanOrEqual(10);

      const actions = group.getByRole("button");
      await tabTo(page, actions.last(), edit);
      await page.keyboard.press("Enter");
      const editor = page.getByRole("dialog", { name: `Card actions: ${local}` });
      await expect(editor).toBeVisible();
      await expectInsideWindow(page, editor, width);
      await page.keyboard.press("Escape");
      await expect(editor).toHaveCount(0);
    });

    /// Scenario: At this width, tick a card on the local daemon and one on the build box. The selection bar's Edit card actions is a menu button; Enter opens a menu named "Edit card actions" listing both daemons with the first focused, inside the window; ArrowDown and Enter choose the build box, which closes the menu and opens its Card actions editor.
    test(`desktop/agent-cards/117 — a two-daemon selection shows a menu of the daemons at ${width}px`, async ({ page }) => {
      await openScene(page);
      const [local, remote] = await daemonNames(page);
      await page.getByRole("checkbox", { name: "Select Plan / architecture" }).check();
      await page.getByRole("checkbox", { name: "Select API review" }).check();
      const bar = page.getByTestId("agent-cards-selection-bar");
      const edit = bar.getByRole("button", { name: "Edit card actions" });
      await expect(edit).toHaveAttribute("aria-haspopup", "menu");
      await expect(edit).toHaveAttribute("aria-expanded", "false");
      await edit.focus();
      await page.keyboard.press("Enter");
      const menu = page.getByRole("menu", { name: "Edit card actions" });
      await expect(menu).toBeVisible();
      await expect(edit).toHaveAttribute("aria-expanded", "true");
      await expect(menu.getByRole("menuitem")).toHaveText([local, remote]);
      await expect(menu.getByRole("menuitem", { name: local })).toBeFocused();
      await expectInsideWindow(page, menu, width);
      await page.keyboard.press("ArrowDown");
      await expect(menu.getByRole("menuitem", { name: remote })).toBeFocused();
      await page.keyboard.press("Enter");
      await expect(menu).toHaveCount(0);
      const editor = page.getByRole("dialog", { name: `Card actions: ${remote}` });
      await expect(editor).toBeVisible();
      await expectInsideWindow(page, editor, width);
    });

    /// Scenario: At this width, open Plan / architecture's pane from its card. The Card tab's Actions heading has a small Edit button beside it, named "Edit card actions"; Shift+Tab reaches it from the panel's first action icon and Enter opens the local daemon's Card actions editor over the pane, inside the window.
    test(`desktop/agent-cards/118 — the Card tab's Edit opens its daemon's editor at ${width}px`, async ({ page }) => {
      await openScene(page);
      const [local] = await daemonNames(page);
      await page.locator("article[data-card-key]").filter({ hasText: "Plan / architecture" }).getByTestId("agent-card-details").click();
      await page.getByTestId("agent-pane-overlay").getByRole("tab", { name: /^Card/ }).click();
      const panel = page.getByTestId("agent-pane-overlay").getByTestId("agent-card-panel");
      await expect(panel).toBeVisible();
      const actions = panel.getByTestId("agent-card-panel-actions");
      const edit = actions.getByRole("button", { name: "Edit card actions" });
      await edit.scrollIntoViewIfNeeded();
      await expect(edit).toBeVisible();
      await expect(edit).toHaveText("Edit");
      const [heading, button] = [await actions.getByRole("heading", { name: /^Actions/ }).boundingBox(), await edit.boundingBox()];
      expect(Math.abs(heading!.y + heading!.height / 2 - (button!.y + button!.height / 2)), "on the heading's line").toBeLessThan(8);
      // Next in the keyboard order before the action icons it sits above.
      await actions.getByTestId("action-icon-group").getByRole("button").first().focus();
      await page.keyboard.press("Shift+Tab");
      await expect(edit).toBeFocused();
      await page.keyboard.press("Enter");
      const editor = page.getByRole("dialog", { name: `Card actions: ${local}` });
      await expect(editor).toBeVisible();
      await expectInsideWindow(page, editor, width);
    });
  });
}
