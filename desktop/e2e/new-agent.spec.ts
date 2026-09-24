import { expect, test } from "@playwright/test";

import { openOverview } from "./support/overview";

/**
 * PRD #1223 M4/M5 — the New agent flow, end to end on the fixture bridge: from
 * the overview, through the one dialog's deck field, a directory browsed on the
 * chosen deck and the form below it, to the new agent's pane open over the
 * overview. There are no steps: every field is on screen at once (PRD #1223,
 * the voice-first redesign).
 *
 * The vitest tier drives each field against a fake runtime; what only this tier
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
    // One surface: the deck field, the directory panel and the form are all
    // there before anything is chosen, the form waiting for a directory.
    await expect(page.getByTestId("new-agent-deck-list")).toBeFocused();
    await expect(page.getByTestId("new-agent-directory-idle")).toBeVisible();
    await expect(page.getByTestId("new-agent-name")).toBeDisabled();
    await expect(page.getByTestId("new-agent-start")).toBeDisabled();
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

    await expect(page.getByTestId("new-agent-name")).toBeFocused();
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
   * Scenario (PRD #1223 U4): start an agent in `scratch` on the remote deck and
   * close its pane. On the overview, press Stop on its row: a confirmation
   * names the agent and the deck. Confirm it, and the agent leaves that deck's
   * group — the fixture deck drops it the way a live deck's agent list does.
   */
  test("stops an agent it started from the overview, and the agent leaves the fleet", async ({ page }) => {
    await openOverview(page, "fleet");

    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
    const directories = page.getByTestId("new-agent-directory-list");
    await expect(directories).toBeFocused();
    await page.keyboard.press("j");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/scratch");
    await page.keyboard.press(" ");
    await page.getByTestId("new-agent-start").click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);

    const remoteGroup = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`);
    await expect(remoteGroup.getByRole("button", { name: "Open scratch agent", exact: true })).toBeVisible();
    await remoteGroup.getByRole("button", { name: "Stop scratch agent", exact: true }).click();
    const confirm = page.getByRole("alertdialog");
    await expect(confirm).toContainText("Stop scratch?");
    await expect(confirm).toContainText(REMOTE_DECK);
    await confirm.getByRole("button", { name: "Stop agent" }).click();

    await expect(confirm).toHaveCount(0);
    await expect(remoteGroup.getByRole("button", { name: "Open scratch agent", exact: true })).toHaveCount(0);
  });

  /**
   * Scenario (PRD #1223 U4): launch `demo-loop` in `demo-project` on the remote
   * deck and close the start role's pane. On the overview, press Close on the
   * orchestration's card: the confirmation says it stops every role and names
   * planner and builder. Confirm it, and both roles leave that deck's group.
   */
  test("closes an orchestration it launched, and every role leaves the fleet", async ({ page }) => {
    await openOverview(page, "fleet");

    await page.getByTestId("overview-new-agent").click();
    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
    await expect(page.getByTestId("new-agent-directory-list")).toBeFocused();
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/demo-project");
    await page.keyboard.press(" ");
    await page.getByTestId("new-agent-mode-orch:demo-loop").click();
    await expect(page.getByTestId("new-agent-name")).toHaveValue("demo-project-orchestrator-1");
    await page.getByTestId("new-agent-start").click();
    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);

    const remoteGroup = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`);
    await expect(remoteGroup.getByRole("button", { name: "Open planner agent", exact: true })).toBeVisible();
    await remoteGroup.getByRole("button", { name: "Close demo-project-orchestrator-1 orchestration" }).click();
    const confirm = page.getByRole("alertdialog");
    await expect(confirm).toContainText("This stops every role of this orchestration");
    await expect(confirm).toContainText("planner, builder");
    await confirm.getByRole("button", { name: "Stop all 2 roles" }).click();

    await expect(confirm).toHaveCount(0);
    await expect(remoteGroup.getByRole("button", { name: "Open planner agent", exact: true })).toHaveCount(0);
    await expect(remoteGroup.getByRole("button", { name: "Open builder agent", exact: true })).toHaveCount(0);
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

    await expect(page.getByTestId("new-agent-name")).toBeFocused();
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
   * Scenario (PRD #1223 M6): on the fleet, open New agent and choose the
   * remote deck. Its home marks `demo-project` as a project; enter it and use
   * it. The Mode row offers `Orch: demo-loop` after No mode; select it with the
   * arrow keys and the Name becomes `demo-project-orchestrator-1` while Command
   * disappears. Start launches the orchestration on that deck: the START
   * role's pane opens over the overview, and back on the overview both roles
   * are rows of the remote deck.
   */
  test("launches an orchestration in a project directory and opens its start role's pane", async ({ page }) => {
    await openOverview(page, "fleet");

    await page.getByTestId("overview-new-agent").click();
    const dialog = page.getByTestId("new-agent-dialog");
    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
    const directories = page.getByTestId("new-agent-directory-list");
    await expect(directories).toBeFocused();
    await expect(directories.locator("[aria-selected='true']")).toHaveAttribute("data-path", "/home/build/demo-project");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/demo-project");
    await page.keyboard.press(" ");

    await expect(page.getByTestId("new-agent-name")).toBeFocused();
    await expect(page.getByTestId("new-agent-name")).toHaveValue("demo-project");
    const modes = page.getByTestId("new-agent-modes").getByRole("button");
    await expect(modes).toHaveText(["No mode", "Orch: demo-loop", "schedule", "schedule: issues", "dispatcher"]);
    await page.getByTestId("new-agent-mode-none").focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("new-agent-mode-orch:demo-loop")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("new-agent-name")).toHaveValue("demo-project-orchestrator-1");
    await expect(page.getByTestId("new-agent-command")).toHaveCount(0);
    await page.getByTestId("new-agent-start").click();

    const overlay = page.getByTestId("agent-pane-overlay");
    await expect(overlay).toBeVisible();
    await expect(overlay).toHaveAttribute("aria-label", "Claude agent");
    await expect(dialog).toHaveCount(0);

    await page.keyboard.press("Escape");
    await expect(overlay).toHaveCount(0);
    const remoteGroup = page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`);
    await expect(remoteGroup.getByRole("button", { name: "Open planner agent", exact: true })).toBeVisible();
    await expect(remoteGroup.getByRole("button", { name: "Open builder agent", exact: true })).toBeVisible();
  });

  /**
   * Scenario (PRD #1223 audit F2): on the fleet, open New agent on the remote
   * deck and browse into `scratch/twin-project`, whose config defines
   * `twin-loop` twice and `solo-loop` once. The Mode row shows both
   * `twin-loop` chips disabled after `solo-loop`, with a line saying to rename
   * one; the arrow keys go from No mode to `solo-loop` and past the namesakes.
   */
  test("shows a project's namesake orchestrations disabled with the reason", async ({ page }) => {
    await openOverview(page, "fleet");

    await page.getByTestId("overview-new-agent").click();
    const dialog = page.getByTestId("new-agent-dialog");
    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build");
    const directories = page.getByTestId("new-agent-directory-list");
    await expect(directories).toBeFocused();
    await page.keyboard.press("j");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/scratch");
    await page.keyboard.press("j");
    await expect(directories.locator("[aria-selected='true']")).toHaveAttribute("data-path", "/home/build/scratch/twin-project");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/scratch/twin-project");
    await page.keyboard.press(" ");

    await expect(page.getByTestId("new-agent-name")).toBeFocused();
    const modes = page.getByTestId("new-agent-modes").getByRole("button");
    await expect(modes).toHaveText(["No mode", "Orch: solo-loop", "Orch: twin-loop", "Orch: twin-loop", "schedule", "schedule: issues", "dispatcher"]);
    await expect(page.getByTestId("new-agent-mode-ambiguous-0")).toBeDisabled();
    await expect(page.getByTestId("new-agent-mode-ambiguous-1")).toBeDisabled();
    await expect(page.getByTestId("new-agent-orchestration-ambiguous")).toHaveText("This project defines more than one orchestration named twin-loop; rename one to launch it here.");
    await page.getByTestId("new-agent-mode-none").focus();
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("new-agent-mode-orch:solo-loop")).toHaveAttribute("aria-pressed", "true");
    await page.keyboard.press("ArrowRight");
    await expect(page.getByTestId("new-agent-mode-schedule")).toHaveAttribute("aria-pressed", "true");
  });

  /**
   * Scenario (PRD #1223 M6): with the remote deck playing one built for a
   * non-Unix platform, open the flow from its header, browse into
   * `demo-project` and use it. The deck cannot start a role with its configured
   * command, so no orchestration chip is offered and the form says why.
   */
  test("withholds orchestrations on a deck that cannot start configured roles", async ({ page }) => {
    await page.goto(`/?fixture=1&state=fleet&nonunix=${encodeURIComponent(REMOTE_DECK)}`);
    await page.getByTestId("open-overview").click();
    // Opened from the deck's header, the deck is chosen and its home listed
    // with no key pressed; focus is already in the browser.
    await page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`).getByTestId("daemon-new-agent").click();
    const directories = page.getByTestId("new-agent-directory-list");
    await expect(directories).toBeFocused();
    await expect(directories.locator("[aria-selected='true']")).toHaveAttribute("data-path", "/home/build/demo-project");
    await page.keyboard.press("Enter");
    await expect(page.getByTestId("new-agent-current-path")).toHaveText("/home/build/demo-project");
    await page.keyboard.press(" ");

    await expect(page.getByTestId("new-agent-orchestrations-withheld")).toContainText("configured commands");
    await expect(page.getByTestId("new-agent-modes").getByRole("button")).toHaveText(["No mode", "schedule", "schedule: issues", "dispatcher"]);
  });

  /**
   * Scenario (PRD #1223 U1): with the remote deck playing a deck from before
   * PRD #1223, it has no directory listing, and browsing is the only way the
   * flow chooses a directory. Its header offers no New agent; opened from the
   * top bar, the deck field lists it disabled with the deck's reason, and
   * clicking it chooses nothing. The local deck — the one eligible deck, so
   * chosen on open — still lists its home.
   */
  test("disables a deck without the listing verb in the deck field", async ({ page }) => {
    await page.goto(`/?fixture=1&state=fleet&older=${encodeURIComponent(REMOTE_DECK)}`);
    await page.getByTestId("open-overview").click();
    await expect(page.locator(`[data-testid="daemon-group"][data-daemon-id="${REMOTE_DECK}"]`).getByTestId("daemon-new-agent")).toHaveCount(0);

    await page.getByTestId("overview-new-agent").click();
    const dialog = page.getByTestId("new-agent-dialog");
    const remote = page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`);
    await expect(remote).toHaveAttribute("aria-disabled", "true");
    await expect(remote).toContainText("does not advertise list-directories");
    // Playwright will not click an `aria-disabled` element, which is the point;
    // the click is dispatched to prove the row itself ignores it too.
    await remote.dispatchEvent("click");
    await expect(dialog).toBeVisible();
    await expect(remote).not.toHaveAttribute("data-chosen", "true");
    await expect(page.getByTestId("new-agent-deck-list").locator("[data-chosen='true']")).toHaveCount(1);
    await expect(page.getByTestId("new-agent-directory-list")).toBeFocused();
    await expect(page.getByTestId("new-agent-path")).toHaveCount(0);
  });

  /**
   * Scenario (PRD #1223, the voice-first redesign): open New agent from the top
   * bar and choose the remote deck by clicking it. Focus moves to its listing
   * without a Next; Space uses the home directory and focus moves on to Name.
   * Typing a `q` there types it — `q` closes only from the browser — and
   * Shift+Tab walks back up through Mode into the browser — there is no Agent
   * picker between them any more (PRD #1223). There,
   * `q` closes the dialog and focus returns to the button that opened it.
   */
  test("moves focus field by field and scopes q to the browser", async ({ page }) => {
    await openOverview(page, "fleet");

    const opener = page.getByTestId("overview-new-agent");
    await opener.click();
    const dialog = page.getByTestId("new-agent-dialog");
    await page.getByTestId("new-agent-deck-list").locator(`[data-deck-id="${REMOTE_DECK}"]`).click();
    const directories = page.getByTestId("new-agent-directory-list");
    await expect(directories).toBeFocused();
    await page.keyboard.press(" ");

    const name = page.getByTestId("new-agent-name");
    await expect(name).toBeFocused();
    await expect(name).toHaveValue("build");
    await page.keyboard.press("q");
    await expect(name).toHaveValue("buildq");
    await expect(dialog).toBeVisible();

    await expect(page.getByTestId("new-agent-agent")).toHaveCount(0);
    await page.keyboard.press("Shift+Tab");
    await expect(page.getByTestId("new-agent-modes").locator(":focus")).toHaveCount(1);
    await directories.focus();
    await page.keyboard.press("q");
    await expect(dialog).toHaveCount(0);
    await expect(opener).toBeFocused();
  });

  /**
   * `aria-modal="true"` made true for this dialog, in the only tier that can
   * tell — the sibling of `agent-pane-modal.spec.ts`, which says the same for
   * the pane and explains why the claim is a security one there.
   *
   * Greptile's review of PR #1235 found the dialog declaring itself modal while
   * Tab still walked into the overview behind it, and nothing giving focus back
   * to the button that opened it. The fix reuses `useInertBackground` rather
   * than adding a keyboard-only trap, so the vitest tier can assert that the
   * marking happens and can assert nothing about what `inert` DOES: jsdom
   * implements no focus semantics for it. Both directions are pressed here,
   * because a trap that contains Tab and leaks Shift+Tab is still a leak.
   *
   * The Voice trigger is allowed by identity for the pane spec's reason: it is
   * a peer surface rather than background, so the hook exempts it, and the
   * exemption is inherited here rather than re-decided.
   *
   * Scenario: open New agent from the overview's top bar, then press Tab
   * fifteen times and Shift+Tab fifteen times. Focus never lands on a control
   * outside the dialog except that one trigger — not the column picker, not
   * Refresh, not a deck group's own New agent — the overview behind is inert
   * rather than merely unfocused, and Esc gives the screen back and returns
   * focus to the button that opened the dialog.
   */
  test("contains Tab inside the dialog and gives focus back to its opener", async ({ page }) => {
    await openOverview(page, "fleet");

    const opener = page.getByTestId("overview-new-agent");
    const refresh = page.getByTestId("overview-refresh");
    await expect(refresh).toBeVisible();
    expect(await refresh.evaluate((node) => node.closest("[inert]") !== null)).toBe(false);

    await opener.click();
    const flow = page.getByTestId("new-agent-dialog");
    await expect(flow).toBeVisible();
    expect(await flow.evaluate((node) => node.contains(document.activeElement))).toBe(true);

    const voiceTrigger = page.getByTestId("voice-trigger");
    await expect(voiceTrigger).toBeVisible();

    const escaped = async () => flow.evaluate((node) => {
      const active = document.activeElement;
      // `<body>` is where a browser parks focus when it wraps past the last tab
      // stop, and it is not a control — only a real element outside the dialog
      // would be an escape.
      if (active === null || active === document.body || node.contains(active)) return null;
      if (active.getAttribute("data-testid") === "voice-trigger") return null;
      return active.getAttribute("data-testid") ?? active.tagName;
    });

    for (let press = 0; press < 15; press += 1) {
      await page.keyboard.press("Tab");
      expect(await escaped(), `Tab #${press + 1} left the dialog`).toBeNull();
    }
    for (let press = 0; press < 15; press += 1) {
      await page.keyboard.press("Shift+Tab");
      expect(await escaped(), `Shift+Tab #${press + 1} left the dialog`).toBeNull();
    }

    // Not merely unfocused: inert takes the background out of hit testing too,
    // so the overview cannot be clicked through the backdrop either.
    expect(await refresh.evaluate((node) => node.closest("[inert]") !== null)).toBe(true);
    expect(await voiceTrigger.evaluate((node) => node.closest("[inert]") !== null)).toBe(false);

    await page.keyboard.press("Escape");
    await expect(flow).toHaveCount(0);
    expect(await refresh.evaluate((node) => node.closest("[inert]") !== null)).toBe(false);
    await expect(opener).toBeFocused();
  });
});
