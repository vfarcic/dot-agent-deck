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
    // `vmForks`, not the default `forks`: it starts one worker process per
    // lane instead of one per file. That is a weaker isolation boundary than
    // `forks` — the files a worker runs share its process, each in a separate
    // VM context, so module-level globals are per file but anything held by
    // the process (native modules, `process` state, leaked timers) is not. The reason is a start timeout this config cannot
    // raise: vitest 5.0.3 fails a worker that does not report "started"
    // within a hard-coded 60s (`START_TIMEOUT` in its `PoolRunner`), and
    // under `forks` that window covers importing jsdom and creating its
    // window, which was 95% of the measured start time (114s of 120.6s summed
    // over 59 starts, 2026-10-10). On this repo's loaded box (load 60-90,
    // swapping, IO pressure ~80%) one jsdom setup averaged 14.5s and a full
    // run died with 15 "Failed to start forks worker ... Timeout waiting for
    // worker to respond" errors and no failed assertion. Under `vmForks` the
    // environment is set up when a file runs, outside that window: measured
    // 15 starts averaging 0.2s against 59 averaging 1.0-2.0s, with the same
    // 2021 tests passing and a wall clock no slower in alternating runs.
    pool: "vmForks",
    css: true,
  },
});
