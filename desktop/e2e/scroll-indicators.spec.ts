import { expect, test, type Page } from "@playwright/test";
import { selectOverview } from "./support/overview";

/**
 * Issue #1492 — the dashboard and an agent's output are each one page taller
 * than the window, and the scrollbar is the only thing that says so. Left to
 * the system and to xterm, both scrollbars showed only while the user was
 * scrolling, so a dashboard or a terminal with more above or below looked
 * complete. These check the real geometry: each indicator is drawn at rest
 * when there is more to scroll, and not drawn when there is not.
 */

/** Width the dashboard's vertical scrollbar takes: 0 when none is drawn at rest. */
function dashboardScrollbarWidth(page: Page): Promise<number> {
  return page.locator(".overview-body").evaluate((region) => (region as HTMLElement).offsetWidth - region.clientWidth);
}

/** Whether the dashboard's region has more daemons than it shows. */
function dashboardOverflows(page: Page): Promise<boolean> {
  return page.locator(".overview-body").evaluate((region) => region.scrollHeight > region.clientHeight + 1);
}

/** The open agent pane's terminal scrollbar, read after xterm's fade has had time to run. */
async function terminalScrollbar(page: Page) {
  const overlay = page.getByTestId("agent-pane-overlay");
  await expect(overlay.locator(".xterm-screen")).toBeVisible();
  // Away from the terminal, and past xterm's 800ms fade-out.
  await page.mouse.move(2, 2);
  await page.waitForTimeout(1_200);
  return overlay.evaluate((root) => {
    const bar = root.querySelector<HTMLElement>(".xterm-scrollable-element > .scrollbar.vertical");
    const slider = bar?.querySelector<HTMLElement>(".slider");
    if (!bar || !slider) throw new Error("the terminal has no vertical scrollbar element");
    return { opacity: Number(getComputedStyle(bar).opacity), slider: slider.getBoundingClientRect().height, track: bar.getBoundingClientRect().height };
  });
}

async function openDashboard(page: Page, height: number) {
  await page.setViewportSize({ width: 1280, height });
  await page.goto("/?fixture=1&state=docs-fleet");
  await selectOverview(page);
}

test.describe("scroll indicators", () => {
  /** Scenario: a dashboard taller than a short window shows its scrollbar without the user scrolling, and the window itself does not scroll; in a window tall enough for the whole dashboard, no scrollbar is drawn. */
  test("the dashboard's scrollbar is shown at rest only when there is more to scroll", async ({ page, browserName }) => {
    // Playwright launches headless Chromium with `--hide-scrollbars`, so it
    // draws no scrollbar whatever the CSS says. Lifting the switch would
    // change the layout every other Chromium spec measures; WebKit, the engine
    // the macOS and Linux app runs on, checks this one.
    test.skip(browserName === "chromium", "headless Chromium runs with --hide-scrollbars");
    await openDashboard(page, 420);
    expect(await dashboardOverflows(page)).toBe(true);
    expect(await dashboardScrollbarWidth(page)).toBeGreaterThan(0);
    expect(await page.evaluate(() => document.documentElement.scrollHeight <= innerHeight)).toBe(true);

    await page.setViewportSize({ width: 1280, height: 1200 });
    expect(await dashboardOverflows(page)).toBe(false);
    expect(await dashboardScrollbarWidth(page)).toBe(0);
  });

  /** Scenario: an agent pane whose output is taller than its terminal shows the terminal's scrollbar while the pointer is elsewhere; a pane whose output fits shows none. */
  test("an agent terminal's scrollbar is shown at rest only when it has output to scroll", async ({ page }) => {
    await openDashboard(page, 420);
    await page.getByRole("button", { name: "Open Desktop implementation agent" }).click();
    const short = await terminalScrollbar(page);
    expect(short.slider).toBeLessThan(short.track);
    expect(short.opacity).toBe(1);

    await page.getByRole("button", { name: "Back to dashboard" }).click();
    await page.setViewportSize({ width: 1280, height: 1200 });
    await page.getByRole("button", { name: "Open Desktop implementation agent" }).click();
    const tall = await terminalScrollbar(page);
    expect(tall.opacity).toBe(0);
  });

  /** Scenario: the dashboard scrolls from the keyboard as the window did: Page Down, End and Page Up with nothing focused or with a top-bar button focused, and the arrow keys once the dashboard itself has focus. Keys pressed inside the Settings sheet or typed into the New agent dialog's Filter field leave it where it is. */
  test("the dashboard scrolls from the keyboard", async ({ page }) => {
    await openDashboard(page, 420);
    const region = page.locator(".overview-body");
    const top = () => region.evaluate((element) => element.scrollTop);
    // Page and Home/End scroll smoothly: the next key is pressed once the last one has finished moving it.
    const settled = async () => {
      let last = -1;
      await expect.poll(async () => { const now = await top(); const still = now === last; last = now; return still; }, { intervals: [150] }).toBe(true);
    };
    const room = await region.evaluate((element) => element.scrollHeight - element.clientHeight);
    expect(room).toBeGreaterThan(0);

    await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
    await page.keyboard.press("PageDown");
    await expect.poll(top).toBeGreaterThan(0);
    await settled();
    await page.keyboard.press("End");
    await expect.poll(top).toBeGreaterThanOrEqual(room - 1);
    await settled();
    await page.keyboard.press("Home");
    await expect.poll(top).toBe(0);

    await settled();
    await page.getByTestId("overview-refresh").focus();
    await page.keyboard.press("PageDown");
    await expect.poll(top).toBeGreaterThan(0);
    await settled();
    await page.keyboard.press("PageUp");
    await expect.poll(top).toBe(0);

    await region.focus();
    await page.keyboard.press("ArrowDown");
    await expect.poll(top).toBeGreaterThan(0);

    // Settings leaves the dashboard's rows mounted, so this is the stand-down
    // for an open dialog rather than the handler being absent. First with
    // focus left on the rail button that opened it, outside the sheet.
    await page.getByTestId("open-settings").click();
    await expect(page.getByTestId("open-settings")).toBeFocused();
    await page.waitForTimeout(500);
    const fromRail = await top();
    await page.keyboard.press("End");
    await page.keyboard.press("PageDown");
    await page.waitForTimeout(500);
    expect(await top()).toBe(fromRail);
    await page.getByRole("button", { name: "Close settings" }).focus();
    await page.waitForTimeout(500);
    const behindSettings = await top();
    await page.keyboard.press("End");
    await page.keyboard.press("PageDown");
    await page.waitForTimeout(500);
    expect(await top()).toBe(behindSettings);
    await page.getByRole("button", { name: "Close settings" }).click();

    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await page.getByTestId("new-agent-filter").focus();
    // Measured once Chromium's animated arrow-key scroll above has settled.
    await page.waitForTimeout(500);
    const before = await top();
    await page.keyboard.press("End");
    await page.keyboard.press("PageDown");
    await page.waitForTimeout(500);
    expect(await top()).toBe(before);
  });
});
