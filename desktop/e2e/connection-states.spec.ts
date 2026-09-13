import { expect, test, type Page } from "@playwright/test";

import type { FixtureScenario } from "./support/overview";

/**
 * Each connection state puts its own screen in front of the reader — issue
 * #836's third item.
 *
 * The four are genuinely different answers, not four wordings of one. Connected
 * with a fleet is a table; connected with nothing is the FIRST-RUN screen, and
 * saying "no daemon" there would be a lie about a healthy install; a daemon that
 * is down and a daemon this build cannot speak to need different remedies and
 * so get different notes. The state that used to be got wrong is `empty` — it
 * fell through to the disconnected branch, so a fresh install was told its
 * daemon was missing (PRD #745 M2).
 *
 * Which screen renders is a DOM-SHAPE question and is asserted as one: the
 * note's own `data-testid`, the counts, the daemon message. Two things here are
 * not. The note is measured — a non-empty box, below the top bar, inside the
 * viewport — because a screen whose only content is clipped to nothing is
 * indistinguishable from the right one in the DOM. And the connection lamp is
 * read as a COMPOSITED COLOUR: it resolves through custom properties and the
 * dark-appearance cascade, so what it paints is an engine's answer rather than
 * a class name.
 */

interface StateExpectation {
  name: string;
  scenario: FixtureScenario;
  /** The note that should be on screen, or `null` when the fleet table is. */
  note: string | null;
  /** The lamp class the connection carries. DOM-shape; the colour is measured separately. */
  lampClass: string;
  /** What the AGENTS instrument prints — a number when countable, an em dash when not. */
  agents: string;
  /** The daemon's own message, shown only when it says something the lamp does not. */
  daemonState: string | null;
}

const STATES: StateExpectation[] = [
  {
    name: "connected with a fleet",
    scenario: "connected",
    note: null,
    lampClass: "connection-connected",
    agents: "4",
    daemonState: null,
  },
  {
    name: "connected with nothing running",
    scenario: "empty",
    note: "overview-first-run",
    lampClass: "connection-connected",
    agents: "0",
    daemonState: null,
  },
  {
    name: "no daemon listening",
    scenario: "disconnected",
    note: "overview-disconnected",
    lampClass: "connection-disconnected",
    agents: "—",
    daemonState: "No deck is listening on the configured socket.",
  },
  {
    name: "a daemon this build cannot speak to",
    scenario: "error",
    note: "overview-incompatible",
    lampClass: "connection-error",
    agents: "—",
    daemonState: "Protocol handshake failed. Desktop expects v6; deck reported v5.",
  },
];

/** Every note this screen can show. Exactly one of them, or the table, is right. */
const ALL_NOTES = ["overview-first-run", "overview-disconnected", "overview-incompatible", "overview-loading"];

async function openState(page: Page, scenario: FixtureScenario): Promise<void> {
  await page.goto(`/?fixture=1&state=${scenario}`);
  await page.getByTestId("open-overview").click();
  await expect(page.getByTestId("daemon-group")).toBeVisible();
}

/** The lamp in the daemon group's own header, as the engine painted it. */
async function lampPaint(page: Page): Promise<{ className: string; background: string }> {
  return page.locator(".daemon-group-header .connection-lamp").evaluate((node) => ({
    className: node.className,
    background: getComputedStyle(node).backgroundColor,
  }));
}

for (const state of STATES) {
  test(`${state.name} renders its own screen`, async ({ page }) => {
    await openState(page, state.scenario);

    const shown = state.note ?? "overview-table-region";
    await expect(page.getByTestId(shown)).toBeVisible();

    // Exactly one answer: everything else this screen can say is absent, not
    // merely stacked underneath. DOM-shape.
    for (const other of [...ALL_NOTES, "overview-table-region"]) {
      if (other === shown) continue;
      await expect(page.getByTestId(other), `"${other}" rendered alongside "${shown}"`).toHaveCount(0);
    }

    /*
      The header must not contradict the body. Both the `disconnected` and
      `error` fixtures still carry the previous snapshot's four agents — a
      reconnect failure keeps the fleet it last knew — so an instrument reading
      straight off `snapshot.agents` printed `AGENTS 4` above a body correctly
      saying the fleet cannot be read. An em dash is the honest answer.
    */
    await expect(page.getByTestId("overview-count-agents").locator("strong")).toHaveText(state.agents);

    // The daemon's own message, which renders only when it says something the
    // lamp beside it does not. A healthy connection's message is the lamp
    // restated in words, so it is suppressed.
    if (state.daemonState === null) await expect(page.getByTestId("daemon-state")).toHaveCount(0);
    else await expect(page.getByTestId("daemon-state")).toHaveText(state.daemonState);

    const lamp = await lampPaint(page);
    expect(lamp.className.split(/\s+/)).toContain(state.lampClass);

    /*
      Measured, not merely present: the screen a reader is sent to has to be
      somewhere a reader can see. A note laid out at zero height, or pushed
      under the sticky top bar, or off the right edge is a blank screen with the
      right `data-testid` on it — which is the exact failure this tier exists to
      tell apart from the right answer.
    */
    const placement = await page.getByTestId(shown).evaluate((node) => {
      const box = node.getBoundingClientRect();
      return {
        width: box.width,
        height: box.height,
        top: box.top,
        left: box.left,
        right: box.right,
        topbarBottom: document.querySelector(".topbar")!.getBoundingClientRect().bottom,
        viewportWidth: window.innerWidth,
      };
    });
    expect(placement.width, "laid out with no width").toBeGreaterThan(0);
    expect(placement.height, "laid out with no height").toBeGreaterThan(0);
    expect(placement.top, "starts underneath the sticky top bar").toBeGreaterThanOrEqual(placement.topbarBottom);
    expect(placement.left).toBeGreaterThanOrEqual(0);
    expect(placement.right).toBeLessThanOrEqual(placement.viewportWidth);
  });
}

/*
  Not "a different colour for each state" — `empty` and `connected` share one
  deliberately, and the pair below says why. What the lamp separates is fault
  from health, and the two faults from each other.
*/
test("the daemon lamp's colour separates fault from health", async ({ page }) => {
  const painted = new Map<string, string>();
  for (const state of STATES) {
    await openState(page, state.scenario);
    const lamp = await lampPaint(page);
    expect(lamp.background, `the lamp for "${state.name}" painted nothing`).toMatch(/^rgba?\(/);
    painted.set(state.scenario, lamp.background);
  }

  /*
    The one pair that must MATCH. A daemon that is up and owns nothing is
    healthy, and a lamp that dimmed for it would be reporting a fault the app
    has just gone to some trouble to say is not there.
  */
  expect(painted.get("empty"), "an idle daemon's lamp is not the healthy colour").toBe(painted.get("connected"));

  // And the three that must differ. These resolve through `--status-ok`,
  // `--shell-faint` and `--status-error` under the dark appearance, so the
  // values compared here are composited rather than declared.
  expect(painted.get("disconnected")).not.toBe(painted.get("connected"));
  expect(painted.get("error")).not.toBe(painted.get("connected"));
  expect(painted.get("error"), "a downed daemon and an incompatible one paint the same lamp").not.toBe(painted.get("disconnected"));
});
