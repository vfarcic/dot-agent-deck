import { expect, test, type Page } from "@playwright/test";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/**
 * PRD #802 — the rows that are not navigation, driven through the real surface.
 *
 * `voice-control.spec.ts` beside this one owns the microphone's own behaviour
 * and the reserved row's geometry. This file owns what the newer rows do, and
 * it exists as a separate file for the same reason its vitest counterpart does:
 * a different question, asked of the same surface.
 *
 * Every test here scripts the preview's microphone with `?voice=`, which is
 * what lets a browser test ask about the utterance AFTER the first one — the
 * question dictation is entirely about.
 */

/**
 * Load the browser fixture with speech enabled, saying `script` in order.
 *
 * The settings document is seeded before the page loads because the bridge
 * reads it on its first status poll; `voice: undefined` is what the preview
 * treats as *no backend*, so a document naming the section is what turns the
 * simulated microphone on.
 */
async function openSpeaking(page: Page, script: string[], state = "connected") {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  const spoken = script.map((phrase) => `voice=${encodeURIComponent(phrase)}`).join("&");
  await page.goto(`/?fixture=1&state=${state}&${spoken}`);
}

function voiceButton(page: Page) {
  return page.getByTestId("voice-trigger");
}

test.describe("voice off, said out loud", () => {
  /**
   * Scenario: turn voice on in the browser preview and let it say "voice off".
   * The button returns to its off state with the table's own report sentence
   * beside it, which is the whole loop — resolve, dispatch, release — driven
   * with no microphone and no credential anywhere near it.
   */
  test("turns the Voice button off from the deck", async ({ page }) => {
    await openSpeaking(page, ["voice off"]);
    const trigger = voiceButton(page);

    await trigger.click();

    await expect(page.getByText("Voice control off.")).toBeVisible();
    await expect(trigger).toHaveText(/voice\s+off/i);
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
  });

  /**
   * Scenario: say it from the agent overview instead, the screen that mounts no
   * deck and therefore serves the fewest context members. It still stops —
   * because the member the row needs comes from the voice surface, which is a
   * peer of the screen switch rather than part of either screen.
   */
  test("turns the Voice button off from the overview as well", async ({ page }) => {
    await openSpeaking(page, ["show me every agent", "voice off"]);
    const trigger = voiceButton(page);

    await trigger.click();

    await expect(page.getByTestId("overview-table-region")).toBeVisible();
    await expect(page.getByText("Voice control off.")).toBeVisible();
    await expect(trigger).toHaveAttribute("aria-pressed", "false");
  });
});
