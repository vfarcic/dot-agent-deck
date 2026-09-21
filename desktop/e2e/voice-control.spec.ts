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

/**
 * The reserved bottom row (PRD #802, the product owner's real-microphone
 * review).
 *
 * The trigger and the report used to be two independently fixed boxes floating
 * over the screen, and the report was drawn over whatever was under it —
 * including over the enlarged agent pane, which is the one place the user is
 * actually working. These are geometry tests because jsdom computes no layout:
 * the vitest tier can assert that the row contains both cells and nothing about
 * whether anything covers it.
 *
 * `elementFromPoint` is the assertion that matters. A box comparison proves the
 * pane's CONTENT stops above the row; only a hit test proves the row is what a
 * click at that point actually reaches, which is the property the scrim over it
 * would break. Playwright's `click({ trial: true })` is the same question asked
 * of a real control: it runs every actionability check, hit-testing included,
 * and performs no click.
 */
test.describe("the voice row is reserved space", () => {
  /** Every box read in one round trip, so nothing can reflow between two reads. */
  async function rowGeometry(page: Page) {
    return page.evaluate(() => {
      const row = document.querySelector<HTMLElement>(".voice-row");
      const rail = document.querySelector<HTMLElement>(".rail");
      if (!row || !rail) throw new Error("the voice row or the rail is not mounted");
      const box = row.getBoundingClientRect();
      const centre = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
      return {
        row: { top: box.top, bottom: box.bottom, left: box.left, right: box.right, height: box.height },
        railRight: rail.getBoundingClientRect().right,
        viewport: { width: window.innerWidth, height: window.innerHeight },
        // What a click at the row's own centre would reach. `false` is the
        // failure this whole change is about: something is drawn over the row.
        centreIsInsideTheRow: centre !== null && row.contains(centre),
      };
    });
  }

  /**
   * Scenario: open Planner's pane over the deck, so the agent is enlarged to the
   * whole window, and measure the voice row underneath it. The row sits on the
   * window's bottom edge from the rail's right edge across, the pane's content
   * stops above it, a click at the row's centre reaches the row rather than the
   * pane's scrim, and the Voice button is genuinely pressable.
   */
  test("is never covered by the enlarged agent pane", async ({ page }) => {
    await openWithSpeech(page);
    await page.getByRole("button", { name: "Open Planner agent" }).click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();

    const geometry = await rowGeometry(page);
    // Pinned to the window's bottom edge, and to the content area rather than
    // over the rail — whose own bottom controls it would otherwise cover.
    expect(Math.abs(geometry.row.bottom - geometry.viewport.height)).toBeLessThanOrEqual(1);
    expect(Math.abs(geometry.row.right - geometry.viewport.width)).toBeLessThanOrEqual(1);
    expect(Math.abs(geometry.row.left - geometry.railRight)).toBeLessThanOrEqual(1);
    expect(geometry.row.height).toBeGreaterThan(0);

    // The overlay still covers the window — a modal scrim should — and its
    // CONTENT box is what stops above the row. The tile is that content.
    const tile = await overlay.locator('.agent-tile[data-presentation="overlay"]').boundingBox();
    expect(tile, "the enlarged pane has no layout box").not.toBeNull();
    expect(
      tile!.y + tile!.height,
      "the enlarged agent extends into the reserved voice row",
    ).toBeLessThanOrEqual(geometry.row.top + 1);

    expect(geometry.centreIsInsideTheRow, "something is drawn over the voice row").toBe(true);
    // And the control in it is reachable, not merely visible: this is the
    // `VOICE_PEER_PROPS` exemption and the z-order, asserted together.
    await voiceButton(page).click({ trial: true });
  });

  /**
   * Scenario: let the speech fixture's utterance navigate to the overview, then
   * look at the row it reported in. The Undo beside the sentence is hit-testable
   * rather than merely rendered, and the overview's own content stops above the
   * row instead of running underneath it.
   */
  test("keeps Undo reachable and leaves the screen content above it", async ({ page }) => {
    await openWithSpeech(page);
    await voiceButton(page).click();

    await expect(page.getByText("Opening the agent overview.")).toBeVisible();
    const undo = page.getByRole("button", { name: "Undo" });
    await expect(undo).toBeVisible();
    // The real question about a control: can it be clicked. `trial` runs every
    // actionability check — visible, stable, receives events — and clicks
    // nothing, so the Undo's ten-second window is not consumed by the test.
    await undo.click({ trial: true });

    const geometry = await rowGeometry(page);
    expect(geometry.centreIsInsideTheRow).toBe(true);
    const region = await page.getByTestId("overview-table-region").boundingBox();
    expect(region, "the overview has no layout box").not.toBeNull();
    expect(
      region!.y + region!.height,
      "the overview's table region runs under the reserved voice row",
    ).toBeLessThanOrEqual(geometry.row.top + 1);
  });
});
