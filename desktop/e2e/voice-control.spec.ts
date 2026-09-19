import { expect, test, type Page } from "@playwright/test";

/** Open the voice surface and submit one typed utterance through its real form. */
async function submitVoice(page: Page, utterance: string) {
  const trigger = page.getByRole("button", { name: "Voice", exact: true });
  await expect(trigger, "Voice control trigger is missing from the primary surface").toBeVisible({ timeout: 1_000 });
  await trigger.click({ timeout: 1_000 });
  const panel = page.getByRole("dialog", { name: "Voice control" });
  await expect(panel).toBeVisible();
  await panel.getByRole("textbox", { name: "Command" }).fill(utterance);
  await panel.getByRole("button", { name: "Run command" }).click();
  return panel;
}

test.describe("voice control through the browser fixture", () => {
  /**
   * Scenario: open Voice while the browser fixture reports that transcription
   * is off. Typed input remains visible, no microphone control is offered, and
   * the deliberate off state is not presented as a failure.
   */
  test("transcription off offers typed input without a broken microphone", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    const trigger = page.getByRole("button", { name: "Voice", exact: true });
    await expect(trigger, "Voice control trigger is missing from the primary surface").toBeVisible({ timeout: 1_000 });
    await trigger.click();

    const panel = page.getByRole("dialog", { name: "Voice control" });
    await expect(panel.getByRole("textbox", { name: "Command" })).toBeVisible();
    await expect(panel.getByRole("button", { name: "Start listening" })).toHaveCount(0);
    await expect(panel).not.toContainText(/not configured|unavailable|failed|error|broken|degraded/i);
  });

  /**
   * Scenario: open Voice from the deck, type the fixture's overview utterance
   * and submit it. The Rust-shaped success sentence appears and the real app
   * navigates to the agent overview in both browser engines.
   */
  test("a typed command dispatches and reports what happened", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");

    const panel = await submitVoice(page, "show me every agent");

    await expect(panel).toContainText("Opening the agent overview.");
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
  });

  /**
   * Scenario: open Planner's agent pane, then reach the peer Voice surface and
   * ask to close the agent view. The real browser accepts the trigger click,
   * dispatches close_agent_view, and returns to the deck in both engines.
   */
  test("voice stays reachable over an agent pane and closes that view", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    await page.getByRole("button", { name: "Open Planner agent" }).click();
    const pane = page.getByTestId("agent-pane-overlay");
    await expect(pane).toBeVisible();

    const trigger = page.getByRole("button", { name: "Voice", exact: true });
    await expect(trigger).toBeVisible();
    const panel = await submitVoice(page, "close this");

    await expect(panel).toContainText("Closing the agent view.");
    await expect(pane).toHaveCount(0);
    await expect(page.getByTestId("agent-tile-planner")).toBeVisible();
  });

  /**
   * Scenario: open the overview first, then ask to open it again through Voice.
   * The fixture returns the table's unavailable sentence naming the deck
   * prerequisite, never the generic no-match explanation.
   */
  test("an unavailable command explains where it is available", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("overview-table-region")).toBeVisible();

    const panel = await submitVoice(page, "show me every agent");

    await expect(panel).toContainText("Not here — the agent overview opens from the deck.");
    await expect(panel).not.toContainText("I don't know how to do that");
  });

  /**
   * Scenario: submit an unsupported request containing odd casing,
   * punctuation and an inner quote. The no-match report says exactly what was
   * heard, preserving that transcript in the rendered browser DOM.
   */
  test("a no-match report shows exactly what was heard", async ({ page }) => {
    await page.goto("/?fixture=1&state=connected");
    const utterance = 'Go, BACK to "Deck"?!';

    const panel = await submitVoice(page, utterance);

    await expect(panel).toContainText('Heard: “Go, BACK to "Deck"?!” — no matching action.');
  });
});
