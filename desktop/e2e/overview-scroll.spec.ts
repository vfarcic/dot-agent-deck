import { expect, test } from "@playwright/test";

import { ALIGNMENT_TOLERANCE_PX, ALL_COLUMNS, groupCards, openOverview, rects, showAllColumns, tableRegion } from "./support/overview";

/**
 * The overview's horizontal scroll region under overflow — issue #836's
 * highest-value case, and the one the PRD #745 M12 implementation flagged as
 * "unverified in a real browser".
 *
 * The contract, in the words of the CSS comment that implements it: ONE scroll
 * region for the legend and every group card together, never one per card,
 * because two cards at different horizontal offsets stop lining up "exactly
 * when the chosen set is wide enough to overflow, which is exactly when a
 * reader depends on that alignment".
 *
 * None of this is reachable from jsdom, which performs no layout: every
 * assertion below reads `getBoundingClientRect`, `scrollWidth`, `scrollLeft` or
 * a resolved grid track, and there is nothing behind any of those until an
 * engine has laid the page out.
 */

// Narrow enough that all nine columns cannot fit, wide enough to stay clear of
// the 680px breakpoint where padding changes. The overflow is asserted rather
// than assumed, so a future layout change that removes it fails here loudly
// instead of quietly turning this file into a no-op.
test.use({ viewport: { width: 900, height: 800 } });

test.describe("the overview's horizontal scroll region", () => {
  test("scrolls as one region, with every group card in step", async ({ page }) => {
    await openOverview(page, "crowded");
    await showAllColumns(page);

    const region = tableRegion(page);
    const overflow = await region.evaluate((node) => ({
      scrollWidth: node.scrollWidth,
      clientWidth: node.clientWidth,
    }));
    expect(
      overflow.scrollWidth,
      "nine columns at 900px must overflow the region, or this file proves nothing",
    ).toBeGreaterThan(overflow.clientWidth);

    // Precondition: more than one card, or "cards stay in step" is vacuous.
    const cards = groupCards(page);
    expect(await cards.count()).toBeGreaterThan(1);

    // No card is a scroll container of its own. A card that scrolled
    // independently is the failure mode; this is the direct statement of it.
    const perCard = await cards.evaluateAll((nodes) =>
      nodes.map((node) => ({ scrollLeft: node.scrollLeft, scrollWidth: node.scrollWidth, clientWidth: node.clientWidth })),
    );
    for (const card of perCard) {
      expect(card.scrollLeft).toBe(0);
      expect(card.scrollWidth).toBe(card.clientWidth);
    }

    const before = await rects(cards);
    const legendBefore = (await rects(page.locator(".overview-legend")))[0];
    for (const card of before) expect(Math.abs(card.left - before[0].left)).toBeLessThanOrEqual(ALIGNMENT_TOLERANCE_PX);

    /*
      Column TRACKS line up across cards, which is the property a reader
      actually depends on — a shared left edge on the cards would survive two
      cards resolving the grid template differently.

      The tracks are compared, not the cell boxes, because one cell box
      deliberately does not fill its track: `.overview-tool-count` carries
      `justify-self: end`, so a row showing `132` and a row showing `2` put
      their left edges 5px apart by design. Comparing boxes there would fail on
      correct layout. `grid-template-columns` in a computed style is the USED
      track list in px — `minmax()` and `fr` already resolved against the real
      container width — which is why this is a browser-only assertion:
      resolving those tracks means laying the grid out against a real container
      width, which is exactly the step jsdom does not take.

      Each cell is then asserted to sit INSIDE its own track, which is what
      makes the track comparison mean something about what is on screen.
    */
    const geometry = await cards.evaluateAll((nodes) =>
      nodes.map((node) => {
        const row = node.querySelector(".overview-row");
        if (!(row instanceof HTMLElement)) throw new Error("group card has no row");
        const style = getComputedStyle(row);
        const box = row.getBoundingClientRect();
        return {
          template: style.gridTemplateColumns,
          tracks: style.gridTemplateColumns.split(" ").map(Number.parseFloat),
          gap: Number.parseFloat(style.columnGap),
          contentLeft: box.left + Number.parseFloat(style.paddingLeft) + Number.parseFloat(style.borderLeftWidth),
          width: box.width,
          cells: Array.from(row.children).map((cell) => {
            const rect = cell.getBoundingClientRect();
            return { left: rect.left, right: rect.right };
          }),
        };
      }),
    );

    const reference = geometry[0];
    expect(reference.tracks.length).toBe(ALL_COLUMNS.length);
    for (const card of geometry) {
      expect(card.template, "two cards resolved the shared grid template to different track widths").toBe(reference.template);
      expect(Math.abs(card.contentLeft - reference.contentLeft)).toBeLessThanOrEqual(ALIGNMENT_TOLERANCE_PX);
      expect(Math.abs(card.width - reference.width)).toBeLessThanOrEqual(ALIGNMENT_TOLERANCE_PX);
      expect(card.cells.length).toBe(reference.tracks.length);

      let trackLeft = card.contentLeft;
      card.tracks.forEach((track, column) => {
        const cell = card.cells[column];
        expect(cell.left, `column ${column} starts left of its track`).toBeGreaterThanOrEqual(trackLeft - ALIGNMENT_TOLERANCE_PX);
        expect(cell.right, `column ${column} ends right of its track`).toBeLessThanOrEqual(trackLeft + track + ALIGNMENT_TOLERANCE_PX);
        trackLeft += track + card.gap;
      });
    }

    // Scroll the region and everything inside it must move by the same amount.
    // `scrollLeft` is read back rather than assumed, because a browser clamps it
    // to the real overflow and asserting against the requested value would be
    // asserting against a number this test made up.
    const applied = await region.evaluate((node) => {
      node.scrollLeft = 120;
      return node.scrollLeft;
    });
    expect(applied).toBeGreaterThan(0);

    const after = await rects(cards);
    const legendAfter = (await rects(page.locator(".overview-legend")))[0];
    after.forEach((card, index) => {
      expect(
        Math.abs(before[index].left - card.left - applied),
        `card ${index} did not move with the region`,
      ).toBeLessThanOrEqual(ALIGNMENT_TOLERANCE_PX);
    });
    expect(
      Math.abs(legendBefore.left - legendAfter.left - applied),
      "the legend did not move with the cards, so the column labels no longer name the columns under them",
    ).toBeLessThanOrEqual(ALIGNMENT_TOLERANCE_PX);
  });
});
