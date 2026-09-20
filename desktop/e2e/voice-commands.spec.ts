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

test.describe("what can I say?", () => {
  /**
   * Scenario: ask the preview what can be said. An overlay opens over the deck
   * listing the rows the browser fixture can actually resolve, split by whether
   * this screen can run them, and its Close button dismisses it — all of it
   * generated from the same vocabulary the fixture resolves against.
   */
  test("opens a list generated from the fixture's own vocabulary", async ({ page }) => {
    await openSpeaking(page, ["what can I say?"]);

    await voiceButton(page).click();

    const overlay = page.getByTestId("voice-help");
    await expect(overlay).toBeVisible();
    await expect(overlay.locator('[data-where="here"] [data-command="open_overview"]')).toBeVisible();
    // The deck cannot run `open_deck`, so it is listed under the other heading
    // rather than left out — knowing a command exists is most of discovery.
    await expect(overlay.locator('[data-where="elsewhere"] [data-command="open_deck"]')).toBeVisible();

    await page.getByTestId("voice-help-close").click();
    await expect(overlay).toHaveCount(0);
  });

  /**
   * Scenario: open the list while an agent's pane is enlarged over the whole
   * window. It is reachable and its Close button is genuinely clickable, which
   * is the `VOICE_PEER_PROPS` exemption doing its job for a second element —
   * the overlay is a child of the reserved row precisely so that it does.
   */
  test("is reachable from behind an enlarged agent pane", async ({ page }) => {
    await openSpeaking(page, ["what can I say?"]);
    await page.getByRole("button", { name: "Open Planner agent" }).click();
    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();

    await voiceButton(page).click();

    const overlay = page.getByTestId("voice-help");
    await expect(overlay).toBeVisible();
    await expect(overlay.locator('[data-where="here"] [data-command="close_agent_view"]')).toBeVisible();
    // `trial` runs every actionability check — visible, stable, receives
    // events — and clicks nothing, which is the question an `inert` ancestor
    // would answer with a failure.
    await page.getByTestId("voice-help-close").click({ trial: true });
  });
});

test.describe("the empty report row", () => {
  /**
   * Scenario: press Voice in the preview and read the row before it has heard
   * anything. It names the phrase that lists everything, the phrase that
   * stops, and the button — and it fits the reserved row rather than growing
   * it, which is the one property the row has to keep.
   */
  test("names both ways out and does not grow the row", async ({ page }) => {
    // A script with nothing in it: the fixture then hears nothing at all, which
    // is the state this row is about. An empty `?voice=` is filtered out, so
    // this says it with a phrase no fixture row matches — the hint has to
    // survive until an utterance REPORTS, not merely until one is heard.
    await openSpeaking(page, ["    "]);
    const before = await page.locator(".voice-row").boundingBox();

    await voiceButton(page).click();

    const hint = page.getByTestId("voice-hint");
    await expect(hint).toContainText("what can I say?");
    await expect(hint).toContainText("voice off");
    await expect(hint).toContainText("Voice button");

    const after = await page.locator(".voice-row").boundingBox();
    expect(before, "the voice row has no layout box").not.toBeNull();
    expect(after!.height, "the empty-state hint grew the reserved row").toBe(before!.height);
  });
});

/**
 * PRD #802 D6 — the feature's own point, driven as a user meets it.
 *
 * These are the tests that exercise the whole aimed loop in a real browser:
 * the pane opens, the words land in the terminal the user is looking at, the
 * countdown is on screen, and the phrase ends it. The preview's microphone is
 * scripted with `?voice=`, which is what makes an utterance AFTER the first one
 * askable at all.
 */
test.describe("dictating into an agent", () => {
  /**
   * Scenario: say "type to the coder" and then a sentence. Planner's pane
   * opens over the deck, the sentence appears in that agent's own terminal —
   * the visible input, not a buffer — and the row counts down to a send instead
   * of submitting it.
   */
  test("opens the agent's pane and types into its terminal", async ({ page }) => {
    await openSpeaking(page, ["type to the coder", "run the login tests"], "crowded");

    await voiceButton(page).click();

    const pane = page.getByTestId("agent-pane-overlay");
    await expect(pane).toBeVisible();
    // The pane that opened is the one being typed into, and it accepts input:
    // dictating into a pane the app itself renders as unwritable would be the
    // hidden buffer this feature is defined against. `builder` holds a write
    // lease; `planner`, which the other tests here open, does not.
    // The pane that opened is the agent that was named — by the control that
    // closes it, which carries the agent's own label rather than its role.
    await expect(pane.getByRole("button", { name: /close coder agent/i })).toBeVisible();
    // And it ACCEPTS typing. Dictating into a pane the app itself renders as
    // unwritable would be demonstrating the opposite of the feature, which is
    // why the crowded fleet is the one loaded here — see the fixture row.
    await expect(pane).not.toContainText("Terminal input unavailable");
    await expect(pane).not.toContainText("This agent has finished its work");

    const line = page.getByTestId("voice-dictation");
    await expect(line).toContainText("Typing to coder");
    await expect(line).toContainText("sending in");
  });

  /**
   * Scenario: end it by saying so. The countdown and the aim both go, the row
   * says what happened, and voice stays on — which is the difference between
   * this and "voice off".
   */
  test("ends on the exit phrase and leaves voice listening", async ({ page }) => {
    await openSpeaking(page, ["type to the coder", "run the login tests", "stop dictation"], "crowded");

    await voiceButton(page).click();

    await expect(page.getByText(/Dictation off/)).toBeVisible();
    await expect(page.getByTestId("voice-dictation")).toHaveCount(0);
    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "true");
  });

  /**
   * Scenario: "voice off" while aimed stops everything. The precedence chosen
   * here is the bigger stop, because the failure it avoids is a user who
   * believes the microphone is closed while it is open.
   */
  test("voice off while dictating stops the microphone too", async ({ page }) => {
    await openSpeaking(page, ["type to the coder", "run the login tests", "voice off"], "crowded");

    await voiceButton(page).click();

    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("voice-dictation")).toHaveCount(0);
  });
});
