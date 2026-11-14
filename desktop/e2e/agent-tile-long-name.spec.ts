import { expect, test } from "@playwright/test";
import { openOverview } from "./support/overview";

/**
 * Issue #2001 made the tile's title the agent's name, which the daemon accepts
 * up to 128 bytes long. jsdom lays nothing out, so whether a long name pushes
 * the status out of a narrow header is a question only an engine can answer.
 */
test.describe("agent pane header with a long name at 400x780", () => {
  test.use({ viewport: { width: 400, height: 780 } });

  /**
   * Scenario: open Planner's pane on a phone-width window and give its title a
   * long unbroken name. The title is cut with an ellipsis and the status beside
   * it stays inside the header.
   *
   * The fixture offers no rename (only a live deck does), so the name is
   * written into the rendered title in place. What is under test is the
   * stylesheet, which lays out whatever text the title holds.
   */
  test("cuts a long name and keeps the status in view", async ({ page }) => {
    const longName = "an-agent-whose-name-is-far-too-long-to-fit-beside-its-status-on-a-phone-width-pane";
    await openOverview(page, "connected");
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();

    const title = overlay.getByRole("heading", { level: 2 });
    await expect(title).toHaveText("Plan / architecture");
    await title.evaluate((el, name) => { el.textContent = name; }, longName);
    expect(await title.evaluate((el) => el.scrollWidth > el.clientWidth), "the long name is cut rather than shown whole").toBe(true);

    const header = await overlay.locator(".agent-header").boundingBox();
    const status = await overlay.locator(".agent-title-line .status-label").boundingBox();
    expect(header, "the header has no layout box").not.toBeNull();
    expect(status, "the status has no layout box").not.toBeNull();
    expect(status!.x + status!.width).toBeLessThanOrEqual(header!.x + header!.width);
    expect(status!.width).toBeGreaterThan(0);
  });

  /**
   * Scenario: open Planner's pane on a phone-width window, crowd its title line
   * with badges and put the rename pencil at its end, as a live deck draws it
   * (issue #1798). The line wraps, so the pencil stays inside the header where
   * it can be pressed, instead of being pushed out past the pane's edge.
   *
   * The fixture offers no rename, so the badges and the pencil are added to the
   * rendered line in place; what is under test is the stylesheet.
   */
  test("keeps the rename pencil inside a crowded header", async ({ page }) => {
    await openOverview(page, "connected");
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();

    await overlay.locator(".agent-title-line").evaluate((line) => {
      for (const text of ["ORCHESTRATOR", "? waiting 3m", "PR #1234 · review required", "✉ 3 deliveries uncertain"]) {
        const badge = document.createElement("span");
        badge.className = "coordinator-badge";
        badge.textContent = text;
        line.append(badge);
      }
      const pencil = document.createElement("button");
      pencil.className = "agent-rename-trigger";
      pencil.textContent = "✎";
      line.append(pencil);
    });

    const header = await overlay.locator(".agent-header").boundingBox();
    const pencil = await overlay.locator(".agent-title-line .agent-rename-trigger").boundingBox();
    expect(header, "the header has no layout box").not.toBeNull();
    expect(pencil, "the pencil has no layout box").not.toBeNull();
    expect(pencil!.x + pencil!.width, "the pencil is pushed out of the header").toBeLessThanOrEqual(header!.x + header!.width);
  });

  /**
   * Scenario: open Planner's pane on a phone-width window and show a not
   * delivered notice that says how the agent's pane ended (issue #1933). The
   * notice wraps inside the header, so the exit code at its end can be read,
   * rather than being cut off with an ellipsis.
   *
   * The fixture has no undeliverable message, so the notice row is added to the
   * rendered header in place; what is under test is the stylesheet.
   */
  test("wraps a not-delivered notice so its exit code shows", async ({ page }) => {
    const notice = "not delivered: the command started in the agent's pane exited (code 7) before taking this message";
    await openOverview(page, "connected");
    await page.getByRole("button", { name: "Open Plan / architecture agent" }).click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();

    await overlay.locator(".agent-title-line").evaluate((line, text) => {
      const row = document.createElement("p");
      row.className = "agent-delivery-notice";
      row.textContent = text;
      line.after(row);
    }, notice);

    const row = overlay.locator(".agent-delivery-notice");
    await expect(row).toHaveText(notice);
    expect(await row.evaluate((el) => el.scrollWidth <= el.clientWidth), "the notice is cut rather than wrapped").toBe(true);
    const header = await overlay.locator(".agent-header").boundingBox();
    const box = await row.boundingBox();
    expect(box!.x + box!.width).toBeLessThanOrEqual(header!.x + header!.width);
  });
});
