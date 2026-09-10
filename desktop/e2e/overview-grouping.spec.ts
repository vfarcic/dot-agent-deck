import { expect, test, type Page } from "@playwright/test";

import { openOverview } from "./support/overview";

/**
 * Fifteen agents, grouped the way the deck groups them — issue #836's fourth
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
    /** Which row carries the COORDINATOR badge, or `null` for a group that has no start role. */
    coordinatorRow: null as number | null,
  },
  {
    kind: "orchestration",
    title: "PRD 745 · agent overview",
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

  test("badges the coordinator, which is not simply each orchestration's first row", async ({ page }) => {
    await openOverview(page, "crowded");
    const groups = await readGroups(page);

    // DOM-shape: exactly one badge per orchestration, on the row the daemon
    // marked as the start role, and none at all outside an orchestration —
    // there is no agent to message on a mode tab or a loose pane.
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
        expect(row.coordinator.text).toBe("COORDINATOR");

        /*
          Real geometry. The badge is styled only inside `.overview-row` —
          `.coordinator-badge` is shared with the deck's agent tile and has no
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
