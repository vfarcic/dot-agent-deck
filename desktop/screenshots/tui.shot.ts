import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { expect, test } from "@playwright/test";

import { SHOT_OPTIONS, THEMES, TUI_HTML_DIR, fontsSettled, imagePath } from "./support";

/**
 * The rasterizer for the TUI half of the docs screenshots (issue #1322).
 *
 * `tests/e2e_docs_screenshots.rs` drives the real binary and writes each frame
 * as `<scenario>-tui.html` — every cell with its colours and attributes, from
 * `xtask_screenshots::terminal_html`. This screenshots the page's `#terminal`
 * element, so the PNG is the frame at the size the grid lays out to, in the
 * same Chromium and scale factor as the desktop images.
 *
 * One test per HTML file present and theme of the run, titled
 * `tui <scenario> <theme>`; with no directory set (a desktop-only run) there
 * are none. The page carries both terminal palettes (PR #2491): the light one
 * under `prefers-color-scheme: light` or `data-theme="light"`, so each test
 * sets both and checks the ground it got.
 */

const SUFFIX = "-tui.html";

const scenarios = TUI_HTML_DIR && fs.existsSync(TUI_HTML_DIR)
  ? fs.readdirSync(TUI_HTML_DIR).filter((file) => file.endsWith(SUFFIX)).sort().map((file) => file.slice(0, -SUFFIX.length))
  : [];

for (const scenario of scenarios) {
  for (const theme of THEMES) {
    test(`tui ${scenario} ${theme}`, async ({ page }) => {
      await page.emulateMedia({ colorScheme: theme });
      await page.goto(pathToFileURL(path.join(TUI_HTML_DIR!, `${scenario}${SUFFIX}`)).href);
      await page.evaluate((mode) => document.documentElement.setAttribute("data-theme", mode), theme);
      const terminal = page.locator("#terminal");
      await expect(terminal).toBeVisible();
      // The page's own palette variable, resolved: light on a light ground, dark on a dark one.
      const ground = await terminal.evaluate((node) => getComputedStyle(node).backgroundColor);
      const channels = ground.match(/\d+/g)!.slice(0, 3).map(Number);
      const light = channels.reduce((sum, value) => sum + value, 0) / 3 > 128;
      expect(light, `the ${theme} terminal's ground (${ground})`).toBe(theme === "light");
      await fontsSettled(page);
      await terminal.screenshot({ ...SHOT_OPTIONS, path: imagePath(scenario, "tui", theme) });
    });
  }
}
