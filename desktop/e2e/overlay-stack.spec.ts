import { expect, type Locator, type Page } from "@playwright/test";
import { test } from "./support/load-budget";
import { enterDeck, selectOverview } from "./support/overview";

/**
 * PR #1680 review (Qodo, findings 14 and 15) — what is drawn on top is what
 * takes input, and what voice's "close" closes, asked of the real stylesheet.
 *
 * The overlay stack in `useInertBackground` orders the app's overlays for the
 * fence, for voice's "close" and for the pull request browser's "covered".
 * jsdom paints nothing, so the vitest tier could not see that the stack and the
 * screen disagreed: two dialogs on one CSS layer, ordered by when they opened
 * while the browser paints them in document order, and a sheet chosen from the
 * command menu raised over a deck that had nothing fenced up. Every assertion
 * here is about what the browser draws at a point (`elementFromPoint`) and what
 * a real click or key does there, never about a class name or a layer number.
 */

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** Load the fixture with speech on, saying `script` in order (see `voice-commands.spec.ts`). */
async function load(page: Page, state: string, script: string[] = []) {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  const spoken = script.map((phrase) => `&voice=${encodeURIComponent(phrase)}`).join("");
  await page.goto(`/?fixture=1&experimental=1&state=${state}${spoken}`);
}

/** Whether `locator` is inside an `inert` subtree: out of hit testing and the tab order. */
function inert(locator: Locator): Promise<boolean> {
  return locator.evaluate((node) => node.closest("[inert]") !== null);
}

/**
 * Whether what the browser PAINTS at the centre of `locator` is `locator` or
 * inside it.
 *
 * `elementFromPoint` hit-tests, and an inert element is not hit-testable, so on
 * its own it answers "what would a click reach" — which is what the fence
 * decides, and so cannot check the fence. Measured in Chromium: a dialog drawn
 * in front but inert is skipped, and the dialog behind it is returned. So the
 * attribute is lifted for one synchronous probe and put back before anything
 * can render, leaving the answer to the stylesheet alone; whether the element
 * then takes input is asked separately, with {@link inert} and a real click.
 */
function drawnOnTop(locator: Locator): Promise<boolean> {
  return locator.evaluate((node) => {
    const box = node.getBoundingClientRect();
    const marked = Array.from(document.querySelectorAll("[inert]"));
    for (const element of marked) element.removeAttribute("inert");
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    for (const element of marked) element.setAttribute("inert", "");
    return hit !== null && node.contains(hit);
  });
}

/**
 * Turn voice on from the keyboard, so the scripted microphone says its next
 * line. A dialog's scrim covers the voice row, so a pointer cannot reach the
 * trigger while one is up; the trigger is the one control outside a modal that
 * stays in the tab order, which is how a keyboard user reaches it.
 */
async function pressVoice(page: Page) {
  await page.getByTestId("voice-trigger").press("Enter");
}

const settings = (page: Page) => page.getByRole("dialog", { name: "Settings" });
const pane = (page: Page) => page.getByTestId("agent-pane-overlay");
const commandMenu = (page: Page) => page.getByRole("dialog", { name: "Command menu" });
const shortcutGuide = (page: Page) => page.getByRole("dialog", { name: "Control keys" });

/** Open Settings from the deck's command menu (Ctrl+K). */
async function settingsFromCommandMenu(page: Page) {
  await page.keyboard.press("Control+k");
  await commandMenu(page).getByRole("button", { name: /Open settings/ }).click();
  await expect(settings(page)).toBeVisible();
  await expect(commandMenu(page)).toHaveCount(0);
}

test.describe("a sheet chosen from the command menu on a plain deck", () => {
  /**
   * Scenario: on the deck with nothing open, press Ctrl+K and choose Open
   * settings. The deck under Settings stays live, and Settings closes with its
   * Close button and, opened the same way again, with Escape.
   */
  test("leaves the deck live, and closes with Close or Escape", async ({ page }) => {
    await load(page, "connected");
    await enterDeck(page);
    const openPlanner = page.getByRole("button", { name: "Open Planner agent" });

    await settingsFromCommandMenu(page);
    expect(await inert(openPlanner), "Settings opened over nothing fenced must not fence the deck").toBe(false);
    expect(await drawnOnTop(settings(page).getByRole("button", { name: "Close settings" }))).toBe(true);
    await settings(page).getByRole("button", { name: "Close settings" }).click();
    await expect(settings(page)).toHaveCount(0);

    await settingsFromCommandMenu(page);
    expect(await inert(openPlanner)).toBe(false);
    await page.keyboard.press("Escape");
    await expect(settings(page)).toHaveCount(0);
    expect(await inert(openPlanner)).toBe(false);
  });

  /**
   * Scenario: on the deck, open Settings from Ctrl+K, then open Planner's pane
   * from the keyboard. The pane comes up drawn over Settings and takes clicks:
   * its Back to dashboard button closes it, and Settings is still there under
   * it.
   */
  test("puts an agent pane opened afterwards on top, where it takes clicks", async ({ page }) => {
    await load(page, "connected");
    await enterDeck(page);
    await settingsFromCommandMenu(page);

    await page.getByRole("button", { name: "Open Planner agent" }).press("Enter");
    await expect(pane(page)).toBeVisible();
    const back = pane(page).getByRole("button", { name: "Back to dashboard" });
    expect(await inert(pane(page)), "the pane opened last must take input").toBe(false);
    expect(await drawnOnTop(back), "the pane opened last must be drawn over Settings").toBe(true);

    await back.click();
    await expect(pane(page)).toHaveCount(0);
    await expect(settings(page)).toBeVisible();
  });
});

test.describe("the shortcut guide and the command menu together", () => {
  /** Which of the two dialogs the browser draws where they overlap. */
  async function drawnWhereTheyOverlap(page: Page): Promise<"Command menu" | "Control keys"> {
    const name = await page.evaluate(() => {
      const menu = document.querySelector("[role='dialog'][aria-label='Command menu']")!.getBoundingClientRect();
      const guide = document.querySelector("[role='dialog'][aria-labelledby='shortcut-title']")!.getBoundingClientRect();
      const left = Math.max(menu.left, guide.left);
      const right = Math.min(menu.right, guide.right);
      const top = Math.max(menu.top, guide.top);
      const bottom = Math.min(menu.bottom, guide.bottom);
      if (left >= right || top >= bottom) return "no overlap";
      // Painted, not hit-tested: see `drawnOnTop`.
      const marked = Array.from(document.querySelectorAll("[inert]"));
      for (const element of marked) element.removeAttribute("inert");
      const hit = document.elementFromPoint((left + right) / 2, (top + bottom) / 2);
      for (const element of marked) element.setAttribute("inert", "");
      const dialog = hit?.closest("[role='dialog']");
      return dialog?.getAttribute("aria-label") ?? (dialog?.getAttribute("aria-labelledby") === "shortcut-title" ? "Control keys" : "neither");
    });
    expect(["Command menu", "Control keys"], `drawn where the two overlap: ${name}`).toContain(name);
    return name as "Command menu" | "Control keys";
  }

  async function guideThenMenu(page: Page) {
    await page.keyboard.press("?");
    await expect(shortcutGuide(page)).toBeVisible();
    await page.keyboard.press("Control+k");
    await expect(commandMenu(page)).toBeVisible();
  }

  /**
   * Scenario: on the deck press ?, then Ctrl+K. Whichever of the shortcut
   * guide and the command menu is drawn in front takes input, and a real click
   * on its own control works; Escape then closes it.
   */
  test("the one drawn in front takes clicks, and Escape closes it", async ({ page }) => {
    await load(page, "connected");
    await enterDeck(page);
    await guideThenMenu(page);

    const front = await drawnWhereTheyOverlap(page);
    const dialog = front === "Command menu" ? commandMenu(page) : shortcutGuide(page);
    expect(await inert(dialog), `${front} is drawn in front and must take input`).toBe(false);
    if (front === "Control keys") {
      const close = shortcutGuide(page).getByRole("button", { name: "Close shortcut guide" });
      expect(await drawnOnTop(close)).toBe(true);
      await close.click();
      await expect(shortcutGuide(page)).toHaveCount(0);
      await expect(commandMenu(page)).toBeVisible();
      expect(await inert(commandMenu(page)), "the menu left on screen takes input").toBe(false);
    } else {
      const entry = commandMenu(page).getByRole("button", { name: /Open settings/ });
      expect(await drawnOnTop(entry)).toBe(true);
      await entry.click();
      await expect(settings(page)).toBeVisible();
    }

    // Ctrl+K toggles the menu, so start the second round from a clear deck.
    await page.keyboard.press("Escape");
    await expect(commandMenu(page)).toHaveCount(0);
    await expect(shortcutGuide(page)).toHaveCount(0);
    await expect(settings(page)).toHaveCount(0);
    await guideThenMenu(page);
    const again = await drawnWhereTheyOverlap(page);
    await page.keyboard.press("Escape");
    await expect(again === "Command menu" ? commandMenu(page) : shortcutGuide(page)).toHaveCount(0);
  });

  /**
   * Scenario: on the deck press ?, then Ctrl+K, and say "close this". The
   * dialog drawn in front closes, and the one behind it stays.
   */
  test("voice's close closes the one drawn in front", async ({ page }) => {
    await load(page, "connected", ["close this"]);
    await enterDeck(page);
    await guideThenMenu(page);
    const front = await drawnWhereTheyOverlap(page);

    await pressVoice(page);
    await expect(page.getByText("Closed.")).toBeVisible();
    const [closed, kept] = front === "Command menu" ? [commandMenu(page), shortcutGuide(page)] : [shortcutGuide(page), commandMenu(page)];
    await expect(closed, `${front} was drawn in front`).toHaveCount(0);
    await expect(kept).toBeVisible();
  });
});

test.describe("sheets, the agent pane and the pull request page", () => {
  /**
   * Scenario: Qodo's earlier case. On the dashboard open Settings, then the
   * agent's pane from the keyboard, then its pull request from the pane's
   * badge. The page is drawn on top; "close this" closes the page, then the
   * pane, then Settings, one each time it is said.
   */
  test("close closes the page, then the pane, then Settings", async ({ page }) => {
    await load(page, "docs-pr", ["close this", "voice off", "close this", "voice off", "close this", "voice off"]);
    await selectOverview(page);
    await page.getByRole("complementary", { name: "Primary navigation" }).getByRole("button", { name: "Settings" }).click();
    await expect(settings(page)).toBeVisible();
    await page.getByRole("button", { name: "Open Desktop implementation agent" }).press("Enter");
    await expect(pane(page)).toBeVisible();
    await pane(page).getByTestId("pr-badge-2").click();
    const browser = page.getByTestId("pr-browser");
    await expect(browser).toBeVisible();
    expect(await drawnOnTop(browser.getByRole("button", { name: "Close pull request" })), "the page is drawn on top").toBe(true);

    const trigger = page.getByTestId("voice-trigger");
    await pressVoice(page);
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
    await expect(browser).toHaveCount(0);
    await expect(pane(page)).toBeVisible();
    await expect(settings(page)).toBeVisible();
    expect(await drawnOnTop(pane(page).getByRole("button", { name: "Back to dashboard" })), "the pane is drawn over Settings").toBe(true);

    await pressVoice(page);
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
    await expect(pane(page)).toHaveCount(0);
    await expect(settings(page)).toBeVisible();

    await pressVoice(page);
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
    await expect(settings(page)).toHaveCount(0);
  });

  /**
   * Scenario: on the deck open the agent's pane and its pull request, then
   * press Ctrl+K and choose Open settings. Settings is drawn over the page and
   * takes clicks; "close this" closes Settings alone, and the page and the
   * pane are still there.
   */
  test("a sheet opened over the page from the command menu is on top, and close closes only it", async ({ page }) => {
    await load(page, "docs-pr", ["close this"]);
    await enterDeck(page);
    await page.getByRole("button", { name: "Open Codex agent" }).click();
    await expect(pane(page)).toBeVisible();
    await pane(page).getByTestId("pr-badge-2").click();
    const browser = page.getByTestId("pr-browser");
    await expect(browser).toBeVisible();

    await settingsFromCommandMenu(page);
    const sections = settings(page).getByRole("tab");
    expect(await inert(settings(page)), "Settings opened over the page takes input").toBe(false);
    expect(await drawnOnTop(settings(page).getByRole("button", { name: "Close settings" })), "Settings is drawn over the page").toBe(true);
    if (await sections.count() > 1) {
      await sections.nth(1).click();
      await expect(sections.nth(1)).toHaveAttribute("aria-selected", "true");
    }

    await pressVoice(page);
    await expect(page.getByText("Closed.")).toBeVisible();
    await expect(settings(page)).toHaveCount(0);
    await expect(browser).toBeVisible();
    await expect(pane(page)).toBeVisible();
    expect(await drawnOnTop(browser.getByRole("button", { name: "Close pull request" }))).toBe(true);
  });
});
