import { expect, test, type Page } from "@playwright/test";
import { enterDeck, selectOverview } from "./support/overview";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

async function open(page: Page, state: string, utterance?: string) {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  await page.goto(`/?fixture=1&state=${state}${utterance ? `&voice=${encodeURIComponent(utterance)}` : ""}`);
  await selectOverview(page);
}

async function turnOnVoice(page: Page) {
  const trigger = page.getByTestId("voice-trigger");
  await trigger.focus();
  await page.keyboard.press("Enter");
  await expect(trigger).toHaveAttribute("aria-pressed", "true");
}

async function openCrowdedDialog(page: Page, voice: boolean) {
  await open(page, "voice-pages");
  await page.getByTestId("overview-new-agent").click();
  if (voice) await turnOnVoice(page);
  const decks = page.getByTestId("new-agent-deck-list").getByRole("option");
  await expect(decks).toHaveCount(6);
  return decks;
}

async function useDocsDirectory(page: Page) {
  await page.getByTestId("new-agent-filter").fill("docs");
  await page.getByTestId("new-agent-directory-list").getByRole("option").filter({ hasText: "docs" }).click();
  await page.getByTestId("new-agent-use-directory").click();
  await expect(page.getByTestId("new-agent-modes")).toBeVisible();
}

test.describe("visible pages for voice-selected lists", () => {
  /** Scenario: with Voice off, the New agent directory keeps its ordinary scrollable list and shows no page marker. */
  test("voice-off directory keeps scrolling without a page marker", async ({ page }) => {
    const decks = await openCrowdedDialog(page, false);
    await decks.first().click();
    const list = page.getByTestId("new-agent-directory-list");
    await expect(list.getByRole("option")).toHaveCount(31);
    await expect(page.getByTestId("new-agent-dialog").getByText(/Page \d+ of \d+/i)).toHaveCount(0);
    expect(await list.evaluate((element) => getComputedStyle(element).overflowY)).toBe("auto");
    expect(await list.evaluate((element) => element.scrollHeight > element.clientHeight)).toBe(true);
  });

  for (const voice of [false, true]) {
    /** Scenario: all six usable daemons fit inside the New agent dialog and viewport, and neither their list nor an ancestor scrolls. The disconnected seventh daemon is absent from this chooser. */
    test(`New agent daemon options stay fully visible with Voice ${voice ? "on" : "off"}`, async ({ page }) => {
      await page.setViewportSize({ width: 1280, height: 900 });
      const decks = await openCrowdedDialog(page, voice);
      const geometry = await decks.first().evaluate((first) => {
        const list = first.parentElement!;
        const dialog = first.closest<HTMLElement>("[data-testid='new-agent-dialog']")!;
        const dialogBox = dialog.getBoundingClientRect();
        const options = [...list.querySelectorAll<HTMLElement>("[role='option']")];
        let ancestor: HTMLElement | null = list;
        const scrolling: string[] = [];
        while (ancestor && dialog.contains(ancestor)) {
          if (ancestor.scrollHeight > ancestor.clientHeight + 1) scrolling.push(ancestor.className);
          ancestor = ancestor.parentElement;
        }
        return {
          count: options.length,
          clipped: options.some((option) => {
            const box = option.getBoundingClientRect();
            return box.top < dialogBox.top || box.bottom > dialogBox.bottom || box.top < 0 || box.bottom > innerHeight;
          }),
          scrolling,
        };
      });
      expect(geometry.count).toBe(6);
      expect(geometry.clipped).toBe(false);
      expect(geometry.scrolling).toEqual([]);
    });
  }

  /** Scenario: Voice shows a wide crowded directory as a full, non-scrolling multi-column page. The page marker is visible and fewer than one row of usable height is wasted. */
  test("voice-on directory fills a wide page without a scrollbar", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 720 });
    const decks = await openCrowdedDialog(page, true);
    await decks.first().click();
    const list = page.getByTestId("new-agent-directory-list");
    await expect(page.getByTestId("new-agent-dialog").getByText(/Page 1 of [2-9]\d*/i)).toBeVisible();
    const geometry = await list.evaluate((element) => {
      const rows = [...element.querySelectorAll<HTMLElement>("[role='option']")];
      const boxes = rows.map((row) => row.getBoundingClientRect());
      const bottom = Math.max(...boxes.map((box) => box.bottom));
      const style = getComputedStyle(element);
      const width = element.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
      return {
        count: rows.length,
        columns: new Set(boxes.map((box) => Math.round(box.left))).size,
        // Every column a 160px-minimum cell with 2px gaps can have (`DIRECTORY_CELL`).
        fitColumns: Math.floor((width + 2) / 162),
        scrolls: element.scrollHeight > element.clientHeight + 1,
        spare: element.getBoundingClientRect().bottom - bottom,
        rowHeight: boxes[0]?.height ?? 0,
      };
    });
    expect(geometry.count).toBeLessThan(31);
    expect(geometry.columns).toBeGreaterThanOrEqual(2);
    expect(geometry.columns).toBe(geometry.fitColumns);
    expect(geometry.scrolls).toBe(false);
    expect(geometry.spare).toBeGreaterThanOrEqual(0);
    expect(geometry.spare).toBeLessThan(geometry.rowHeight);
  });

  /** Scenario: with Voice on in a wide window, opening a folder that holds only three long-named directories shows them in one column across the list's width, each name whole rather than cut short with an ellipsis, with its full name as a tooltip and no page marker (issue #1494). */
  test("voice-on directory shows a few long names whole in one column", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 900 });
    const decks = await openCrowdedDialog(page, true);
    await decks.first().click();
    const list = page.getByTestId("new-agent-directory-list");
    await list.getByRole("option").filter({ hasText: "folder-01" }).click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev/folder-01");
    await expect(list.getByRole("option")).toHaveCount(4);
    await expect(page.getByTestId("new-agent-directory-page")).toHaveCount(0);
    const geometry = await list.evaluate((element) => {
      const rows = [...element.querySelectorAll<HTMLElement>("[role='option']")];
      const names = rows.map((row) => row.querySelector<HTMLElement>(".new-agent-row-name")!);
      return {
        cut: names.filter((name) => name.scrollWidth > name.clientWidth).map((name) => name.textContent),
        columns: new Set(rows.map((row) => Math.round(row.getBoundingClientRect().left))).size,
        titles: names.slice(1).map((name) => name.title),
      };
    });
    expect(geometry.cut).toEqual([]);
    expect(geometry.columns).toBe(1);
    expect(geometry.titles).toEqual([
      "customer-onboarding-service-integration-tests",
      "payments-reconciliation-batch-worker-archive",
      "observability-dashboards-and-alerting-rules",
    ]);
  });

  /** Scenario: after a spoken page turn, the directory choices change and the visible numbering starts again at one. */
  test("directory numbers restart on page two", async ({ page }) => {
    await page.setViewportSize({ width: 900, height: 500 });
    await open(page, "voice-pages", "next page");
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-dialog").getByText(/Page 2 of \d+/i)).toBeVisible();
    const rows = page.getByTestId("new-agent-directory-list").getByRole("option");
    await expect(rows.first()).toHaveAccessibleName(/^1\./);
    await expect(rows.nth(1)).toHaveAccessibleName(/^2\./);
  });

  /** Scenario: after keyboard selection reaches the last directory on a later voice page, a narrower window shows the new page containing that row. Enter then opens the directory that is still visible. */
  test("directory page follows its selected row after the window shrinks", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 720 });
    await open(page, "voice-pages", "next page");
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await turnOnVoice(page);
    const marker = page.getByTestId("new-agent-directory-page");
    await expect(marker).toContainText(/Page 2 of \d+/i);
    const list = page.getByTestId("new-agent-directory-list");
    const rows = list.getByRole("option");
    const initialCapacity = await rows.count();
    expect(initialCapacity).toBeGreaterThan(1);
    await list.focus();
    for (let index = 1; index < initialCapacity; index += 1) await page.keyboard.press("ArrowDown");
    const selectedPath = await rows.last().getAttribute("data-path");
    expect(selectedPath).toBeTruthy();
    await expect(rows.last()).toHaveAttribute("aria-selected", "true");

    await page.setViewportSize({ width: 720, height: 720 });
    await expect.poll(() => rows.count()).toBeLessThan(initialCapacity);
    await expect(list.getByRole("option", { selected: true })).toHaveAttribute("data-path", selectedPath!);
    await expect(marker).toContainText(/Page [2-9]\d* of \d+/i);
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText(selectedPath!);
  });

  /** Scenario: on a crowded dashboard, Voice exposes only agents that fit the current page. A page marker changes when the window gains enough height to fit more rows. */
  test("crowded dashboard recomputes its voice page after resize", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 480 });
    await open(page, "voice-pages");
    await turnOnVoice(page);
    const marker = page.getByText(/Page \d+ of \d+/i);
    await expect(marker).toBeVisible();
    const smallPage = (await marker.textContent())!;
    const smallCount = await page.locator(".overview-row:visible").count();
    expect(smallCount).toBeLessThan(25);

    await page.setViewportSize({ width: 1280, height: 900 });
    await expect.poll(async () => marker.textContent()).not.toBe(smallPage);
    const tallCount = await page.locator(".overview-row:visible").count();
    expect(tallCount).toBeGreaterThan(smallCount);
  });

  /** Scenario: in a short New agent window, every mode is either in the visible dialog or reached through a visible page indicator while Voice is on. */
  test("voice-on modes are visible or paged in a short window", async ({ page }) => {
    await page.setViewportSize({ width: 720, height: 360 });
    const decks = await openCrowdedDialog(page, true);
    await decks.first().click();
    await useDocsDirectory(page);
    const modes = page.getByTestId("new-agent-modes");
    const geometry = await modes.evaluate((element) => {
      const dialog = element.closest<HTMLElement>("[data-testid='new-agent-dialog']")!;
      const body = element.closest<HTMLElement>(".new-agent-body")!;
      const dialogBox = dialog.getBoundingClientRect();
      const bodyBox = body.getBoundingClientRect();
      const chips = [...element.querySelectorAll<HTMLElement>("button")];
      return {
        count: chips.length,
        clipped: chips.some((chip) => {
          const box = chip.getBoundingClientRect();
          return box.top < bodyBox.top || box.bottom > bodyBox.bottom || box.top < dialogBox.top || box.bottom > dialogBox.bottom || box.bottom > innerHeight;
        }),
      };
    });
    expect(geometry.count).toBeGreaterThan(0);
    expect(geometry.clipped).toBe(false);
    await expect(page.getByTestId("new-agent-dialog").getByText(/Page 1 of [2-9]\d*/i)).toBeVisible();
  });

  /** Scenario: with Voice off, the last of the crowded project's modes is chosen in a short window. Turning Voice on, and then shrinking the window, each shows the Mode page holding that chosen chip. */
  test("mode page shows the chosen mode when voice turns on and the window shrinks", async ({ page }) => {
    await page.setViewportSize({ width: 720, height: 420 });
    const decks = await openCrowdedDialog(page, false);
    await decks.first().click();
    await useDocsDirectory(page);
    const modes = page.getByTestId("new-agent-modes");
    const chosen = modes.getByRole("button", { disabled: false }).last();
    const chosenMode = await chosen.getAttribute("data-mode");
    expect(chosenMode).toBeTruthy();
    await chosen.click();
    const pressed = modes.getByRole("button", { pressed: true });
    await expect(pressed).toHaveAttribute("data-mode", chosenMode!);

    await turnOnVoice(page);
    const marker = page.getByTestId("new-agent-mode-page");
    await expect(marker).toContainText(/Page [2-9]\d* of \d+/i);
    await expect(pressed).toHaveAttribute("data-mode", chosenMode!);
    await expect(pressed).toBeVisible();

    const pages = async () => Number((await marker.textContent())?.match(/of (\d+)/i)?.[1]);
    const before = await pages();
    await page.setViewportSize({ width: 560, height: 360 });
    await expect.poll(pages).toBeGreaterThan(before);
    await expect(pressed).toHaveAttribute("data-mode", chosenMode!);
    await expect(pressed).toBeVisible();
  });

  /** Scenario: a short dialog pages the crowded project's modes, while a small set of ordinary modes fits without any marker. Those few chips share the Mode row's whole width rather than leaving an empty column beside them, and each carries its label as a tooltip (issue #1494). */
  test("fitting modes have no page marker", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    const decks = await openCrowdedDialog(page, true);
    await decks.first().click();
    await page.getByTestId("new-agent-use-directory").click();
    const modes = page.getByTestId("new-agent-modes");
    await expect(modes.getByRole("button")).toHaveCount(3);
    await expect(page.getByTestId("new-agent-mode-page")).toHaveCount(0);
    const geometry = await modes.evaluate((element) => {
      const chips = [...element.querySelectorAll<HTMLElement>("button")];
      const style = getComputedStyle(element);
      const contentRight = element.getBoundingClientRect().right - parseFloat(style.paddingRight) - parseFloat(style.borderRightWidth);
      return {
        unused: contentRight - Math.max(...chips.map((chip) => chip.getBoundingClientRect().right)),
        titles: chips.map((chip) => chip.title),
      };
    });
    expect(geometry.unused).toBeLessThan(1);
    expect(geometry.titles.every((title) => title.length > 0)).toBe(true);
  });

  /** Scenario: the Daemons screen pages its fifteen tiles with Voice on, and the four tiles on page one bear the same numbers as their focus keys. */
  test("Daemons tiles page and agree with focus keys", async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    await open(page, "voice-pages");
    await enterDeck(page);
    await turnOnVoice(page);
    await expect(page.getByText(/Page 1 of [2-9]\d*/i)).toBeVisible();
    const tiles = page.locator(".agent-grid .agent-tile:visible");
    await expect(tiles).toHaveCount(4);
    for (let index = 0; index < 4; index += 1) await expect(tiles.nth(index)).toHaveAccessibleName(new RegExp(`^${index + 1}\\.`));
    await page.keyboard.press("3");
    await expect(tiles.nth(2)).toHaveClass(/is-selected/);
  });

  /** Scenario: every daemon in the selector menu remains within the viewport and menu when Voice is off. */
  test("DeckSelector shows every fixture daemon without clipping", async ({ page }) => {
    await open(page, "fleet");
    await page.getByTestId("deck-selector-toggle").click();
    const menu = page.getByTestId("deck-selector-menu");
    await expect(menu).toBeVisible();
    const geometry = await menu.evaluate((element) => {
      const menuBox = element.getBoundingClientRect();
      const options = [...element.querySelectorAll<HTMLElement>("[data-testid^='deck-selector-option-']")];
      return {
        count: options.length,
        clipped: options.some((option) => {
          const box = option.getBoundingClientRect();
          return box.top < menuBox.top || box.bottom > menuBox.bottom || box.top < 0 || box.bottom > innerHeight;
        }),
        scrolls: element.scrollHeight > element.clientHeight,
      };
    });
    expect(geometry.count).toBeGreaterThanOrEqual(2);
    expect(geometry.clipped).toBe(false);
    expect(geometry.scrolls).toBe(false);
  });
});
