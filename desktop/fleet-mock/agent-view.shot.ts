import fs from "node:fs";
import path from "node:path";

import { expect, test, type Page } from "@playwright/test";

/* PRD 2557 — MOCK-UP renderer, round 2, never merged: the fleet view composed entirely by an agent (two independent compositions of the same moment), each theme. */
const OUT = path.resolve(process.env.FLEET_MOCK_OUT ?? "test-results/fleet-mock-out");
const THEMES = (process.env.FLEET_MOCK_THEMES ?? "light,dark").split(",") as ("light" | "dark")[];
const CAPTURED = (JSON.parse(fs.readFileSync(path.join(import.meta.dirname, "..", "src", "data", "fleetMock.live.json"), "utf8")) as { captured_at: string }).captured_at;
const SETTINGS_KEY = "dot-agent-deck.desktop-settings";
const HEIGHT = Number(process.env.FLEET_MOCK_HEIGHT ?? "2200");
const COMPOSERS = (process.env.FLEET_MOCK_COMPOSERS ?? "a,b").split(",");
/* Round 2 (`a`, `b`) is the `agent` view; round 3 (`dispatcher`, `standin-N`) the `surface` view. */
const viewOf = (composer: string) => (composer === "a" || composer === "b" ? "agent" : "surface");

test.use({ viewport: { width: 1280, height: HEIGHT } });

async function shoot(page: Page, composer: string, theme: "light" | "dark"): Promise<string> {
  await page.clock.setFixedTime(new Date(CAPTURED));
  await page.emulateMedia({ colorScheme: theme });
  await page.addInitScript(([key, mode]) => {
    window.localStorage.setItem(key, JSON.stringify({ version: 1, appearance: { mode }, zoom: { level: 1 } }));
  }, [SETTINGS_KEY, theme] as const);
  await page.goto(`/?fixture=1&state=docs&cards=fleet-mock&mock=composed&view=${viewOf(composer)}&composer=${composer}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  if (!(await page.getByTestId("overview-mode-switch").isVisible())) await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("fleet-view")).toBeVisible();
  await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
  await page.mouse.move(0, HEIGHT - 1);
  await page.evaluate(async () => { await document.fonts.ready; });
  const file = path.join(OUT, `agent-${composer}-${theme}.png`);
  // Cropped to the composition: the dashboard shell is viewport-high, so the page itself never grows.
  const bottom = await page.getByTestId("fleet-view").evaluate((el) => el.getBoundingClientRect().bottom);
  await page.screenshot({ path: file, animations: "disabled", caret: "hide", style: ".fixture-bar { display: none !important; }", clip: { x: 0, y: 0, width: 1280, height: Math.min(HEIGHT, Math.max(900, Math.ceil(bottom) + 24)) } });
  return file;
}

for (const composer of COMPOSERS) {
  for (const theme of THEMES) {
    test(`agent-view ${composer} ${theme}`, async ({ page }) => {
      fs.mkdirSync(OUT, { recursive: true });
      await shoot(page, composer, theme);
    });
  }
}
