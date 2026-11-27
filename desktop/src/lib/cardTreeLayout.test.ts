import { describe, expect, it } from "vitest";

import { layoutTreeLevel, TREE_CARD_WIDTH, TREE_ITEM_PAD, TREE_SPINE_LANE, type TreeLevelLayout } from "./cardTreeLayout";

/*
 * Issue #2533 (PRD #2374 decision 2026-11-26) — the cards under an opened card
 * wrap into rows under a bus instead of scrolling sideways. Written from the
 * decision's diagram, not from the code: rows fill from the left, a spine
 * drops from the parent and runs down a gap between two cards to each next
 * row, and each row hangs from its own horizontal line.
 */

interface Node { key: string; children: Node[] }

const leaf = (key: string): Node => ({ key, children: [] });
const leaves = (prefix: string, count: number) => Array.from({ length: count }, (_, at) => leaf(`${prefix}${at + 1}`));
const ITEM = TREE_CARD_WIDTH + 2 * TREE_ITEM_PAD;

function layout(nodes: Node[], width: number, open: ReadonlySet<string> = new Set()): TreeLevelLayout<Node> {
  const level = layoutTreeLevel(nodes, width, { keyOf: (node) => node.key, isOpen: (node) => open.has(node.key) });
  if (!level) throw new Error("nothing laid out");
  return level;
}

const rowKeys = (level: TreeLevelLayout<Node>) => level.rows.map((row) => row.items.map((item) => item.node.key));

describe("desktop/agent-cards/143 — the cards under a card wrap into rows under a bus (issue #2533)", () => {
  /// Scenario: lay out seven cards under one card at widths that hold seven, four, two and one card side by side. They take one, two, four and seven rows, filled from the left in the daemon's order.
  it("desktop/agent-cards/143: the number of rows follows the width, and each row fills from the left in order", () => {
    const seven = leaves("u", 7);
    expect(rowKeys(layout(seven, 7 * ITEM))).toEqual([["u1", "u2", "u3", "u4", "u5", "u6", "u7"]]);
    expect(rowKeys(layout(seven, 4 * ITEM + 20))).toEqual([["u1", "u2", "u3", "u4"], ["u5", "u6", "u7"]]);
    expect(rowKeys(layout(seven, 2 * ITEM))).toEqual([["u1", "u2"], ["u3", "u4"], ["u5", "u6"], ["u7"]]);
    expect(rowKeys(layout(seven, ITEM + 40))).toEqual([["u1"], ["u2"], ["u3"], ["u4"], ["u5"], ["u6"], ["u7"]]);
    for (const width of [7 * ITEM, 4 * ITEM + 20, 2 * ITEM, ITEM + 40]) {
      const level = layout(seven, width);
      expect(level.width, `at ${width}px the level fits`).toBeLessThanOrEqual(width);
      for (const row of level.rows) expect(row.items[0].left).toBe(0);
    }
  });

  /// Scenario: lay out cards that fit in one row. The level is as wide as its cards, the parent's line comes in at its middle, and the bus runs from the first card's middle to the last's, with no spine: the look before wrapping existed.
  it("desktop/agent-cards/143: a level that fits in one row is drawn as before", () => {
    const level = layout(leaves("r", 3), 1000);
    expect(level.rows).toHaveLength(1);
    expect(level.width).toBe(3 * ITEM);
    expect(level.stem).toBe(1.5 * ITEM);
    expect(level.rows[0].bus).toEqual({ left: ITEM / 2, right: 2.5 * ITEM });
    expect(level.rows[0].spine).toBeUndefined();
    expect(level.single).toBe(false);
    const one = layout([leaf("only")], 1000);
    expect(one.single).toBe(true);
    expect(one.rows[0].bus).toBeUndefined();
    expect(one.stem).toBe(one.rows[0].items[0].center);
  });

  /// Scenario: lay out six cards in a width that holds four. The spine leaves the first row down the gap nearest the parent's line, the second row's bus reaches back to it, and the first row's bus spans every card and the spine; the last row has no spine.
  it("desktop/agent-cards/143: each row hangs from its own bus, and the spine runs to the next row down a gap between two cards", () => {
    const level = layout(leaves("u", 6), 4 * ITEM);
    expect(rowKeys(level)).toEqual([["u1", "u2", "u3", "u4"], ["u5", "u6"]]);
    const [first, second] = level.rows;
    // The gap between the second and third cards, where the parent's line comes in.
    expect(level.stem).toBe(2 * ITEM);
    expect(first.spine).toBe(2 * ITEM);
    expect(first.bus).toEqual({ left: ITEM / 2, right: 3.5 * ITEM });
    expect(second.bus).toEqual({ left: ITEM / 2, right: 2 * ITEM });
    expect(second.spine).toBeUndefined();
    // The spine is in a gap, never over a card.
    for (const item of first.items) expect(first.spine! <= item.left || first.spine! >= item.left + item.width).toBe(true);
  });

  /// Scenario: lay out three cards, the middle one open with five cards under it, in a width too narrow for them side by side. Each card takes a row of its own; the spine passes each of those rows in the lane kept free at its right, and the open card's own five wrap within the room left to it.
  it("desktop/agent-cards/143: an open card's cards wrap too, and a row of one passes the spine in the lane at its right", () => {
    const width = 3 * ITEM;
    const level = layout([leaf("a"), { key: "b", children: leaves("b", 5) }, leaf("c")], width, new Set(["b"]));
    expect(rowKeys(level)).toEqual([["a"], ["b"], ["c"]]);
    const open = level.rows[1].items[0];
    expect(open.level, "the open card's level is laid out").toBeDefined();
    expect(open.level!.rows.length).toBeGreaterThan(1);
    expect(open.width).toBeLessThanOrEqual(width - TREE_SPINE_LANE);
    expect(open.level!.width).toBeLessThanOrEqual(open.width - 2 * TREE_ITEM_PAD);
    // A level under a card hangs from that card's middle.
    expect(open.center).toBe(open.left + open.width / 2);
    for (const row of level.rows.slice(0, -1)) {
      expect(row.spine).toBe(row.width + TREE_SPINE_LANE / 2);
      expect(row.spine!).toBeLessThanOrEqual(level.width);
    }
    expect(level.width).toBeLessThanOrEqual(width);
  });

  /// Scenario: lay out a card open three levels deep in a narrow width. Every level fits inside the one above it, and each card is as wide as a card is, or as the room it has when that is narrower.
  it("desktop/agent-cards/143: a deep tree grows downwards, each level inside the one above it", () => {
    const deep: Node = { key: "d1", children: [{ key: "d2", children: [{ key: "d3", children: leaves("x", 4) }] }] };
    const width = 400;
    const top = layout([deep], width, new Set(["d1", "d2", "d3"]));
    let level: TreeLevelLayout<Node> | undefined = top;
    let room = width;
    let depth = 0;
    while (level) {
      expect(level.width, `level ${depth} fits the room it was given`).toBeLessThanOrEqual(room);
      for (const row of level.rows) for (const item of row.items) expect(item.cardWidth).toBeLessThanOrEqual(TREE_CARD_WIDTH);
      const open: TreeLevelLayout<Node>["rows"][number]["items"][number] | undefined = level.rows.flatMap((row) => row.items).find((item) => item.level);
      room = open ? open.width - 2 * TREE_ITEM_PAD : 0;
      level = open?.level;
      depth += 1;
    }
    expect(depth).toBe(4);
  });

  /// Scenario: lay out a list in which a card is listed under itself. The card met again below itself is not laid out, so the layout ends.
  it("desktop/agent-cards/143: a card met again below itself is not laid out", () => {
    const loop: Node = { key: "loop", children: [] };
    loop.children.push(loop, leaf("other"));
    const level = layout([loop], 1000, new Set(["loop"]));
    expect(rowKeys(level.rows[0].items[0].level!)).toEqual([["other"]]);
  });

  /// Scenario: lay out a chain seven levels deep, each level an open card beside a closed one, in 300 pixels. Every level fits the room it was given, however narrow its cards have to be (PR #2550 review).
  it("desktop/agent-cards/143: a deep tree never comes out wider than the room it is given", () => {
    let chain: Node = { key: "end", children: leaves("e", 2) };
    const open = new Set(["end"]);
    for (let depth = 7; depth > 0; depth -= 1) {
      chain = { key: `c${depth}`, children: [chain, leaf(`s${depth}`)] };
      open.add(`c${depth}`);
    }
    for (const width of [300, 340, 600]) {
      let level: TreeLevelLayout<Node> | undefined = layout([chain], width, open);
      let room = width;
      while (level) {
        expect(level.width, `a level given ${room}px at a ${width}px start`).toBeLessThanOrEqual(room);
        const inner: TreeLevelLayout<Node>["rows"][number]["items"][number] | undefined = level.rows.flatMap((row) => row.items).find((item) => item.level);
        room = inner ? inner.width - 2 * TREE_ITEM_PAD : 0;
        level = inner?.level;
      }
    }
  });

  /// Scenario: lay out a chain twenty levels deep whose every level wraps, counting how often the layout asks whether a card is open. The count grows with the tree, not doubling at each level (PR #2550 review).
  it("desktop/agent-cards/143: a deep tree that wraps at every level is laid out without repeating the work below it", () => {
    let chain: Node = leaf("end");
    for (let depth = 20; depth > 0; depth -= 1) chain = { key: `c${depth}`, children: [chain, leaf(`s${depth}`)] };
    let asked = 0;
    const level = layoutTreeLevel([chain], 900, { keyOf: (node) => node.key, isOpen: () => { asked += 1; return true; } });
    expect(level).not.toBeNull();
    expect(asked, "questions asked of a 41-card tree").toBeLessThan(5000);
  });
});

