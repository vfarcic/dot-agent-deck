import { expect, type Page } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * PRD #1790 stage 2 — a deck's interrupted work, as a user sees it: the
 * banner on the deck's dashboard section counting what a restart of its
 * daemon left behind, and the "Interrupted work" dialog it opens.
 *
 * `interrupted=1` makes the preview's daemon list three items — a dispatched
 * unit, an orchestration and a plain agent — and act on dismiss and reclaim
 * as a daemon would: the item is gone from the next list.
 */

async function openDashboard(page: Page, interrupted = "1"): Promise<void> {
  await page.goto(`/?fixture=1&interrupted=${interrupted}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
}

test.describe("Interrupted work", () => {
  /** Scenario: open the dashboard; the deck's banner reads "3 interrupted items from before the restart". Show opens the "Interrupted work" dialog; selecting the plain agent under "Other agents" and pressing Dismiss says "Dismissed.", and the banner then counts 2. */
  test("opens from the banner, and a dismissal drops the banner's count", async ({ page }) => {
    await openDashboard(page);
    const banner = page.getByTestId("interrupted-banner");
    await expect(banner).toContainText("3 interrupted items from before the restart");
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByRole("heading", { name: "Interrupted work" })).toBeVisible();
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await expect(dialog.getByTestId("interrupted-other-agents")).toHaveText("Other agents");

    await dialog.getByTestId("interrupted-row").filter({ hasText: "scratch-shell" }).click();
    await expect(dialog.getByTestId("interrupted-reclaim")).toHaveCount(0);
    await dialog.getByTestId("interrupted-dismiss").click();
    await expect(dialog.getByTestId("interrupted-message")).toHaveText("Dismissed.");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(2);
    await expect(banner).toContainText("2 interrupted items from before the restart");
  });

  /** Scenario: in the dialog, Reclaim on the unit shows the dry run's verdict "Reclaim would remove its worktree and keep its branch." and a Reclaim confirm; pressing it says "Reclaimed: its worktree was removed and its branch kept." and the unit leaves the list. Back from a dry run sends nothing. */
  test("reclaims only after the dry run's confirm", async ({ page }) => {
    await openDashboard(page);
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await dialog.getByTestId("interrupted-reclaim").click();
    await expect(dialog.getByTestId("interrupted-reclaim-verdict")).toContainText("Reclaim would remove its worktree and keep its branch.");
    await dialog.getByTestId("interrupted-back").click();
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-reclaim").click();
    await dialog.getByTestId("interrupted-reclaim-confirm").click();
    await expect(dialog.getByTestId("interrupted-message")).toHaveText("Reclaimed: its worktree was removed and its branch kept.");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(2);
    await expect(dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" })).toHaveCount(0);
    await expect(page.getByTestId("interrupted-banner")).toContainText("2 interrupted items from before the restart");
  });

  /** Scenario: without `interrupted=1` the preview's daemon does not support interrupted work, and the dashboard shows no banner. */
  test("shows nothing for a daemon without interrupted work", async ({ page }) => {
    await page.goto("/?fixture=1");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group").first()).toBeVisible();
    await expect(page.getByTestId("interrupted-banner")).toHaveCount(0);
  });

  /** Scenario: several daemons each answer with an empty list that says their journal could not be read whole. Each connected daemon's section shows a banner saying some interrupted work may not be listed; `I` opens nothing and says the daemons may have interrupted work. Tabbing to one banner's Show and pressing Enter opens that daemon's dialog, which says the list is incomplete. */
  test("offers a banner to open each daemon whose empty list is incomplete", async ({ page }) => {
    await page.goto("/?fixture=1&state=fleet&interrupted=unread");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await page.getByTestId("open-overview").click();
    const banners = page.getByTestId("interrupted-banner");
    await expect.poll(() => banners.count()).toBeGreaterThan(1);
    for (const banner of await banners.all()) await expect(banner).toContainText("some interrupted work may not be listed");

    await page.keyboard.press("I");
    await expect(page.getByTestId("overview-daemon-notice")).toContainText("Several daemons may have interrupted work");
    await expect(page.getByTestId("interrupted-dialog")).toHaveCount(0);

    const second = banners.nth(1);
    await second.getByTestId("interrupted-banner-show").focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByRole("heading", { name: "Interrupted work" })).toBeVisible();
    await expect(dialog).toContainText("The deck could not read its whole work journal, so an item may be missing or out of date.");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(0);
  });

  /** Scenario: several daemons each answer with an empty list that says their journal could not be read whole, and every banner is hidden with its Hide button. `I` brings the banners back while it tells the user to open one from its banner; tabbing to one banner's Show and pressing Enter opens that daemon's dialog. */
  test("brings hidden banners back when I says to open one from its banner", async ({ page }) => {
    await page.goto("/?fixture=1&state=fleet&interrupted=unread");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await page.getByTestId("open-overview").click();
    const banners = page.getByTestId("interrupted-banner");
    await expect.poll(() => banners.count()).toBeGreaterThan(1);
    const shown = await banners.count();
    while ((await banners.count()) > 0) await banners.first().getByTestId("interrupted-banner-hide").click();
    await expect(banners).toHaveCount(0);

    await page.keyboard.press("I");
    await expect(page.getByTestId("overview-daemon-notice")).toContainText("Open one from its banner");
    await expect(banners).toHaveCount(shown);
    await expect(page.getByTestId("interrupted-dialog")).toHaveCount(0);

    await banners.nth(1).getByTestId("interrupted-banner-show").focus();
    await page.keyboard.press("Enter");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByRole("heading", { name: "Interrupted work" })).toBeVisible();
  });

  /* PRD #1790 stage 2, desktop review findings 3, 5 and 8. */

  /** Scenario: with keyboard focus on one of the dialog's controls, Enter does what that control does: on Dismiss all it asks the question, on Close it closes, on Reclaim it runs the dry run, and on a row other than the selected one it opens that row's details; in an item's details, Enter on Back returns to the list rather than asking for reports. */
  test("Enter on a focused control does what that control does", async ({ page }) => {
    await openDashboard(page);
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-dismiss-all-open").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-dismiss-all-question")).toBeVisible();
    await expect(dialog.getByTestId("interrupted-details-view")).toHaveCount(0);
    await dialog.getByTestId("interrupted-back").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-reclaim").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-reclaim-verdict")).toContainText("Reclaim would remove its worktree and keep its branch.");
    await dialog.getByTestId("interrupted-back").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-row").filter({ hasText: "release-team" }).focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-details-view")).toContainText("release-team");
    await dialog.getByTestId("interrupted-back").click();

    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await dialog.getByTestId("interrupted-details").click();
    await expect(dialog.getByTestId("interrupted-show-reports")).toBeVisible();
    await dialog.getByTestId("interrupted-back").focus();
    await page.keyboard.press("Enter");
    await expect(dialog.getByTestId("interrupted-details-view")).toHaveCount(0);
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await dialog.getByTestId("interrupted-close").focus();
    await page.keyboard.press("Enter");
    await expect(dialog).toHaveCount(0);
  });

  /** Scenario: with thirty items, more than the dialog shows at once, pressing `j` moves the selection down past the last row on screen; the selected row is scrolled into view each time, so `d` dismisses the row the user can see selected. */
  test("keeps the selected row in view as j moves it", async ({ page }) => {
    await openDashboard(page, "many");
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(30);
    for (let at = 0; at < 25; at += 1) await page.keyboard.press("j");
    const selected = dialog.locator(".interrupted-row.is-selected");
    await expect(selected).toContainText("extra-unit-24");
    const box = await dialog.boundingBox();
    const row = await selected.boundingBox();
    expect(box && row).toBeTruthy();
    expect(row!.y).toBeGreaterThanOrEqual(box!.y);
    expect(row!.y + row!.height).toBeLessThanOrEqual(box!.y + box!.height + 1);
    await page.keyboard.press("d");
    await expect(dialog.getByTestId("interrupted-message")).toHaveText("Dismissed.");
    await expect(dialog.getByTestId("interrupted-row").filter({ hasText: "extra-unit-24" })).toHaveCount(0);
  });

  /** Scenario: on the dashboard `I` opens the Interrupted work dialog, and `q` closes it. In the dialog, `D` followed at once by `y`, typed as one burst, asks the Dismiss all question and dismisses nothing; `n` cancels it. Asked again and given time to be read, `y` dismisses every item. */
  test("opens with I, and a key burst cannot answer a question before it is presented", async ({ page }) => {
    await openDashboard(page);
    await expect(page.getByTestId("interrupted-banner")).toBeVisible();
    await page.keyboard.press("I");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await page.keyboard.press("q");
    await expect(dialog).toHaveCount(0);
    await page.keyboard.press("I");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await page.keyboard.type("Dyyy");
    await expect(dialog.getByTestId("interrupted-dismiss-all-question")).toBeVisible();
    await expect(dialog.getByTestId("interrupted-message")).toHaveCount(0);
    await page.keyboard.press("n");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);

    await page.keyboard.press("D");
    await expect(dialog.getByTestId("interrupted-dismiss-all")).toHaveAttribute("data-presented", "true");
    await page.keyboard.press("y");
    await expect(dialog.getByTestId("interrupted-empty")).toBeVisible();
  });
  /* PRD #1790 stage 2, final reviewer 3 (CLAUDE.md rule 22): the TUI's X and its j/k scrolling. */

  /** Scenario: on the dashboard `X` hides the deck's banner, as the TUI's `X` hides its row; `I` then opens the dialog with all three items still listed, so nothing was dismissed. */
  test("X hides the banner and dismisses nothing", async ({ page }) => {
    await openDashboard(page);
    await expect(page.getByTestId("interrupted-banner")).toBeVisible();
    await page.keyboard.press("X");
    await expect(page.getByTestId("interrupted-banner")).toHaveCount(0);
    await page.keyboard.press("I");
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
  });

  /** Scenario: with the Interrupted work dialog open, `X` hides the deck's banner behind it, as the TUI's `X` hides its row from the overlay, and leaves the dialog open with all three items listed; it does the same from an item's details and from the Dismiss all question, and dismisses nothing. */
  test("X in the dialog hides the banner and leaves the dialog open", async ({ page }) => {
    const dialog = page.getByTestId("interrupted-dialog");
    const banner = page.getByTestId("interrupted-banner");
    const from: Record<string, () => Promise<void>> = {
      list: async () => {},
      details: async () => {
        await page.keyboard.press("Enter");
        await expect(dialog.getByTestId("interrupted-details-view")).toBeVisible();
      },
      "the Dismiss all question": async () => {
        await page.keyboard.press("D");
        await expect(dialog.getByTestId("interrupted-dismiss-all")).toHaveAttribute("data-presented", "true");
      },
    };
    for (const [view, reach] of Object.entries(from)) {
      await openDashboard(page);
      await expect(banner).toBeVisible();
      await page.keyboard.press("I");
      await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
      await reach();
      await page.keyboard.press("X");
      await expect(banner, view).toHaveCount(0);
      await expect(dialog, view).toBeVisible();
      await page.keyboard.press("q");
      await page.keyboard.press("q");
      await expect(dialog, view).toHaveCount(0);
      await page.keyboard.press("I");
      await expect(dialog.getByTestId("interrupted-row"), `${view}: nothing was dismissed`).toHaveCount(3);
    }
  });

  /** Scenario: in a window too short for the dialog, `j` scrolls an item's details down and `k` back up, the same with its report text shown and with the Dismiss all question; none of them changes the selected item, answers the question or dismisses anything. */
  test("j and k scroll the details, the report text and a question", async ({ page }) => {
    await openDashboard(page);
    await page.getByTestId("interrupted-banner-show").click();
    const dialog = page.getByTestId("interrupted-dialog");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await page.setViewportSize({ width: 900, height: 170 });
    const scrollTop = () => dialog.evaluate((element) => element.scrollTop);
    const overflows = () => dialog.evaluate((element) => element.scrollHeight > element.clientHeight + 4);
    const scrolls = async () => {
      await dialog.evaluate((element) => { element.scrollTop = 0; });
      expect(await overflows()).toBe(true);
      await page.keyboard.press("j");
      await expect.poll(scrollTop).toBeGreaterThan(0);
      const down = await scrollTop();
      await page.keyboard.press("k");
      await expect.poll(scrollTop).toBeLessThan(down);
    };

    await dialog.getByTestId("interrupted-details").click();
    await expect(dialog.getByTestId("interrupted-details-view")).toContainText("fix-login");
    await scrolls();
    await dialog.getByTestId("interrupted-show-reports").click();
    await expect(dialog.getByTestId("interrupted-report-body")).toBeVisible();
    await scrolls();
    await expect(dialog.getByTestId("interrupted-details-view")).toContainText("fix-login");
    await page.keyboard.press("q");
    await expect(dialog.locator(".interrupted-row.is-selected")).toContainText("fix-login");

    await page.keyboard.press("D");
    await expect(dialog.getByTestId("interrupted-dismiss-all")).toHaveAttribute("data-presented", "true");
    await page.setViewportSize({ width: 900, height: 90 });
    await scrolls();
    await expect(dialog.getByTestId("interrupted-dismiss-all-question")).toBeVisible();
    await expect(dialog.getByTestId("interrupted-message")).toHaveCount(0);
    await page.keyboard.press("n");
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(3);
    await expect(dialog.locator(".interrupted-row.is-selected")).toContainText("fix-login");
  });

  /** Scenario: open the dashboard of a deck that restarts while the app watches it, with interrupted work listed; the restart notice in the corner adds "3 interrupted items from before the restart" with a Show button whose whole label is on one line, and Show opens the "Interrupted work" dialog. */
  test("the restart notice's Show is a whole button that opens the dialog", async ({ page }) => {
    await page.goto("/?fixture=1&state=docs-restart&interrupted=1");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await page.getByTestId("open-overview").click();
    const hint = page.getByTestId("restart-toast").getByTestId("restart-toast-interrupted");
    await expect(hint).toContainText("3 interrupted items from before the restart");
    const show = hint.getByTestId("restart-toast-interrupted-show");
    await expect(show).toHaveText("Show");
    // PR #2431 round 7: the toast's 22 px close-button rule once squeezed this
    // button to one letter, wrapping "Show" down the toast.
    const fits = await show.evaluate((el) => el.scrollWidth <= el.clientWidth && el.scrollHeight <= el.clientHeight);
    expect(fits, "Show's label overflows its button").toBe(true);
    const lineHeight = await show.evaluate((el) => parseFloat(getComputedStyle(el).fontSize) * 2);
    expect((await show.boundingBox())!.height).toBeLessThan(lineHeight + 16);
    await show.click();
    await expect(page.getByTestId("interrupted-dialog").getByRole("heading", { name: "Interrupted work" })).toBeVisible();
  });
});

/** Opened with every experimental surface off: restore is not behind the flag (issue #2494). */
async function openRestorePreview(page: Page, fixture = "restore") {
  await page.goto(`/?fixture=1&interrupted=${fixture}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("interrupted-banner")).toContainText("2 interrupted items from before the restart");
  await page.keyboard.press("I");
  const dialog = page.getByTestId("interrupted-dialog");
  await expect(dialog.getByTestId("interrupted-row")).toHaveCount(2);
  return dialog;
}

test.describe("journal/restore/009 — desktop restore in the browser", () => {
  for (const [key, name, button] of [
    ["c", "Restore and continue", "interrupted-restore-continue"],
    ["o", "Restore only", "interrupted-restore-only"],
  ] as const) {
    /** Scenario: open Interrupted work with I, select fix-login and press s. After inspecting the resume question, choose c or o: the dialog closes on the dashboard, the banner count falls, and the reopened list no longer has the item. */
    test(`journal/restore/009 ${key} confirms ${name}`, async ({ page }) => {
      const dialog = await openRestorePreview(page);
      await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
      await page.keyboard.press("s");
      const question = dialog.getByTestId("interrupted-restore-question");
      await expect(question).toContainText("Restore fix-login with its conversation?");
      await expect(question).toContainText("It checks what already happened and carries on where it stopped.");
      await expect(question).toContainText("It comes back and waits until you tell it to continue.");
      await expect(question.getByTestId(button)).toHaveText(name);
      await expect(question.getByTestId("interrupted-start-over")).toHaveCount(0);
      await expect(question).toHaveAttribute("data-presented", "true");
      await page.keyboard.press(key);
      // Adapted to the landed TUI behaviour: a clean restore closes the
      // dialog on the dashboard, where the agent is back, as the TUI's overlay
      // closes. Reopening it shows the item left the list.
      await expect(dialog).toHaveCount(0);
      await expect(page.getByTestId("interrupted-banner")).toContainText("1 interrupted item from before the restart");
      await page.keyboard.press("I");
      await expect(dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" })).toHaveCount(0);
      await expect(dialog.getByTestId("interrupted-row")).toHaveCount(1);
    });
  }

  /** Scenario: select the Pi whose conversation cannot be restored and click Restore. Its question explains the gap and offers Start over instead of either resume choice; w closes the dialog and removes that stopped item from the list. */
  test("journal/restore/009 w starts over only when the verdict offers it", async ({ page }) => {
    const dialog = await openRestorePreview(page);
    await dialog.getByTestId("interrupted-row").filter({ hasText: "scratch-pi" }).click();
    await dialog.getByTestId("interrupted-restore").click();
    const question = dialog.getByTestId("interrupted-restore-question");
    await expect(question).toContainText("Start scratch-pi over in this worktree, without its conversation?");
    await expect(question).toContainText("The agent cannot be restored with its conversation yet.");
    await expect(question.getByTestId("interrupted-start-over")).toHaveText("Start over in this worktree");
    await expect(question.getByTestId("interrupted-restore-continue")).toHaveCount(0);
    await expect(question.getByTestId("interrupted-restore-only")).toHaveCount(0);
    await expect(question).toHaveAttribute("data-presented", "true");
    await page.keyboard.press("w");
    // Adapted as above: a clean start-over closes the dialog; reopened, it lists the other item only.
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("interrupted-banner")).toContainText("1 interrupted item from before the restart");
    await page.keyboard.press("I");
    await expect(dialog.getByTestId("interrupted-row").filter({ hasText: "scratch-pi" })).toHaveCount(0);
    await expect(dialog.getByTestId("interrupted-row")).toHaveCount(1);
  });

  /** Scenario: press s and immediately c in one key burst. The question paints without starting the agent; Escape, q and n each return to the list with its two items still present. */
  test("journal/restore/009 shows the question before accepting a confirm and permits every back key", async ({ page }) => {
    const dialog = await openRestorePreview(page);
    for (const back of ["Escape", "q", "n"]) {
      // Dispatch in one browser event-loop turn, as the Stage 2 D/y guard test
      // does for an unpresented question; do not insert a wait between keys.
      await page.evaluate(() => {
        const node = document.querySelector('[data-testid="interrupted-dialog"]')!;
        for (const key of ["s", "c", "c"]) node.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));
      });
      await expect(dialog.getByTestId("interrupted-restore-question")).toBeVisible();
      await page.keyboard.press(back);
      await expect(dialog.getByTestId("interrupted-restore-question")).toHaveCount(0);
      await expect(dialog.getByTestId("interrupted-row")).toHaveCount(2);
    }
  });

  /** Scenario: in a window too short for the whole restore question, select fix-login and press s: c is ignored while part of the question has never been on screen. Scroll through the question with j; once every part of it has been shown, c restores the agent and the dialog closes. */
  test("journal/restore/009 a question taller than the window is presented once scrolled through", async ({ page }) => {
    const dialog = await openRestorePreview(page);
    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await page.setViewportSize({ width: 900, height: 120 });
    await page.keyboard.press("s");
    const question = dialog.getByTestId("interrupted-restore-question");
    await expect(question).toContainText("Restore fix-login with its conversation?");
    const body = question.locator(":scope > div").first();
    const taller = await body.evaluate((element) => {
      const box = element.closest(".interrupted-dialog")!;
      return element.getBoundingClientRect().height > box.clientHeight;
    });
    expect(taller, "premise: the question does not fit in the dialog at once").toBe(true);
    await dialog.evaluate((element) => { element.scrollTop = 0; });
    // Longer than the presentation delay: a question never wholly on screen
    // is still not presented, and its keys act on nothing.
    await page.waitForTimeout(800);
    await expect(question).not.toHaveAttribute("data-presented", "true");
    await page.keyboard.press("c");
    await page.waitForTimeout(300);
    await expect(question).toBeVisible();
    await expect(page.getByTestId("interrupted-banner")).toContainText("2 interrupted items from before the restart");
    for (let step = 0; step < 30; step += 1) {
      if ((await question.getAttribute("data-presented")) === "true") break;
      await page.keyboard.press("j");
      await page.waitForTimeout(450);
    }
    await expect(question).toHaveAttribute("data-presented", "true");
    await page.keyboard.press("c");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("interrupted-banner")).toContainText("1 interrupted item from before the restart");
  });

  /** Scenario: in a short, narrow window, open the restore question for an agent whose long name wraps taller than the dialog, with the page's clock held so nothing counts before the test scrolls. Showing only the middle of the name, then only its end, then the choices below it leaves c ignored however long each is shown; scrolling through every line from the top makes c restore the agent. */
  test("journal/restore/009 a part taller than the window counts only once all of it was shown", async ({ page }) => {
    test.setTimeout(90_000);
    const dialog = await openRestorePreview(page, "restore-long");
    await dialog.getByTestId("interrupted-row").filter({ hasText: "fix-login" }).click();
    await page.setViewportSize({ width: 360, height: 120 });
    await page.clock.install();
    await page.keyboard.press("s");
    const question = dialog.getByTestId("interrupted-restore-question");
    await expect(question).toContainText("fix-login part0 part1");
    const name = question.locator(".interrupted-verdict").first();
    const { part, box, offset } = await name.evaluate((element) => {
      const dialogBox = element.closest(".interrupted-dialog")!;
      const shown = element.getBoundingClientRect();
      return { part: shown.height, box: dialogBox.clientHeight, offset: shown.top - dialogBox.getBoundingClientRect().top + dialogBox.scrollTop };
    });
    expect(part, "premise: the name's part is taller than the dialog").toBeGreaterThan(box);
    // Only its middle, then its end, then the choices below it: never the
    // whole of the name's part, and never its first line.
    for (const at of [offset + part / 2 - box / 2, offset + part - box / 2, offset + part + 4000]) {
      await dialog.evaluate((element, top) => { element.scrollTop = top; }, at);
      await page.clock.runFor(800);
      await expect(question).not.toHaveAttribute("data-presented", "true");
      await page.keyboard.press("c");
      await page.clock.runFor(200);
      await expect(question).toBeVisible();
      await expect(page.getByTestId("interrupted-banner")).toContainText("2 interrupted items from before the restart");
    }
    // Every line of it, top to bottom.
    for (let top = 0; top <= offset + part + box; top += Math.max(5, Math.floor(box / 2))) {
      await dialog.evaluate((element, at) => { element.scrollTop = at; }, top);
      await page.clock.runFor(400);
      if ((await question.getAttribute("data-presented")) === "true") break;
    }
    await expect(question).toHaveAttribute("data-presented", "true");
    await page.clock.resume();
    await page.keyboard.press("c");
    await expect(dialog).toHaveCount(0);
    await expect(page.getByTestId("interrupted-banner")).toContainText("1 interrupted item from before the restart");
  });
});
