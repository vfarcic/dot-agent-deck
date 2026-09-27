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
    daemonState: "No daemon is listening on the configured socket.",
  },
  {
    name: "a daemon this build cannot speak to",
    scenario: "error",
    note: "overview-incompatible",
    lampClass: "connection-error",
    agents: "—",
    daemonState: "Protocol handshake failed. Desktop expects v6; daemon reported v5.",
  },
];

/** Every note this screen can show. Exactly one of them, or the table, is right. */
const ALL_NOTES = ["overview-first-run", "overview-disconnected", "overview-incompatible", "overview-loading", "overview-pending"];

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

/**
 * PRD #742 M4 — PARTIAL connectivity, which is a fifth state the four above
 * cannot express.
 *
 * Each of those is one deck in one condition, and the screen's answer is
 * whole-screen: a table, or a note. A fleet is two answers at once, and the
 * thing that has to read correctly is the boundary between them — the healthy
 * decks keep their tables, the unreachable one degrades ON ITS OWN, and the
 * header says the totals beside it are over the daemons that answered.
 *
 * The failure this is really about does not look like a failure. A header that
 * summed over every deck would print a larger number with no lamp, no note and
 * nothing out of place — a daemon's whole fleet quietly counted as the agents it
 * happened to be running the last time anyone could see it, or as zero. So the
 * assertions here are about what is stated, and one of them is that a specific
 * wrong number is NOT on screen.
 */
test.describe("partial connectivity (PRD #742 M4)", () => {
  async function openFleet(page: Page): Promise<void> {
    await page.goto("/?fixture=1&state=fleet");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group").first()).toBeVisible();
  }

  /** Scenario: One deck down among two up reads as one deck's problem, not the screen's. */
  test("one deck down among two up reads as one deck's problem, not the screen's", async ({ page }) => {
    await openFleet(page);
    const decks = page.getByTestId("daemon-group");
    await expect(decks).toHaveCount(4);

    // The two that answered keep their tables and their pips.
    for (const index of [0, 1]) {
      const deck = decks.nth(index);
      await expect(deck.getByTestId("overview-table-region")).toBeVisible();
      await expect(deck.locator(".daemon-pips")).toBeVisible();
      await expect(deck.getByTestId("overview-disconnected")).toHaveCount(0);
    }

    /*
      The one that did not says so in its own section: its own lamp, its own
      note, its own remedy — and an em dash where its neighbours print counts,
      because what it is running is UNKNOWN rather than none.
    */
    const down = decks.nth(2);
    await expect(down).toHaveAttribute("data-deck-connected", "no");
    await expect(down.getByTestId("daemon-identity")).toHaveText("ci@runner-7");
    await expect(down.getByTestId("overview-disconnected")).toBeVisible();
    await expect(down.getByTestId("daemon-state")).toHaveText("No daemon is listening on the configured socket.");
    await expect(down.getByTestId("daemon-unknown")).toHaveText("—");
    await expect(down.locator(".overview-row")).toHaveCount(0);
    await expect(down.locator(".daemon-pips")).toHaveCount(0);

    /*
      Real geometry: the degraded section is a box a reader can see, inside the
      viewport and below the daemon above it. A note rendered at zero height is a
      blank screen with the right `data-testid` on it, which is exactly what
      this tier exists to tell apart from the right answer.
    */
    const placement = await down.evaluate((node) => {
      const box = node.getBoundingClientRect();
      const note = node.querySelector("[data-testid='overview-disconnected']")!.getBoundingClientRect();
      return { height: box.height, noteWidth: note.width, noteHeight: note.height, right: box.right, viewport: window.innerWidth };
    });
    expect(placement.height, "the degraded deck laid out with no height").toBeGreaterThan(0);
    expect(placement.noteWidth, "the degraded deck's note laid out with no width").toBeGreaterThan(0);
    expect(placement.noteHeight, "the degraded deck's note laid out with no height").toBeGreaterThan(0);
    expect(placement.right).toBeLessThanOrEqual(placement.viewport);
  });

  test("the header counts the daemons that answered and never the fleet the silent one was last seen running", async ({ page }) => {
    await openFleet(page);

    // Two of four, stated. Without this the four counts beside it are a total
    // with no way to know it is partial.
    await expect(page.getByTestId("overview-count-decks").locator("strong")).toHaveText("2/4");

    /*
      Seven, not nine. The unreachable deck was last seen running two agents and
      the fixture carries them, exactly as live mode does — a reconnect failure
      replaces the connection and keeps the fleet it last knew. Summing over
      every deck prints nine; treating the silent deck as zero prints seven by
      luck and would print seven for the wrong reason only if it had none, which
      is why the fixture gives it some.
    */
    await expect(page.getByTestId("overview-count-agents").locator("strong")).toHaveText("7");
    await expect(page.getByTestId("overview-count-agents").locator("strong")).not.toHaveText("9");

    // Every lamp on the page, so "which deck is in which state" is legible at a
    // glance rather than only in a note somebody has to read.
    const lamps = await page.locator(".daemon-group-header .connection-lamp").evaluateAll((nodes) =>
      nodes.map((node) => ({ className: node.className, background: getComputedStyle(node).backgroundColor })),
    );
    expect(lamps).toHaveLength(4);
    expect(lamps[0].className).toContain("connection-connected");
    expect(lamps[1].className).toContain("connection-connected");
    expect(lamps[2].className).toContain("connection-disconnected");
    expect(lamps[3].className).toContain("connection-loading");
    expect(lamps[2].background, "a downed deck's lamp paints the same as a healthy one's").not.toBe(lamps[0].background);
    /*
      PRD #742 M14: and the daemon that has not reported paints as NEITHER. A
      reader tells the two apart at a glance or not at all — the note says which
      is which, but the note is the thing they have to already be reading.
    */
    expect(lamps[3].background, "a waiting deck's lamp paints the same as a healthy one's").not.toBe(lamps[0].background);
    expect(lamps[3].background, "a waiting deck's lamp paints the same as a downed one's").not.toBe(lamps[2].background);
  });
});

/**
 * PRD #742 M14 — a daemon that has NOT REPORTED YET, which is a sixth state and
 * the one a reader is most likely to misread.
 *
 * It is on screen for a reason no other state is: not because something failed
 * but because something has not finished. `desktop_bootstrap` answers the
 * resolved deck alone, and a remote deck arrives a tunnel, a handshake and a
 * `ListAgents` later — up to 30s if the tunnel never comes up. Until M14 the
 * deck was simply absent, so the header's own TOTAL climbed under the reader:
 * `1/1`, then `2/2`.
 *
 * This is the tier that catches "it renders, but it reads as an error". The DOM
 * assertions in vitest cannot tell a calm note from an alarming one, and they
 * cannot tell a lamp apart from the two beside it — both are questions about
 * what the engine actually paints.
 */
test.describe("a daemon that has not reported yet (PRD #742 M14)", () => {
  test("is a group of its own, named, waiting, and never a daemon that failed", async ({ page }) => {
    await page.goto("/?fixture=1&state=fleet");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group").first()).toBeVisible();

    const decks = page.getByTestId("daemon-group");
    await expect(decks).toHaveCount(4);
    const waiting = decks.nth(3);

    /*
      Named by its address, which is the half that needs the crate's `observed`
      list: `fleet` carries `deck-<16 hex>` hashes, and a group built from an id
      alone falls through to "Local daemon" for a daemon on another machine.
    */
    await expect(waiting).toHaveAttribute("data-deck-connected", "no");
    await expect(waiting.getByTestId("daemon-identity")).toHaveText("ops@edge-3");
    await expect(waiting.getByTestId("daemon-state")).toHaveText("In the fleet, waiting for it to report.");
    await expect(waiting.getByTestId("daemon-unknown")).toHaveText("—");

    // Its own note, and NOT the disconnected one — whose remedy is a Reconnect
    // that would re-establish the whole fleet to hurry a daemon already on its
    // way. There is nothing here for the reader to press.
    await expect(waiting.getByTestId("overview-pending")).toBeVisible();
    await expect(waiting.getByTestId("overview-disconnected")).toHaveCount(0);
    await expect(waiting.getByTestId("overview-loading")).toHaveCount(0);
    await expect(waiting.locator("button")).toHaveCount(0);
    await expect(waiting.locator(".overview-row")).toHaveCount(0);
    await expect(waiting.locator(".daemon-pips")).toHaveCount(0);

    /*
      Real geometry, the same contract the degraded deck above it is held to: a
      box a reader can see, inside the viewport. A note rendered at zero height
      is a blank screen with the right `data-testid` on it.
    */
    const placement = await waiting.evaluate((node) => {
      const box = node.getBoundingClientRect();
      const note = node.querySelector("[data-testid='overview-pending']")!.getBoundingClientRect();
      return { height: box.height, noteWidth: note.width, noteHeight: note.height, right: box.right, viewport: window.innerWidth };
    });
    expect(placement.height, "the waiting deck's section laid out with no height").toBeGreaterThan(0);
    expect(placement.noteWidth, "the waiting deck's note laid out with no width").toBeGreaterThan(0);
    expect(placement.noteHeight, "the waiting deck's note laid out with no height").toBeGreaterThan(0);
    expect(placement.right).toBeLessThanOrEqual(placement.viewport);
  });

  test("is counted in the fleet's total and never among the daemons that answered", async ({ page }) => {
    await page.goto("/?fixture=1&state=fleet");
    await page.getByTestId("open-overview").click();
    await expect(page.getByTestId("daemon-group").first()).toBeVisible();

    /*
      Four configured, two answering. The number that used to be wrong is the
      DENOMINATOR: a daemon nothing had been heard from was absent from the fleet
      entirely, so this read `2/3` — and would have gone on reading as correct.
    */
    const decks = page.getByTestId("overview-count-decks").locator("strong");
    await expect(decks).toHaveText("2/4");
    await expect(decks).not.toHaveText("2/3");

    // And the counts beside it are unchanged: the waiting deck contributes no
    // agents, exactly as the unreachable one contributes none.
    await expect(page.getByTestId("overview-count-agents").locator("strong")).toHaveText("7");
  });
});
