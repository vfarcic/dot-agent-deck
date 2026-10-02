import { expect, test, type Locator, type Page } from "@playwright/test";
import { enterDeck, selectOverview } from "./support/overview";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** The fixture microphone uses the same settings gate as the desktop bridge. */
async function openWithSpeech(page: Page, state: string, utterances: string[] = []) {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  const script = utterances.map((phrase) => `voice=${encodeURIComponent(phrase)}`).join("&");
  await page.goto(`/?fixture=1&state=${state}${script ? `&${script}` : ""}`);
}

async function turnOnVoice(page: Page) {
  const trigger = page.getByTestId("voice-trigger");
  await trigger.focus();
  await page.keyboard.press("Enter");
  await expect(trigger).toHaveAttribute("aria-pressed", "true");
}

/** A displayed number belongs in the item's accessible name, before its name. */
async function expectNumbers(items: Locator, first: number) {
  const count = await items.count();
  expect(count).toBeGreaterThan(0);
  for (let index = 0; index < count; index += 1) {
    await expect(items.nth(index)).toHaveAccessibleName(new RegExp(`^${first + index}\\.`));
  }
  return first + count;
}

test.describe("numbered lists while voice is on", () => {
  /** Scenario: with Voice off, dashboard rows and every New agent list keep their existing names. A digit typed into Filter remains text and does not browse. */
  test("voice-off lists remain unnumbered and Filter accepts digits", async ({ page }) => {
    await openWithSpeech(page, "fleet");
    await selectOverview(page);
    const rows = page.locator(".overview-row");
    await expect(rows.first()).toBeVisible();
    for (const row of await rows.all()) await expect(row).not.toHaveAccessibleName(/^\d+\./);
    await page.getByTestId("overview-new-agent").click();
    const decks = page.getByTestId("new-agent-deck-list").getByRole("option");
    await expect(decks.first()).not.toHaveAccessibleName(/^\d+\./);
    await decks.first().click();
    const directories = page.getByTestId("new-agent-directory-list").getByRole("option");
    await expect(directories.first()).toBeVisible();
    for (const item of await directories.all()) await expect(item).not.toHaveAccessibleName(/^\d+\./);
    for (const mode of await page.getByTestId("new-agent-modes").getByRole("button").all()) await expect(mode).not.toHaveAccessibleName(/^\d+\./);
    const original = await page.getByTestId("new-agent-current-path").textContent();
    const filter = page.getByTestId("new-agent-filter");
    await filter.focus();
    await page.keyboard.press("3");
    await expect(filter).toHaveValue("3");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText(original!);
  });

  /** Scenario: dashboard rows across the visible daemon groups have one continuous sequence when Voice is on. Turning Voice off removes those numbers from the row names. */
  test("dashboard agent rows number across daemons and return to ordinary names with voice off", async ({ page }) => {
    await openWithSpeech(page, "docs-fleet");
    await selectOverview(page);
    const rows = page.locator(".overview-row");
    await expect(rows).toHaveCount(6);
    await expect(rows.first()).not.toHaveAccessibleName(/^1\./);

    await turnOnVoice(page);
    await expectNumbers(rows, 1);

    await page.getByTestId("voice-trigger").click();
    await expect(page.getByTestId("voice-trigger")).toHaveAttribute("aria-pressed", "false");
    await expect(rows.first()).not.toHaveAccessibleName(/^1\./);
  });

  /** Scenario: with a silent microphone, the Daemons screen shows four numbered tiles while Voice is on. Pressing 3 selects the tile labelled 3. */
  test("daemon agent tiles agree with their focus keys", async ({ page }) => {
    await openWithSpeech(page, "docs", [""]);
    await enterDeck(page);
    const tiles = page.locator(".agent-grid .agent-tile");
    await expect(tiles).toHaveCount(4);
    await turnOnVoice(page);
    await expect(tiles).toHaveCount(4);
    await expect(tiles.nth(3)).toHaveAccessibleName(/^4\./);
    await expectNumbers(tiles, 1);
    await page.keyboard.press("3");
    await expect(tiles.nth(2)).toHaveClass(/is-selected/);
  });

  /** Scenario: in the New agent dialog, daemon options, directory rows and mode chips share one sequence in reading order. Filtering changes the visible rows and renumbers the remaining items. */
  test("daemon, directory and mode numbers are unique and update after filtering", async ({ page }) => {
    await openWithSpeech(page, "fleet");
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    const decks = page.getByTestId("new-agent-deck-list").getByRole("option");
    await expect(decks).toHaveCount(2);
    await expect(decks.first()).not.toHaveAccessibleName(/^1\./);
    await turnOnVoice(page);
    let next = await expectNumbers(decks, 1);

    await decks.first().click();
    const directories = page.getByTestId("new-agent-directory-list").getByRole("option");
    await expect(directories.first()).toBeVisible();
    next = await expectNumbers(directories, next);
    const modes = page.getByTestId("new-agent-modes").getByRole("button").filter({ visible: true });
    await expectNumbers(modes, next);

    await page.getByTestId("new-agent-filter").fill("project");
    await expect(directories).toHaveCount(2); // parent plus the matching project
    next = await expectNumbers(directories, 3);
    await expectNumbers(modes, next);
  });

  /** Scenario: a digit on the directory list enters the row showing that number. The same digit in the Filter field is text and leaves the current directory alone. */
  test("directory digit keys select a row but digits in Filter are typed", async ({ page }) => {
    await openWithSpeech(page, "fleet");
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await turnOnVoice(page);
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    const list = page.getByTestId("new-agent-directory-list");
    const third = list.getByRole("option").first(); // the first directory follows two daemon numbers
    const path = await third.getAttribute("data-path");
    expect(path).toBeTruthy();
    await list.focus();
    await page.keyboard.press("3");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText(path!);

    const filter = page.getByTestId("new-agent-filter");
    await filter.focus();
    const current = await page.getByTestId("new-agent-current-path").textContent();
    await page.keyboard.press("3");
    await expect(filter).toHaveValue("3");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText(current!);
  });

  /** Scenario: the two daemon options are numbered first; saying “two” chooses the second daemon and shows its own home directory. */
  test("a spoken number chooses the displayed daemon option", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["two"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
  });

  /** Scenario: after the two daemon options, the parent directory is 3 and demo-project is 4. Saying “number four” browses into that project. */
  test("a spoken number enters the displayed directory", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["number four"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev/demo-project");
  });

  /** Scenario: with two daemon options and three directory rows before the mode chips, number 7 chooses schedule on the remote daemon. */
  test("a spoken number chooses the displayed mode chip", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["number seven"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").nth(1).click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
    await page.getByTestId("new-agent-use-directory").click();
    await expect(page.getByTestId("new-agent-mode-schedule")).toBeEnabled();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "true");
  });

  for (const phrase of ["three", "number three", "the third one"]) {
    /** Scenario: a bare spoken ordinal selects the item that currently bears 3 on the dashboard, opening that agent without asking for a command. */
    test(`spoken ${phrase} opens the dashboard's third agent`, async ({ page }) => {
      await openWithSpeech(page, "docs", [phrase]);
      await selectOverview(page);
      await turnOnVoice(page);
      await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
      await expect(page.getByTestId("agent-pane-overlay").locator(".agent-assignment p"))
        .toHaveText("Check the payment API for breaking changes.");
      await expect(page.getByRole("dialog", { name: "Which agent?" })).toHaveCount(0);
    });
  }
});
