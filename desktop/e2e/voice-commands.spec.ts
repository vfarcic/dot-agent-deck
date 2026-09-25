import { expect, test, type Page } from "@playwright/test";
import { enterDeck, selectOverview } from "./support/overview";

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
async function openSpeaking(page: Page, script: string[], state = "connected", startOnOverview = false) {
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
  if (!startOnOverview) await enterDeck(page);
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

test.describe("Settings from the overview by voice", () => {
  /**
   * Scenario: start on the overview and say "open settings" to the scripted
   * fixture microphone. The Settings sheet opens on that screen.
   */
  test("opens Settings on the overview", async ({ page }) => {
    await openSpeaking(page, ["open settings"], "connected", true);
    await selectOverview(page);
    await voiceButton(page).click();
    await expect(page.getByRole("dialog", { name: "Settings" })).toBeVisible();
  });

  /**
   * Scenario: on the shipped overview, open Settings from the rail and say
   * "close this" through the scripted microphone. The sheet goes away and
   * the overview remains visible.
   */
  test("closes Settings by voice on the overview", async ({ page }) => {
    await openSpeaking(page, ["close this"], "connected", true);
    await selectOverview(page);
    await page.getByRole("button", { name: "Settings" }).click();
    await expect(page.getByRole("dialog", { name: "Settings" })).toBeVisible();

    await voiceButton(page).click();

    await expect(page.getByRole("dialog", { name: "Settings" })).toHaveCount(0);
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
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
    await expect(overlay.locator('[data-where="here"] [data-command="open_deck"]')).toBeVisible();
    await expect(overlay.locator('[data-where="here"] [data-command="open_settings"]')).toBeVisible();
    await expect(overlay.locator('[data-where="elsewhere"] [data-command="dictate_to_agent"]')).toBeVisible();

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
    await expect(overlay.locator('[data-where="here"] [data-command="close"]')).toBeVisible();
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
 * PRD #802 D6, rebuilt — the feature's own point, driven as a user meets it.
 *
 * These are the tests that exercise the whole loop in a real browser: the pane
 * is open, the words land in the terminal the user is looking at, the countdown
 * is on screen, and a spoken phrase sends it. The preview's microphone is
 * scripted with `?voice=`, which is what makes an utterance AFTER the first one
 * askable at all.
 *
 * The pane is opened by CLICK rather than by voice, and that is a limitation of
 * the preview rather than of the feature: the fixture bridge has no `agent_ref`
 * resolver, deliberately, so `open_agent` is the one row it cannot answer. What
 * the click buys is the precondition the row actually needs — `screens =
 * ["agent"]` — after which everything under test is spoken.
 */
test.describe("typing into the open agent", () => {
  /**
   * Open the crowded fleet's `coder` pane, which is the one agent in any
   * fixture whose pane accepts typing — see the fixture row for why that is the
   * feature's own requirement rather than an arbitrary pick.
   */
  async function openCoder(page: Page) {
    await page.getByRole("button", { name: "Open coder agent" }).click();
    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  }

  /**
   * Scenario: with the coder's pane open, say "type run the login tests". The
   * words appear in that agent's own terminal — the visible input, not a buffer
   * — and the row counts down to a send instead of submitting it.
   */
  test("types into the open pane and counts down instead of sending", async ({ page }) => {
    await openSpeaking(page, ["type run the login tests"], "crowded");
    await openCoder(page);

    const pane = page.getByTestId("agent-pane-overlay");
    // It ACCEPTS typing. Dictating into a pane the app itself renders as
    // unwritable would be demonstrating the opposite of the feature.
    await expect(pane).not.toContainText("Terminal input unavailable");
    await expect(pane).not.toContainText("This agent has finished its work");

    await voiceButton(page).click();

    await expect(page.getByText("Typed: “run the login tests”.")).toBeVisible();
    const line = page.getByTestId("voice-dictation");
    await expect(line).toContainText("coder");
    await expect(line).toContainText("sending in");
  });

  /**
   * Scenario: the same words said on the DECK, with no pane open. Rust's own
   * not-here sentence names the prerequisite rather than typing into a terminal
   * nobody can see, which is the whole of the new targeting rule.
   */
  test("names the prerequisite when no pane is open", async ({ page }) => {
    await openSpeaking(page, ["type run the login tests"], "crowded");

    await voiceButton(page).click();

    await expect(page.getByText(/typing to an agent needs that agent's pane open/)).toBeVisible();
    await expect(page.getByTestId("voice-dictation")).toHaveCount(0);
  });

  /**
   * Scenario: say the words, then say "send it". The countdown goes at once
   * rather than being waited out — the third way to send, beside the timer and
   * the user's own keyboard.
   */
  test("sends at once when the user says so", async ({ page }) => {
    await openSpeaking(page, ["type run the login tests", "send it"], "crowded");
    await openCoder(page);

    await voiceButton(page).click();

    await expect(page.getByText("Sent.")).toBeVisible();
    await expect(page.getByTestId("voice-dictation")).toHaveCount(0);
    // And voice is still on, listening for the next thing — which is the
    // difference between this and "voice off".
    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "true");
  });

  /**
   * Scenario: an utterance that ENDS in a submit phrase is typed, not obeyed.
   * This is the false positive the product owner asked about — a trailing rule
   * would submit *"the meeting is at the"* and deliver half an instruction to
   * an agent, which is unrecoverable because sending is the last thing that
   * happens.
   */
  test("types a trailing submit phrase rather than obeying it", async ({ page }) => {
    await openSpeaking(page, ["type the meeting is at the end"], "crowded");
    await openCoder(page);

    await voiceButton(page).click();

    await expect(page.getByText("Typed: “the meeting is at the end”.")).toBeVisible();
    await expect(page.getByTestId("voice-dictation")).toContainText("sending in");
  });
});

test.describe("closing what is on top", () => {
  /**
   * Scenario: the overlay is opened by voice and closed by voice — the defect
   * this row was added for. Before it, the list could only be dismissed by a
   * click or Escape, which breaks the premise of a hands-free surface.
   */
  test("dismisses the list of commands", async ({ page }) => {
    await openSpeaking(page, ["what can I say?", "close this"]);

    await voiceButton(page).click();

    await expect(page.getByTestId("voice-help")).toHaveCount(0);
    await expect(page.getByText("Closed.")).toBeVisible();
  });

  /**
   * Scenario: with no overlay up, the same word closes the agent's pane. The
   * precedence is decided at dispatch because *"an overlay is open"* is not a
   * screen, and this is the "otherwise" half of it.
   */
  test("closes the agent's pane when no overlay is up", async ({ page }) => {
    await openSpeaking(page, ["close this"]);
    await page.getByRole("button", { name: "Open Planner agent" }).click();
    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();

    await voiceButton(page).click();

    await expect(page.getByTestId("agent-pane-overlay")).toHaveCount(0);
  });

  /**
   * Scenario: nothing is on top, so the surface says so rather than claiming a
   * close. The row is callable everywhere — the overlay can be up over any
   * screen — so Rust cannot render a not-here refusal for it.
   */
  test("says so when there is nothing to close", async ({ page }) => {
    await openSpeaking(page, ["close this"]);

    await voiceButton(page).click();

    await expect(page.getByText(/Nothing to close/)).toBeVisible();
  });
});
