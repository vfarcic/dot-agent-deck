import { expect, test } from "@playwright/test";

import { openOverview } from "./support/overview";

/**
 * PRD #1223 M4/M5 — the New agent flow, end to end on the fixture bridge: from
 * the overview, through the deck step, a directory browsed on the chosen deck
 * and the form, to the new agent's pane open over the overview.
 *
 * The vitest tier drives each step against a fake runtime; what only this tier
 * has is the production bundle, a real keyboard delivering the picker's keys to
 * the element that has focus, and the fixture deck adding the agent to its
 * fleet entry the way a live deck's snapshot does — so the pane opening here is
 * the flow's own wait for the fleet, not a stub standing in for it.
 */

/** The remote deck of the `fleet` scenario (`FIXTURE_REMOTE_DAEMON_ID`), copied rather than imported for `support/overview.ts`'s reason. */
const REMOTE_DECK = "dev@build-box";

test.describe("the New agent flow", () => {
  /**
   * Scenario: on the four-deck fleet, click New agent in the overview's top
   * bar. The unreachable and pending decks are listed disabled; choose the
   * remote deck. Its home lists a project directory and an ordinary one; move
   * to the ordinary one with `j`, enter it with Enter, and use it with Space.
   * The form names the agent after the directory and prefills the deck's
   * configured command; Start opens the new agent's pane over the overview.
   */
  test("starts an agent in a browsed directory and opens its pane", async ({ page }) => {
    await openOverview(page, "fleet");

    await page.getByTestId("overview-new-agent").click();
    const dialog = page.getByTestId("new-agent-dialog");
    await expect(dialog).toHaveAttribute("data-step", "deck");
    const decks = page.getByTestId("new-agent-deck-list").getByRole("option");
    await expect(decks).toHaveCount(4);
    await expect(decks.and(page.locator("[aria-disabled='true']"))).toHaveCount(2);

    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();

    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
    const directories = page.getByTestId("new-agent-directory-list");
    await expect(directories).toBeFocused();
    await expect(directories.getByTestId("new-agent-project-mark")).toHaveCount(1);
    await page.keyboard.press("j");
    await expect(directories.locator("[aria-selected='true']")).toHaveAttribute("data-path", "/home/build/scratch");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/scratch");
    await page.keyboard.press(" ");

    await expect(dialog).toHaveAttribute("data-step", "form");
    await expect(page.getByTestId("new-agent-dir")).toHaveText("/home/build/scratch");
    await expect(page.getByTestId("new-agent-name")).toHaveValue("scratch");
    await expect(page.getByTestId("new-agent-command")).toHaveValue("claude");
    await page.getByTestId("new-agent-start").click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay).toHaveAttribute("aria-label", "Claude agent");
    await expect(dialog).toHaveCount(0);

    // The remote deck's group now lists it; closing the pane lands back on
    // the overview, where it is a row of that deck and of no other.
    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    const remoteGroup = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`);
    await expect(remoteGroup.getByRole("button", { name: "Open scratch agent", exact: true })).toBeVisible();
  });

  /**
   * Scenario (PRD #1223 M7): on the fleet, open New agent and choose the
   * remote deck, whose own experimental flag is on. Use its home directory
   * with Space. The Mode row offers the three authoring agents; move to
   * `schedule: issues` with the arrow keys, clear the prefilled Command and
   * Start. The blank Command resolves to `claude` rather than the deck's
   * default shell, and the new authoring agent's pane opens over the overview.
   */
  test("starts an authoring agent with its blank Command resolved and opens its pane", async ({ page }) => {
    await openOverview(page, "fleet");

    await page.getByTestId("overview-new-agent").click();
    const dialog = page.getByTestId("new-agent-dialog");
    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
    await expect(page.getByTestId("new-agent-directory-list")).toBeFocused();
    await page.keyboard.press(" ");

    await expect(dialog).toHaveAttribute("data-step", "form");
    const modes = page.getByTestId("new-agent-modes").getByRole("button");
    await expect(modes).toHaveText(["No mode", "schedule", "schedule: issues", "dispatcher"]);
    await page.getByTestId("new-agent-mode-none").focus();
    await page.keyboard.press("ArrowRight");
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("new-agent-mode-schedule-issues")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("new-agent-mode-schedule-issues")).toBeFocused();
    await page.getByTestId("new-agent-command").fill("");
    await expect(page.getByTestId("new-agent-command")).toHaveAttribute("placeholder", "Empty starts claude");
    await page.getByTestId("new-agent-start").click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay).toHaveAttribute("aria-label", "Claude agent");
    await expect(dialog).toHaveCount(0);
  });

  /**
   * Scenario (PRD #1223 M7): with the remote deck playing a deck from before
   * PRD #1223, open the flow from its header and type a path. The form offers
   * No mode alone and says why the authoring agents are missing.
   */
  test("withholds the authoring agents on a deck that cannot compose their seeds", async ({ page }) => {
    await page.goto(`/?fixture=1&state=fleet&older=${encodeURIComponent(REMOTE_DECK)}`);
    await page.getByTestId("open-overview").click();
    await page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`).getByTestId("daemon-new-agent").click();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-path")).toBeFocused();
    await page.keyboard.type("/srv/checkouts/repo");
    await page.keyboard.press("Enter");

    await expect(page.getByTestId("new-agent-authoring-withheld")).toBeVisible();
    await expect(page.getByTestId("new-agent-modes").getByRole("button")).toHaveText(["No mode"]);
  });

  /**
   * Scenario: with the remote deck playing a deck from before PRD #1223, open
   * the flow from that deck's own header. It is preselected and one Enter
   * confirms it; there is no listing, so type the directory's path and use it.
   * The form offers this app's own agent list and says so, and Start opens the
   * pane as before.
   */
  test("falls back to a typed path on a deck without the listing verb", async ({ page }) => {
    await page.goto(`/?fixture=1&state=fleet&older=${encodeURIComponent(REMOTE_DECK)}`);
    await page.getByTestId("open-overview").click();
    const remoteGroup = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`);
    await remoteGroup.getByTestId("daemon-new-agent").click();

    await expect(page.getByTestId("new-agent-deck-list").locator("[aria-selected='true']")).toHaveAttribute("data-deck-id", REMOTE_DECK);
    await page.keyboard.press("Enter");

    await expect(page.getByTestId("new-agent-no-browse")).toBeVisible();
    await expect(page.getByTestId("new-agent-directory-list")).toHaveCount(0);
    await expect(page.getByTestId("new-agent-path")).toBeFocused();
    await page.keyboard.type("/srv/checkouts/repo");
    await page.keyboard.press("Enter");

    await expect(page.getByTestId("new-agent-dir")).toHaveText("/srv/checkouts/repo");
    await expect(page.getByTestId("new-agent-desktop-registry")).toBeVisible();
    await page.getByTestId("new-agent-start").click();

    await expect(page.getByTestId("agent-pane-overlay")).toBeVisible();
  });
});
