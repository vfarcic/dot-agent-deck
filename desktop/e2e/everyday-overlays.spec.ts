import { expect, type Locator, type Page } from "@playwright/test";
import { test } from "./support/load-budget";
import { enterDeck, openOverview } from "./support/overview";

/**
 * PR #1680 review — the everyday overlays, none of which involves the pull
 * request browser, each opened on its own the way a user opens it: the New
 * agent dialog, Settings from its rail button, the shortcut guide, the command
 * menu, and the agent's pane. The overlay stack in `useInertBackground` was
 * reordered (by paint order) and a sheet's raise is now read from the stack,
 * so this asks the real stylesheet that each one still comes up in front,
 * takes input, closes, and gives the screen behind it back — what each does on
 * `main`. It asserts what a user sees and can click, not how it is done, and
 * passes against `main` as it stands: so it says nothing about whether the
 * shortcut guide and the command menu fence the deck behind their scrim, which
 * they do on this branch (they joined the stack earlier in PR #1680) and do
 * not on `main`.
 */

/** Whether `locator` is inside an `inert` subtree. */
function inert(locator: Locator): Promise<boolean> {
  return locator.evaluate((node) => node.closest("[inert]") !== null);
}

/** Whether a click at the centre of `locator` reaches it (see `overlay-stack.spec.ts` for why hit-testing is the question here). */
function clickReaches(locator: Locator): Promise<boolean> {
  return locator.evaluate((node) => {
    const box = node.getBoundingClientRect();
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return hit !== null && node.contains(hit);
  });
}

async function onDeck(page: Page) {
  await page.goto("/?fixture=1&experimental=1&state=connected");
  await enterDeck(page);
}

const deckControl = (page: Page) => page.getByRole("button", { name: "Open Planner agent" });

test.describe("the New agent dialog on the dashboard", () => {
  /**
   * Scenario: on the dashboard click New agent. The dialog is in front and
   * takes clicks, the rail behind it does not, and its Close button closes it
   * and gives the rail back.
   */
  test("New agent", async ({ page }) => {
    await openOverview(page, "fleet");
    const rail = page.getByRole("complementary", { name: "Primary navigation" }).getByRole("button", { name: "Settings" });
    await page.getByTestId("overview-new-agent").click();
    const dialog = page.getByTestId("new-agent-dialog");
    await expect(dialog).toBeVisible();
    const close = dialog.getByRole("button", { name: "Close new agent" });
    expect(await inert(dialog)).toBe(false);
    expect(await clickReaches(close)).toBe(true);
    expect(await inert(rail)).toBe(true);
    await close.click();
    await expect(dialog).toHaveCount(0);
    expect(await inert(rail)).toBe(false);
  });
});

test.describe("everyday overlays on the deck", () => {
  /**
   * Scenario: on the deck click Settings in the rail. Settings is in front and
   * takes clicks, the deck is not fenced, and its Close button closes it.
   */
  test("Settings from its button", async ({ page }) => {
    await onDeck(page);
    await page.getByRole("complementary", { name: "Primary navigation" }).getByRole("button", { name: "Settings" }).click();
    const settings = page.getByRole("dialog", { name: "Settings" });
    await expect(settings).toBeVisible();
    const close = settings.getByRole("button", { name: "Close settings" });
    expect(await inert(settings)).toBe(false);
    expect(await clickReaches(close)).toBe(true);
    expect(await inert(deckControl(page))).toBe(false);
    await close.click();
    await expect(settings).toHaveCount(0);
  });

  /**
   * Scenario: on the deck press ?. The shortcut guide is in front and takes
   * clicks, and its Close button closes it and gives the deck back.
   */
  test("the shortcut guide", async ({ page }) => {
    await onDeck(page);
    await page.keyboard.press("?");
    const guide = page.getByRole("dialog", { name: "Control keys" });
    await expect(guide).toBeVisible();
    const close = guide.getByRole("button", { name: "Close shortcut guide" });
    expect(await clickReaches(close)).toBe(true);
    await close.click();
    await expect(guide).toHaveCount(0);
    expect(await inert(deckControl(page))).toBe(false);
  });

  /**
   * Scenario: on the deck press Ctrl+K. The command menu is in front, takes
   * typing and clicks, and Escape closes it and gives the deck back.
   */
  test("the command menu", async ({ page }) => {
    await onDeck(page);
    await page.keyboard.press("Control+k");
    const menu = page.getByRole("dialog", { name: "Command menu" });
    await expect(menu).toBeVisible();
    await page.keyboard.type("settings");
    await expect(menu.getByRole("button", { name: /Open settings/ })).toBeVisible();
    expect(await clickReaches(menu.getByRole("button", { name: /Open settings/ }))).toBe(true);
    await page.keyboard.press("Escape");
    await expect(menu).toHaveCount(0);
    expect(await inert(deckControl(page))).toBe(false);
  });

  /**
   * Scenario: on the deck open Planner's pane. It is in front and takes
   * clicks, the deck selector behind it does not, and its Back to dashboard
   * button closes it and gives the deck back.
   */
  test("the agent pane opens and closes", async ({ page }) => {
    await onDeck(page);
    await deckControl(page).click();
    const pane = page.getByTestId("agent-pane-overlay");
    await expect(pane).toBeVisible();
    const back = pane.getByRole("button", { name: "Back to dashboard" });
    expect(await clickReaches(back)).toBe(true);
    expect(await inert(page.getByTestId("deck-selector-toggle"))).toBe(true);
    await back.click();
    await expect(pane).toHaveCount(0);
    expect(await inert(deckControl(page))).toBe(false);
  });
});
