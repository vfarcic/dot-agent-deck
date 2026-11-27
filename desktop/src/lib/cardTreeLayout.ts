/**
 * Issue #2533 (PRD #2374 decision 2026-11-26) — where each card under an
 * opened card goes: the agents under one card wrap into as many rows as the
 * width allows, at every level, instead of making the level wider than the
 * frame. A vertical spine drops from the parent, and each row hangs from it by
 * its own horizontal line (its bus), so which card is whose parent stays
 * readable from the lines:
 *
 *         [ parent ]
 *             |
 *   +-----+---+-+-----+
 *   |     |     |     |
 * [a]   [b]   [c]   [d]
 *             |
 *   +-----+---+
 *   |     |
 * [e]   [f]
 *
 * Everything here is arithmetic on the width available, so the layout is the
 * same in a test without a layout engine as in the browser, and nothing is
 * measured after it is drawn. Only the horizontal geometry is decided: each
 * row is as tall as its tallest item, and the lines that run down a row (the
 * drops to its cards, the spine to the next row) stretch with it in CSS.
 *
 * A level never comes out wider than the room it is given: a card in a level
 * deeper than the width allows is drawn narrower than {@link TREE_CARD_WIDTH}
 * rather than spilling out of its box (PR #2550 review), so the guarantee
 * holds while the room is at least {@link TREE_SPINE_LANE} plus both sides'
 * {@link TREE_ITEM_PAD}.
 *
 * A level that fits in one row is laid out as it was before wrapping existed:
 * its items side by side, the parent centred over them, the bus from the first
 * card's middle to the last's.
 */

/** A card's width on the tree, in CSS pixels, when there is room for it. */
export const TREE_CARD_WIDTH = 224;
/** Each item's space on either side of its card, so two cards side by side are twice this apart. */
export const TREE_ITEM_PAD = 5;
/** The room kept at a row's right end for the spine when the row holds a single item and the spine must pass it. */
export const TREE_SPINE_LANE = 16;

/** What the layout needs of a card on the tree: the cards under it, and how it is told apart from the others. */
export interface TreeLayoutNode<T> {
  children: readonly T[];
}

/** How the layout reads the cards: each one's key, and whether its own level is drawn. */
export interface TreeLayoutReader<T> {
  keyOf: (node: T) => string;
  isOpen: (node: T) => boolean;
}

/** One card on a level, and its own level when it is open. */
export interface TreeItemLayout<T> {
  node: T;
  /** The item's left edge, from the level's left edge. */
  left: number;
  /** The item's width: its card's, or its open level's when that is wider, plus {@link TREE_ITEM_PAD} on each side. */
  width: number;
  /** The width its card is drawn at. */
  cardWidth: number;
  /** Its card's middle, from the level's left edge: where the line down to the card runs. */
  center: number;
  /** The level under it, when it is open and anything under it is drawn. */
  level?: TreeLevelLayout<T>;
}

/** One row of a level. */
export interface TreeRowLayout<T> {
  items: TreeItemLayout<T>[];
  /** The items' total width; rows are drawn from the level's left edge. */
  width: number;
  /** The row's horizontal line, from the level's left edge; absent when the level has a single card. */
  bus?: { left: number; right: number };
  /** Where the spine leaves this row for the next one, from the level's left edge; absent on the last row. */
  spine?: number;
}

/** One level of the tree: the cards under one card, in rows. */
export interface TreeLevelLayout<T> {
  width: number;
  /** Where the line from the parent comes in, from the level's left edge: the level's middle. */
  stem: number;
  rows: TreeRowLayout<T>[];
  /** Whether the level holds one card, which the parent's line reaches straight, with no bus. */
  single: boolean;
}

interface Measured<T> {
  node: T;
  width: number;
  cardWidth: number;
  level?: TreeLevelLayout<T>;
}

/**
 * Lays out the cards under one card in `available` pixels. `read.isOpen`
 * says whether a card's own level is drawn; `above` is every card on the way
 * down to this level, and a card met again below itself is not laid out, so
 * even a cyclic list ends. `null` when nothing is left to draw.
 */
export function layoutTreeLevel<T extends TreeLayoutNode<T>>(
  nodes: readonly T[],
  available: number,
  read: TreeLayoutReader<T>,
  above: ReadonlySet<string> = new Set(),
): TreeLevelLayout<T> | null {
  return levelIn({ read, levels: new Map() }, nodes, available, [...above]);
}

/**
 * One layout pass: what it reads the cards with, and every open card's level
 * already laid out, by card, room and the path down to it. A level that wraps
 * measures its cards a second time in a narrower room, which measures each
 * open card's level again; without this, that doubles at every level that
 * wraps (PR #2550 review).
 */
interface Pass<T> {
  read: TreeLayoutReader<T>;
  levels: Map<string, TreeLevelLayout<T> | null>;
}

function levelIn<T extends TreeLayoutNode<T>>(pass: Pass<T>, nodes: readonly T[], available: number, path: readonly string[]): TreeLevelLayout<T> | null {
  const above = new Set(path);
  const shown = nodes.filter((node) => !above.has(pass.read.keyOf(node)));
  if (shown.length === 0) return null;
  const measure = (room: number) => shown.map((node) => measureItem(pass, node, room, path));
  let items = measure(available);
  let rows = pack(items, available);
  // A level that wraps keeps a lane at the right of its items, so the spine can pass a row that holds one wide item.
  if (rows.length > 1) {
    items = measure(available - TREE_SPINE_LANE);
    rows = pack(items, available);
  }
  const single = shown.length === 1;
  const widths = rows.map((row) => row.reduce((sum, item) => sum + item.width, 0));
  const lanes = rows.slice(0, -1).flatMap((row, at) => (row.length === 1 ? [widths[at] + TREE_SPINE_LANE] : []));
  const width = Math.max(...widths, ...lanes);
  const stem = width / 2;
  let entry = stem;
  const laid = rows.map((row, at): TreeRowLayout<T> => {
    let left = 0;
    const placed = row.map((item): TreeItemLayout<T> => {
      const out = { node: item.node, left, width: item.width, cardWidth: item.cardWidth, center: left + item.width / 2, ...(item.level ? { level: item.level } : {}) };
      left += item.width;
      return out;
    });
    const last = at === rows.length - 1;
    const spine = last ? undefined : spineOf(placed, widths[at], entry);
    const xs = [...placed.map((item) => item.center), entry, ...(spine === undefined ? [] : [spine])];
    const out: TreeRowLayout<T> = { items: placed, width: widths[at], ...(single ? {} : { bus: { left: Math.min(...xs), right: Math.max(...xs) } }), ...(spine === undefined ? {} : { spine }) };
    if (spine !== undefined) entry = spine;
    return out;
  });
  return { width, stem, rows: laid, single };
}

function measureItem<T extends TreeLayoutNode<T>>(pass: Pass<T>, node: T, room: number, path: readonly string[]): Measured<T> {
  const inner = Math.max(0, room - 2 * TREE_ITEM_PAD);
  const cardWidth = Math.min(TREE_CARD_WIDTH, inner);
  let level: TreeLevelLayout<T> | null = null;
  if (node.children.length > 0 && pass.read.isOpen(node)) {
    const key = pass.read.keyOf(node);
    const id = `${inner}\u0000${path.join("\u0000")}\u0000${key}`;
    if (pass.levels.has(id)) level = pass.levels.get(id) ?? null;
    else {
      level = levelIn(pass, node.children, inner, [...path, key]);
      pass.levels.set(id, level);
    }
  }
  return { node, width: Math.max(cardWidth, level?.width ?? 0) + 2 * TREE_ITEM_PAD, cardWidth, ...(level ? { level } : {}) };
}

/** Fills rows from the left, starting a new row when the next item would not fit. A row always takes at least one item. */
function pack<T>(items: Measured<T>[], available: number): Measured<T>[][] {
  const rows: Measured<T>[][] = [];
  let used = 0;
  for (const item of items) {
    const row = rows[rows.length - 1];
    if (row && used + item.width <= available + 0.5) {
      row.push(item);
      used += item.width;
    } else {
      rows.push([item]);
      used = item.width;
    }
  }
  return rows;
}

/**
 * Where the spine runs down past a row to the next one: in a gap between two
 * of its items, the one nearest where the spine came in, so a grid of closed
 * cards has a straight spine; past a row of one item, in the lane at its right.
 * The gaps run the row's full height, since each item is a column holding its
 * card and anything open under it.
 */
function spineOf<T>(items: readonly TreeItemLayout<T>[], width: number, entry: number): number {
  if (items.length < 2) return width + TREE_SPINE_LANE / 2;
  const gaps = items.slice(0, -1).map((item) => item.left + item.width);
  return gaps.reduce((best, gap) => (Math.abs(gap - entry) < Math.abs(best - entry) ? gap : best));
}
