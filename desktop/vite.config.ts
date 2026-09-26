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
    css: true,
  },
});
