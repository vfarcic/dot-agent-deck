import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { expect, test } from "@playwright/test";

import { SHOT_OPTIONS, TUI_HTML_DIR, fontsSettled, imagePath } from "./support";

/**
 * The rasterizer for the TUI half of the docs screenshots (issue #1322).
 *
 * `tests/e2e_docs_screenshots.rs` drives the real binary and writes each frame
 * as `<scenario>-tui.html` — every cell with its colours and attributes, from
 * `xtask_screenshots::terminal_html`. This screenshots the page's `#terminal`
 * element, so the PNG is the frame at the size the grid lays out to, in the
 * same Chromium and scale factor as the desktop images.
 *
 * One test per HTML file present, titled `tui <scenario>`; with no directory
 * set (a desktop-only run) there are none.
 */

const SUFFIX = "-tui.html";

const scenarios = TUI_HTML_DIR && fs.existsSync(TUI_HTML_DIR)
  ? fs.readdirSync(TUI_HTML_DIR).filter((file) => file.endsWith(SUFFIX)).sort().map((file) => file.slice(0, -SUFFIX.length))
  : [];

for (const scenario of scenarios) {
  test(`tui ${scenario}`, async ({ page }) => {
    await page.goto(pathToFileURL(path.join(TUI_HTML_DIR!, `${scenario}${SUFFIX}`)).href);
    const terminal = page.locator("#terminal");
    await expect(terminal).toBeVisible();
    await fontsSettled(page);
    await terminal.screenshot({ ...SHOT_OPTIONS, path: imagePath(scenario, "tui") });
  });
}
