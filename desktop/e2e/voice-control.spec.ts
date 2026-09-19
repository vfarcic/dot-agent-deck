import { expect, test, type Page } from "@playwright/test";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** Load the browser fixture with its speech backend enabled and canned utterance ready. */
async function openWithSpeech(page: Page) {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  await page.goto("/?fixture=1&state=connected");
}

function voiceButton(page: Page) {
  return page.getByTestId("voice-trigger");
}

test.describe("voice control through the browser fixture", () => {
  /**
   * Scenario: load the default browser fixture, where transcription is off,
   * and press Voice. A report points to Settings → Voice without opening a dialog or typed-command field.
   */
  test("unavailable voice explains how to enable it", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    const trigger = voiceButton(page);
    await expect(trigger).toBeVisible();

    await trigger.click();

    await expect(page.getByText(/Settings → Voice/i)).toBeVisible();
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByRole("dialog", { name: "Voice control" })).toHaveCount(0);
    await expect(page.getByRole("textbox", { name: "Command" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Run command" })).toHaveCount(0);
  });

  /**
   * Scenario: load the speech-enabled fixture and press the same Voice button twice.
   * Its visible and semantic state changes from off to on and back to off.
   */
  test("the Voice button toggles continuous control and shows its state", async ({ page }) => {
    await openWithSpeech(page);
    const trigger = voiceButton(page);

    await expect(trigger).toHaveText(/voice\s+off/i);
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
    await trigger.click();
    await expect(trigger).toHaveText(/voice\s+on/i);
    await expect(trigger).toHaveAttribute("aria-pressed", "true");

    await trigger.click();

    await expect(trigger).toHaveText(/voice\s+off/i);
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
  });

  /**
   * Scenario: turn on the speech-enabled fixture and let its canned utterance complete.
   * The real app navigates to the overview, reports the fixture sentence, and remains on for another utterance.
   */
  test("a fixture utterance resolves and leaves Voice on", async ({ page }) => {
    await openWithSpeech(page);
    const trigger = voiceButton(page);

    await trigger.click();

    await expect(page.getByText("Opening the agent overview.")).toBeVisible();
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
    await expect(trigger).toHaveText(/voice\s+on/i);
    await expect(trigger).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByRole("dialog", { name: "Voice control" })).toHaveCount(0);
  });
});
