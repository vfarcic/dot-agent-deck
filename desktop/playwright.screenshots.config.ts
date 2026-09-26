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

// Its own port, so a screenshot run and a `pnpm test:browser` run on the same
// machine do not serve each other's bundle.
const PORT = 4183;

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
  outputDir: path.join(HERE, "test-results", "docs-screenshots"),
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
  webServer: {
    // Always a fresh build, never a reused server: an image of a stale bundle
    // would be committed as documentation. See `playwright.config.ts` for why
    // the POSIX form uses the local `vite` bin and `exec`.
    command:
      process.platform === "win32"
        ? `pnpm exec vite build && pnpm exec vite preview --port ${PORT} --strictPort --host 127.0.0.1`
        : `./node_modules/.bin/vite build && exec ./node_modules/.bin/vite preview --port ${PORT} --strictPort --host 127.0.0.1`,
    url: `http://127.0.0.1:${PORT}/`,
    reuseExistingServer: false,
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
