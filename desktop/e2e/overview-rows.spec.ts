import { expect, test } from "@playwright/test";

import { openOverview, rects, showAllColumns } from "./support/overview";

/**
 * Row heights stay uniform however long the prompt is — the assertion issue
 * #836 records as unmakeable.
 *
 * An audit found a vitest test whose comment claimed it proved a hostile-prompt
 * row was the same height as a blank one. It could not: jsdom performs no
 * layout, so it measures no height. That test was strengthened to assert the
 * cell's computed `white-space`/`overflow`/`text-overflow` declarations —
 * jsdom's ceiling — and its comment corrected to stop claiming a height check.
 * This is the height check.
 *
 * The two halves are split on purpose and neither replaces the other: the
 * SANITISATION of a hostile prompt (control and bidi codepoints stripped,
 * length clamped to `DISPLAY_LIMITS.prompt`) is a string property that vitest
 * tests properly, and the GEOMETRY is what only a layout engine can answer.
 */

// Nine columns at 900px puts the flexible tracks — the prompt among them — at
// their `minmax()` minimum, so every prompt on screen is wider than its column
// and the clipping rule is what stops the row growing. A wide viewport would
// leave the prompts fitting and prove nothing.
test.use({ viewport: { width: 900, height: 800 } });

/**
 * The longest string the app can put in a prompt cell: `displayText` clamps to
 * `DISPLAY_LIMITS.prompt` (160) and appends one elision marker. Anything the
 * daemon sends — up to its own 64 KiB bound — arrives at the cell no longer
 * than this, so this is the worst case the layout has to survive.
 */
const LONGEST_RENDERABLE_PROMPT = "p".repeat(161);

test.describe("overview row geometry", () => {
  test("every row is the same height, whatever its prompt says", async ({ page }) => {
    await openOverview(page, "crowded");
    await showAllColumns(page);

    // The crowded fixture carries both cases already: agents with a prompt
    // longer than the column can show, and agents the daemon has reported no
    // prompt for at all (`Scratch shell`, `publisher`, `pi-extension spike`),
    // whose cell is empty. If those two rendered at different heights the table
    // would visibly ripple.
    const withPrompt = await page.locator(".overview-prompt").evaluateAll((nodes) =>
      nodes.filter((node) => (node.textContent ?? "").trim().length > 0).length,
    );
    const blank = await page.locator(".overview-prompt").evaluateAll((nodes) =>
      nodes.filter((node) => (node.textContent ?? "").trim().length === 0).length,
    );
    expect(withPrompt, "no populated prompt cell on screen").toBeGreaterThan(0);
    expect(blank, "no blank prompt cell on screen").toBeGreaterThan(0);

    // A card's first row has no top border and every later row has one, so the
    // two sets are compared separately rather than against a magic 1px.
    const laterRows = await rects(page.locator(".overview-row:not(:first-child)"));
    const firstRows = await rects(page.locator(".overview-row:first-child"));
    expect(laterRows.length).toBeGreaterThan(1);
    expect(firstRows.length).toBeGreaterThan(1);
    for (const row of laterRows) expect(row.height).toBeCloseTo(laterRows[0].height, 1);
    for (const row of firstRows) expect(row.height).toBeCloseTo(firstRows[0].height, 1);
  });

  test("the longest prompt the app can render does not grow its row", async ({ page }) => {
    await openOverview(page, "crowded");
    await showAllColumns(page);

    const row = page.locator(".overview-row:not(:first-child)").first();

    /*
      Written straight into the cell rather than through the fixture: the
      fixture is a fixed scenario and carries no such agent, and the string
      below is what the app's own sanitiser would have produced anyway, so going
      through it would test `displayText` (which vitest already covers) instead
      of the layout. The point of the injection is to hand the ENGINE the widest
      single line a cell can ever hold.

      Measured, and put back, inside ONE synchronous callback. The overview
      re-renders on its own ten-second clock, which would restore the fixture's
      prompt underneath a test that measured across two round trips — a race,
      and one that would show up as a rare red rather than as a wrong answer.
      Nothing can re-render between these statements, and `getBoundingClientRect`
      forces the layout each time, so the two measurements are of the same row
      differing only in its prompt.
    */
    const measured = await row.evaluate((node, text) => {
      const cell = node.querySelector(".overview-prompt");
      if (!(cell instanceof HTMLElement)) throw new Error("row has no .overview-prompt cell");
      const original = cell.textContent;
      const before = node.getBoundingClientRect();
      cell.textContent = text;
      const after = node.getBoundingClientRect();
      cell.textContent = original;
      return {
        before: { height: before.height, width: before.width },
        after: { height: after.height, width: after.width },
        rendered: cell.textContent === original,
      };
    }, LONGEST_RENDERABLE_PROMPT);

    expect(measured.rendered, "the cell's original text was not restored").toBe(true);
    expect(
      measured.after.height,
      "the prompt cell wrapped instead of clipping, so one long prompt makes its row taller than its neighbours",
    ).toBeCloseTo(measured.before.height, 1);

    // And the row did not get WIDER either: the cell clips inside its grid
    // track rather than pushing the track out and dragging every other column
    // sideways.
    expect(measured.after.width).toBeCloseTo(measured.before.width, 1);
  });
});
