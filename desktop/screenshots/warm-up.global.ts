import fs from "node:fs";
import path from "node:path";
import { pathToFileURL } from "node:url";

import { chromium } from "@playwright/test";

import { TUI_HTML_DIR } from "./support";

/** How long the warm-up waits for a page and its fonts before giving up on it. */
const WARM_UP_BUDGET_MS = 180_000;

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
 * long that took. It writes no image and asserts nothing. When the fonts have
 * not settled within the budget it warns and lets the run go on, because it is
 * only a warm-up: three minutes is not a cold start, so the shots that follow
 * are likely to fail on their own budgets, and that failure names a shot.
 *
 * **The budget is enforced here, not by Playwright** (PR #1672 review).
 * `page.setDefaultTimeout` bounds actions and navigations, not
 * `page.evaluate`, so an `evaluate` awaiting `document.fonts.ready` would wait
 * for as long as the fonts never settle — and `globalSetup` runs outside every
 * test's timeout, so nothing else would stop it.
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
    page.setDefaultTimeout(WARM_UP_BUDGET_MS);
    if (first) {
      await page.goto(pathToFileURL(path.join(TUI_HTML_DIR!, first)).href);
    } else {
      await page.setContent('<p style="font-family: monospace">0</p><p>0</p>');
    }
    const fonts = page.evaluate(async () => {
      await document.fonts.ready;
    });
    // When the budget wins, closing the browser below rejects this evaluate;
    // that rejection is the expected end of an abandoned wait, not an error.
    fonts.catch(() => undefined);
    let timer: ReturnType<typeof setTimeout> | undefined;
    const settled = await Promise.race([
      fonts.then(() => true),
      new Promise<false>((resolve) => {
        timer = setTimeout(() => resolve(false), WARM_UP_BUDGET_MS);
      }),
    ]).finally(() => clearTimeout(timer));
    if (!settled) {
      console.warn(`chromium warm-up: fonts had not settled after ${WARM_UP_BUDGET_MS}ms; continuing without the warm-up`);
      return;
    }
  } finally {
    await browser.close();
  }
  console.log(`chromium warm-up: ${Date.now() - started}ms to settled fonts`);
}
