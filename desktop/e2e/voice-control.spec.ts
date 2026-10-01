import { expect, test, type Page } from "@playwright/test";
import { enterDeck } from "./support/overview";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";

/** Load the browser fixture with its speech backend enabled and canned utterance ready. */
async function openWithSpeech(page: Page, path = "/?fixture=1&state=connected") {
  await page.addInitScript((key) => {
    window.localStorage.setItem(key, JSON.stringify({
      version: 1,
      appearance: { mode: "light" },
      voice: { activation: "toggle", intent: "claude", transcription: "remote" },
      zoom: { level: 1 },
    }));
  }, SETTINGS_KEY);
  await page.goto(path);
  await enterDeck(page);
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

    await expect(page.getByText("Opening the agent dashboard.")).toBeVisible();
    await expect(page.getByTestId("overview-table-region")).toBeVisible();
    await expect(trigger).toHaveText(/voice\s+on/i);
    await expect(trigger).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByRole("dialog", { name: "Voice control" })).toHaveCount(0);
  });
});

test.describe("numbered voice choices through the browser fixture", () => {
  const choicePath = "/?fixture=1&state=connected&voice=open%20the%20agent";
  const firstLabel = "1. Plan / architecture";
  const secondLabel = "2. Desktop implementation";

  async function offerChoice(page: Page) {
    await openWithSpeech(page, choicePath);
    await voiceButton(page).click();
    const choice = page.getByTestId("voice-choice");
    await expect(choice.getByRole("button", { name: firstLabel })).toBeVisible();
    await expect(choice.getByRole("button", { name: secondLabel })).toBeVisible();
    return choice;
  }

  for (const [label, agentId, report] of [
    [firstLabel, "planner", "Opening Plan / architecture."],
    [secondLabel, "builder", "Opening Desktop implementation."],
  ] as const) {
    /** Scenario: the numbered entries are real browser controls. Clicking one
     * opens its own agent pane and reports the chosen agent's name. */
    test(`clicking ${label} opens that agent`, async ({ page }) => {
      const choice = await offerChoice(page);
      const entry = choice.getByRole("button", { name: label });
      await entry.click({ trial: true });
      await entry.click();
      await expect(page.getByText(report)).toBeVisible();
      await expect(page.getByTestId("agent-pane-overlay").getByTestId(`terminal-${agentId}`)).toBeVisible();
      await expect(choice).toHaveCount(0);
    });
  }

  /** Scenario: a keyboard user can tab between numbered entries and activate
   * the second one with Enter, opening its pane without a pointer. */
  test("numbered entries are tabbable and keyboard-activatable", async ({ page }) => {
    const choice = await offerChoice(page);
    const first = choice.getByRole("button", { name: firstLabel });
    const second = choice.getByRole("button", { name: secondLabel });
    await first.focus();
    await expect(first).toBeFocused();
    await page.keyboard.press("Tab");
    await expect(second).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.getByText("Opening Desktop implementation.")).toBeVisible();
    await expect(page.getByTestId("agent-pane-overlay").getByTestId("terminal-builder")).toBeVisible();
    await expect(choice).toHaveCount(0);
  });

  /** Scenario: Cancel dismisses an offered choice; it opens neither pane and
   * leaves voice listening for another utterance. */
  test("Cancel dismisses the choice without opening an agent", async ({ page }) => {
    const choice = await offerChoice(page);
    const cancel = choice.getByRole("button", { name: "Cancel" });
    await cancel.click({ trial: true });
    await cancel.click();
    await expect(choice).toHaveCount(0);
    await expect(page.getByTestId("agent-pane-overlay")).toHaveCount(0);
    await expect(page.getByTestId("voice-report")).toContainText(/cancelled/i);
    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "true");
  });

  /** Scenario: the fixture hears a second utterance, "two", while its list is
   * open; that spoken answer opens the second pane and closes the list. */
  test("a spoken number answers the pending choice", async ({ page }) => {
    await openWithSpeech(page, `${choicePath}&voice=two`);
    await voiceButton(page).click();
    await expect(page.getByText("Opening Desktop implementation.")).toBeVisible();
    await expect(page.getByTestId("agent-pane-overlay").getByTestId("terminal-builder")).toBeVisible();
    await expect(page.getByTestId("voice-choice")).toHaveCount(0);
    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "true");
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
   * Scenario: open Planner's pane over the daemon, so the agent is enlarged to the
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

  /** What the row and its text measure, and what the page reserves for it. */
  async function rowSize(page: Page) {
    return page.evaluate(() => {
      const row = document.querySelector<HTMLElement>(".voice-row");
      const label = document.querySelector<HTMLElement>(".voice-trigger span");
      if (!row || !label) throw new Error("the voice row is not mounted");
      return {
        height: row.getBoundingClientRect().height,
        label: parseFloat(getComputedStyle(label).fontSize),
        sentences: [...row.querySelectorAll<HTMLElement>(".voice-sentence")].map((sentence) => parseFloat(getComputedStyle(sentence).fontSize)),
        reserved: parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--voice-row-height")),
      };
    });
  }

  /**
   * Scenario: PR #1451 — turn voice on and let the fixture's utterance report.
   * The row and its text are half as large again while voice is on, the page
   * reserves the row's real height, and turning voice off returns the row and
   * the reservation to their resting 40px.
   */
  test("is half as large again while voice is on and back to its resting size when off", async ({ page }) => {
    await openWithSpeech(page);
    const off = await rowSize(page);
    expect(off.height).toBeCloseTo(40, 0);
    expect(off.reserved).toBe(40);

    await voiceButton(page).click();
    await expect(page.getByText("Opening the agent dashboard.")).toBeVisible();
    const on = await rowSize(page);
    expect(on.height / off.height).toBeGreaterThanOrEqual(1.5);
    expect(on.label / off.label).toBeCloseTo(1.5, 2);
    expect(on.sentences.length).toBeGreaterThan(0);
    for (const size of on.sentences) expect(size).toBeCloseTo(16.5, 2);
    expect(on.reserved).toBeCloseTo(on.height, 0);

    await voiceButton(page).click();
    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "false");
    const again = await rowSize(page);
    expect(again.height).toBeCloseTo(40, 0);
    expect(again.reserved).toBe(40);
    expect(again.label).toBe(off.label);
    for (const size of again.sentences) expect(size).toBeCloseTo(11, 2);
  });

  /**
   * Scenario: PR #1451 — at a narrow window, enter typing mode in a writable
   * agent's pane. The larger typing-mode status wraps onto more lines rather
   * than being cut off or overflowing, the row grows to hold it, and the
   * enlarged agent still stops above the taller row.
   */
  test("wraps the larger typing-mode status at a narrow window rather than cutting it off", async ({ page }) => {
    await page.setViewportSize({ width: 420, height: 760 });
    await openWithSpeech(page, "/?fixture=1&state=crowded&voice=type%20on");
    await page.getByRole("button", { name: "Open coder agent" }).click();
    await voiceButton(page).click();
    const status = page.getByTestId("voice-dictating");
    await expect(status).toHaveText("Typing to coder. Say “type off” to stop, “send it” to send.");

    const text = await status.evaluate((element) => {
      const box = element.getBoundingClientRect();
      return {
        scrollWidth: element.scrollWidth,
        clientWidth: element.clientWidth,
        height: box.height,
        right: box.right,
        lineHeight: parseFloat(getComputedStyle(element).lineHeight),
        pageScrollWidth: document.documentElement.scrollWidth,
        viewportWidth: window.innerWidth,
      };
    });
    expect(text.scrollWidth, "the status is cut off inside its box").toBeLessThanOrEqual(text.clientWidth + 1);
    expect(text.height, "the status did not wrap").toBeGreaterThan(text.lineHeight * 1.5);
    expect(text.right, "the status runs past the window").toBeLessThanOrEqual(text.viewportWidth + 1);
    expect(text.pageScrollWidth, "the page scrolls sideways").toBeLessThanOrEqual(text.viewportWidth);

    const geometry = await rowGeometry(page);
    const statusBox = await status.boundingBox();
    expect(statusBox).not.toBeNull();
    expect(statusBox!.y).toBeGreaterThanOrEqual(geometry.row.top - 1);
    expect(statusBox!.y + statusBox!.height).toBeLessThanOrEqual(geometry.row.bottom + 1);
    expect(geometry.centreIsInsideTheRow).toBe(true);
    const tile = await page.getByTestId("agent-pane-overlay").locator('.agent-tile[data-presentation="overlay"]').boundingBox();
    expect(tile, "the enlarged pane has no layout box").not.toBeNull();
    expect(tile!.y + tile!.height, "the enlarged agent runs under the taller voice row").toBeLessThanOrEqual(geometry.row.top + 1);
  });

  /** Scenario: open a writable agent's modal pane and enter dictation with the
   * browser fixture's spoken script. Stop typing stays in the tab order and
   * passes the browser's real click hit test behind that modal. */
  test("Stop typing is clickable and tabbable behind the agent pane", async ({ page }) => {
    await openWithSpeech(page, "/?fixture=1&state=crowded&voice=type%20on");
    await page.getByRole("button", { name: "Open coder agent" }).click();
    await voiceButton(page).click();
    const stop = page.getByRole("button", { name: "Stop typing" });
    await expect(stop).toBeVisible();
    await expect.poll(() => stop.evaluate((button) => button.closest("[inert]") === null && (button as HTMLButtonElement).tabIndex >= 0)).toBe(true);
    await stop.click({ trial: true });
    await stop.focus();
    await expect(stop).toBeFocused();
  });

  /**
   * Scenario: PR #1451 — enter typing mode in a writable agent's pane and
   * dictate a sentence, then say nothing. After the pause the "“send it” to
   * send" words are visibly highlighted, the status reads exactly as before,
   * and nothing was sent.
   */
  test("highlights how to send after a pause in typing mode", async ({ page }) => {
    await openWithSpeech(page, "/?fixture=1&state=crowded&voice=type%20on&voice=fix%20the%20bug");
    await page.getByRole("button", { name: "Open coder agent" }).click();
    await voiceButton(page).click();
    const hint = page.getByTestId("voice-send-hint");
    await expect(page.getByTestId("voice-report")).toContainText("fix the bug");
    await expect(hint).not.toHaveAttribute("data-nudge", "on");

    await expect(hint).toHaveAttribute("data-nudge", "on", { timeout: 10_000 });
    await expect(page.getByTestId("voice-dictating")).toHaveText("Typing to coder. Say “type off” to stop, “send it” to send.");
    const lit = await hint.evaluate((element) => getComputedStyle(element).backgroundColor);
    expect(lit, "the nudge is not visibly highlighted").not.toBe("rgba(0, 0, 0, 0)");
    await expect(page.getByTestId("voice-report")).not.toContainText(/sent/i);
  });

  /** Scenario: the browser fixture enters typing mode in a writable agent pane, then hears a punctuated stop command. It ends the mode without typing those command words into the pane. */
  test("a spoken punctuated stop ends typing mode without typing it", async ({ page }) => {
    await openWithSpeech(page, "/?fixture=1&state=crowded&voice=type%20on&voice=stop%20typing.");
    await page.getByRole("button", { name: "Open coder agent" }).click();
    await voiceButton(page).click();

    await expect(page.getByTestId("voice-report")).toContainText("Typing mode off. Nothing was sent to coder.");
    await expect(page.getByTestId("voice-dictating")).toHaveCount(0);
    await expect(page.getByTestId("voice-report")).not.toContainText("Typed:");
    await expect(voiceButton(page)).toHaveAttribute("aria-pressed", "true");
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

    await expect(page.getByText("Opening the agent dashboard.")).toBeVisible();
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
