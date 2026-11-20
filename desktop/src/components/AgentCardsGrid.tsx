import { BellRing } from "lucide-react";
import { useLayoutEffect, useMemo, useState } from "react";

import { placedCardKey, type CardResults, type PlacedCard } from "../lib/agentCards";
import { cardKinds, cardTree, needsYouPath, type CardNode } from "../lib/agentCardsTree";
import { DISPLAY_LIMITS, displayText, domIdentity } from "../lib/displayText";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import { AgentCard, type CardControls, type CardPlace } from "./AgentCard";

/**
 * PRD #2374 — the Cards view's line above every daemon's section: how many
 * agents the cards show and how many need the user, the Needs you filter, and
 * what to say when there is nothing to show. `cards` is every card on screen,
 * across the daemons, in the daemons' order.
 */
export function AgentCardsToolbar({ cards, needsYouOnly, controls }: {
  cards: readonly PlacedCard[];
  needsYouOnly: boolean;
  controls: Pick<VoiceActionContext, "setNeedsYouFilter">;
}) {
  const total = cards.length;
  const needYou = cards.filter((card) => card.entry.needs_you).length;
  return (
    <section className="agent-cards" aria-label="Agent cards" data-testid="agent-cards">
      <div className="agent-cards-toolbar">
        <span className="agent-cards-count">{total} {total === 1 ? "agent" : "agents"} · {needYou} {needYou === 1 ? "needs" : "need"} you</span>
        <button
          type="button"
          className={`button secondary compact agent-cards-filter${needsYouOnly ? " is-on" : ""}`}
          data-testid="agent-cards-needs-you-filter"
          aria-pressed={needsYouOnly}
          title={needsYouOnly ? "Show every card as bright" : "Open the way to every card that needs you, and fade the rest"}
          onClick={() => VOICE_ACTIONS.toggleNeedsYouFilter.run(controls, { needsYouOnly: !needsYouOnly })}
        ><BellRing size={13} aria-hidden="true" /><span>Needs you only</span></button>
      </div>
      {total === 0 && <p className="agent-cards-empty">No agent has a card yet. Every agent the daemon runs gets one.</p>}
      {total > 0 && needsYouOnly && needYou === 0 && <p className="agent-cards-empty" data-testid="agent-cards-none-need-you">No card needs you right now.</p>}
    </section>
  );
}

/** A card's width on the tree, in CSS pixels, and the gap between two cards side by side. */
const CARD_WIDTH = 224;
const CARD_GAP = 12;
const SLOT = CARD_WIDTH + CARD_GAP;

/**
 * The cards under one card, as a level of the tree, each opening further to
 * any depth. `above` is every card on the way down to this level: a card met
 * again below itself is not drawn, so even a cyclic list cannot recurse.
 */
function TreeLevel({ nodes, depth, top, view, above }: { nodes: readonly CardNode[]; depth: number; top?: boolean; view: TreeView; above: ReadonlySet<string> }) {
  const shown = nodes.filter((node) => !above.has(placedCardKey(node.card)));
  if (shown.length === 0) return null;
  return (
    <ul className={`agent-cards-tree-level${top ? " is-top" : ""}`}>
      {shown.map((node) => {
        const key = placedCardKey(node.card);
        const open = node.children.length > 0 && view.openCards.has(key);
        return (
          <li key={key}>
            <AgentCard card={node.card} controls={view.controls} results={view.results?.(node.card)} place={view.place(node, depth, open)} />
            {open && <TreeLevel nodes={node.children} depth={depth + 1} view={view} above={new Set([...above, key])} />}
          </li>
        );
      })}
    </ul>
  );
}

/** What every level of one deck's tree draws with. */
interface TreeView {
  openCards: ReadonlySet<string>;
  controls: CardControls;
  results?: (card: PlacedCard) => CardResults;
  place: (node: CardNode, depth: number, open: boolean) => CardPlace;
}

/**
 * Issue #2418, "Row, wrapping, roots stay put": each opened root in a row gets
 * its own level under the row, in the roots' order. Its tree starts under the
 * root and shifts left only when it would run past the frame; a strip at the
 * right is kept free for the lines, so none leaves the frame. One line per
 * root runs from the root down to its own tree, going around the trees above
 * it on the right. Measured from the DOM after every render and written
 * straight onto it, so the roots' own boxes are never touched.
 *
 * A tree wider than the frame scrolls inside its block. Its first level joins
 * the root's line at the level's middle — the first card itself when it is
 * the only one, else the rail over the cards — so the line ends there, where
 * the block's scroll has put it. When the block first overflows (or its width
 * changes) it is scrolled to put that point in the middle; a later scroll
 * lays the row out again, and while the point is scrolled out of view the
 * line ends at the block's nearer edge, inside the frame.
 */
function layoutRow(row: HTMLElement) {
  const grid = row.querySelector<HTMLElement>(":scope > .agent-cards-roots");
  const svg = row.querySelector<SVGSVGElement>(":scope > .agent-cards-lines");
  if (!grid || !svg) return;
  const avail = row.clientWidth;
  const rowBottom = grid.offsetTop + grid.offsetHeight;
  const bands = [...row.querySelectorAll<HTMLElement>(":scope > .agent-cards-band")];
  // The first level's top gap holds one detour lane per later root, so their sideways runs never share a line.
  const firstPad = 16 + 8 * Math.max(0, bands.length - 1);
  bands.forEach((band, index) => { band.style.paddingTop = `${index === 0 ? firstPad : 22}px`; });
  // A strip kept free at the right, wide enough for this row's detour lines.
  const reserve = bands.length > 1 ? 16 + 7 * (bands.length - 2) : 0;
  const room = Math.max(0, avail - reserve);
  const placed = bands.map((band) => {
    const block = band.querySelector<HTMLElement>(":scope > .agent-cards-block");
    const col = Number(band.dataset.col ?? 0);
    if (!block) return { band, col, width: 0, left: 0, attach: 0 };
    block.style.maxWidth = `${room}px`;
    const width = block.offsetWidth;
    const left = Math.max(0, Math.min(col * SLOT, room - width));
    block.style.marginLeft = `${left}px`;
    const level = block.querySelector<HTMLElement>(":scope > .agent-cards-tree-level");
    const overflow = block.scrollWidth - block.clientWidth;
    if (overflow > 0.5 && block.dataset.centredFor !== `${block.scrollWidth}/${block.clientWidth}`) {
      block.dataset.centredFor = `${block.scrollWidth}/${block.clientWidth}`;
      block.scrollLeft = level ? level.offsetLeft + level.offsetWidth / 2 - block.clientWidth / 2 : overflow / 2;
    } else if (overflow <= 0.5) delete block.dataset.centredFor;
    if (!block.onscroll) block.onscroll = () => layoutRow(row);
    // Where the first level joins the line, in the block's own box, after its scroll.
    const middle = level ? level.offsetLeft + level.offsetWidth / 2 - block.scrollLeft : width / 2;
    const attach = left + Math.max(0, Math.min(width, middle));
    return { band, col, width, left, attach };
  });
  const paths = placed.map((place, k) => {
    const pad = k === 0 ? firstPad : 22;
    const rootX = place.col * SLOT + CARD_WIDTH / 2;
    const blockX = place.attach;
    const top = place.band.offsetTop + pad;
    const elbowY = top - 7;
    const above = placed.slice(0, k);
    const blocked = above.some((other) => rootX >= other.left - 4 && rootX <= other.left + other.width + 4);
    if (!blocked) return `M${rootX},${rowBottom} V${elbowY} H${blockX} V${top}`;
    // Around the trees above, on the right: every earlier root is to this one's left, so a run to the right never
    // crosses their lines. Later roots take lanes nearer the row and columns further out, so no two detours share a segment.
    const out = Math.max(...above.map((other) => other.left + other.width)) + 10 + 7 * (k - 1);
    const lane = rowBottom + 6 + 8 * (placed.length - 1 - k);
    return `M${rootX},${rowBottom} V${lane} H${out} V${elbowY} H${blockX} V${top}`;
  });
  svg.setAttribute("width", String(avail));
  svg.setAttribute("height", String(row.offsetHeight));
  svg.replaceChildren(...paths.map((d, index) => {
    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", d);
    path.setAttribute("data-testid", "agent-cards-line");
    path.setAttribute("data-root-key", placed[index].band.dataset.rootKey ?? "");
    return path;
  }));
}

/**
 * PRD #2374, issue #2418 — one daemon's cards, the body of that daemon's
 * section in the Cards view (its header, with the daemon's own controls,
 * stays above it): its root agents in a wrapping grid whose places never
 * change, and under each row the trees of the roots opened in it. The daemon
 * hangs each card under the card that started it and sums up what is under
 * each; this draws that. Needs you only fades the cards that neither need the
 * user nor lead to one. A daemon older than the tree has its cards drawn as
 * roots, with a note saying why.
 */
export function AgentCardsDeckBody({ deckId, cards, tree, openCards, needsYouOnly, controls, results }: {
  deckId: string;
  cards: readonly PlacedCard[];
  /** Whether this deck's daemon builds the card tree. */
  tree: boolean;
  /** The cards whose tree is open, by placed key. */
  openCards: ReadonlySet<string>;
  needsYouOnly: boolean;
  controls: CardControls;
  /** The last answer to each action pressed on a card, to show under its primary button. */
  results?: (card: PlacedCard) => CardResults;
}) {
  /* The frame as a state, set by its ref: it is not mounted while there are
     no cards to show, so measuring follows it in and out — a deck that starts
     empty, or that the dashboard filter empties and restores, is measured
     (and watched for resizes) from the moment its frame appears. */
  const [frame, setFrame] = useState<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(0);
  const roots = useMemo(() => cardTree(cards, tree), [cards, tree]);
  const kinds = useMemo(() => cardKinds(cards, tree), [cards, tree]);
  const path = useMemo(() => (needsYouOnly ? needsYouPath(roots).path : undefined), [needsYouOnly, roots]);
  useLayoutEffect(() => {
    if (!frame) return undefined;
    const measure = () => setWidth(frame.clientWidth);
    measure();
    if (typeof ResizeObserver === "undefined") return undefined;
    const observer = new ResizeObserver(measure);
    observer.observe(frame);
    return () => observer.disconnect();
  }, [frame]);
  /* Every render may move what is under a row: lay each row out again. */
  useLayoutEffect(() => {
    frame?.querySelectorAll<HTMLElement>(":scope > .agent-cards-rows > .agent-cards-row").forEach(layoutRow);
  });
  if (cards.length === 0) return null;
  const columns = Math.max(1, Math.floor((width + CARD_GAP) / SLOT));
  const rows: CardNode[][] = [];
  for (let at = 0; at < roots.length; at += columns) rows.push(roots.slice(at, at + columns));
  const view: TreeView = {
    openCards,
    controls,
    results,
    place: (node, depth, open) => ({
      depth,
      kind: kinds.get(placedCardKey(node.card)) ?? "agent",
      hasChildren: node.children.length > 0,
      open,
      faded: path !== undefined && !path.has(placedCardKey(node.card)),
      tree,
    }),
  };
  return (
    <div className="agent-cards-deck" ref={setFrame} data-testid={`agent-cards-deck-${domIdentity(deckId)}`}>
      {!tree && <p className="agent-cards-note" data-testid="agent-cards-older-daemon">This daemon is older than the app, so its cards are shown side by side rather than as a tree.</p>}
      <div className="agent-cards-rows" data-testid="agent-cards-tree">
        {rows.map((row) => {
          const opened = row.map((node, col) => ({ node, col })).filter(({ node }) => node.children.length > 0 && openCards.has(placedCardKey(node.card)));
          return (
            <div className="agent-cards-row" key={placedCardKey(row[0].card)}>
              <svg className="agent-cards-lines" aria-hidden="true" />
              <div className="agent-cards-roots" data-testid="agent-cards-roots" style={{ gridTemplateColumns: `repeat(${columns}, minmax(0, ${CARD_WIDTH}px))` }}>
                {row.map((node) => {
                  const open = node.children.length > 0 && openCards.has(placedCardKey(node.card));
                  return <AgentCard key={placedCardKey(node.card)} card={node.card} controls={controls} results={results?.(node.card)} place={view.place(node, 0, open)} />;
                })}
              </div>
              {opened.map(({ node, col }) => (
                <div className="agent-cards-band" key={placedCardKey(node.card)} data-testid="agent-cards-band" data-root-key={node.card.entry.key} data-col={col}>
                  <div className="agent-cards-block" aria-label={`Agents under ${displayText(node.card.entry.name, DISPLAY_LIMITS.name)}`}>
                    <TreeLevel nodes={node.children} depth={1} top view={view} above={new Set([placedCardKey(node.card)])} />
                  </div>
                </div>
              ))}
            </div>
          );
        })}
      </div>
    </div>
  );
}
