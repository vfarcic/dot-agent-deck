import { expect, test, type Locator, type Page } from "@playwright/test";
import { openOverview } from "./support/overview";

interface TerminalLayout {
  paneContentWidth: number;
  viewportWidth: number;
  viewportContentWidth: number;
  hostWidth: number;
  hostHeight: number;
  fitAvailableWidth: number;
  fitAvailableHeight: number;
  cellWidth: number;
  cellHeight: number;
  proposedCols: number;
  proposedRows: number;
}

interface CapturedTerminal {
  element?: HTMLElement;
  options?: { scrollback?: number; overviewRuler?: { width?: number } };
  _core?: {
    _renderService?: {
      dimensions?: { css?: { cell?: { width?: number; height?: number } } };
    };
  };
}

/**
 * Capture xterm's measured cell size without reading its final cols/rows. The
 * final grid may be restored to the daemon-applied size after every fit, while
 * the cell metrics and the browser boxes still describe the proposal exactly.
 */
async function captureTerminalMetrics(page: Page): Promise<void> {
  await page.addInitScript(() => {
    const originalSet = Map.prototype.set;
    const terminals: unknown[] = [];
    Object.defineProperty(window, "__dadE2eTerminals", { value: terminals });
    Object.defineProperty(Map.prototype, "set", {
      configurable: true,
      writable: true,
      value(this: Map<unknown, unknown>, key: unknown, value: unknown) {
        if (value && typeof value === "object") {
          const candidate = value as { element?: unknown; cols?: unknown; rows?: unknown };
          if (candidate.element instanceof HTMLElement && typeof candidate.cols === "number" && typeof candidate.rows === "number") {
            terminals.push(value);
          }
        }
        return Reflect.apply(originalSet, this, [key, value]);
      },
    });
  });
}

/** Replicate FitAddon's proposal from browser layout, never xterm's applied grid. */
async function terminalLayout(viewport: Locator): Promise<TerminalLayout> {
  await expect(viewport.locator(".xterm-screen")).toBeVisible();
  return viewport.evaluate((root) => {
    const terminalList = (window as typeof window & { __dadE2eTerminals?: CapturedTerminal[] }).__dadE2eTerminals ?? [];
    const terminal = terminalList.find((candidate) => candidate.element && root.contains(candidate.element));
    const host = root.querySelector<HTMLElement>(".terminal-host");
    const pane = root.closest<HTMLElement>(".agent-panel");
    const xterm = terminal?.element;
    const cell = terminal?._core?._renderService?.dimensions?.css?.cell;
    if (!terminal || !host || !pane || !xterm || !cell?.width || !cell.height) {
      throw new Error("terminal layout is not ready for measurement");
    }

    const viewportBox = root.getBoundingClientRect();
    const hostBox = host.getBoundingClientRect();
    const paneBox = pane.getBoundingClientRect();
    const viewportStyle = getComputedStyle(root);
    const hostStyle = getComputedStyle(host);
    const xtermStyle = getComputedStyle(xterm);
    const paddingX = parseFloat(viewportStyle.paddingLeft) + parseFloat(viewportStyle.paddingRight);
    const scrollbarWidth = terminal.options?.scrollback === 0 ? 0 : terminal.options?.overviewRuler?.width || 14;
    const fitAvailableWidth = Math.max(0, parseInt(hostStyle.width))
      - parseInt(xtermStyle.paddingLeft)
      - parseInt(xtermStyle.paddingRight)
      - scrollbarWidth;
    const fitAvailableHeight = parseInt(hostStyle.height)
      - parseInt(xtermStyle.paddingTop)
      - parseInt(xtermStyle.paddingBottom);

    return {
      paneContentWidth: paneBox.width,
      viewportWidth: viewportBox.width,
      viewportContentWidth: viewportBox.width - paddingX,
      hostWidth: hostBox.width,
      hostHeight: hostBox.height,
      fitAvailableWidth,
      fitAvailableHeight,
      cellWidth: cell.width,
      cellHeight: cell.height,
      proposedCols: Math.max(2, Math.floor(fitAvailableWidth / cell.width)),
      proposedRows: Math.max(1, Math.floor(fitAvailableHeight / cell.height)),
    };
  });
}

function expectPaneTerminalToGrow(tile: TerminalLayout, pane: TerminalLayout, origin: string): void {
  expect.soft(
    pane.viewportWidth,
    `${origin} pane terminal viewport does not span the pane content`,
  ).toBeCloseTo(pane.paneContentWidth, 0);
  expect.soft(
    pane.hostWidth,
    `${origin} pane xterm host does not receive the terminal viewport's content width`,
  ).toBeCloseTo(pane.viewportContentWidth, 0);
  expect.soft(
    pane.hostWidth,
    `${origin} pane xterm host stays at the tile's ${tile.hostWidth}px width`,
  ).toBeGreaterThan(tile.hostWidth);
  // Rows are the control for the reported width-only failure: both axes are
  // asserted separately so a narrow pane reports columns red and rows green.
  expect.soft(
    pane.proposedRows,
    `${origin} pane fit proposal keeps the tile's ${tile.proposedRows} rows`,
  ).toBeGreaterThan(tile.proposedRows);
  expect.soft(
    pane.proposedCols,
    `${origin} pane fit proposal keeps the tile's ${tile.proposedCols} columns despite ${pane.fitAvailableWidth}px of fit width`,
  ).toBeGreaterThan(tile.proposedCols);
}

/** Assert the pane covers the browser viewport rather than merely being larger than a tile. */
async function expectToOccupyWindow(page: Page, overlay: Locator): Promise<void> {
  const box = await overlay.boundingBox();
  const viewport = await page.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }));
  expect(box, "the agent pane has no browser layout box").not.toBeNull();
  expect(Math.abs(box!.x)).toBeLessThanOrEqual(1);
  expect(Math.abs(box!.y)).toBeLessThanOrEqual(1);
  expect(Math.abs(box!.width - viewport.width)).toBeLessThanOrEqual(1);
  expect(Math.abs(box!.height - viewport.height)).toBeLessThanOrEqual(1);
}

test.describe("agent pane overlay", () => {
  /**
   * Scenario: open Planner from the deck in the built browser bundle. Its
   * dialog covers the viewport while the grid and another tile remain mounted;
   * its xterm host and independently computed fit proposal grow in both axes.
   * Escape removes the dialog and exposes the same deck again.
   */
  test("occupies the window over a still-mounted deck and closes on Escape", async ({ page }) => {
    await captureTerminalMetrics(page);
    await page.goto("/?fixture=1&state=connected");

    const grid = page.locator(".agent-grid");
    const otherTile = page.getByTestId("agent-tile-builder");
    await expect(grid).toBeVisible();
    const tileLayout = await terminalLayout(page.getByTestId("terminal-planner"));
    const originalGrid = await grid.elementHandle();
    expect(originalGrid, "the deck has no grid node to preserve").not.toBeNull();
    await page.getByRole("button", { name: "Open Planner agent" }).click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toHaveAttribute("role", "dialog");
    await expect(overlay).toHaveAttribute("aria-label", "Planner agent");
    await expect(overlay).toHaveAttribute("aria-modal", "true");
    await expect(overlay.getByRole("heading", { name: "Planner" })).toBeVisible();
    await expect(grid).toBeAttached();
    await expect(otherTile).toBeAttached();
    expect(await originalGrid!.evaluate((node) => node.isConnected)).toBe(true);
    expect(await page.evaluate((node) => document.querySelector(".agent-grid") === node, originalGrid)).toBe(true);
    await expectToOccupyWindow(page, overlay);
    const paneLayout = await terminalLayout(overlay.locator(".terminal-viewport"));
    expectPaneTerminalToGrow(tileLayout, paneLayout, "deck-origin");

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    await expect(grid).toBeVisible();
    await expect(page.getByTestId("agent-tile-planner")).toBeVisible();
  });

  /**
   * Scenario: open Planner from its terminal-free overview card in the built
   * browser bundle. The overview remains mounted below the full-window pane,
   * whose terminal width and fit proposal grow like the deck-origin pane;
   * Escape returns to that overview rather than to the deck.
   */
  test("keeps the overview mounted below the pane and returns there on Escape", async ({ page }) => {
    await captureTerminalMetrics(page);
    await page.goto("/?fixture=1&state=connected");
    const tileLayout = await terminalLayout(page.getByTestId("terminal-planner"));
    await openOverview(page, "connected");

    const overview = page.getByTestId("overview-table-region");
    await expect(page.locator(".terminal-viewport")).toHaveCount(0);
    const originalOverview = await overview.elementHandle();
    expect(originalOverview, "the overview has no table-region node to preserve").not.toBeNull();
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overview).toBeAttached();
    expect(await originalOverview!.evaluate((node) => node.isConnected)).toBe(true);
    expect(await page.evaluate((node) => document.querySelector('[data-testid="overview-table-region"]') === node, originalOverview)).toBe(true);
    await expectToOccupyWindow(page, overlay);
    const paneLayout = await terminalLayout(overlay.locator(".terminal-viewport"));
    expectPaneTerminalToGrow(tileLayout, paneLayout, "overview-origin");

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    await expect(overview).toBeVisible();
    await expect(page.getByRole("button", { name: "Open Plan / architecture agent" })).toBeVisible();
  });

  /**
   * Scenario: choose All Decks, open build-box's same-id agent while the local
   * deck remains the selected deck, and inspect the built bundle. The remote
   * pane has a real terminal and input element immediately; it never degrades
   * to an explanation or an empty terminal because its deck is not selected.
   */
  test("opens a non-selected deck's overview agent with a live terminal", async ({ page }) => {
    await openOverview(page, "fleet");
    await page.getByTestId("deck-selector-toggle").click();
    await page.getByTestId("deck-selector-option-all").click();
    await expect(page.getByTestId("deck-selector-current")).toHaveText("All Decks");

    await page.getByRole("button", { name: "Open Nightly build watch agent" }).click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay.getByRole("heading", { name: "Builder" })).toBeVisible();
    await expect(overlay.locator(".agent-terminal-stack")).toHaveAttribute("data-terminal-state", "attached");
    await expect(overlay.locator(".terminal-viewport")).toBeVisible();
    await expect(overlay.getByLabel("Builder terminal input")).toBeAttached();
    await expect(overlay.getByTestId("terminal-absent-planner")).toHaveCount(0);
    await expect(page.getByTestId("deck-selector-current")).toHaveText("All Decks");
  });
});
