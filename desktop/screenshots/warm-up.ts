/**
 * The screenshot generator's font warm-up, apart from the Playwright it runs
 * under so `warm-up.test.ts` can drive it with a stand-in browser.
 * `warm-up.global.ts` wires it to Chromium and has the why.
 *
 * **Every failure warns and continues** (PR #1672 review). It is only a
 * warm-up: a browser that does not launch, a page that does not load, a fonts
 * wait that rejects or overruns its budget, a browser that does not close — each
 * ends in one warning and a resolved promise, never a throw. A throw here would
 * abort Playwright's `globalSetup` and with it every shot of the run, over a
 * step that asserts nothing; a real problem with the browser or the page fails
 * the first shot instead, and that failure names a shot.
 */

/** The slice of Playwright's `Page` the warm-up uses. */
export interface WarmUpPage {
  setDefaultTimeout(timeout: number): void;
  goto(url: string): Promise<unknown>;
  setContent(html: string): Promise<void>;
  evaluate(pageFunction: () => Promise<void>): Promise<void>;
}

/** The slice of Playwright's `Browser` the warm-up uses. */
export interface WarmUpBrowser {
  newPage(): Promise<WarmUpPage>;
  close(): Promise<void>;
}

/** Where the warm-up writes its one line. */
export interface WarmUpLog {
  log(message: string): void;
  warn(message: string): void;
}

/** How the warm-up ended; it never ends by throwing. */
export type WarmUpOutcome =
  | { kind: "settled"; ms: number }
  | { kind: "timed-out" }
  | { kind: "failed"; error: unknown };

export interface WarmUpOptions {
  /** Starts the browser. A rejection is a failed warm-up, not a thrown one. */
  launch: () => Promise<WarmUpBrowser>;
  /** The page to open, or `undefined` for a page of monospace and proportional text. */
  url: string | undefined;
  /** How long the page and its fonts may take, enforced here: `setDefaultTimeout` does not bound `evaluate`. */
  budgetMs: number;
  log?: WarmUpLog;
}

/** Opens one page, waits for its fonts under the budget, and says how that went. */
export async function warmUpWith({ launch, url, budgetMs, log = console }: WarmUpOptions): Promise<WarmUpOutcome> {
  const started = Date.now();
  let outcome: WarmUpOutcome;
  let browser: WarmUpBrowser | undefined;
  try {
    browser = await launch();
    outcome = await waitForFonts(browser, url, budgetMs, started);
  } catch (error) {
    outcome = { kind: "failed", error };
  }
  if (browser) {
    try {
      await browser.close();
    } catch (error) {
      // The fonts' answer stands; a browser that will not close is the shots'
      // problem to meet, and they launch their own.
      log.warn(`chromium warm-up: closing the warm-up browser failed (${describe(error)}); continuing`);
    }
  }
  switch (outcome.kind) {
    case "settled":
      log.log(`chromium warm-up: ${outcome.ms}ms to settled fonts`);
      break;
    case "timed-out":
      log.warn(`chromium warm-up: fonts had not settled after ${budgetMs}ms; continuing without the warm-up`);
      break;
    case "failed":
      log.warn(`chromium warm-up: failed (${describe(outcome.error)}); continuing without the warm-up`);
      break;
  }
  return outcome;
}

async function waitForFonts(
  browser: WarmUpBrowser,
  url: string | undefined,
  budgetMs: number,
  started: number,
): Promise<WarmUpOutcome> {
  const page = await browser.newPage();
  page.setDefaultTimeout(budgetMs);
  if (url) {
    await page.goto(url);
  } else {
    await page.setContent('<p style="font-family: monospace">0</p><p>0</p>');
  }
  const fonts = page.evaluate(async () => {
    await document.fonts.ready;
  });
  // When the budget wins, closing the browser rejects this evaluate; that
  // rejection is the expected end of an abandoned wait, not an error. When the
  // fonts lose some other way first, the race below sees the same rejection
  // and reports it.
  fonts.catch(() => undefined);
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    return await Promise.race([
      fonts.then((): WarmUpOutcome => ({ kind: "settled", ms: Date.now() - started })),
      new Promise<WarmUpOutcome>((resolve) => {
        timer = setTimeout(() => resolve({ kind: "timed-out" }), budgetMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
