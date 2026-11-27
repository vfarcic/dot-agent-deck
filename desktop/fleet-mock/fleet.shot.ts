import fs from "node:fs";
import path from "node:path";

import { expect, test, type Page } from "@playwright/test";

/* Kept in step with `src/data/fleetMock.ts` (which Node cannot import: it imports JSON for vite). */
type FleetMockState = "first-open" | "composed" | "unpredicted" | "delivered" | "safe-to-close" | "stale";
const FLEET_MOCK_STATES: FleetMockState[] = ["first-open", "composed", "unpredicted", "delivered", "safe-to-close", "stale"];
const CAPTURED = (JSON.parse(fs.readFileSync(path.join(import.meta.dirname, "..", "src", "data", "fleetMock.live.json"), "utf8")) as { captured_at: string }).captured_at;
const SHIFT: Partial<Record<FleetMockState, number>> = { delivered: 2, stale: 25 };
const fleetMockNow = (state: FleetMockState) => new Date(new Date(CAPTURED).getTime() + (SHIFT[state] ?? 0) * 60_000).toISOString();

/* PRD 2557 milestone 1 — MOCK-UP renderer, never merged. Each state, each theme: the fleet view and the Cards view of the same moment, then the two side by side. */
const OUT = path.resolve(process.env.FLEET_MOCK_OUT ?? "test-results/fleet-mock-out");
const THEMES = (process.env.FLEET_MOCK_THEMES ?? "light,dark").split(",") as ("light" | "dark")[];
const ONLY = process.env.FLEET_MOCK_STATES?.split(",") as FleetMockState[] | undefined;
const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

const CAPTION: Record<FleetMockState, string> = {
  "first-open": "1 · First open: the deck's own facts, and \"asked the dispatcher\"",
  composed: "2 · The dispatcher's composed view",
  unpredicted: "3 · Next steps nobody could have predicted (outlined: an annotation, not part of the design)",
  delivered: "4 · A delivered action, back to \"working on it\"",
  "safe-to-close": "5 · \"Safe to close\" and its one close action",
  stale: "6 · A dispatcher that stopped answering",
};

async function useTheme(page: Page, theme: "light" | "dark"): Promise<void> {
  await page.emulateMedia({ colorScheme: theme });
  await page.addInitScript(([key, mode]) => {
    window.localStorage.setItem(key, JSON.stringify({ version: 1, appearance: { mode }, zoom: { level: 1 } }));
  }, [SETTINGS_KEY, theme] as const);
}

async function shoot(page: Page, state: FleetMockState, theme: "light" | "dark", view: "fleet" | "cards"): Promise<string> {
  await page.clock.setFixedTime(new Date(fleetMockNow(state)));
  await useTheme(page, theme);
  await page.goto(`/?fixture=1&state=docs&cards=fleet-mock&mock=${state}&view=${view}`);
  await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  // The dashboard is the landing screen; its rail control is clicked only when the page is not already on it (a dialog the scene opens fences the rail).
  if (!(await page.getByTestId("overview-mode-switch").isVisible())) await page.getByTestId("open-overview").click();
  if (view === "cards") {
    await page.getByTestId("overview-mode-cards").click();
    await expect(page.locator(".agent-card").first()).toBeVisible();
    await page.getByRole("button", { name: "Open dispatcher's tree" }).click({ timeout: 5000 });
    await expect(page.locator("article[data-card-key]")).toHaveCount(4);
  } else {
    await expect(page.getByTestId("fleet-view")).toBeVisible();
    if (state === "safe-to-close") {
      await expect(page.getByRole("alertdialog")).toBeVisible();
    }
    if (state === "safe-to-close") await expect(page.getByText("Keep the worktree")).toBeVisible();
  }
  await expect(page.locator("html")).toHaveAttribute("data-theme", theme);
  await page.mouse.move(0, page.viewportSize()!.height - 1);
  await page.evaluate(async () => { await document.fonts.ready; });
  const file = path.join(OUT, "parts", `${state}-${view}-${theme}.png`);
  /* State 3 only: an annotation, not product — the items whose next steps no fixed field could have offered are outlined, the rest dimmed. */
  const annotate = state === "unpredicted" && view === "fleet"
    ? " .fleet-item:not([data-testid=fleet-item-n3]):not([data-testid=fleet-item-n5]):not([data-testid=fleet-item-n6]), .fleet-section.tone-done, .fleet-section.tone-working { opacity: .35; } [data-testid=fleet-item-n3], [data-testid=fleet-item-n5], [data-testid=fleet-item-n6] { outline: 3px solid #d63384; outline-offset: 3px; }"
    : "";
  await page.screenshot({ path: file, animations: "disabled", caret: "hide", style: `.fixture-bar { display: none !important; }${annotate}` });
  return file;
}

for (const state of FLEET_MOCK_STATES.filter((s) => !ONLY || ONLY.includes(s))) {
  for (const theme of THEMES) {
    test(`fleet-mock ${state} ${theme}`, async ({ page, browser }) => {
      fs.mkdirSync(path.join(OUT, "parts"), { recursive: true });
      const fleet = await shoot(page, state, theme, "fleet");
      const cardsPage = await browser.newPage({ viewport: page.viewportSize()!, deviceScaleFactor: 1.5 });
      const cards = await shoot(cardsPage, state, theme, "cards");
      await cardsPage.close();
      const img = (file: string) => `data:image/png;base64,${fs.readFileSync(file).toString("base64")}`;
      const dark = theme === "dark";
      const composite = await browser.newPage({ viewport: { width: 2600, height: 1800 }, deviceScaleFactor: 1 });
      await composite.setContent(`<!doctype html><html><body style="margin:0;padding:16px 20px 20px;background:${dark ? "#0d1110" : "#d9d6cd"};font:600 20px system-ui,sans-serif;color:${dark ? "#d8ded9" : "#1d2522"}">
        <div style="margin:0 0 12px;font-size:24px;font-weight:800">${CAPTION[state]} <span style="font-weight:500;opacity:.7">· ${theme}</span></div>
        <div style="display:flex;gap:20px;align-items:flex-start">
          <figure style="margin:0;width:1270px"><figcaption style="margin:0 0 6px">Fleet view (mock-up)</figcaption><img style="width:1270px;display:block;border:1px solid #777" src="${img(fleet)}"></figure>
          <figure style="margin:0;width:1270px"><figcaption style="margin:0 0 6px">Cards view, same moment (real components, live cards)</figcaption><img style="width:1270px;display:block;border:1px solid #777" src="${img(cards)}"></figure>
        </div></body></html>`);
      await composite.screenshot({ path: path.join(OUT, `${state}-${theme}.png`), fullPage: true });
      await composite.close();
    });
  }
}
