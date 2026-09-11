import { expect, test, type Page } from "@playwright/test";

/**
 * The Decks settings section, driven in an engine that lays out and paints
 * (PRD #741 M7).
 *
 * Two of these are here rather than in the vitest suite because jsdom cannot
 * answer them at all:
 *
 * - **The two-column row convention** is GEOMETRY. The house rule
 *   (`docs/develop/desktop-gui.md`) is a fixed 132px label column, a 16px
 *   gutter, then the control — so every row's control starts at the same x and a
 *   panel of settings reads as two columns rather than a ragged stack. jsdom
 *   computes no boxes, so the only thing it could assert is that a class name
 *   was written.
 * - **A bidi override reordering the deck label** is TEXT SHAPING. jsdom can say
 *   the codepoint was stripped from a string; whether the glyphs a reader sees
 *   run left to right is a question only a real engine answers, and the answer
 *   is the whole reason the strip exists.
 *
 * The third — that the section is reachable at all — is cheap here and worth
 * having in both engines, because a registry row that renders in Chromium and
 * not in WebKit is exactly the class of thing this tier was added for.
 *
 * Nothing here needs a daemon or a credential: the settings document in the
 * browser preview lives in `localStorage`, so a spec can seed one.
 */

/** `FIXTURE_SETTINGS_KEY` — deliberately unscoped, because a theme is global. */
const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** A right-to-left override: one character, and the reason this file exists. */
const RLO = "‮";

/** Seed the preview's settings document, then open the Decks section. */
async function openDecks(page: Page, endpoints?: unknown) {
  await page.addInitScript(
    ([key, document]) => {
      window.localStorage.setItem(key as string, JSON.stringify(document));
    },
    [SETTINGS_KEY, { version: 1, appearance: { mode: "light" }, zoom: { level: 1 }, endpoints }] as const,
  );
  await page.goto("/?fixture=1");
  await expect(page.getByTestId("open-settings")).toBeVisible();
  await page.getByTestId("open-settings").click();
  await page.getByTestId("settings-section-decks").click();
  await expect(page.getByTestId("settings-panel-decks")).toBeVisible();
}

/**
 * Where a text node's first and last glyph were actually painted.
 *
 * A `Range` over the text node gives the engine's own laid-out rectangles, so
 * this reads the visual order rather than the source order. Under an active
 * right-to-left override the last character is painted to the LEFT of the
 * first, and `last - first` goes negative.
 */
async function glyphAdvance(page: Page, testId: string): Promise<number> {
  return page.evaluate((id) => {
    const host = document.querySelector(`[data-testid="${id}"]`);
    if (!host) throw new Error(`no element with data-testid=${id}`);
    const walker = document.createTreeWalker(host, NodeFilter.SHOW_TEXT);
    const node = walker.nextNode() as Text | null;
    if (!node || node.data.length < 2) throw new Error(`no measurable text in ${id}`);
    const rectAt = (offset: number) => {
      const range = document.createRange();
      range.setStart(node, offset);
      range.setEnd(node, offset + 1);
      return range.getBoundingClientRect();
    };
    return rectAt(node.data.length - 1).left - rectAt(0).left;
  }, testId);
}

test.describe("the Decks settings section", () => {
  test("is reachable from the settings sheet and shows the local deck without configuration", async ({ page }) => {
    // No `endpoints` in the seeded document at all — the fresh-install case.
    await openDecks(page);

    await expect(page.getByTestId("deck-choice-local")).toBeVisible();
    await expect(page.getByTestId("deck-choice-local")).toContainText("This machine");
    await expect(page.getByTestId("deck-choice-local").locator("input")).toBeChecked();
    // The section column is back, because the registry now holds three rows.
    await expect(page.getByTestId("settings-layout")).not.toHaveClass(/is-single/);
  });

  test("lays a deck's fields out as the house two-column row", async ({ page }) => {
    await openDecks(page, {
      remote: [{ host: "build-box", id: "deck0000000000aa", port: 22 }],
      selection: "deck0000000000aa",
    });

    /*
      Everything in one `evaluate`, the way `column-picker.spec.ts` does it: two
      round trips can straddle a re-render, and a geometry comparison across one
      is worse than no comparison.

      The assertion is the CONVENTION rather than a pixel count for one row: the
      label column is a fixed width, so every control in the panel starts at the
      same x. A row that opted out — a full-width input, a label above its
      control — moves its own control and fails here.
    */
    const rows = await page.evaluate(() => {
      const panel = document.querySelector('[data-testid="settings-panel-decks"]')!;
      return Array.from(panel.querySelectorAll(".settings-row")).map((row) => {
        // `:scope >` so a `<label>` *inside* a control — every deck choice has
        // one — cannot stand in for the row's own label.
        const label = row.querySelector(":scope > legend, :scope > label, :scope > .settings-row-label")!;
        const control = row.querySelector(":scope > input, :scope > .deck-choices")!;
        const labelBox = label.getBoundingClientRect();
        const controlBox = control.getBoundingClientRect();
        return {
          text: (label.textContent ?? "").trim(),
          labelLeft: Math.round(labelBox.left),
          controlLeft: Math.round(controlBox.left),
          // The TRACK, not the element's own box: `justify-items: start` makes
          // every grid item shrink to its content, so a label reads 36px wide
          // while sitting in a 132px column. The column is the convention.
          firstTrack: getComputedStyle(row).gridTemplateColumns.split(" ")[0],
          fontSize: getComputedStyle(label).fontSize,
          fontWeight: getComputedStyle(label).fontWeight,
        };
      });
    });

    expect(rows.length, "the chooser plus the chosen deck's own fields").toBeGreaterThan(3);
    const [first, ...rest] = rows;
    for (const row of rest) {
      expect(row.labelLeft, `“${row.text}” starts its label at a different x`).toBe(first.labelLeft);
      expect(row.controlLeft, `“${row.text}” starts its control at a different x`).toBe(first.controlLeft);
    }
    // 132px label column + 16px gutter, measured rather than asserted from the
    // stylesheet — a `.settings-row` that lost the grid would still carry the
    // class.
    expect(first.controlLeft - first.labelLeft).toBe(148);
    for (const row of rows) expect(row.firstTrack).toBe("132px");
    // The row label is a 13px/650 sub-header, which is part of the convention:
    // a panel whose labels are 11px next to another panel's 13px is the same
    // setting rendered at two different weights.
    for (const row of rows) {
      expect(row.fontSize).toBe("13px");
      expect(row.fontWeight).toBe("650");
    }
  });

  test("a bidi override in a settings-supplied host cannot reverse the deck label", async ({ page }) => {
    await openDecks(page, {
      remote: [{ host: `build${RLO}xob`, id: "deck0000000000aa", port: 22 }],
      selection: "local",
    });

    const label = page.getByTestId("deck-choice-deck0000000000aa");
    await expect(label).toBeVisible();

    // The strip happened: the codepoint is not in the DOM at all.
    const text = await label.locator("label > span").textContent();
    expect(text).not.toContain(RLO);
    expect(text).toBe("buildxob");

    // And the engine painted it left to right. The control below is what makes
    // this a measurement rather than a tautology: the same engine, the same
    // font, the same box, with the override actually present, must go the other
    // way — otherwise this test would pass in an engine that ignores bidi
    // entirely and would be measuring nothing.
    const stripped = await glyphAdvance(page, "deck-choice-deck0000000000aa");
    expect(stripped, "the rendered deck label did not advance left to right").toBeGreaterThan(0);

    const reversed = await page.evaluate((rlo) => {
      const host = document.querySelector('[data-testid="deck-choice-deck0000000000aa"] label > span')!;
      const probe = document.createElement("span");
      // Same font as the label it stands in for, but out of the layout so the
      // label's own `overflow: hidden` cannot clip it.
      probe.style.font = getComputedStyle(host).font;
      probe.style.position = "absolute";
      probe.style.left = "0";
      probe.style.top = "0";
      probe.style.whiteSpace = "nowrap";
      // The override opens the run, so every character after it is reversed —
      // which is what makes the comparison between offset 1 and the last offset
      // a measurement. A string with LTR text in FRONT of the override would
      // still advance left to right overall and would prove nothing.
      probe.textContent = `${rlo}buildxob`;
      document.body.appendChild(probe);
      const node = probe.firstChild as Text;
      const rectAt = (offset: number) => {
        const range = document.createRange();
        range.setStart(node, offset);
        range.setEnd(node, offset + 1);
        return range.getBoundingClientRect();
      };
      const advance = rectAt(node.data.length - 1).left - rectAt(1).left;
      probe.remove();
      return advance;
    }, RLO);
    expect(
      reversed,
      "the control did not reverse, so this engine reorders nothing and the test above proves nothing",
    ).toBeLessThan(0);
  });

  test("says what it cannot do here rather than inventing a verdict", async ({ page }) => {
    await openDecks(page, {
      remote: [{ host: "build-box", id: "deck0000000000aa", port: 22 }],
      selection: "deck0000000000aa",
    });

    await page.getByTestId("test-connection").click();

    // The browser preview has no socket, no ssh and no handshake. A synthesised
    // "reachable" would make a screen that can never be wrong, so the preview
    // reports a real state with a real sentence — which is also what makes the
    // result block assertable here at all.
    const result = page.getByTestId("deck-result");
    await expect(result).toBeVisible();
    await expect(result).toHaveAttribute("data-state", "ssh_unavailable");
    await expect(page.getByTestId("deck-result-message")).toContainText("Browser preview");
    // The panel is still a panel: the deck list and the fields are where they
    // were, which is the property a screen that blanked on a failure would lose.
    await expect(page.getByTestId("deck-choices")).toBeVisible();
    await expect(page.getByLabel("Host", { exact: true })).toHaveValue("build-box");
  });
});
