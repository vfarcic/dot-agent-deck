/**
 * PR #1451 round 3, change 4 (PRD #1261) — everything voice can select is on
 * screen.
 *
 * While voice is on, a list voice selects from that does not fit where it is
 * shown is split into pages instead of scrolling: the New agent dialog's
 * directories and Mode chips and the Daemons screen's tiles. The agent
 * dashboard is not paged: it scrolls, voice on or off, so nothing on it is
 * hidden (issue #1492). Each page fills the space the list has (directories and
 * modes in as many columns as fit), "Page N of M" is shown beside it, and
 * "next page" / "previous page" turn it. A list that fits is shown whole, with
 * no marker, and with voice off every list scrolls as it always has. The New
 * agent dialog's daemons are never paged: they are always shown in full.
 *
 * Voice acts only on what is visible. The numbers on a page restart at 1, the
 * numbered list declared for voice is the page showing (`useVoiceNumbers`), and
 * a name said for an item on another page is refused with the page it is on —
 * by Rust for a directory or a Mode chip, which are declared with their other
 * pages (`VoicePagingDto`), and here, at dispatch, for anything else.
 *
 * This file is the arithmetic and the sentences; `hooks/useVoicePages.ts` is
 * the measuring and the registry the shell turns pages through.
 */
import type { VoiceResolvedParamDto } from "./bridge";
import type { VoiceNumberedKind } from "./voiceNumbers";
import { spokenWords } from "./voiceChoice";

/** One page of a list: which page (from 1), how many there are, and the slice of items it shows. */
export interface PageSlice {
  page: number;
  pages: number;
  start: number;
  end: number;
}

/**
 * The page `page` (from 1, clamped into range) of `count` items shown
 * `capacity` at a time. A capacity below 1 is read as 1, so a list always
 * shows something.
 */
export function pageSlice(count: number, capacity: number, page: number): PageSlice {
  const size = Math.max(1, Math.floor(capacity));
  const pages = Math.max(1, Math.ceil(count / size));
  const at = Math.min(Math.max(1, Math.floor(page)), pages);
  const start = (at - 1) * size;
  return { page: at, pages, start, end: Math.min(count, start + size) };
}

/** How a box of `width` × `height` holds cells of `rowHeight` and at least `minColumnWidth`, with `gap` between them. */
export interface GridFit {
  columns: number;
  rows: number;
}

/** The most whole rows and columns of cells that fit. At least one of each. */
export function gridFit(width: number, height: number, cell: { rowHeight: number; minColumnWidth: number; gap: number }): GridFit {
  const fit = (room: number, size: number) => Math.max(1, Math.floor((room + cell.gap) / (size + cell.gap)));
  return { columns: fit(width, cell.minColumnWidth), rows: fit(height, cell.rowHeight) };
}

/** The visible marker beside a paged list. */
export function pageMarker(slice: Pick<PageSlice, "page" | "pages">): string {
  return `Page ${slice.page} of ${slice.pages}`;
}

/** One item of a paged list that is on another page: what it is, its name, and its page. */
export interface VoiceOffPageItem {
  kind: VoiceNumberedKind;
  /** Its identity on that list — the same `value` a numbered entry carries. */
  value: string;
  deckId?: string;
  label: string;
  page: number;
}

/**
 * A list on screen split into pages, as it tells the shell: where it is, how
 * to turn it, and what is on its other pages.
 */
export interface VoicePager {
  page: number;
  pages: number;
  /** Show the page `delta` away. Called only when that page exists. */
  turn: (delta: 1 | -1) => void;
  /** Every item on a page that is not showing — of this list and of any other paged list beside it. */
  elsewhere: readonly VoiceOffPageItem[];
}

/** "next page" with nothing on screen split into pages. */
export const NOTHING_PAGES = "Nothing on screen is split into pages, so there is no other page to show.";
/** "next page" on the last page. */
export const LAST_PAGE = "This is the last page, so there is no next page.";
/** "previous page" on the first page. */
export const FIRST_PAGE = "This is the first page, so there is no previous page.";

/**
 * Why a page turn by `delta` cannot happen on `pager`, or `undefined` when it
 * can. A paged list's pager is published only while it pages, so a missing
 * one is a screen with nothing split into pages.
 */
export function pageTurnRefusal(pager: Pick<VoicePager, "page" | "pages"> | undefined, delta: 1 | -1): string | undefined {
  if (!pager || pager.pages < 2) return NOTHING_PAGES;
  if (delta > 0 && pager.page >= pager.pages) return LAST_PAGE;
  if (delta < 0 && pager.page <= 1) return FIRST_PAGE;
  return undefined;
}

/** What is said of an item on another page — Rust's `off_page`, word for word. */
export function offPageSentence(label: string, page: number, current: number): string {
  return `“${label}” is on page ${page}: say “${page > current ? "next page" : "previous page"}”.`;
}

/**
 * The item on another page that a dispatch's resolved params name, if any:
 * an agent, a directory, a Mode chip or a daemon the list shows on a page that
 * is not showing. Matched by identity (`value`, and for an agent its deck),
 * never by words, since the params are already resolved.
 *
 * An agent's identity is COMPOSITE (PR #1451 round 3 review): two daemons can
 * each run an agent with the same id. A param the backend resolved carries no
 * deck of its own — Rust resolves it against the selected daemon — so it is
 * that daemon's agent, `selectedDeckId`. An agent param whose deck is known
 * neither way names no off-page item: it is never matched against whichever
 * daemon's namesake happens to sit on another page.
 */
export function offPageTarget(params: readonly VoiceResolvedParamDto[], elsewhere: readonly VoiceOffPageItem[], selectedDeckId?: string): VoiceOffPageItem | undefined {
  const kinds: Partial<Record<VoiceResolvedParamDto["kind"], readonly VoiceNumberedKind[]>> = {
    agent_ref: ["agent"],
    dir_ref: ["directory", "parent"],
    mode_ref: ["mode"],
    deck_ref: ["deck", "deck_switch"],
  };
  for (const param of params) {
    const wanted = kinds[param.kind];
    if (!wanted) continue;
    const deckId = param.deckId ?? selectedDeckId;
    const item = elsewhere.find((candidate) => wanted.includes(candidate.kind) && candidate.value === param.value
      && (candidate.kind !== "agent" || (deckId !== undefined && candidate.deckId === deckId)));
    if (item) return item;
  }
  return undefined;
}

/**
 * The item on another page that an utterance names, by its words — for an
 * utterance nothing on screen answered (no match, or a name nothing showing
 * matches). Every word of the item's name must be in the utterance; the
 * longest such name wins, so "docs site" is `docs-site` and not `docs`.
 */
export function offPageNamed(utterance: string, elsewhere: readonly VoiceOffPageItem[]): VoiceOffPageItem | undefined {
  const said = new Set(spokenWords(utterance));
  let best: { item: VoiceOffPageItem; size: number } | undefined;
  for (const item of elsewhere) {
    const words = spokenWords(item.label);
    if (words.length === 0 || !words.every((word) => said.has(word))) continue;
    if (!best || words.length > best.size) best = { item, size: words.length };
  }
  return best?.item;
}
