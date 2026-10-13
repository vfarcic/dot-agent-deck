import { configDefaults, defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_ENV_"],
  build: {
    target: process.env.TAURI_ENV_PLATFORM === "windows" ? "chrome105" : "safari15",
    minify: process.env.TAURI_ENV_DEBUG ? false : "esbuild",
    sourcemap: Boolean(process.env.TAURI_ENV_DEBUG),
  },
  test: {
    environment: "jsdom",
    // The browser tier's specs live in `e2e/` and are driven by Playwright, not
    // by vitest. Vitest's default `include` matches `**/*.spec.ts`, so without
    // this it would collect them, fail to resolve `@playwright/test`'s runner
    // globals and redden `pnpm test` for a reason that has nothing to do with
    // the app. Spread rather than replace: the defaults are
    // `**/node_modules/**` and `**/.git/**` (verified identical in vitest 4.1.11
    // and 5.0.0), and dropping them would collect specs out of `node_modules`.
    //
    // `driver/` is the same shape one rung up (issue #953): its `*.test.ts`
    // files are `node:test` suites that launch the real window through
    // tauri-driver, run by `pnpm test:driver`, and vitest would find no suite in
    // them.
    exclude: [...configDefaults.exclude, "e2e/**", "driver/**"],
    setupFiles: ["./src/test/setup.ts"],
    // Bound concurrent jsdom bootstraps on shared development hosts. Letting
    // the pool use every available core timed out 15 worker startups while
    // Rust builds were active, before those files could run any assertions.
    maxWorkers: 4,
    // A ceiling, not a pace: a passing test waits no longer for it. vitest's
    // default of 5s is an idle machine's budget, and under the parallel load
    // this repo's agents put on one box (load averages of 90 to 130 on 16
    // cores, measured 2026-10-01) nine `App.test.tsx` scenarios that pass
    // alone ran 5–10s and timed out. A timed-out test is also not cancelled:
    // its remaining steps keep firing into `document.body` while the next test
    // renders, so one timeout can surface as a different failure in a
    // neighbour (those runs also failed "Found multiple elements" beside the
    // timeouts). Testing Library's own wait budget is raised beside this, in
    // `src/test/setup.ts`.
    testTimeout: 30_000,
    css: true,
  },
});
