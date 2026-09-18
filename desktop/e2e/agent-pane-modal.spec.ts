import { expect, test } from "@playwright/test";

/**
 * `aria-modal="true"` made true, in the only tier that can tell.
 *
 * PRD #1105's security audit found the pane declaring itself modal over a base
 * screen that is deliberately still MOUNTED — so the rail, the tiles and, above
 * all, the `DeckSelector` that retargets the whole app to another deck kept
 * their place in the tab order behind a full-window dialog. Reaching the
 * selector from there and choosing a deck that runs an agent of the same
 * per-daemon monotonic id promoted THAT agent into the open pane, under the
 * same role and display text, with the user's keystrokes following it.
 *
 * The vitest tier can assert that the `inert` attribute is applied — it does,
 * in `src/AgentPaneDeckIdentity.test.tsx` — and can assert nothing about what the
 * attribute DOES, because jsdom implements no focus semantics for it. So the
 * claim that actually matters, that Tab cannot leave the pane, is only
 * observable here, against a real engine's tab order. Both engines, because
 * `inert` is a comparatively recent platform feature and WebKit is the half of
 * this pair that ships in the packaged app.
 */
test.describe("the agent pane is a real modal", () => {
  /**
   * Scenario: open Planner's pane from the deck, then press Tab twenty times.
   * Focus never lands outside the pane — not on the deck selector behind it,
   * not on the rail, not on another tile — and closing gives all of them back.
   */
  test("contains Tab inside the pane and gives the screen back on close", async ({ page }) => {
    await page.goto("/?fixture=1&state=fleet");

    const selector = page.getByTestId("deck-selector-toggle");
    await expect(selector).toBeVisible();
    // Reachable before the pane opens, which is what makes it worth fencing.
    await selector.focus();
    expect(await page.evaluate(() => document.activeElement?.getAttribute("data-testid"))).toBe("deck-selector-toggle");

    await page.getByRole("button", { name: "Open Planner agent" }).click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();

    // Focus was taken off the background control the click left it near, and
    // put inside the dialog.
    expect(await overlay.evaluate((node) => node.contains(document.activeElement))).toBe(true);

    for (let press = 0; press < 20; press += 1) {
      await page.keyboard.press("Tab");
      const inside = await overlay.evaluate((node) => {
        const active = document.activeElement;
        // `<body>` is where a browser parks focus when it wraps past the last
        // tab stop, and it is not a control — only a real element outside the
        // pane would be an escape.
        return active === null || active === document.body || node.contains(active);
      });
      expect(inside, `Tab #${press + 1} left the agent pane`).toBe(true);
    }

    // The deck selector is not merely unfocused: it is inert, so a click cannot
    // reach it either.
    expect(await selector.evaluate((node) => node.closest("[inert]") !== null)).toBe(true);

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    expect(await selector.evaluate((node) => node.closest("[inert]") !== null)).toBe(false);
    await selector.click();
    await expect(page.getByTestId("deck-selector-menu")).toBeVisible();
  });
});
