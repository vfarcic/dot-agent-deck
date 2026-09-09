import { defineConfig, devices } from "@playwright/test";

/**
 * The browser-level test tier for the desktop web build (issues #823, #836).
 *
 * **What it drives.** `webServer` below runs `vite build` and then serves the
 * resulting `desktop/dist` with `vite preview`, so every spec under `e2e/`
 * loads the same production bundle `pnpm build` produces — minified, targeted
 * at `safari15`, with the real stylesheet. It is deliberately not the dev
 * server and deliberately not the jsdom tree the vitest suite renders into.
 *
 * **Why two projects.** jsdom computes no geometry, so the ~200 vitest tests
 * can assert what React renders and nothing about what a reader sees. These run
 * in engines that lay out and paint. Chromium is the cheap one; WebKit is here
 * because the app SHIPS on WebKit — WebKitGTK under Tauri on Linux, WKWebView
 * on macOS — and, until this tier existed, every automated check of this app
 * ran in Chromium or in jsdom. Playwright's WebKit is its own build from the
 * WebKit source tree (the binary it launches on Linux is `minibrowser-gtk`,
 * WebKit's GTK port), so it answers WebCore and JavaScriptCore questions the
 * way WebKit answers them. It is NOT the distribution's WebKitGTK, not
 * WKWebView, and not a Tauri window; see `docs/develop/desktop-gui.md` for what
 * that leaves uncovered.
 *
 * **No retries, on purpose.** A retry turns a flake into a green tick and
 * deletes the evidence, which is the failure mode issue #807 documents for the
 * Rust e2e tier on a 4-vCPU runner. The specs wait on state — a locator being
 * visible, the legend printing a chosen column set, a count reaching fifteen —
 * and never on a timer, so a failure here is meant to be a fact rather than a
 * coin toss. `.config/nextest.toml` takes the same position for the Rust tiers.
 */
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  // A `test.only` left in a spec silently shrinks the suite to one test, and
  // the job still reports green. Fail the run instead, but only in CI, where
  // narrowing to one test is never what the author meant.
  forbidOnly: !!process.env.CI,
  retries: 0,
  // One worker in CI to start. Layout is not timing-dependent, but browser
  // startup on a 4-vCPU runner contends with itself, and this job has no honest
  // runs yet — the same reason `e2e-deterministic` was added advisory. Raising
  // it is a deliberate act once the job has a run history to argue from.
  workers: process.env.CI ? 1 : undefined,
  reporter: process.env.CI
    ? [["github"], ["html", { open: "never" }], ["list"]]
    : [["list"]],
  use: {
    baseURL: "http://127.0.0.1:4173",
    // Failure-only, so a green run writes nothing and a red one hands CI a
    // trace to open. The CI job uploads both directories.
    trace: "retain-on-failure",
    screenshot: "only-on-failure",
  },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
  webServer: {
    // Build then serve, as one command, so there is no ordering trap where a
    // stale `dist` is tested and the run still passes. `--strictPort` makes a
    // busy port an error rather than a silent move to another one, which would
    // leave `baseURL` pointing at whatever else is listening.
    command: "pnpm exec vite build && pnpm exec vite preview --port 4173 --strictPort --host 127.0.0.1",
    url: "http://127.0.0.1:4173/",
    reuseExistingServer: !process.env.CI,
    // The build is inside this command, so the default 60s is not enough on a
    // cold runner.
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
