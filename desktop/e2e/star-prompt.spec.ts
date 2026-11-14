import { expect } from "@playwright/test";
import { test } from "./support/load-budget";

/**
 * PRD #2174 — the star reminder, as a user sees it when the app starts on a
 * launch the shared reminder state says is due (`?star=1` plays one).
 */

test.describe("the star reminder", () => {
  /** Scenario: the app starts on a due launch. The reminder is up over the dashboard, whole on screen, with the TUI's words; `l` closes it and the dashboard is usable again. */
  test("shows at startup on a due launch, and l closes it", async ({ page }) => {
    await page.goto("/?fixture=1&star=1");
    const dialog = page.getByRole("alertdialog", { name: "Enjoying dot-agent-deck?" });
    await expect(dialog).toBeVisible();
    await expect(dialog).toContainText("please consider starring the repo!");
    await expect(dialog).toContainText("github.com/vfarcic/dot-agent-deck");
    for (const name of ["s Star", "l Later", "d Don't ask again"]) await expect(dialog.getByRole("button", { name })).toBeInViewport({ ratio: 1 });
    await page.keyboard.press("l");
    await expect(dialog).toBeHidden();
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
  });

  /** Scenario: the user clicks Star. The reminder closes and the app thanks them, as the TUI's status line does. */
  test("thanks the user after Star", async ({ page }) => {
    await page.goto("/?fixture=1&star=1");
    await page.getByRole("alertdialog", { name: "Enjoying dot-agent-deck?" }).getByRole("button", { name: "s Star" }).click();
    await expect(page.getByTestId("star-toast")).toContainText("Thanks for starring!");
    await expect(page.getByRole("alertdialog", { name: "Enjoying dot-agent-deck?" })).toBeHidden();
  });

  /** Scenario: the app starts on a launch that is not due. No reminder shows. */
  test("shows nothing on a launch that is not due", async ({ page }) => {
    await page.goto("/?fixture=1");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await expect(page.getByTestId("star-prompt-dialog")).toHaveCount(0);
  });
});
