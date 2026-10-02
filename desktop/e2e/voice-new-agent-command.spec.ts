import { expect, test, type Page } from "@playwright/test";
import { selectOverview } from "./support/overview";

const SETTINGS_KEY = "dot-agent-deck.desktop-settings";
/** The remote deck of the `fleet` scenario (`FIXTURE_REMOTE_DAEMON_ID`), copied rather than imported for `support/overview.ts`'s reason. */
const REMOTE_DECK = "dev@build-box";

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

/** Open New agent, choose the remote deck and use its `scratch` directory, so the form is live. */
async function openLiveForm(page: Page) {
  await page.getByTestId("overview-new-agent").click();
  await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
  await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
  await page.getByTestId("new-agent-directory-list").locator("[data-path='/home/build/scratch']").click();
  await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/scratch");
  await page.getByTestId("new-agent-use-directory").click();
  await expect(page.getByTestId("new-agent-dir")).toHaveText("/home/build/scratch");
  await expect(page.getByTestId("new-agent-command")).toHaveValue("claude");
}

async function turnOnVoice(page: Page) {
  const trigger = page.getByTestId("voice-trigger");
  await trigger.focus();
  await page.keyboard.press("Enter");
  await expect(trigger).toHaveAttribute("aria-pressed", "true");
}

test.describe("voice sets the New agent command", () => {
  /** Scenario: the maintainer's report. With the New agent form live, saying
   * "Set the command to devbox run agent." puts exactly devbox run agent in
   * Command and reports it; the dialog stays open and nothing starts. */
  test("the spoken command fills the Command field and starts nothing", async ({ page }) => {
    await openSpeaking(page, "Set the command to devbox run agent.");
    await openLiveForm(page);

    await turnOnVoice(page);

    await expect(page.getByTestId("new-agent-command")).toHaveValue("devbox run agent");
    await expect(page.getByTestId("voice-report").getByText("Command: “devbox run agent”.")).toBeVisible();
    await expect(page.getByTestId("voice-report")).not.toContainText("no matching action");
    await expect(page.getByTestId("new-agent-dialog")).toBeVisible();
    await expect(page.getByTestId("new-agent-start")).toBeEnabled();
    await expect(page.getByTestId("agent-pane-overlay")).toHaveCount(0);
  });

  /** Scenario: with the dialog open but no directory chosen the form is not
   * live, so the same sentence is refused with what to do first and Command
   * stays empty. */
  test("the spoken command is refused until the form is live", async ({ page }) => {
    await openSpeaking(page, "Set the command to devbox run agent.");
    await page.getByTestId("overview-new-agent").click();
    await expect(page.getByTestId("new-agent-name")).toBeDisabled();

    await turnOnVoice(page);

    await expect(page.getByTestId("voice-report")).toContainText("Not here — setting the command needs a daemon and a directory chosen in the New agent dialog; choose those first.");
    await expect(page.getByTestId("new-agent-command")).toHaveValue("");
  });
});
