import { expect, test, type Page } from "@playwright/test";
import { selectOverview } from "./support/overview";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

async function openSpeaking(page: Page, utterance: string) {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  await page.goto(`/?fixture=1&state=fleet&voice=${encodeURIComponent(utterance)}`);
  await selectOverview(page);
}

async function openDirectoryBrowser(page: Page) {
  await page.getByTestId("overview-new-agent").click();
  await page.getByTestId("new-agent-deck-list").getByRole("option").first().click();
  await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/dev");
}

function directoryNames(page: Page) {
  return page.getByTestId("new-agent-directory-list").locator('[role="option"][data-path^="/home/dev/"]');
}

async function turnOnVoice(page: Page) {
  const trigger = page.getByTestId("voice-trigger");
  await trigger.focus();
  await page.keyboard.press("Enter");
  await expect(trigger).toHaveAttribute("aria-pressed", "true");
}

test.describe("voice directory filter", () => {
  /** Scenario: in the open directory browser, a longer spoken request extracts
   * the letter D into the Filter box. The same case-insensitive contains match
   * as typing leaves demo-project visible and hides scratch, and Voice reports it. */
  test("spoken letter filters the visible directory names", async ({ page }) => {
    await openSpeaking(page, "show only those starting with letter D");
    await openDirectoryBrowser(page);
    await expect(directoryNames(page)).toHaveCount(2);

    await turnOnVoice(page);

    await expect(page.getByTestId("new-agent-filter")).toHaveValue("d");
    await expect(directoryNames(page)).toHaveCount(1);
    await expect(directoryNames(page).first()).toContainText("demo-project");
    await expect(page.getByTestId("voice-report").getByText("Filtering by “d”.")).toBeVisible();
  });

  /** Scenario: a typed capital D uses the same contains match as spoken d,
   * leaving demo-project visible and hiding scratch. */
  test("typed capital D matches the spoken filter listing", async ({ page }) => {
    await openSpeaking(page, "unused phrase");
    await openDirectoryBrowser(page);

    await page.getByTestId("new-agent-filter").fill("D");

    await expect(directoryNames(page)).toHaveCount(1);
    await expect(directoryNames(page).first()).toContainText("demo-project");
  });

  /** Scenario: with a typed filter already narrowing the directory browser,
   * saying clear filter empties the box, restores both names and reports that
   * the filter was cleared. */
  test("clear filter by voice restores the listing", async ({ page }) => {
    await openSpeaking(page, "clear filter");
    await openDirectoryBrowser(page);
    await page.getByTestId("new-agent-filter").fill("D");
    await expect(directoryNames(page)).toHaveCount(1);

    await turnOnVoice(page);

    await expect(page.getByTestId("new-agent-filter")).toHaveValue("");
    await expect(directoryNames(page)).toHaveCount(2);
    await expect(page.getByTestId("voice-report").getByText("Filter cleared.")).toBeVisible();
  });

  /** Scenario: when no New agent dialog is open, a spoken filter request has
   * no directory browser to edit and does not claim that a filter was applied. */
  test("closed dialog refuses a spoken directory filter", async ({ page }) => {
    await openSpeaking(page, "filter docs");
    await page.getByTestId("voice-trigger").click();

    await expect(page.getByTestId("new-agent-dialog")).toHaveCount(0);
    await expect(page.getByTestId("new-agent-filter")).toHaveCount(0);
    await expect(page.getByTestId("voice-report")).toContainText("Not here — filtering needs the New agent dialog's directory listing");
  });
});
