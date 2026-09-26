import path from "node:path";
import { fileURLToPath } from "node:url";

import { expect, test, type Page } from "@playwright/test";

/**
 * Shared settings for the docs-screenshot generator (issue #1322). Read by
 * `desktop.shot.ts` and `tui.shot.ts`; `playwright.screenshots.config.ts`
 * fixes the browser, viewport and scale factor they both run under.
 */

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** Where the PNGs land. `cargo docs-screenshots` always sets it; the default is the docs tree. */
export const OUT_DIR = path.resolve(process.env.DAD_DOCS_SCREENSHOTS_OUT ?? path.join(HERE, "..", "..", "docs", "img"));

/** Where the TUI capture wrote its `<scenario>-tui.html` files, when this run includes any. */
export const TUI_HTML_DIR = process.env.DAD_DOCS_SCREENSHOTS_TUI_HTML;

/**
 * The instant every desktop scenario's clock is frozen at. The fixture computes
 * each agent's age as `Date.now()` minus a fixed number of minutes, and the
 * overview prints ages relative to `Date.now()`, so with the clock held here
 * every "3m" and "2h" reads the same on every run.
 */
export const FROZEN_NOW = new Date("2026-09-01T12:00:00Z");

/** The PNG path for one scenario and client — `<scenario>-<client>.png`. */
export function imagePath(scenario: string, client: "tui" | "desktop"): string {
  return path.join(OUT_DIR, `${scenario}-${client}.png`);
}

/**
 * The options every screenshot is taken with. Animations are finished rather
 * than caught mid-way, the text caret is hidden, and the fixture's "DEMO DATA"
 * banner is removed — it tells a developer the screen is not a live deck, which
 * a docs reader does not need to be told about a picture.
 */
export const SHOT_OPTIONS = {
  animations: "disabled",
  caret: "hide",
  scale: "device",
  style: ".fixture-bar { display: none !important; }",
} as const;

/** Wait until every web font the page asked for has loaded, so text never renders in a fallback. */
export async function fontsSettled(page: Page): Promise<void> {
  await page.evaluate(async () => {
    await document.fonts.ready;
  });
}

/**
 * Register one desktop scenario. The test title is `desktop <name>`, which is
 * what `cargo docs-screenshots --scenario` selects by, and the name must be
 * listed in `xtask/screenshots/src/scenarios.rs` — a unit test there fails when
 * the two disagree. `prepare` puts the screen in the state the image shows and
 * waits for it; it must wait on state, never on a timer.
 */
export function desktopScenario(name: string, prepare: (page: Page) => Promise<void>): void {
  test(`desktop ${name}`, async ({ page }) => {
    await page.clock.setFixedTime(FROZEN_NOW);
    await prepare(page);
    // Park the pointer where nothing reacts to it, so a hover tooltip from the
    // last click is not in the picture.
    await page.mouse.move(0, page.viewportSize()!.height - 1);
    await expect(page.locator("[role=tooltip]:visible")).toHaveCount(0);
    await fontsSettled(page);
    await page.screenshot({ ...SHOT_OPTIONS, path: imagePath(name, "desktop") });
  });
}
