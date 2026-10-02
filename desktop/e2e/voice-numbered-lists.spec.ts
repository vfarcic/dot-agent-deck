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

  /** Scenario: with the Daemons screen's tiles numbered, a digit typed into an agent's terminal reaches that agent and selects no tile, and #1422's Shift+Enter reaches it as a newline without changing the selection either. */
  test("digits and #1422 chords typed into an agent's terminal never select a numbered tile", async ({ page }) => {
    await page.addInitScript(() => {
      const originalSet = Map.prototype.set;
      const terminals: unknown[] = [];
      Object.defineProperty(window, "__dadE2eTerminals", { value: terminals });
      Object.defineProperty(Map.prototype, "set", {
        configurable: true,
        writable: true,
        value(this: Map<unknown, unknown>, key: unknown, value: unknown) {
          if (value && typeof value === "object") {
            const candidate = value as { element?: unknown; onData?: unknown };
            if (candidate.element instanceof HTMLElement && typeof candidate.onData === "function") terminals.push(value);
          }
          return Reflect.apply(originalSet, this, [key, value]);
        },
      });
    });
    await openWithSpeech(page, "docs", [""]);
    await enterDeck(page);
    const tiles = page.locator(".agent-grid .agent-tile");
    await expect(tiles).toHaveCount(4);
    await turnOnVoice(page);
    await expectNumbers(tiles, 1);
    await page.keyboard.press("1");
    await expect(tiles.nth(0)).toHaveClass(/is-selected/);

    const writable = tiles.filter({ has: page.locator('[aria-disabled="false"]') }).first();
    await expect(writable).toBeVisible();
    const viewport = writable.locator('[aria-disabled="false"]').first();
    await viewport.locator("textarea.xterm-helper-textarea").focus();
    await viewport.evaluate((root) => {
      type Recording = Window & { __dadE2eTerminals?: { element?: HTMLElement; onData(listener: (data: string) => void): unknown }[]; __dadE2eSent?: string[] };
      const recording = window as Recording;
      const terminal = recording.__dadE2eTerminals?.find((candidate) => candidate.element && root.contains(candidate.element));
      if (!terminal) throw new Error("the writable tile's xterm was not captured");
      recording.__dadE2eSent = [];
      terminal.onData((data) => recording.__dadE2eSent?.push(data));
    });
    const selectedBefore = await tiles.evaluateAll((all) => all.findIndex((tile) => tile.classList.contains("is-selected")));
    for (const key of ["2", "3", "4", "Shift+Enter"]) await page.keyboard.press(key);
    expect(await page.evaluate(() => (window as Window & { __dadE2eSent?: string[] }).__dadE2eSent)).toEqual(["2", "3", "4", "\x1b[13;2u"]);
    expect(await tiles.evaluateAll((all) => all.findIndex((tile) => tile.classList.contains("is-selected")))).toBe(selectedBefore);
  });

  /** Scenario: the New agent dialog numbers daemons, directories (including ..) and modes separately from 1. Filtering renumbers only the directory section. */
  test("daemon, directory and mode numbers each start at one and update after filtering", async ({ page }) => {
    await openWithSpeech(page, "fleet");
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    const decks = page.getByTestId("new-agent-deck-list").getByRole("option");
    await expect(decks).toHaveCount(2);
    await expect(decks.first()).not.toHaveAccessibleName(/^1\./);
    await turnOnVoice(page);
    await expectNumbers(decks, 1);

    await decks.first().click();
    const directories = page.getByTestId("new-agent-directory-list").getByRole("option");
    await expect(directories.first()).toBeVisible();
    await expectNumbers(directories, 1);
    await expect(directories.first()).toContainText("..");
    const modes = page.getByTestId("new-agent-modes").getByRole("button").filter({ visible: true });
    await expectNumbers(modes, 1);

    await page.getByTestId("new-agent-filter").fill("project");
    await expect(directories).toHaveCount(2); // parent plus the matching project
    await expectNumbers(directories, 1);
    await expectNumbers(modes, 1);
  });

  /** Scenario: key 3 on the directory list enters directory 3. The same key in Filter, Name and Command types text without browsing. */
  test("directory digit key selects directory 3 but digits in fields are typed", async ({ page }) => {
    await openWithSpeech(page, "fleet");
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await turnOnVoice(page);
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    const list = page.getByTestId("new-agent-directory-list");
    const third = list.getByRole("option").nth(2);
    await expect(third).toHaveAccessibleName(/^3\./);
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
    await filter.fill("");
    await page.getByTestId("new-agent-use-directory").click();
    for (const field of ["new-agent-name", "new-agent-command"]) {
      const input = page.getByTestId(field);
      await input.fill("");
      await input.focus();
      await page.keyboard.press("3");
      await expect(input).toHaveValue("3");
      await expect(page.getByTestId("new-agent-current-path")).toHaveText(current!);
    }
  });

  /** Scenario: key 3 in the Mode chips chooses the chip labelled 3; it does not enter directory 3 or choose daemon 3. */
  test("mode digit key selects mode 3 in the focused section", async ({ page }) => {
    await openWithSpeech(page, "fleet");
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await page.getByTestId("new-agent-use-directory").click();
    await turnOnVoice(page);
    const modes = page.getByTestId("new-agent-modes");
    const third = modes.getByRole("button").nth(2);
    await expect(third).toHaveAccessibleName(/^3\./);
    await modes.getByRole("button").first().focus();
    await page.keyboard.press("3");
    await expect(third).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
  });

  /** Scenario: the two daemon options are numbered first; saying “two” chooses the second daemon and shows its own home directory. */
  test("a spoken number chooses the displayed daemon option", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["two"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
  });

  /** Scenario: the New agent dialog shows daemon 1. Saying “Select daemon 1” chooses that displayed daemon and opens its directory browser. */
  test("Select daemon 1 chooses the daemon showing 1", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["Select daemon 1"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    const daemon = page.getByTestId("new-agent-deck-list").getByRole("option").first();
    const deckId = await daemon.getAttribute("data-deck-id");
    await turnOnVoice(page);
    await expect(daemon).toHaveAccessibleName(/^1\./);
    await expect(page.getByTestId("new-agent-deck-list").locator("[data-chosen='true']")).toHaveAttribute("data-deck-id", deckId!);
    await expect(page.getByTestId("new-agent-directory-list")).toBeVisible();
    await expect(page.getByTestId("voice-report")).not.toContainText("no matching action");
  });

  /** Scenario: with only the directory section showing 3, saying “three” enters that directory directly without a choice dialog. */
  test("a spoken number enters the displayed directory", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["three"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev/scratch");
    await expect(page.getByTestId("voice-choice")).toHaveCount(0);
  });

  /** Scenario: “directory 3” names the third directory even while the Mode section also shows a 3. Its kind removes the ambiguity. */
  test("directory 3 chooses its section without offering a choice", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["directory 3"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await page.getByTestId("new-agent-use-directory").click();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev/scratch");
    await expect(page.getByTestId("voice-choice")).toHaveCount(0);
  });

  for (const answer of ["Directory", "Mode"]) {
    /** Scenario: two sections show 3, so bare “three” offers exactly Directory 3 and Mode 3. Choosing either labelled answer acts on that section. */
    test(`bare three offers both sections and choosing ${answer} acts there`, async ({ page }) => {
      await openWithSpeech(page, "fleet", ["three"]);
      await selectOverview(page);
      await page.getByTestId("overview-new-agent").click();
      await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
      await page.getByTestId("new-agent-use-directory").click();
      const modeId = await page.getByTestId("new-agent-modes").getByRole("button").nth(2).getAttribute("data-mode");
      await turnOnVoice(page);
      const choice = page.getByTestId("voice-choice");
      await expect(choice).toBeVisible();
      await expect(choice.getByRole("button")).toHaveCount(3); // two answers and Cancel
      await expect(choice.getByRole("button", { name: /^1\. Directory 3: scratch$/ })).toBeVisible();
      const modeAnswer = choice.getByRole("button", { name: /^2\. Mode 3: / });
      await expect(modeAnswer).toBeVisible();
      if (answer === "Directory") {
        await choice.getByRole("button", { name: /^1\. Directory 3: scratch$/ }).click();
        await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev/scratch");
      } else {
        await modeAnswer.click();
        await expect(page.getByTestId(`new-agent-mode-${modeId}`)).toHaveAttribute("aria-pressed", "true");
        await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
      }
      await expect(choice).toHaveCount(0);
    });
  }

  /** Scenario: a crowded directory page visibly includes row 13. Saying “Select directory 13” enters that row and the voice report does not claim there was no action. */
  test("Select directory 13 enters the directory showing 13", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 720 });
    await openWithSpeech(page, "voice-pages", ["Select directory 13"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    /* Read row 13 before Voice turns on: the scripted utterance is answered as
       soon as it does, and entering the directory replaces the listing, so a
       read after that races the answer. */
    const options = page.getByTestId("new-agent-directory-list").getByRole("option");
    await expect(options.nth(12)).toBeAttached();
    const path = await options.nth(12).getAttribute("data-path");
    expect(path).toBeTruthy();
    await expect(page.getByTestId("new-agent-current-path")).not.toHaveText(path!);
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText(path!);
    await expect(page.getByTestId("voice-report")).not.toContainText("no matching action");
  });

  /** Scenario: “mode 2” chooses the mode visibly numbered 2, independent of daemon and directory counts. */
  test("mode 2 chooses the displayed mode chip", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["mode 2"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").nth(1).click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
    await page.getByTestId("new-agent-use-directory").click();
    await expect(page.getByTestId("new-agent-mode-schedule")).toBeEnabled();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "true");
  });

  /** Scenario: Schedule shows 2 in the New agent form. Saying “choose mode 2” selects that chip. */
  test("choose mode 2 selects the mode showing 2", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["choose mode 2"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").nth(1).click();
    await page.getByTestId("new-agent-use-directory").click();
    const schedule = page.getByTestId("new-agent-mode-schedule");
    await turnOnVoice(page);
    await expect(schedule).toHaveAccessibleName(/^2\./);
    await expect(schedule).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("voice-report")).not.toContainText("no matching action");
  });

  /** Scenario: directory 13 is visible but “select daemon 13” names the wrong kind. The browser stays in place and reports the mismatch briefly. */
  test("select daemon 13 refuses a directory numbered 13", async ({ page }) => {
    await page.setViewportSize({ width: 1440, height: 720 });
    await openWithSpeech(page, "voice-pages", ["select daemon 13"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-directory-list").getByRole("option", { name: /^13\./ })).toBeVisible();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
    const report = page.getByTestId("voice-report");
    await expect(report).toContainText(/no daemon.*13|13 is.*directory|13.*not.*daemon/i);
    await expect(report).not.toContainText("no matching action");
  });

  /** Scenario: no directory shows 99, so “select directory 99” leaves the browser in place and says the requested number is out of range. */
  test("select directory 99 refuses an unseen number", async ({ page }) => {
    await openWithSpeech(page, "fleet", ["select directory 99"]);
    await selectOverview(page);
    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
    await turnOnVoice(page);
    await expect(page.getByTestId("new-agent-directory-list").getByRole("option", { name: /^99\./ })).toHaveCount(0);
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
    const report = page.getByTestId("voice-report");
    await expect(report).toContainText(/no directory.*99|99.*not|99.*out of range/i);
    await expect(report).not.toContainText("no matching action");
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

  /** Scenario: “open agent 3” opens the dashboard row visibly numbered 3, just as saying the bare number does. */
  test("open agent 3 opens the dashboard agent showing 3", async ({ page }) => {
    await openWithSpeech(page, "docs", ["open agent 3"]);
    await selectOverview(page);
    await turnOnVoice(page);
    await expect(page.getByRole("row", { name: /^3\./ })).toHaveCount(1);
    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
    await expect(page.getByTestId("agent-pane-overlay").locator(".agent-assignment p"))
      .toHaveText("Check the payment API for breaking changes.");
    await expect(page.getByTestId("voice-report")).not.toContainText("no matching action");
  });
});
