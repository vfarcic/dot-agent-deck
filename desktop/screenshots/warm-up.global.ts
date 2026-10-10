import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { chromium } from "@playwright/test";

import { TUI_HTML_DIR } from "./support";

/**
 * Pays Chromium's first-use font cost once, before any shot's 30-second budget
 * is running — the screenshot generator's counterpart of the browser tier's
 * `e2e/webkit-warm-up.setup.ts`.
 *
 * **What it absorbs.** On 2026-10-10, under a load average of about 47, a
 * `cargo docs-screenshots --client tui` run timed out its FIRST shot
 * (`tui dashboard-empty`) after 30s inside `fontsSettled`, waiting on
 * `document.fonts.ready`, while the six shots after it took 0.2–1.8s. Run
 * again on its own, in a fresh browser, the same shot took 251ms; an earlier
 * single-scenario run's only shot took 9.3s. So the cost belongs to the
 * machine's first use of these fonts rather than to one browser process, and a
 * separate warm-up browser can pay it. The fonts are local (the TUI HTML names
 * an installed font stack and loads nothing over the network).
 *
 * **What it does.** It opens the first TUI page this run will rasterize, or a
 * page of monospace and proportional text when there is none (a desktop-only
 * run), waits for its fonts under a deliberately generous budget, and logs how
 * long that took. It writes no image and asserts nothing. A run where it fails
 * has a real problem, because three minutes is not a cold start.
 *
 * **Why `globalSetup` and not a setup project.** `cargo docs-screenshots`
 * selects shots with `--grep` anchored to scenario titles, which a setup
 * project's test would not match; `globalSetup` runs whatever the filter.
 */
export default async function warmUp(): Promise<void> {
  const started = Date.now();
  const first = TUI_HTML_DIR && fs.existsSync(TUI_HTML_DIR)
    ? fs.readdirSync(TUI_HTML_DIR).filter((file) => file.endsWith("-tui.html")).sort()[0]
    : undefined;
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    page.setDefaultTimeout(180_000);
    if (first) {
      await page.goto(pathToFileURL(path.join(TUI_HTML_DIR!, first)).href);
    } else {
      await page.setContent('<p style="font-family: monospace">0</p><p>0</p>');
    }
    await page.evaluate(async () => {
      await document.fonts.ready;
    });
  } finally {
    await browser.close();
  }
  console.log(`chromium warm-up: ${Date.now() - started}ms to settled fonts`);
}
