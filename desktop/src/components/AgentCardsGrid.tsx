import { SlidersHorizontal, X } from "lucide-react";
import { useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";

import { placedCardKey, type CardResults, type PlacedCard } from "../lib/agentCards";
import { cardKinds, cardTree, needsYouPath, type CardNode } from "../lib/agentCardsTree";
import { layoutTreeLevel, TREE_CARD_WIDTH, type TreeLevelLayout } from "../lib/cardTreeLayout";
import { DISPLAY_LIMITS, displayText, domIdentity } from "../lib/displayText";
import { endedRoleOfCard } from "../lib/endedRoles";
import { VOICE_ACTIONS, type VoiceActionContext } from "../lib/voiceActions";
import type { CardSelection } from "../hooks/useAgentCardsView";
import type { EndedRole } from "../types";
import { CardActionIcon, cardIconName } from "../lib/cardIcons";
import { ActionIconGroup, ActionTipButton } from "./ActionTipButton";
import { AgentCard, batchChoiceHover, type CardControls, type CardPlace } from "./AgentCard";
import { MoreMenu } from "./MoreMenu";

/**
 * Issue #2399, decision 2026-11-24 — a batch button's icon: the action's
 * catalog icon, or for a close the cross the card's own Close draws.
 */
export function BatchChoiceIcon({ choice }: { choice: { icon?: string | null; kind?: string } }) {
  if (batchIconName(choice) === "close") return <X size={15} aria-hidden="true" focusable="false" />;
  return <CardActionIcon name={choice.icon} size={15} />;
}

/** The icon {@link BatchChoiceIcon} draws, by name (`data-icon`). */
export function batchIconName(choice: { icon?: string | null; kind?: string }): string {
  return choice.kind === "close" || choice.kind === "close_keep_worktree" ? "close" : cardIconName(choice.icon);
}

/**
 * PR #2491 review S1 — the daemons on screen whose agents the cards do not
 * account for, by name: connected daemons that report no cards (their rows
 * stay as they are under Needs you only) and daemons that are not answering.
 * Needs you only and the top bar's count read only cards, so neither may say
 * nothing needs the user while one of these is on screen.
 */
export interface UncountedDaemons {
  notReporting: readonly string[];
  notAnswering: readonly string[];
}

export const NO_UNCOUNTED: UncountedDaemons = { notReporting: [], notAnswering: [] };

const hasUncounted = (uncounted: UncountedDaemons) => uncounted.notReporting.length > 0 || uncounted.notAnswering.length > 0;

/** "A", "A and B", "A, B and C". */
function nameList(names: readonly string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}

/**
 * What the dashboard says about the daemons the cards do not count: their
 * agents are "shown as usual" (the note above the daemons) or "not counted"
 * (the top bar's ⋯). Empty when every daemon on screen reports cards.
 */
export function uncountedDaemonsText(uncounted: UncountedDaemons, agents: "shown as usual" | "not counted"): string {
  const { notReporting, notAnswering } = uncounted;
  const parts: string[] = [];
  if (notReporting.length > 0) {
    parts.push(`Agents on ${nameList(notReporting)} are ${agents}: ${notReporting.length === 1 ? "that daemon does" : "those daemons do"} not report cards.`);
  }
  if (notAnswering.length > 0) {
    const one = notAnswering.length === 1;
    parts.push(`${nameList(notAnswering)} ${one ? "is" : "are"} not answering, so ${one ? "its" : "their"} agents are not counted.`);
  }
  return parts.join(" ");
}

/**
 * The top bar's ⋯ description: how many agents' cards need the user, and,
 * while a daemon on screen is not counted, which. `undefined` when there is
 * nothing to say (no card needs the user and every daemon is counted).
 */
export function needsYouCountDescription(count: number, uncounted: UncountedDaemons): string | undefined {
  const counted = count > 0 ? `${count} ${count === 1 ? "agent needs" : "agents need"} you` : undefined;
  if (!hasUncounted(uncounted)) return counted;
  return `${counted ? `${counted}.` : "No card needs you."} ${uncountedDaemonsText(uncounted, "not counted")}`;
}

/**
 * PRD #2374 — what the dashboard says above the daemons' sections when the
 * cards have nothing to show: no agent has a card yet (the Cards view), or,
 * with Needs you only on, no card needs the user (either view). The line that
 * counted the cards and held the Needs you only and Card actions buttons is
 * gone (PR #2491 decision 2026-11-25): each daemon's header counts its
 * agents, the top bar's ⋯ counts what needs the user and holds Needs you
 * only, and each daemon's ⋯ opens its Card actions. `cards` is every card on
 * screen, across the daemons.
 *
 * "Nothing needs you right now" is said only while no daemon is in
 * `uncounted`: a daemon that does not report cards and has agents on screen,
 * or one that is not answering. Otherwise the note says no card needs the
 * user and names those daemons (PR #2491 review S1). The empty Cards view's
 * "Every agent the daemon runs gets one" is held to the same condition, and
 * names the same daemons otherwise (PR #2491 review).
 */
export function AgentCardsNotes({ cards, needsYouOnly, cardsView, uncounted = NO_UNCOUNTED }: {
  cards: readonly PlacedCard[];
  needsYouOnly: boolean;
  /** Whether the Cards view is on screen (the Table says nothing about cards it does not draw). */
  cardsView: boolean;
  /** The daemons on screen whose agents have no cards to count. */
  uncounted?: UncountedDaemons;
}) {
  const total = cards.length;
  const needYou = cards.filter((card) => card.entry.needs_you).length;
  const empty = cardsView && total === 0;
  const noneNeedYou = !empty && needsYouOnly && needYou === 0;
  if (!empty && !noneNeedYou) return null;
  const scoped = hasUncounted(uncounted);
  return (
    <div className="agent-cards-notes">
      {empty && !scoped && <p className="agent-cards-empty" data-testid="agent-cards-empty">No agent has a card yet. Every agent the daemon runs gets one.</p>}
      {empty && scoped && <p className="agent-cards-empty" data-testid="agent-cards-empty-scoped">No agent has a card yet. {uncountedDaemonsText(uncounted, "shown as usual")}</p>}
      {noneNeedYou && !scoped && <p className="agent-cards-empty" data-testid="agent-cards-none-need-you">Nothing needs you right now. Needs you only is on: turn it off from the top bar to see every agent.</p>}
      {noneNeedYou && scoped && <p className="agent-cards-empty" data-testid="agent-cards-none-need-you-scoped">No card needs you. {uncountedDaemonsText(uncounted, "shown as usual")}</p>}
    </div>
  );
}

/**
 * PR #2491 review — the selection bar: shown at the foot of the Dashboard as
 * soon as a card is selected, and gone when the selection is emptied. It says
 * how many cards are selected, offers each action the selected cards offer as
 * an icon (pressing one opens the batch's confirmation; nothing is sent
 * before it), and Clear. It floats over the end of the dashboard's scroll
 * region, which leaves room for it below the last card.
 */
export function AgentCardsSelectionBar({ selection, controls, editor }: SelectionBarProps) {
  if (selection.selected.size === 0) return null;
  return <SelectionBar selection={selection} controls={controls} editor={editor} />;
}

/**
 * PR #2491 (the maintainer, 2026-11-25) — the selection bar's shortcut to the
 * Card actions editor: the daemons of the selected cards whose ⋯ menu offers
 * it, in the dashboard's order, and the editor's members its presses go through.
 */
export interface SelectionBarEditor {
  daemons: readonly { deckId: string; name: string }[];
  controls: Pick<VoiceActionContext, "openCardActionsEditor"> & Partial<Pick<VoiceActionContext, "reportRefused" | "askVoiceChoice">>;
}

interface SelectionBarProps {
  selection: CardSelection;
  controls: Pick<VoiceActionContext, "clearCardSelection" | "runSelectionBatch"> & Partial<Pick<VoiceActionContext, "reportRefused">>;
  editor?: SelectionBarEditor;
}

const EDIT_CARD_ACTIONS = "Edit card actions";

/**
 * PR #2491 (the maintainer, 2026-11-25) — Edit card actions in the selection
 * bar, after the batch actions and set apart from them so it does not read as
 * one more action to send: with every selected card on one daemon, a button
 * that opens that daemon's editor (read-only on a release that cannot edit
 * them, as from its ⋯ menu); with the selection across daemons, a menu of
 * those daemons, each opening its own. The daemon's ⋯ menu stays the main way in.
 */
function SelectionEditButton({ editor }: { editor: SelectionBarEditor }) {
  const { daemons, controls } = editor;
  if (daemons.length === 0) return null;
  const icon = <SlidersHorizontal size={15} aria-hidden="true" focusable="false" />;
  if (daemons.length === 1) {
    const [daemon] = daemons;
    return (
      <span className="agent-cards-selection-edit">
        <button
          type="button"
          className="icon-button agent-cards-selection-edit-button"
          data-testid="agent-cards-selection-edit"
          data-icon="sliders-horizontal"
          aria-label={EDIT_CARD_ACTIONS}
          title={EDIT_CARD_ACTIONS}
          onClick={() => VOICE_ACTIONS.openCardActionsEditor.run(controls, daemon.deckId)}
        >{icon}</button>
      </span>
    );
  }
  return (
    <span className="agent-cards-selection-edit" data-icon="sliders-horizontal">
      <MoreMenu
        label={EDIT_CARD_ACTIONS}
        menuLabel={EDIT_CARD_ACTIONS}
        testId="agent-cards-selection-edit"
        menuTestId="agent-cards-selection-edit-menu"
        className="agent-cards-selection-edit-menu"
        buttonClassName="icon-button agent-cards-selection-edit-button"
        icon={icon}
        items={daemons.map((daemon) => ({
          key: daemon.deckId,
          label: displayText(daemon.name, DISPLAY_LIMITS.name),
          testId: `agent-cards-selection-edit-${domIdentity(daemon.deckId)}`,
          title: `Add, change, hide or remove the actions ${displayText(daemon.name, DISPLAY_LIMITS.name)}'s agents offer on their cards`,
          onSelect: () => VOICE_ACTIONS.openCardActionsEditor.run(controls, daemon.deckId),
        }))}
      />
    </span>
  );
}

/**
 * PR #2491 review H1 — where focus goes when the bar goes while it has it
 * (Clear pressed from the keyboard, or the selection emptied by voice or by
 * its cards leaving): the checkbox of the card ticked or unticked last, if it
 * is still drawn, else the dashboard's scroll region. Left on the removed
 * button, focus falls to the document, and WebKit's next Tab then starts from
 * the page's header with the dashboard scrolled back to the top.
 */
function focusTargetAfterBar(bar: HTMLElement, last: string | undefined): HTMLElement | null {
  const region = bar.closest<HTMLElement>(".overview-body") ?? bar.parentElement;
  if (!region) return null;
  const card = last === undefined ? undefined : `agent-card-${domIdentity(last)}`;
  const checkbox = card === undefined ? undefined : [...region.querySelectorAll<HTMLElement>("[data-testid='agent-card-select']")]
    .find((box) => box.closest("article")?.getAttribute("data-testid") === card);
  return checkbox ?? region;
}

function SelectionBar({ selection, controls, editor }: SelectionBarProps) {
  const count = selection.selected.size;
  const bar = useRef<HTMLDivElement>(null);
  const last = useRef(selection.last);
  useLayoutEffect(() => { last.current = selection.last; }, [selection.last]);
  /* A layout effect's cleanup runs while the bar is still in the document, so focus can be moved before the button holding it is removed. */
  useLayoutEffect(() => {
    const node = bar.current;
    return () => {
      if (!node || !node.contains(document.activeElement)) return;
      focusTargetAfterBar(node, last.current)?.focus({ preventScroll: true });
    };
  }, []);
  return (
    <div ref={bar} className="agent-cards-selection-bar" role="region" aria-label="Selected cards" data-testid="agent-cards-selection-bar">
      <span className="agent-cards-selection-count" data-testid="agent-cards-selection-count">{count} selected</span>
      {selection.choices.length === 0
        ? <span className="agent-cards-selection-hint">No action is offered by the selected cards.</span>
        : (
          <ActionIconGroup className="agent-cards-selection-actions" label={`Send to the ${count} selected ${count === 1 ? "card" : "cards"}`}>
            {selection.choices.map((choice) => (
              <ActionTipButton
                key={choice.id}
                className="icon-button agent-cards-batch-icon"
                data-testid={`agent-cards-batch-action-${domIdentity(choice.id)}`}
                data-icon={batchIconName(choice)}
                aria-label={displayText(choice.label, DISPLAY_LIMITS.name)}
                tip={batchChoiceHover(choice, `the ${count} selected ${count === 1 ? "agent" : "agents"}`)}
                onClick={() => VOICE_ACTIONS.runSelectionBatch.run(controls, { cardActionId: choice.id })}
              ><BatchChoiceIcon choice={choice} /></ActionTipButton>
            ))}
          </ActionIconGroup>
        )}
      {editor && <SelectionEditButton editor={editor} />}
      <button
        type="button"
        className="button secondary compact agent-cards-selection-clear"
        data-testid="agent-cards-selection-clear"
        title="Unselect every card"
        onClick={() => VOICE_ACTIONS.clearCardSelection.run(controls)}
      >Clear</button>
    </div>
  );
}

/** A root card's width, and the gap between two roots side by side. */
const CARD_WIDTH = TREE_CARD_WIDTH;
const CARD_GAP = 12;
const SLOT = CARD_WIDTH + CARD_GAP;

/** An opened tree's box: its padding and border on each side, and the caption above its cards. */
const BOX_PAD = 8;
const BOX_BORDER = 1;
const BOX_EDGES = 2 * (BOX_PAD + BOX_BORDER);

/** No card's tree open: the default for a deck body given no open cards. */
const NO_OPEN_CARDS: ReadonlySet<string> = new Set();

/**
 * Issue #2533 — one level of an opened tree, laid out by
 * {@link layoutTreeLevel}: its cards in rows, each row hanging from the spine
 * by its own bus, each card a column holding the level under it when it is
 * open. The level's width and every line's place across come from the
 * layout; the lines' heights follow the rows in CSS.
 *
 * Every card is a child of the level itself, keyed by the card, and a row
 * ends at a break between two of them, so a card that moves to another row
 * (opening it, or the window narrowing) stays mounted and keeps the keyboard
 * (PR #2550 review). Each row's first card carries its row's bus and spine,
 * placed from its own left edge; it is as tall as the row, so the spine runs
 * down to where the next row starts.
 */
function TreeLevel({ level, depth, top, view }: { level: TreeLevelLayout<CardNode>; depth: number; top?: boolean; view: TreeView }) {
  const classes = ["agent-cards-tree-level", top ? "is-top" : "", level.single ? "is-single" : ""].filter(Boolean).join(" ");
  return (
    <div
      className={classes}
      role="list"
      data-testid="agent-cards-tree-level"
      data-rows={level.rows.length}
      data-stem-x={level.stem}
      style={{ width: `${level.width}px`, "--stem-x": `${level.stem}px` } as CSSProperties}
    >
      {level.rows.flatMap((row, at) => {
        const items = row.items.map((item, index) => {
          const key = view.keyOf(item.node);
          const open = item.node.children.length > 0 && view.openCards.has(key);
          const first = index === 0;
          return (
            <div
              className="agent-cards-tree-item"
              role="listitem"
              key={key}
              data-row={at}
              data-center-x={item.center}
              style={{ width: `${item.width}px`, "--tree-card-width": `${item.cardWidth}px` } as CSSProperties}
            >
              {first && row.bus && <span className="agent-cards-bus" aria-hidden="true" data-testid="agent-cards-bus" data-row={at} style={{ left: `${row.bus.left - item.left}px`, width: `${row.bus.right - row.bus.left}px` }} />}
              {first && row.spine !== undefined && <span className="agent-cards-spine" aria-hidden="true" data-testid="agent-cards-spine" data-row={at} style={{ left: `${row.spine - item.left}px` }} />}
              <AgentCard card={item.node.card} controls={view.controls} results={view.results?.(item.node.card)} place={view.place(item.node, depth, open)} ended={endedRoleOfCard(view.endedRoles, item.node.card.entry)} />
              {item.level && <TreeLevel level={item.level} depth={depth + 1} view={view} />}
            </div>
          );
        });
        return at === 0 ? items : [<span className="agent-cards-tree-break" aria-hidden="true" key={`row-break\u0000${view.keyOf(row.items[0].node)}`} />, ...items];
      })}
    </div>
  );
}

/** What every level of one deck's tree draws with. */
interface TreeView {
  openCards: ReadonlySet<string>;
  keyOf: (node: CardNode) => string;
  controls: CardControls;
  results?: (card: PlacedCard) => CardResults;
  place: (node: CardNode, depth: number, open: boolean) => CardPlace;
  /** Issue #1965 — this deck's exited orchestration roles, whose cards say why. */
  endedRoles?: readonly EndedRole[];
}

/** The strip kept free at a row's right for the lines that go around the trees above, when `trees` roots of the row are open. */
const detourReserve = (trees: number) => (trees > 1 ? 16 + 7 * (trees - 2) : 0);

/** The width each opened root's box is laid out in, when `trees` roots of a row are open in a frame `frame` pixels wide. */
const boxRoom = (frame: number, trees: number) => Math.max(0, frame - detourReserve(trees));

/**
 * Issue #2418, "Row, wrapping, roots stay put": each opened root in a row gets
 * its own level under the row, in the roots' order, in its own box (issue
 * #2533). The box starts under the root and shifts left only when it would
 * run past the frame; a strip at the right is kept free for the lines, so
 * none leaves the frame. One line per root runs from the root down to the top
 * of its box, where the box's own stem takes over, going around the boxes
 * above it on the right. The boxes are placed when they are drawn; this
 * measures where they ended up after every render and draws the lines, so the
 * roots' own boxes are never touched.
 */
function layoutRow(row: HTMLElement) {
  const grid = row.querySelector<HTMLElement>(":scope > .agent-cards-roots");
  const svg = row.querySelector<SVGSVGElement>(":scope > .agent-cards-lines");
  if (!grid || !svg) return;
  const avail = row.clientWidth;
  const at = row.getBoundingClientRect();
  const rowBottom = grid.offsetTop + grid.offsetHeight;
  const bands = [...row.querySelectorAll<HTMLElement>(":scope > .agent-cards-band")];
  // The first level's top gap holds one detour lane per later root, so their sideways runs never share a line.
  const firstPad = 16 + 8 * Math.max(0, bands.length - 1);
  bands.forEach((band, index) => { band.style.paddingTop = `${index === 0 ? firstPad : 22}px`; });
  const placed = bands.map((band) => {
    const box = band.querySelector<HTMLElement>(":scope > .agent-cards-box");
    const col = Number(band.dataset.col ?? 0);
    if (!box) return { band, col, width: 0, left: 0, attach: 0 };
    const frame = box.getBoundingClientRect();
    const left = frame.left - at.left;
    const level = box.querySelector<HTMLElement>(":scope > .agent-cards-tree-level");
    // Where the box's stem starts: the line from the root ends on it, on the box's top edge.
    const attach = level ? level.getBoundingClientRect().left - at.left + Number(level.dataset.stemX ?? 0) : left + frame.width / 2;
    return { band, col, width: frame.width, left, attach };
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
export function AgentCardsDeckBody({ deckId, cards, tree = false, openCards = NO_OPEN_CARDS, needsYouOnly, controls, results, endedRoles, selection }: {
  deckId: string;
  cards: readonly PlacedCard[];
  /** Whether this deck's daemon builds the card tree; without it, its cards are drawn side by side, as an older daemon's are. */
  tree?: boolean;
  /** The cards whose tree is open, by placed key; none when absent. */
  openCards?: ReadonlySet<string>;
  needsYouOnly: boolean;
  controls: CardControls;
  /** The last answer to each action pressed on a card, for its Close control's tooltip. */
  results?: (card: PlacedCard) => CardResults;
  /** Issue #1965 — this deck's exited orchestration roles, whose cards say why. */
  endedRoles?: readonly EndedRole[];
  /** Issue #2399 — the selection, when this deck's daemon does batch actions: each card then carries its checkbox, always (PR #2491 review). */
  selection?: CardSelection;
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
  const keyOf = (node: CardNode) => placedCardKey(node.card);
  const isOpen = (node: CardNode) => openCards.has(keyOf(node));
  const view: TreeView = {
    openCards,
    keyOf,
    controls,
    results,
    endedRoles,
    place: (node, depth, open) => ({
      depth,
      kind: kinds.get(placedCardKey(node.card)) ?? "agent",
      hasChildren: node.children.length > 0,
      open,
      faded: path !== undefined && !path.has(placedCardKey(node.card)),
      tree,
      ...(selection ? { selected: selection.selected.has(placedCardKey(node.card)) } : {}),
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
                  return <AgentCard key={placedCardKey(node.card)} card={node.card} controls={controls} results={results?.(node.card)} place={view.place(node, 0, open)} ended={endedRoleOfCard(endedRoles, node.card.entry)} />;
                })}
              </div>
              {opened.map(({ node, col }) => {
                const room = boxRoom(width, opened.length);
                const level = layoutTreeLevel(node.children, room - BOX_EDGES, { keyOf, isOpen }, new Set([keyOf(node)]));
                if (!level) return null;
                const boxWidth = level.width + BOX_EDGES;
                const left = Math.max(0, Math.min(col * SLOT, room - boxWidth));
                const name = displayText(node.card.entry.name, DISPLAY_LIMITS.name);
                return (
                  <div className="agent-cards-band" key={placedCardKey(node.card)} data-testid="agent-cards-band" data-root-key={node.card.entry.key} data-col={col}>
                    <div
                      className="agent-cards-box"
                      role="group"
                      aria-label={`Agents under ${name}`}
                      data-testid="agent-cards-box"
                      style={{ width: `${boxWidth}px`, marginLeft: `${left}px` }}
                    >
                      <div className="agent-cards-box-caption" data-testid="agent-cards-box-caption" title={name} aria-hidden="true" style={{ maxWidth: `${Math.max(0, level.stem - 8)}px` }}>{name}</div>
                      <TreeLevel level={level} depth={1} top view={view} />
                    </div>
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>
    </div>
  );
}
