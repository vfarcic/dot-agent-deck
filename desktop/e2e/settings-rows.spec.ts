import { expect, test, type Page } from "@playwright/test";

/**
 * The house two-column settings row, measured in every panel and in both
 * engines (issue #1032).
 *
 * `docs/develop/desktop-gui.md`'s "Adding a setting" rule is a **132px label
 * column, a 16px gutter, then the control**, so a panel of settings reads as two
 * columns rather than a ragged stack. That is geometry, and the vitest suite
 * runs under jsdom, which computes no boxes at all — the only thing it could
 * assert is that a class name was written.
 *
 * `decks.spec.ts` asserts this for the Decks panel, which is how the WebKit
 * divergence in #1032 was found: the Appearance row made its label a grid item
 * by floating a `<legend>`, WebKit forces a rendered legend's `float` to `none`,
 * and the row silently collapsed to one column on the engine the app actually
 * ships on (WebKitGTK under Tauri on Linux, WKWebView on macOS). Pinning the
 * convention for the newest panel alone is what let two older ones diverge, so
 * this file walks **every** section the registry offers and holds each of them
 * to the same measurement — a panel added later inherits the guard without
 * anyone remembering to write it.
 */

/** `FIXTURE_SETTINGS_KEY` — deliberately unscoped, because a theme is global. */
const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** What one settings row measured to, in the engine under test. */
type RowGeometry = {
  section: string;
  text: string;
  labelLeft: number;
  controlLeft: number;
  firstTrack: string;
  fontSize: string;
  fontWeight: string;
  /** The tag the row's visible label is, which is the whole of #1032. */
  labelTag: string;
};

/** Seed the preview's settings document and open the settings sheet. */
async function openSettings(page: Page) {
  await page.addInitScript(
    ([key, document]) => {
      window.localStorage.setItem(key as string, JSON.stringify(document));
    },
    [
      SETTINGS_KEY,
      {
        version: 1,
        appearance: { mode: "light" },
        zoom: { level: 1 },
        endpoints: { remote: [{ host: "build-box", id: "deck0000000000aa", port: 22 }], selection: "deck0000000000aa" },
      },
    ] as const,
  );
  await page.goto("/?fixture=1");
  await expect(page.getByTestId("open-settings")).toBeVisible();
  await page.getByTestId("open-settings").click();
}

/**
 * Measure every `.settings-row` in the section that is currently open.
 *
 * One `evaluate` for the whole panel, the way `decks.spec.ts` does it: two round
 * trips can straddle a re-render, and a geometry comparison across one is worse
 * than no comparison.
 */
async function measureRows(page: Page, section: string): Promise<RowGeometry[]> {
  return page.evaluate((id) => {
    const panel = document.querySelector(`[data-testid="settings-panel-${id}"]`)!;
    return Array.from(panel.querySelectorAll(".settings-row")).map((row) => {
      // `:scope >` so a `<label>` *inside* a control — every segmented option
      // and every deck choice has one — cannot stand in for the row's own label.
      const label = row.querySelector(":scope > legend, :scope > label, :scope > .settings-row-label")!;
      const control = Array.from(row.children).find((child) => child !== label)!;
      const labelBox = label.getBoundingClientRect();
      const controlBox = control.getBoundingClientRect();
      return {
        section: id,
        text: (label.textContent ?? "").trim(),
        labelLeft: Math.round(labelBox.left),
        controlLeft: Math.round(controlBox.left),
        // The TRACK, not the element's own box: `justify-items: start` makes
        // every grid item shrink to its content, so a label reads 36px wide
        // while sitting in a 132px column. The column is the convention.
        firstTrack: getComputedStyle(row).gridTemplateColumns.split(" ")[0],
        fontSize: getComputedStyle(label).fontSize,
        fontWeight: getComputedStyle(label).fontWeight,
        labelTag: label.tagName.toLowerCase(),
      };
    });
  }, section);
}

/** Every section the registry offers, so a panel added later is measured too. */
async function sectionIds(page: Page): Promise<string[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll("[data-testid^='settings-section-']")).map(
      (node) => node.getAttribute("data-testid")!.replace("settings-section-", ""),
    ),
  );
}

test.describe("the two-column settings row", () => {
  test("holds its 132px label column in every panel the registry offers", async ({ page }) => {
    await openSettings(page);

    const sections = await sectionIds(page);
    // The registry holds three rows today. A bare `for` over an empty list is a
    // test that measures nothing and reports green, which is the failure mode
    // this whole file exists to close.
    expect(sections.length, "the settings sheet offered no sections to measure").toBeGreaterThanOrEqual(3);

    const rows: RowGeometry[] = [];
    for (const section of sections) {
      await page.getByTestId(`settings-section-${section}`).click();
      await expect(page.getByTestId(`settings-panel-${section}`)).toBeVisible();
      const measured = await measureRows(page, section);
      expect(measured.length, `the “${section}” panel has no settings row to measure`).toBeGreaterThan(0);
      rows.push(...measured);
    }

    const [first, ...rest] = rows;
    for (const row of rest) {
      // Across panels, not only within one: every row in the sheet is the same
      // setting shape, so a panel that opted out moves its own control and is
      // exactly what a reader sees as a ragged stack.
      expect(row.labelLeft, `“${row.text}” (${row.section}) starts its label at a different x`).toBe(first.labelLeft);
      expect(row.controlLeft, `“${row.text}” (${row.section}) starts its control at a different x`).toBe(
        first.controlLeft,
      );
    }
    // 132px label column + 16px gutter, measured rather than asserted from the
    // stylesheet — a `.settings-row` that lost the grid would still carry the
    // class. This is the number that goes wrong under WebKit when the label is
    // not a grid item: the control lands in column one, beside its label.
    expect(first.controlLeft - first.labelLeft).toBe(148);
    for (const row of rows) {
      expect(row.firstTrack, `“${row.text}” (${row.section}) is not on the 132px column`).toBe("132px");
      // The row label is a 13px/650 sub-header, which is part of the convention:
      // a panel whose labels are 11px next to another panel's 13px is the same
      // setting rendered at two different weights.
      expect(row.fontSize, `“${row.text}” (${row.section}) is not a 13px label`).toBe("13px");
      expect(row.fontWeight, `“${row.text}” (${row.section}) is not a 650 label`).toBe("650");
    }
  });

  test("uses no rendered legend as a row label, because WebKit will not float one", async ({ page }) => {
    await openSettings(page);

    /*
      The cause behind the measurement above, named so a failure says WHY rather
      than only that a control moved 132px.

      A floated `<legend>` stops being the fieldset's *rendered* legend, which is
      the only way it becomes an ordinary box among the fieldset's contents — the
      grid item in column one. Chromium honours the float; WebKit forces a
      rendered legend's `float` back to `none`, so under WebKit the legend is
      laid out above the contents on a line of its own and the 132px column is
      wasted. `<span class="settings-row-label" id>` plus `aria-labelledby` on
      the control lays out identically in both engines and keeps the group's
      accessible name.
    */
    for (const section of await sectionIds(page)) {
      await page.getByTestId(`settings-section-${section}`).click();
      await expect(page.getByTestId(`settings-panel-${section}`)).toBeVisible();
      const legends = await page
        .getByTestId(`settings-panel-${section}`)
        .locator(".settings-row > legend")
        .count();
      expect(legends, `the “${section}” panel labels a settings row with a legend`).toBe(0);
    }
  });
});
