import path from "node:path";
import { fileURLToPath } from "node:url";

import { defineConfig, devices } from "@playwright/test";

/* PRD 2557 milestone 1 — MOCK-UP renderer, never merged: shoots the fleet view mock-up scene beside the Cards view of the same moment. */
const HERE = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.FLEET_MOCK_PORT ?? "4191");
const WEB_OUT = path.join(HERE, "test-results", "fleet-mock-web");

export default defineConfig({
  testDir: "./fleet-mock",
  testMatch: "*.shot.ts",
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  outputDir: path.join(HERE, "test-results", "fleet-mock"),
  use: {
    ...devices["Desktop Chrome"],
    baseURL: `http://127.0.0.1:${PORT}`,
    viewport: { width: 1280, height: 1640 },
    deviceScaleFactor: 1.5,
    locale: "en-US",
    timezoneId: "UTC",
    reducedMotion: "reduce",
  },
  projects: [{ name: "chromium" }],
  webServer: {
    command: `./node_modules/.bin/vite build --outDir "${WEB_OUT}" --emptyOutDir && exec ./node_modules/.bin/vite preview --outDir "${WEB_OUT}" --port ${PORT} --strictPort --host 127.0.0.1`,
    url: `http://127.0.0.1:${PORT}/`,
    reuseExistingServer: false,
    timeout: 180_000,
  },
});
