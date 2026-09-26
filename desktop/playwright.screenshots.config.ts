import path from "node:path";
import { fileURLToPath } from "node:url";

import { defineConfig, devices } from "@playwright/test";

/**
 * The docs-screenshot generator (issue #1322) — NOT a test tier.
 *
 * `cargo docs-screenshots` runs this config; nothing else should. It is a
 * separate file from `playwright.config.ts` so the browser test tier CI runs
 * (`pnpm test:browser`, `testDir: ./e2e`) can never start writing into
 * `docs/img/`: that config does not look under `./screenshots` at all, and this
 * one looks nowhere else.
 *
 * Two jobs, one rasterizer:
 *
 * - **Desktop scenarios** load the production web build in fixture mode, with
 *   the clock frozen, and screenshot the page.
 * - **TUI scenarios** load an HTML rendering of a vt100 screen the real binary
 *   drew (written by `tests/e2e_docs_screenshots.rs`) and screenshot it. Both
 *   clients' PNGs therefore come out of the same Chromium with the same
 *   settings, which is what makes a rerun byte-identical on one machine.
 *
 * Chromium only: one engine is one set of pixels. WebKit is in the test tier
 * because the app ships on it; a docs image needs to be stable, not to prove
 * an engine. `docs/develop/docs-screenshots.md` is the maintainer page.
 */

const HERE = path.dirname(fileURLToPath(import.meta.url));

// `cargo docs-screenshots` sets this to "0" when no desktop scenario is
// selected (`WEB_BUILD_ENV` in `xtask/screenshots/src/lib.rs`), so a
// `--client tui` run rasterizes the TUI HTML without a `vite build` nothing
// would load. Unset — a hand-run of this config — builds.
const NEEDS_WEB = process.env.DAD_DOCS_SCREENSHOTS_WEB !== "0";

// `cargo docs-screenshots` picks a free localhost port per invocation that
// serves the web build (`PORT_ENV` in `xtask/screenshots/src/lib.rs`; none for a
// terminal-only run, whose `baseURL` is never loaded), so two concurrent runs
// never share a server; `--strictPort` below makes a run that lost the race for it
// fail instead of screenshotting another run's bundle. Unset — a hand-run of
// this config — falls back to 4183, which is not the browser test tier's port,
// so the two do not serve each other's bundle either.
const PORT = Number(process.env.DAD_DOCS_SCREENSHOTS_PORT ?? "4183");
if (!Number.isInteger(PORT) || PORT < 1 || PORT > 65_535) {
  throw new Error(`DAD_DOCS_SCREENSHOTS_PORT is not a port: ${process.env.DAD_DOCS_SCREENSHOTS_PORT}`);
}

// This invocation's private scratch directory (`RUN_DIR_ENV` in
// `xtask/screenshots/src/lib.rs`). The web build goes there rather than into
// the shared `dist/`, so a concurrent run's `vite build` cannot empty the
// bundle this run is serving, and so does Playwright's own output. Unset — a
// hand-run — keeps vite's `dist/` and a `test-results/` under `desktop/`.
const RUN_DIR = process.env.DAD_DOCS_SCREENSHOTS_RUN_DIR;
const WEB_OUT = RUN_DIR ? path.join(RUN_DIR, "web") : path.join(HERE, "dist");
// Double-quoted into the webServer command line, which is one shell word in
// both `sh` and `cmd.exe` unless the path itself holds a character either
// shell still interprets there, so such a path is refused rather than quoted.
const UNQUOTABLE = process.platform === "win32" ? /["%]/ : /["$`\\]/;
if (UNQUOTABLE.test(WEB_OUT)) {
  throw new Error(`cannot pass ${WEB_OUT} to the web server's command line; use a target dir without quotes, $, %, backticks or backslashes`);
}
const OUT_DIR_ARG = `--outDir "${WEB_OUT}"`;
const BUILD = `vite build ${OUT_DIR_ARG} --emptyOutDir`;
const PREVIEW = `vite preview ${OUT_DIR_ARG} --port ${PORT} --strictPort --host 127.0.0.1`;

export default defineConfig({
  testDir: "./screenshots",
  testMatch: "*.shot.ts",
  // One worker: the images do not depend on ordering, but two Chromium
  // processes rasterizing at once is the one variable left that this run can
  // simply not introduce.
  workers: 1,
  fullyParallel: false,
  retries: 0,
  forbidOnly: true,
  reporter: [["list"]],
  // No trace or failure screenshot: this run's only output is the docs images,
  // and a `test-results/` dropped next to them is noise.
  outputDir: RUN_DIR ? path.join(RUN_DIR, "test-results") : path.join(HERE, "test-results", "docs-screenshots"),
  use: {
    ...devices["Desktop Chrome"],
    baseURL: `http://127.0.0.1:${PORT}`,
    viewport: { width: 1280, height: 800 },
    deviceScaleFactor: 2,
    colorScheme: "dark",
    locale: "en-US",
    timezoneId: "UTC",
    reducedMotion: "reduce",
    trace: "off",
    screenshot: "off",
    video: "off",
  },
  projects: [{ name: "chromium" }],
  webServer: NEEDS_WEB ? {
    // Always a fresh build, never a reused server: an image of a stale bundle
    // would be committed as documentation. See `playwright.config.ts` for why
    // the POSIX form uses the local `vite` bin and `exec`.
    command:
      process.platform === "win32"
        ? `pnpm exec ${BUILD} && pnpm exec ${PREVIEW}`
        : `./node_modules/.bin/${BUILD} && exec ./node_modules/.bin/${PREVIEW}`,
    url: `http://127.0.0.1:${PORT}/`,
    reuseExistingServer: false,
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
  } : undefined,
});
