import type { AgentCardCheckedRef, AgentCardEntry, AgentPullRequest } from "../types";
import type { PullRequestChecksDto, PullRequestInfoDto } from "./bridge";
import { placedCardKey, type PlacedCard } from "./agentCards";
import { DASHBOARD_AGENT_TYPES } from "./dashboardFilter";
import { isPullRequestUrl, pullRequestFromDto, pullRequestLabel } from "./pullRequest";
import { cardStatusWord, statusWordLabel, type StatusWord } from "./statusWords";

/*
 * Issue #2418 — the Cards view as a tree of root agents, as the webview DRAWS
 * it. The daemon links each card to the card that started it (`parent`), sums
 * up what is under a card (`rollup`) and checks the pull requests and issues
 * an agent reported (`checked_refs`); nothing here builds a tree from names,
 * counts descendants or looks anything up (CLAUDE.md rule 18). What is decided
 * here is how to draw that: which received card hangs under which, which one
 * colour a closed card takes from its own status and its roll-up's worst, and
 * the user's own choices — which trees are open, and the Needs you only view.
 */

/** One card as the tree draws it, with the cards the daemon hung under it, in the daemon's order. */
export interface CardNode {
  card: PlacedCard;
  children: CardNode[];
}

/**
 * The roots of one deck's cards, each with what hangs under it. `tree` is
 * whether the deck's daemon advertised the card tree: without it, every card
 * is a root. A card whose `parent` is not among `cards` (the dashboard filter
 * hides it, or the daemon no longer lists it) is drawn as a root.
 *
 * The result is always a forest, whatever the links say. The daemon links by
 * exact identity and sends none, but a malformed snapshot could link cards in
 * a cycle, which no root leads to. Then the first card in the daemon's order
 * that no root reaches — on the cycle or below it — becomes a root and its own
 * link is dropped, until every card is reached; so each card appears once in
 * the result and a walk down it ends.
 */
export function cardTree(cards: readonly PlacedCard[], tree: boolean): CardNode[] {
  const nodes = cards.map((card) => ({ card, children: [] as CardNode[] }));
  if (!tree) return nodes;
  const byKey = new Map(nodes.map((node) => [node.card.entry.key, node]));
  const above = new Map<CardNode, CardNode>();
  const roots: CardNode[] = [];
  for (const node of nodes) {
    const parent = node.card.entry.parent;
    const starter = parent && parent !== node.card.entry.key ? byKey.get(parent) : undefined;
    if (starter) {
      starter.children.push(node);
      above.set(node, starter);
    } else roots.push(node);
  }
  const reached = new Set<CardNode>();
  const walk = (start: CardNode) => {
    const stack = [start];
    while (stack.length) {
      const node = stack.pop() as CardNode;
      if (reached.has(node)) continue;
      reached.add(node);
      stack.push(...node.children);
    }
  };
  roots.forEach(walk);
  for (const node of nodes) {
    if (reached.has(node)) continue;
    // On a cycle, or below one: cut this card's link and draw it as a root.
    const starter = above.get(node);
    if (starter) starter.children = starter.children.filter((child) => child !== node);
    roots.push(node);
    walk(node);
  }
  return roots;
}

/**
 * The cards on the way to every card `wanted` picks: `path` holds those cards
 * and every card above them (Needs you only keeps them bright), `open` the
 * cards with a picked card under them, which must be open for it to show.
 * Walks the tree it is given once; a card met again below itself is skipped.
 */
export function pathTo(roots: readonly CardNode[], wanted: (node: CardNode) => boolean): { path: Set<string>; open: Set<string> } {
  const path = new Set<string>();
  const open = new Set<string>();
  const active = new Set<CardNode>();
  const visit = (node: CardNode): boolean => {
    if (active.has(node)) return false;
    active.add(node);
    let below = false;
    for (const child of node.children) if (visit(child)) below = true;
    active.delete(node);
    const picked = wanted(node);
    if (below) open.add(placedCardKey(node.card));
    if (below || picked) path.add(placedCardKey(node.card));
    return below || picked;
  };
  roots.forEach(visit);
  return { path, open };
}

/**
 * Needs you only: the cards the daemon marked as needing the user and every
 * card above them (`path`, which stays bright), and the cards to open so each
 * marked card is on screen (`open`: those with a marked card under them).
 */
export function needsYouPath(roots: readonly CardNode[]): { path: Set<string>; open: Set<string> } {
  return pathTo(roots, (node) => node.card.entry.needs_you);
}

/**
 * The card statuses from the most severe down — the daemon's order for a
 * roll-up's `worst` (issue #2418, D2). Used to draw: to pick the one colour a
 * closed card takes from its own status and its roll-up's worst, and to lay
 * the counts bar out in a fixed order.
 */
const SEVERITY: readonly string[] = ["waiting_for_input", "blocked", "error", "waiting", "working", "thinking", "compacting", "idle", "stopped", "unknown"];

function severity(status: string): number {
  const at = SEVERITY.indexOf(status);
  return at < 0 ? SEVERITY.length : at;
}

/** The colour a card is drawn in, as a status word: its own status, or, closed with a roll-up, the worst of its own and the roll-up's. */
export function cardTone(entry: Pick<AgentCardEntry, "status" | "rollup">, open: boolean): StatusWord | "unknown" {
  const worst = !open && entry.rollup && severity(entry.rollup.worst) < severity(entry.status) ? entry.rollup.worst : entry.status;
  return cardStatusWord(worst) ?? "unknown";
}

/** A card's status in the daemon's words, as the table and the TUI show it; a status this build does not know, as it came. */
function cardStatusText(status: string): string {
  const word = cardStatusWord(status);
  return word ? statusWordLabel(word) : status.replace(/_/g, " ");
}

/** One stretch of a roll-up's counts bar. */
export interface RollupSegment {
  word: StatusWord | "unknown";
  label: string;
  count: number;
}

/** A roll-up's counts as the bar draws them, most severe first; two statuses shown as one word share a stretch. */
export function rollupSegments(counts: Readonly<Record<string, number>>): RollupSegment[] {
  const segments: RollupSegment[] = [];
  const statuses = Object.keys(counts).filter((status) => Number.isFinite(counts[status]) && counts[status] > 0);
  statuses.sort((a, b) => severity(a) - severity(b) || a.localeCompare(b));
  for (const status of statuses) {
    const word = cardStatusWord(status) ?? "unknown";
    const label = cardStatusText(status);
    const same = segments.find((segment) => segment.label === label);
    if (same) same.count += counts[status];
    else segments.push({ word, label, count: counts[status] });
  }
  return segments;
}

/** "4 under it · 1 needs you". */
export function rollupSentence(rollup: { total: number; needs_you: number }): string {
  return `${rollup.total} under it · ${rollup.needs_you} ${rollup.needs_you === 1 ? "needs" : "need"} you`;
}

/** What a card is: one that dispatches others, an orchestration's orchestrator, one of its roles, or a plain agent. */
export type CardKind = "dispatcher" | "orchestrator" | "role" | "agent";

const CARD_KINDS: readonly CardKind[] = ["dispatcher", "orchestrator", "role", "agent"];

/**
 * Each card's kind, by placed key. A daemon that builds the tree (`tree`)
 * sends each card's `kind`, worked out from its own dispatch records, and that
 * is what is drawn — whichever other cards the dashboard filter shows.
 *
 * Fallback for an older daemon, which sends no `kind` (and for a kind this
 * build does not know): read it off the relations among `cards` — an
 * orchestrator or a role by its orchestration relation, a dispatcher when
 * another card in `cards` names it as its dispatcher, otherwise an agent. That
 * guess sees only the cards it is given, so a dispatcher whose units are all
 * filtered out reads as an agent there.
 */
export function cardKinds(cards: readonly PlacedCard[], tree: boolean): Map<string, CardKind> {
  const dispatchers = new Set(cards.flatMap((card) => (card.entry.relations.dispatcher ? [`${card.deckId}\u0000${card.entry.instance_id}\u0000${card.entry.relations.dispatcher.key}`] : [])));
  return new Map(cards.map((card) => {
    const sent = tree ? CARD_KINDS.find((kind) => kind === card.entry.kind) : undefined;
    const orchestration = card.entry.relations.orchestration;
    const kind: CardKind = sent ?? (orchestration?.is_orchestrator ? "orchestrator" : dispatchers.has(placedCardKey(card)) ? "dispatcher" : orchestration ? "role" : "agent");
    return [placedCardKey(card), kind];
  }));
}

/** The agent's type in the words the dashboard's filter uses; one this build does not know, as it came. */
export function agentTypeText(agentType: string | null): string | undefined {
  if (!agentType) return undefined;
  return DASHBOARD_AGENT_TYPES.find((type) => type.id === agentType)?.label ?? agentType.replace(/_/g, " ");
}

/** Where each client remembers which cards are open. */
const CARDS_OPEN_STORAGE_KEY = "dot-agent-deck.cards.open";

/** How many open cards are remembered at most; the newest are kept. */
const MAX_REMEMBERED_OPEN = 500;

/** The longest placed key remembered; a longer one is not a key this app wrote. */
const MAX_OPEN_KEY_LENGTH = 512;

/** The most stored text read back: room for the newest keys at their longest, escaped. */
const MAX_STORED_OPEN_CHARS = MAX_REMEMBERED_OPEN * (MAX_OPEN_KEY_LENGTH * 6 + 3) + 2;

/** `deck\0instance\0key`: a deck and a key that are not empty, an instance that may be. */
const OPEN_KEY_SHAPE = /^[^\u0000]+\u0000[^\u0000]*\u0000[^\u0000]+$/;

function isOpenCardKey(value: unknown): value is string {
  return typeof value === "string" && value.length <= MAX_OPEN_KEY_LENGTH && OPEN_KEY_SHAPE.test(value);
}

/** At most the newest {@link MAX_REMEMBERED_OPEN} of `open`, in the order they were opened. */
export function boundOpenCards(open: ReadonlySet<string>): ReadonlySet<string> {
  return open.size <= MAX_REMEMBERED_OPEN ? open : new Set([...open].slice(-MAX_REMEMBERED_OPEN));
}

/**
 * The remembered open cards (placed keys); none when nothing usable is stored
 * or storage is unavailable. What is read is bounded as what is written is:
 * stored text longer than any this app writes is ignored whole, an entry that
 * is not a placed key of a sane length is dropped, and only the newest
 * {@link MAX_REMEMBERED_OPEN} are kept.
 */
export function readOpenCards(): Set<string> {
  try {
    const raw = window.localStorage.getItem(CARDS_OPEN_STORAGE_KEY);
    if (raw === null || raw.length > MAX_STORED_OPEN_CHARS) return new Set();
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return new Set();
    return new Set(parsed.filter(isOpenCardKey).slice(-MAX_REMEMBERED_OPEN));
  } catch {
    return new Set();
  }
}

/** Remember `open`; a storage that refuses the write keeps the choice for this session only. */
export function writeOpenCards(open: ReadonlySet<string>): void {
  try {
    window.localStorage.setItem(CARDS_OPEN_STORAGE_KEY, JSON.stringify([...open].filter(isOpenCardKey).slice(-MAX_REMEMBERED_OPEN)));
  } catch {
    // Unavailable or full storage: the open cards still apply until the window closes.
  }
}

/**
 * The open cards worth remembering: for a deck whose daemon answered with its
 * cards (`answering`, by deck id — an answer with no cards included), only
 * those it still lists (a card that closed, or a daemon that was replaced,
 * drops them); a deck that did not answer keeps what it had, since it may only
 * be disconnected for now. `listed` is every answering deck's placed cards.
 */
export function pruneOpenCards(open: ReadonlySet<string>, listed: readonly PlacedCard[], answering: ReadonlySet<string>): ReadonlySet<string> {
  const keys = new Set(listed.map(placedCardKey));
  const kept = [...open].filter((key) => !answering.has(key.split("\u0000")[0]) || keys.has(key));
  return kept.length === open.size ? open : new Set(kept);
}

/** What a reported reference shows: a checked badge, or a short text with the reason in its tooltip. */
export type ReferenceView =
  | { kind: "pull_request"; status: "checked" | "stale"; pullRequest: AgentPullRequest; title: string }
  | { kind: "issue"; status: "checked" | "stale"; number: number; state: string; url?: string; title: string }
  | { kind: string; status: "pending" | "unavailable"; text: string; title: string };

/** Why the deck could not check a reference, in a few words. */
const UNAVAILABLE_TEXT: Record<string, string> = {
  no_github_access: "no GitHub access",
  not_found: "not found or private",
  rate_limited: "GitHub rate limit",
  unrecognised: "not a GitHub reference",
  error: "check failed",
};

/** …and in a sentence, for the tooltip. */
const UNAVAILABLE_SENTENCE: Record<string, string> = {
  no_github_access: "the deck's host has no working GitHub login",
  not_found: "GitHub found nothing by that number that the deck's host can see; it may be private",
  rate_limited: "GitHub's rate limit stopped the check; the deck tries again later",
  unrecognised: "it is not a GitHub pull request or issue",
  error: "the check failed",
};

const KIND_WORD: Record<string, string> = { pull_request: "Pull request", issue: "Issue" };

/** A reference's short name: `#123`, or what the agent wrote. */
function referenceName(ref: AgentCardCheckedRef): string {
  return ref.number !== undefined ? `#${ref.number}` : ref.claim;
}

/**
 * Whether `value` is a github.com issue page this app opens: exactly
 * `https://github.com/<owner>/<repo>/issues/<number>`, the number 1 or more
 * with no leading zero, and at most a trailing `/` after it. No other segment,
 * no query and no fragment; no login and no port other than the default (an
 * explicit `:443` is the same address, and URL parsing drops it). The same
 * rule as `pr_browser::is_issue_url`.
 */
export function isIssueUrl(value: string): boolean {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.protocol !== "https:" || url.hostname.toLowerCase() !== "github.com" || url.port !== "" || url.username !== "" || url.password !== "") return false;
  if (url.search !== "" || url.hash !== "" || value.includes("?") || value.includes("#")) return false;
  return /^\/[^/]+\/[^/]+\/issues\/[1-9][0-9]*\/?$/.test(url.pathname);
}

/** The address a checked reference opens, when it is a github.com pull request or issue page. */
export function referenceUrl(ref: Pick<AgentCardCheckedRef, "url">): string | undefined {
  return ref.url !== undefined && (isPullRequestUrl(ref.url) || isIssueUrl(ref.url)) ? ref.url : undefined;
}

/**
 * Issue #2418 — whether the deck's check of a reference lets it open: checked
 * (a stale check included), with its number and a github.com page. What the
 * card's badge, voice and the shell each ask before opening it.
 */
export function openableReference(ref: AgentCardCheckedRef): ref is AgentCardCheckedRef & { number: number; url: string } {
  return ref.result.status === "checked" && ref.number !== undefined && referenceUrl(ref) !== undefined;
}

function clockOf(at: string): string {
  const parsed = new Date(at);
  return Number.isNaN(parsed.getTime()) ? at : parsed.toLocaleString();
}

/** How one reported reference is drawn, from the deck's check of it. */
export function referenceView(ref: AgentCardCheckedRef): ReferenceView {
  const kindWord = KIND_WORD[ref.kind] ?? ref.kind.replace(/_/g, " ");
  const name = referenceName(ref);
  const result = ref.result;
  if (result.status === "checked" && ref.number !== undefined && typeof result.checked_at === "string") {
    const stale = result.stale === true;
    const when = clockOf(result.checked_at);
    const freshness = stale ? `Last checked ${when}; the latest check failed, so this may be out of date.` : `Checked by the deck ${when}.`;
    const title = `${kindWord} ${name}${typeof result.title === "string" ? `: ${result.title}` : ""}. ${freshness} The agent said: ${ref.claim}`;
    const state = typeof result.state === "string" ? result.state : "unknown";
    if (ref.kind === "pull_request") {
      const dto = { number: ref.number, url: ref.url, state, review: result.review, checks: result.ci as PullRequestChecksDto | undefined } as unknown as PullRequestInfoDto;
      const pullRequest = pullRequestFromDto(dto);
      if (pullRequest) return { kind: "pull_request", status: stale ? "stale" : "checked", pullRequest, title };
    } else {
      return { kind: "issue", status: stale ? "stale" : "checked", number: ref.number, state, url: referenceUrl(ref), title };
    }
  }
  if (result.status === "pending") {
    return { kind: ref.kind, status: "pending", text: `${name} · checking…`, title: `The deck has not looked ${name} up yet.` };
  }
  const reason = result.status === "unavailable" && typeof result.reason === "string" ? result.reason : "error";
  const detail = result.status === "unavailable" && typeof result.detail === "string" ? result.detail : undefined;
  const sentence = UNAVAILABLE_SENTENCE[reason] ?? reason.replace(/_/g, " ");
  return {
    kind: ref.kind,
    status: "unavailable",
    text: `${name} · ${UNAVAILABLE_TEXT[reason] ?? reason.replace(/_/g, " ")}`,
    title: `${kindWord} "${ref.claim}" was not checked: ${sentence}.${detail ? ` (${detail})` : ""}`,
  };
}

/**
 * Issue #2418 review — one reported reference in words, for the agent
 * pane's Card tab's list of every reference: what the deck's check found (a pull
 * request's state, review and CI, an issue's state, and the title GitHub
 * gave), how fresh that check is, or why there is no check yet or at all; and
 * what the agent wrote. Agent-written and GitHub-supplied text is returned as
 * received, for the caller to bound and render as text.
 */
export interface ReferenceDetail {
  /** `Pull request #24180: merged, approved; CI: …` or `Issue #3: closed`; absent when the deck has no check of it. */
  checked?: string;
  /** The title GitHub gave it, when checked. */
  title?: string;
  /** When it was checked, that the check is out of date, that it is still to be checked, or why it cannot be. */
  note: string;
  /** What the agent reported, exactly. */
  claim: string;
}

export function referenceDetail(ref: AgentCardCheckedRef): ReferenceDetail {
  const view = referenceView(ref);
  const result = ref.result;
  if ((view.status === "checked" || view.status === "stale") && result.status === "checked" && typeof result.checked_at === "string") {
    const when = clockOf(result.checked_at);
    const note = view.status === "stale" ? `Last checked ${when}; the latest check failed, so this may be out of date.` : `Checked by the deck ${when}.`;
    const checked = "pullRequest" in view ? pullRequestLabel(view.pullRequest) : "number" in view ? `Issue #${view.number}: ${view.state}` : undefined;
    return { ...(checked ? { checked } : {}), ...(typeof result.title === "string" ? { title: result.title } : {}), note, claim: ref.claim };
  }
  return { note: view.title, claim: ref.claim };
}

/** Issue #2418 (D13) — one issue label as a card draws it: its name, and its colour only when that is six hex digits. */
export interface IssueLabelView {
  name: string;
  /** Six lowercase hex digits, no `#`; absent for a label drawn neutral. */
  color?: string;
}

/**
 * Issue #2418 (D13; round 2, R2-A2) — the most labels a card keeps for one
 * issue, and the most characters of a label's name: the daemon's own caps
 * (`MAX_LABELS`, `MAX_LABEL_NAME_CHARS` in `src/reference_check.rs`).
 */
const MAX_ISSUE_LABELS = 20;
const MAX_ISSUE_LABEL_NAME_CHARS = 50;
/** How many entries of a `labels` array are looked at, unusable ones included, before the rest are ignored. */
const MAX_ISSUE_LABEL_ENTRIES_SCANNED = MAX_ISSUE_LABELS * 5;

/**
 * Issue #2418 (D13) — a checked issue's labels, in the daemon's order. The
 * daemon already bounds and validates them; this checks again, since they are
 * repository text and the desktop does not rely on a daemon it may not have
 * built (round 2, R2-A2): at most `MAX_ISSUE_LABEL_ENTRIES_SCANNED` entries
 * are looked at, a label without a usable name is skipped, a name is cut to
 * `MAX_ISSUE_LABEL_NAME_CHARS` characters, at most `MAX_ISSUE_LABELS` are
 * kept, and a colour that is not exactly six hex digits is dropped, so the
 * label is drawn neutral. A pull request, or a reference not (or no longer
 * freshly) checked, has none here — a stale check keeps the labels it last
 * read.
 */
export function issueLabels(ref: AgentCardCheckedRef): IssueLabelView[] {
  if (ref.kind !== "issue" || ref.result.status !== "checked") return [];
  const labels = (ref.result as { labels?: unknown }).labels;
  if (!Array.isArray(labels)) return [];
  const kept: IssueLabelView[] = [];
  const scanned = Math.min(labels.length, MAX_ISSUE_LABEL_ENTRIES_SCANNED);
  for (let index = 0; index < scanned && kept.length < MAX_ISSUE_LABELS; index += 1) {
    const label: unknown = labels[index];
    if (typeof label !== "object" || label === null) continue;
    const { name, color } = label as { name?: unknown; color?: unknown };
    if (typeof name !== "string") continue;
    const cut = cutLabelName(name);
    if (cut === "") continue;
    kept.push({ name: cut, ...(typeof color === "string" && /^[0-9a-fA-F]{6}$/.test(color) ? { color: color.toLowerCase() } : {}) });
  }
  return kept;
}

/** A label's name, trimmed and cut to `MAX_ISSUE_LABEL_NAME_CHARS` characters, without reading past what that needs. */
function cutLabelName(name: string): string {
  // A character is at most two UTF-16 units, so four times the limit in
  // units holds at least twice the limit in characters: enough to cut at the
  // limit without splitting a very long name into characters first.
  const start = name.length - name.trimStart().length;
  const head = name.slice(start, start + MAX_ISSUE_LABEL_NAME_CHARS * 4);
  return Array.from(head).slice(0, MAX_ISSUE_LABEL_NAME_CHARS).join("").trimEnd();
}

/**
 * PR #2491 decision 2026-11-25 — Needs you only in the Table view: the cards
 * whose rows it keeps, by placed key — the cards the daemon marked as needing
 * the user and every card above them in their daemon's tree, so a row that
 * needs the user is shown with the rows of the agents it sits under. The same
 * path the Cards view keeps bright ({@link needsYouPath}); `cards` is every
 * card on screen across the daemons, and each daemon's tree is built apart.
 */
export function needsYouRowKeys(cards: readonly PlacedCard[], treeFor: (deckId: string) => boolean): Set<string> {
  const kept = new Set<string>();
  const deckIds = [...new Set(cards.map((card) => card.deckId))];
  for (const deckId of deckIds) {
    for (const key of needsYouPath(cardTree(cards.filter((card) => card.deckId === deckId), treeFor(deckId))).path) kept.add(key);
  }
  return kept;
}
