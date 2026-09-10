import { expect, test } from "@playwright/test";

import {
  ALL_COLUMNS,
  COLUMN_LEGEND,
  DEFAULT_COLUMNS,
  PERMANENT_COLUMN,
  columnCheckbox,
  columnsMenu,
  legendFor,
  legendLabels,
  openColumnPicker,
  openOverview,
  showAllColumns,
} from "./support/overview";

/**
 * The column picker, driven the way a person drives it — issue #836's first
 * item.
 *
 * Three of these behaviours are the reason this file is here rather than in the
 * vitest suite. Dismissal on an outside click is a question about HIT-TESTING:
 * whether a real pointer landing at a real coordinate reaches an element the
 * picker considers outside itself. The trigger toggling rather than reopening
 * is a question about EVENT ORDER: the dismiss listener runs on `pointerdown`
 * and the trigger on `click`, so the two fire in the engine's own sequence
 * around one press. And the menu floating over the screen is a question about
 * LAYOUT and STACKING: `position: absolute` with a `z-index`, which jsdom does
 * not compute and cannot be asked about.
 *
 * The other three — Escape, the permanent column, Restore defaults — are
 * asserted here on their END STATE: what the legend prints and what the rows
 * show afterwards, in an engine that laid the result out. Where a check is
 * DOM-shape rather than geometry, the comment says so.
 */

test.describe("the overview's column picker", () => {
  test("opens under its trigger and floats over the screen behind it", async ({ page }) => {
    await openOverview(page, "crowded");
    const toggle = page.getByTestId("overview-columns-toggle");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");

    await openColumnPicker(page);
    await expect(toggle).toHaveAttribute("aria-expanded", "true");

    /*
      Everything in one `evaluate`, for the reason `overview-rows.spec.ts`
      explains: the overview re-renders on its own ten-second clock, and two
      round trips can straddle one.
    */
    const geometry = await page.evaluate(() => {
      const menu = document.querySelector('[data-testid="overview-columns-menu"]')!;
      const trigger = document.querySelector('[data-testid="overview-columns-toggle"]')!;
      const topbar = document.querySelector(".topbar")!;
      const box = menu.getBoundingClientRect();
      const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
      return {
        box: { top: box.top, bottom: box.bottom, left: box.left, right: box.right, width: box.width, height: box.height },
        trigger: { bottom: trigger.getBoundingClientRect().bottom, right: trigger.getBoundingClientRect().right },
        topbarBottom: topbar.getBoundingClientRect().bottom,
        viewportWidth: window.innerWidth,
        hitIsInsideMenu: hit !== null && menu.contains(hit),
        hitTag: hit?.tagName ?? "nothing",
      };
    });

    expect(geometry.box.width, "the menu laid out with no width").toBeGreaterThan(0);
    expect(geometry.box.height, "the menu laid out with no height").toBeGreaterThan(0);
    // Below the button that opened it, and right-aligned to it: `top: calc(100%
    // + 6px); right: 0` on an absolutely positioned box inside the picker root.
    expect(geometry.box.top).toBeGreaterThanOrEqual(geometry.trigger.bottom);
    expect(Math.abs(geometry.box.right - geometry.trigger.right)).toBeLessThanOrEqual(0.5);
    // Wholly on screen. A right-aligned menu wider than the space to its left
    // is a menu with options nobody can read.
    expect(geometry.box.left).toBeGreaterThanOrEqual(0);
    expect(geometry.box.right).toBeLessThanOrEqual(geometry.viewportWidth);
    // It hangs BELOW the top bar, over the fleet — which is only possible
    // because it is taken out of flow. In flow it would be clipped inside a
    // 72px-tall bar and most of the options would be unreachable.
    expect(geometry.box.bottom, "the menu did not extend past the top bar, so it is not floating over the screen").toBeGreaterThan(
      geometry.topbarBottom,
    );
    // A real hit test at the menu's own centre. `z-index: 30` is what puts it
    // in front of the content it covers; without that, a click aimed at an
    // option would land on whatever is underneath.
    expect(
      geometry.hitIsInsideMenu,
      `a click at the menu's own centre would land on <${geometry.hitTag}>, not on the menu`,
    ).toBe(true);
  });

  test("dismisses on a click outside it", async ({ page }) => {
    await openOverview(page, "crowded");
    const menu = await openColumnPicker(page);
    const outside = page.getByTestId("daemon-identity");

    /*
      Prove the target really is outside before clicking it. The assertion below
      says nothing if the element we picked has drifted under the menu — it would
      then be an INSIDE click, which the picker deliberately ignores, and the test
      would fail for a reason that has nothing to do with the behaviour.
    */
    const clearOfTheMenu = await page.evaluate(() => {
      const box = document.querySelector('[data-testid="overview-columns-menu"]')!.getBoundingClientRect();
      const target = document.querySelector('[data-testid="daemon-identity"]')!.getBoundingClientRect();
      return target.right < box.left || target.left > box.right || target.bottom < box.top || target.top > box.bottom;
    });
    expect(clearOfTheMenu, "the element this test clicks overlaps the menu, so it is not an outside click").toBe(true);

    await outside.click();
    await expect(menu).toBeHidden();
    await expect(page.getByTestId("overview-columns-toggle")).toHaveAttribute("aria-expanded", "false");
  });

  test("dismisses on Escape", async ({ page }) => {
    await openOverview(page, "crowded");
    const menu = await openColumnPicker(page);

    /*
      The keydown handler is on the picker's ROOT, so Escape only reaches it
      while focus is inside that root. Clicking the trigger puts focus there in
      both engines — measured, and stated here so that if an engine ever stops
      focusing a button on click, this line names the cause instead of leaving
      the Escape below to fail as a mystery.
    */
    await expect(page.getByTestId("overview-columns-toggle")).toBeFocused();

    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
    await expect(page.getByTestId("overview-columns-toggle")).toHaveAttribute("aria-expanded", "false");
  });

  test("toggles on its trigger rather than reopening", async ({ page }) => {
    await openOverview(page, "crowded");
    const toggle = page.getByTestId("overview-columns-toggle");
    const menu = columnsMenu(page);

    await toggle.click();
    await expect(menu).toBeVisible();

    /*
      The trap this pins. The dismiss listener fires on `pointerdown` and the
      trigger fires on `click`, so ONE press produces both, in that order. Were
      the trigger outside the picker's root, the press would close the menu on
      the way down and the release would reopen it — and the button would look
      dead. Playwright dispatches the real sequence, so the ordering is the
      engine's rather than a test's idea of it.
    */
    await toggle.click();
    await expect(menu, "a second press on the trigger left the menu open, which is the close-then-reopen bug").toBeHidden();
    await expect(toggle).toHaveAttribute("aria-expanded", "false");

    // And it is a toggle, not a one-way door.
    await toggle.click();
    await expect(menu).toBeVisible();
  });

  test("keeps the name column whatever else is unticked", async ({ page }) => {
    await openOverview(page, "crowded");
    const menu = await openColumnPicker(page);
    const name = columnCheckbox(page, PERMANENT_COLUMN);

    // DOM-shape: the property that makes it permanent. Asserted directly rather
    // than by trying to click it — `uncheck()` on a disabled input waits until
    // it times out, which would report a timeout instead of this contract.
    await expect(name).toBeChecked();
    await expect(name).toBeDisabled();

    // Untick everything the picker WILL let go of, which is the emptiest state
    // a user can actually reach.
    for (const column of ALL_COLUMNS) {
      if (column === PERMANENT_COLUMN) continue;
      const checkbox = columnCheckbox(page, column);
      if (await checkbox.isChecked()) await checkbox.uncheck();
    }
    await expect(name).toBeChecked();
    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();

    await expect(legendLabels(page)).toHaveText([COLUMN_LEGEND[PERMANENT_COLUMN]]);

    /*
      The point of the contract, in the engine: a one-column screen still names
      every agent. `boundingBox` width is what separates "the name is there" from
      "the name is there in a track crushed to nothing", which is the failure a
      DOM assertion cannot tell apart.
    */
    const names = page.locator(".overview-row .overview-agent-name strong");
    await expect(names).toHaveCount(15);
    const painted = await names.evaluateAll((nodes) =>
      nodes.map((node) => ({ text: (node.textContent ?? "").trim(), width: node.getBoundingClientRect().width })),
    );
    for (const cell of painted) {
      expect(cell.text.length, "a row rendered no name at all").toBeGreaterThan(0);
      expect(cell.width, `the name "${cell.text}" laid out with no width`).toBeGreaterThan(0);
    }
  });

  test("Restore defaults puts back the four columns the screen opens on", async ({ page }) => {
    await openOverview(page, "crowded");
    await expect(legendLabels(page)).toHaveText(legendFor(DEFAULT_COLUMNS));

    await showAllColumns(page);
    await expect(legendLabels(page)).toHaveText(legendFor(ALL_COLUMNS));

    const menu = await openColumnPicker(page);
    await page.getByTestId("overview-columns-reset").click();

    // The screen, not the menu: what a reader sees is the contract.
    await expect(legendLabels(page)).toHaveText(legendFor(DEFAULT_COLUMNS));

    // And the menu agrees with it, so the next tick starts from the truth.
    // DOM-shape, deliberately — a checkbox's checkedness has no geometry.
    for (const column of ALL_COLUMNS) {
      const checkbox = columnCheckbox(page, column);
      if ((DEFAULT_COLUMNS as readonly string[]).includes(column)) await expect(checkbox).toBeChecked();
      else await expect(checkbox).not.toBeChecked();
    }

    await page.keyboard.press("Escape");
    await expect(menu).toBeHidden();
  });
});
