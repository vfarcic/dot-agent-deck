import { expect, test, type Page } from "@playwright/test";

import { openOverview } from "./support/overview";

/**
 * Fifteen agents, grouped the way the daemon groups them — issue #836's fourth
 * item, against `?fixture=1&state=crowded`.
 *
 * The fixture is built to make this hard on purpose (PRD #745): its agents are
 * declared OUT of role order, and orchestration `dot-ai`'s start role is
 * deliberately not its first role. So grouping, ordering and coordinator
 * identification are three separate things a screen has to get right rather
 * than one accident of declaration order, and this file asserts them as three.
 *
 * **What is real-engine here and what is not.** Which agent is in which card,
 * in what order, and which row carries the badge are DOM-SHAPE questions and
 * are asserted as such. What only an engine can answer is whether the reading
 * order matches the SEEN order: every card and every row is measured, and the
 * assertions are that the boxes descend the page in the same sequence the DOM
 * lists them and that no two overlap. A `flex-direction: column-reverse` or a
 * stray `order` would leave every DOM assertion in this file green and put the
 * fleet on screen upside down.
 */

interface CoordinatorShape {
  text: string;
  top: number;
  bottom: number;
  left: number;
  width: number;
  height: number;
  /** The right edge of the agent name in the same cell — the badge sits after it. */
  nameRight: number;
}

interface RowShape {
  /** The `01`-style ordinal an orchestration prints, or `null` outside one. */
  roleIndex: string | null;
  name: string;
  top: number;
  bottom: number;
  coordinator: CoordinatorShape | null;
}

interface GroupShape {
  kind: string | null;
  title: string;
  subtitle: string | null;
  countLabel: string;
  top: number;
  bottom: number;
  rows: RowShape[];
}

/**
 * The whole screen — shape and geometry together — read in ONE round trip.
 *
 * The overview re-renders on its own ten-second clock (`useOverviewClock`), so
 * a shape read and a geometry read taken separately can straddle a repaint and
 * describe two different screens. `overview-rows.spec.ts` documents the same
 * constraint for its injection test.
 */
async function readGroups(page: Page): Promise<GroupShape[]> {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll(".overview-group")).map((card) => {
      const cardBox = card.getBoundingClientRect();
      return {
        kind: card.getAttribute("data-group-kind"),
        title: card.querySelector("h3")?.textContent ?? "",
        subtitle: card.querySelector(".overview-group-subtitle")?.textContent ?? null,
        countLabel: card.querySelector(".overview-group-count")?.textContent ?? "",
        top: cardBox.top,
        bottom: cardBox.bottom,
        rows: Array.from(card.querySelectorAll(".overview-row")).map((row) => {
          const rowBox = row.getBoundingClientRect();
          const badge = row.querySelector(".coordinator-badge");
          const name = row.querySelector(".overview-agent-name strong");
          const badgeBox = badge?.getBoundingClientRect();
          return {
            roleIndex: row.querySelector(".overview-role-index")?.textContent ?? null,
            name: name?.textContent ?? "",
            top: rowBox.top,
            bottom: rowBox.bottom,
            coordinator: badge && badgeBox
              ? {
                text: badge.textContent ?? "",
                top: badgeBox.top,
                bottom: badgeBox.bottom,
                left: badgeBox.left,
                width: badgeBox.width,
                height: badgeBox.height,
                nameRight: name!.getBoundingClientRect().right,
              }
              : null,
          };
        }),
      };
    }),
  );
}

/** What the crowded fixture's fifteen agents must come out as. */
const EXPECTED = [
  {
    kind: "standalone",
    title: "Standalone agents",
    subtitle: null,
    names: ["Scratch shell", "Changelog sweep", "pi-extension spike"],
    /** Which row carries the ORCHESTRATOR badge, or `null` for a group that has no start role. */
    coordinatorRow: null as number | null,
  },
  {
    kind: "orchestration",
    title: "PRD 745 · agent dashboard",
    subtitle: "dot-agent-deck",
    names: ["orchestrator", "coder", "tester", "reviewer", "docs", "release"],
    coordinatorRow: 0,
  },
  {
    kind: "orchestration",
    title: "dot-ai · docs refresh",
    subtitle: "dot-ai",
    names: ["writer", "reviewer", "orchestrator", "publisher"],
    // NOT the first row. `dot-ai`'s start role is its third, which is the whole
    // reason this orchestration is in the fixture.
    coordinatorRow: 2,
  },
  {
    kind: "mode",
    title: "review",
    subtitle: null,
    names: ["Second opinion", "Security pass"],
    coordinatorRow: null,
  },
];

test.describe("the overview at fifteen agents", () => {
  test("groups the fleet into four cards, standalone first, reading down the page", async ({ page }) => {
    await openOverview(page, "crowded");
    const groups = await readGroups(page);

    // DOM-shape: the buckets, their order, and who is in them.
    expect(groups.map((group) => group.kind)).toEqual(EXPECTED.map((expected) => expected.kind));
    expect(groups.map((group) => group.title)).toEqual(EXPECTED.map((expected) => expected.title));
    expect(groups.map((group) => group.subtitle)).toEqual(EXPECTED.map((expected) => expected.subtitle));
    expect(groups.map((group) => group.rows.map((row) => row.name))).toEqual(EXPECTED.map((expected) => expected.names));
    expect(groups.flatMap((group) => group.rows), "the crowded fixture is fifteen agents").toHaveLength(15);

    // Each card counts itself, and says so in the words a reader sees.
    expect(groups.map((group) => group.countLabel)).toEqual(["3 agents", "6 agents", "4 agents", "2 agents"]);

    /*
      Real geometry. Standalone LEADS because the TUI opens on the dashboard tab
      and always keeps it first; an overview that buried the same agents at the
      bottom would be describing a different deck than the one next to it. That
      claim is about what a reader sees, so it is measured rather than inferred
      from document order — and the cards are checked not to overlap, so
      "further down" means genuinely further down and not stacked.
    */
    expect(groups[0].kind).toBe("standalone");
    expect(groups[0].top, "another card is laid out above the standalone one").toBe(Math.min(...groups.map((group) => group.top)));
    for (let index = 1; index < groups.length; index += 1) {
      expect(
        groups[index].top,
        `card ${index} ("${groups[index].title}") is not below card ${index - 1} ("${groups[index - 1].title}")`,
      ).toBeGreaterThanOrEqual(groups[index - 1].bottom);
    }
  });

  test("lists each orchestration's roles in role order", async ({ page }) => {
    await openOverview(page, "crowded");
    const groups = await readGroups(page);

    for (const [index, expected] of EXPECTED.entries()) {
      const group = groups[index];
      if (expected.kind !== "orchestration") {
        // A role ordinal outside an orchestration would be a number with
        // nothing behind it: the daemon derives the role from the agent type
        // there. DOM-shape.
        expect(group.rows.map((row) => row.roleIndex), `"${group.title}" printed a role ordinal`).toEqual(group.rows.map(() => null));
        continue;
      }

      /*
        DOM-shape, and the sharp end of the item. The fixture declares these
        agents shuffled — `orc-745` arrives as coder, orchestrator, docs,
        reviewer, release, tester — so the ordinals only come out `01…06` in
        row order if the screen sorted them by the daemon's own `roleIndex`.
        Reading them straight from the snapshot gives `02 01 05 04 06 03`.
      */
      expect(group.rows.map((row) => row.roleIndex), `"${group.title}" is not in role order`).toEqual(
        expected.names.map((_, position) => String(position + 1).padStart(2, "0")),
      );

      // Real geometry: and that order is the one on screen, top to bottom.
      for (let row = 1; row < group.rows.length; row += 1) {
        expect(
          group.rows[row].top,
          `row ${row} of "${group.title}" is not below row ${row - 1}`,
        ).toBeGreaterThanOrEqual(group.rows[row - 1].bottom);
      }
    }
  });

  /** Scenario: Badges the orchestrator, which is not simply each orchestration's first row. */
  test("badges the orchestrator, which is not simply each orchestration's first row", async ({ page }) => {
    await openOverview(page, "crowded");
    const groups = await readGroups(page);

    // DOM-shape: exactly one badge per orchestration, on the row the daemon
    // marked as the start role, and none at all outside an orchestration —
    // there is no agent to message in a legacy mode group or a loose pane.
    const badgedRows = groups.map((group) => group.rows.findIndex((row) => row.coordinator !== null));
    expect(badgedRows).toEqual(EXPECTED.map((expected) => expected.coordinatorRow ?? -1));
    expect(groups.map((group) => group.rows.filter((row) => row.coordinator !== null).length)).toEqual([0, 1, 1, 0]);

    /*
      The contrast is the assertion. One orchestration's start role is its first
      row and the other's is its third, so a screen that badged "the top row of
      every orchestration" would satisfy half of this and fail the other half.
    */
    expect(badgedRows[1], "the PRD 745 orchestration's coordinator moved off its first row").toBe(0);
    expect(badgedRows[2], "the dot-ai orchestration's coordinator is being read as its first role").toBe(2);

    for (const group of groups) {
      for (const row of group.rows) {
        if (row.coordinator === null) continue;
        expect(row.coordinator.text).toBe("ORCHESTRATOR");

        /*
          Real geometry. The badge is styled only inside `.overview-row` —
          `.coordinator-badge` is shared with the daemon's agent tile and has no
          rule of its own — so an unscoped or missing rule leaves it in the DOM
          and effectively invisible. Measured: it has a box, that box sits
          within its own row rather than spilling into the neighbours, and it
          follows the name in the same cell instead of sitting on top of it.
        */
        expect(row.coordinator.width, `the badge in "${group.title}" laid out with no width`).toBeGreaterThan(0);
        expect(row.coordinator.height, `the badge in "${group.title}" laid out with no height`).toBeGreaterThan(0);
        expect(row.coordinator.top).toBeGreaterThanOrEqual(row.top);
        expect(row.coordinator.bottom).toBeLessThanOrEqual(row.bottom);
        expect(row.coordinator.left, `the badge in "${group.title}" overlaps the agent name`).toBeGreaterThanOrEqual(
          row.coordinator.nameRight,
        );
      }
    }
  });
});

/**
 * PRD #742 M4 — the fleet as a reader sees it, in a real engine.
 *
 * The vitest tier already asserts that every deck section reaches the DOM with
 * the right agents inside it. What only an engine can answer is whether they
 * reach the SCREEN: four sections that descend the page in the order the DOM
 * lists them, none overlapping another, each with a box. A `position: absolute`
 * left over from the single-deck layout, or a `gap` that never applied, would
 * leave every DOM assertion green and stack the fleet on top of itself.
 *
 * The phone width is here because it is where a fleet is most likely to break:
 * the overview's narrow rule hides the status pips and drops the padding, and
 * four stacked sections is four times as much page to get wrong. The contract
 * is the same one `page-overflow.spec.ts` states for one deck — the page body
 * never slides sideways — asserted against the screen that has the most in it.
 */
test.describe("the fleet view (PRD #742 M4)", () => {
  /** Each deck section's identity and box, read in one round trip. */
  async function readDaemons(page: Page) {
    return page.evaluate(() =>
      Array.from(document.querySelectorAll("[data-testid='daemon-group']")).map((section) => {
        const box = section.getBoundingClientRect();
        return {
          deckId: section.getAttribute("data-daemon-id") ?? "",
          connected: section.getAttribute("data-deck-connected") ?? "",
          name: section.querySelector("[data-testid='daemon-identity']")?.textContent ?? "",
          top: box.top,
          bottom: box.bottom,
          left: box.left,
          right: box.right,
          height: box.height,
          rows: Array.from(section.querySelectorAll(".overview-row")).map(
            (row) => row.querySelector(".overview-agent-name strong")?.textContent ?? "",
          ),
        };
      }),
    );
  }

  /** Scenario: Renders one section per deck, in order, each holding only its own deck's rows. */
  test("renders one section per deck, in order, each holding only its own deck's rows", async ({ page }) => {
    await openOverview(page, "fleet");
    const decks = await readDaemons(page);

    // DOM shape: four decks, the local one first, and each named the way a
    // reader is meant to tell them apart — the last of them named from the
    // crate's `observed` list, because nothing has reported for it (M14).
    expect(decks.map((deck) => deck.deckId)).toEqual(["/tmp/dot-agent-deck.sock", "dev@build-box", "ci@runner-7", "ops@edge-3"]);
    expect(decks.map((deck) => deck.connected)).toEqual(["yes", "yes", "no", "no"]);
    expect(decks.map((deck) => deck.name)).toEqual(["Local daemon", "dev@build-box", "ci@runner-7", "ops@edge-3"]);

    // No deck's rows appear under another deck's heading. The two connected
    // decks mint colliding agent ids, so this is about the section a row is
    // IN and not about the id it carries.
    expect(decks[0].rows).toContain("Plan / architecture");
    expect(decks[1].rows).toContain("Nightly build watch");
    expect(decks[0].rows).not.toContain("Nightly build watch");
    expect(decks[1].rows).not.toContain("Plan / architecture");

    /*
      Real geometry, and the half the DOM cannot answer: every section has a
      box, and they descend the page in the order the DOM lists them without
      overlapping. A stacking or ordering fault leaves every assertion above
      green and puts one deck on top of another.
    */
    for (const deck of decks) {
      expect(deck.height, `the "${deck.name}" section laid out with no height`).toBeGreaterThan(0);
    }
    for (let index = 1; index < decks.length; index += 1) {
      expect(
        decks[index].top,
        `the "${decks[index].name}" section is not below "${decks[index - 1].name}"`,
      ).toBeGreaterThanOrEqual(decks[index - 1].bottom);
    }
  });

  test("states how many decks answered, and counts only those", async ({ page }) => {
    await openOverview(page, "fleet");

    // Two of four. The literal string is the contract: it is the only thing
    // on the header that says the four counts beside it are partial.
    await expect(page.getByTestId("overview-count-decks").locator("strong")).toHaveText("2/4");

    // Seven agents across the two answering decks. The unreachable deck was
    // last seen running two more, and adding them would print "9" — a number
    // that looks exactly as correct as this one.
    await expect(page.getByTestId("overview-count-agents").locator("strong")).toHaveText("7");
  });

  /*
    Issue #2312 follow-up. The counters are one button in the header that
    reads the total and what needs someone, and every counter opens under it
    on hover or click. It replaced issue #1676's row of instruments, which had
    to drop counters as the window narrowed.
  */
  test.describe("the counters button", () => {
    /// Scenario: open the fleet dashboard. The button reads the agent total with the needs-input and daemons-answering warnings, and the counters are hidden. Hovering it shows every counter; moving away hides them; a click keeps them open until Escape, and a second click closes them with the mouse still on the button. From 1440px down to 400px wide, either side of each width where the top bar changes, the button and its list stay inside the window and the page does not scroll sideways.
    test("shows every counter on hover or click and fits every width", async ({ page }) => {
      await page.setViewportSize({ width: 1280, height: 800 });
      await openOverview(page, "fleet");
      const button = page.getByTestId("dashboard-counters-button");
      await expect(button).toContainText("7 agents");
      await expect(button).toContainText("needs input");
      await expect(button).toContainText("2/4 daemons");
      const popover = page.getByTestId("dashboard-counters-popover");
      await expect(popover).toBeHidden();

      await button.hover();
      await expect(popover).toBeVisible();
      for (const name of ["agents", "running", "needs-input", "failed", "waiting", "idle", "groups", "decks"]) {
        await expect(page.getByTestId(`overview-count-${name}`), `${name} is not in the popover`).toBeVisible();
      }
      // Down onto the list itself: it stays open on the way.
      await popover.hover();
      await expect(popover).toBeVisible();
      await page.mouse.move(5, 790);
      await expect(popover).toBeHidden();

      await button.click();
      await page.mouse.move(5, 790);
      await expect(popover).toBeVisible();
      await page.keyboard.press("Escape");
      await expect(popover).toBeHidden();

      // A second click closes it with the mouse still on the button.
      await button.click();
      await expect(popover).toBeVisible();
      await button.click();
      await expect(popover).toBeHidden();
      await expect(button).toHaveAttribute("aria-expanded", "false");
      await page.mouse.move(5, 790);

      for (const width of [1440, 1439, 1181, 1180, 961, 681, 400]) {
        await page.setViewportSize({ width, height: 800 });
        await expect(button, `the counters button is off screen at ${width}px`).toBeInViewport();
        await button.hover();
        // The box is read once the popover is open: read straight after the
        // hover it raced the opening, and WebKit lost (PR #2378, null box at 961px).
        await expect(popover, `the counters do not open at ${width}px`).toBeVisible();
        const box = await popover.boundingBox();
        expect((box?.x ?? 0) + (box?.width ?? 0), `the counters run off the right edge at ${width}px`).toBeLessThanOrEqual(width);
        expect(box?.x ?? -1, `the counters run off the left edge at ${width}px`).toBeGreaterThanOrEqual(0);
        await page.mouse.move(5, 790);
        const scroll = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
        expect(scroll, `the page scrolls sideways at ${width}px`).toBeLessThanOrEqual(0);
      }
    });

    /// Scenario: in a window 330px tall the counters list opened from the button stops above the voice row and scrolls, so its last count, DAEMONS, can be scrolled to and read.
    test("stops above the voice row in a short window and scrolls", async ({ page }) => {
      await page.setViewportSize({ width: 1280, height: 330 });
      await openOverview(page, "fleet");
      await page.getByTestId("dashboard-counters-button").click();
      const popover = page.getByTestId("dashboard-counters-popover");
      await expect(popover).toBeVisible();
      const bottom = await popover.evaluate((element) => element.getBoundingClientRect().bottom);
      const voiceTop = await page.locator(".voice-row").evaluate((element) => element.getBoundingClientRect().top);
      expect(bottom, "the counters run under the voice row").toBeLessThanOrEqual(voiceTop);
      await page.getByTestId("overview-count-decks").scrollIntoViewIfNeeded();
      await expect(page.getByTestId("overview-count-decks")).toBeInViewport();
      const decksBottom = await page.getByTestId("overview-count-decks").evaluate((element) => element.getBoundingClientRect().bottom);
      expect(decksBottom, "DAEMONS stays under the voice row").toBeLessThanOrEqual(voiceTop + 0.5);
    });
  });

  /// Scenario: a fresh install with this machine as the only daemon and no agents. The empty note offers New agent and Add a daemon; from a wide window down to the 400px phone layout its heading stays on one line, its words keep a readable column instead of being squeezed beside the two buttons, and the page does not scroll sideways.
  test("keeps the empty note's words readable beside its buttons at every width", async ({ page }) => {
    await page.goto("/?fixture=1&state=empty");
    await expect(page.getByRole("complementary", { name: "Primary navigation" })).toBeVisible();
    await page.getByTestId("open-overview").click();
    const note = page.getByTestId("overview-first-run");
    await expect(note.getByTestId("overview-first-run-new-agent")).toBeVisible();
    await expect(note.getByTestId("overview-first-run-add-daemon")).toBeVisible();

    for (const width of [1280, 900, 681, 400]) {
      await page.setViewportSize({ width, height: 800 });
      const heading = await note.locator("h3").evaluate((element) => {
        const lineHeight = parseFloat(getComputedStyle(element).lineHeight) || parseFloat(getComputedStyle(element).fontSize) * 1.2;
        return { height: element.getBoundingClientRect().height, lineHeight };
      });
      expect(heading.height, `the note's heading wraps at ${width}px`).toBeLessThan(heading.lineHeight * 1.5);
      const text = await note.locator(".overview-note-row-text").boundingBox();
      expect(text?.width ?? 0, `the note's words are squeezed into a narrow column at ${width}px`).toBeGreaterThanOrEqual(220);
      await expect(note.getByTestId("overview-first-run-add-daemon"), `Add a daemon is off screen at ${width}px`).toBeInViewport();
      const scroll = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
      expect(scroll, `the page scrolls sideways at ${width}px`).toBeLessThanOrEqual(0);
    }
  });

  test.describe("at 400x780 phone", () => {
    test.use({ viewport: { width: 400, height: 780 } });

    test("stacks four decks down a phone screen without sliding the page sideways", async ({ page }) => {
      await openOverview(page, "fleet");
      const decks = await readDaemons(page);
      expect(decks).toHaveLength(4);

      // Every section is inside the viewport horizontally, and still stacked.
      for (const deck of decks) {
        expect(deck.height, `the "${deck.name}" section collapsed at phone width`).toBeGreaterThan(0);
        expect(deck.left, `the "${deck.name}" section starts off the left edge`).toBeGreaterThanOrEqual(0);
        expect(deck.right, `the "${deck.name}" section extends past the right edge`).toBeLessThanOrEqual(400.5);
      }
      for (let index = 1; index < decks.length; index += 1) {
        expect(decks[index].top).toBeGreaterThanOrEqual(decks[index - 1].bottom);
      }

      /*
        The same contract `page-overflow.spec.ts` states for one deck. `body`
        carries `overflow-x: hidden`, so content past the right edge is clipped
        rather than reachable — a reader sees a truncated screen and no
        scrollbar saying why — and only `scrollWidth` reports it.
      */
      const metrics = await page.evaluate(() => ({
        rootScroll: document.documentElement.scrollWidth,
        rootClient: document.documentElement.clientWidth,
        bodyScroll: document.body.scrollWidth,
        bodyClient: document.body.clientWidth,
        viewport: window.innerWidth,
      }));
      expect(metrics.bodyScroll, "content extends past the body, clipped by overflow-x: hidden").toBeLessThanOrEqual(metrics.bodyClient);
      expect(metrics.rootScroll, "the root scrolls sideways, so the whole page slides").toBeLessThanOrEqual(metrics.rootClient);
      expect(metrics.rootClient, "the root is wider than the viewport").toBeLessThanOrEqual(metrics.viewport);
    });
  });
});
