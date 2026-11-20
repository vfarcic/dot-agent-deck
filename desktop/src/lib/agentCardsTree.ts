import type { AgentCardCheckedRef, AgentCardEntry, AgentPullRequest } from "../types";
import type { PullRequestChecksDto, PullRequestInfoDto } from "./bridge";
import { placedCardKey, type PlacedCard } from "./agentCards";
import { DASHBOARD_AGENT_TYPES } from "./dashboardFilter";
import { isPullRequestUrl, pullRequestFromDto } from "./pullRequest";
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
 */
export function cardTree(cards: readonly PlacedCard[], tree: boolean): CardNode[] {
  const nodes = cards.map((card) => ({ card, children: [] as CardNode[] }));
  if (!tree) return nodes;
  const byKey = new Map(nodes.map((node) => [node.card.entry.key, node]));
  const roots: CardNode[] = [];
  for (const node of nodes) {
    const parent = node.card.entry.parent;
    const above = parent && parent !== node.card.entry.key ? byKey.get(parent) : undefined;
    if (above) above.children.push(node);
    else roots.push(node);
  }
  /* A cycle would leave its cards reachable from no root; the daemon links by
     exact identity and forms none, but a card is never dropped for it. */
  const reached = new Set<CardNode>();
  const walk = (node: CardNode) => {
    if (reached.has(node)) return;
    reached.add(node);
    node.children.forEach(walk);
  };
  roots.forEach(walk);
  for (const node of nodes) {
    if (!reached.has(node)) {
      roots.push(node);
      walk(node);
    }
  }
  return roots;
}

/**
 * Needs you only: the cards the daemon marked as needing the user and every
 * card above them (`path`, which stays bright), and the cards to open so each
 * marked card is on screen (`open`: those with a marked card under them).
 */
export function needsYouPath(roots: readonly CardNode[]): { path: Set<string>; open: Set<string> } {
  const path = new Set<string>();
  const open = new Set<string>();
  const visit = (node: CardNode): boolean => {
    let below = false;
    for (const child of node.children) if (visit(child)) below = true;
    if (below) open.add(placedCardKey(node.card));
    if (below || node.card.entry.needs_you) path.add(placedCardKey(node.card));
    return below || node.card.entry.needs_you;
  };
  roots.forEach(visit);
  return { path, open };
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
export function cardStatusText(status: string): string {
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

/** What a card is: one that dispatches others, an orchestration's orchestrator, one of its roles, or a plain agent — read off the daemon's relations. */
export type CardKind = "dispatcher" | "orchestrator" | "role" | "agent";

/**
 * Each card's kind, by placed key. A card is a dispatcher when the daemon names
 * it as the dispatcher of another card; an orchestrator or a role by its
 * orchestration relation.
 */
export function cardKinds(cards: readonly PlacedCard[]): Map<string, CardKind> {
  const dispatchers = new Set(cards.flatMap((card) => (card.entry.relations.dispatcher ? [`${card.deckId}\u0000${card.entry.instance_id}\u0000${card.entry.relations.dispatcher.key}`] : [])));
  return new Map(cards.map((card) => {
    const orchestration = card.entry.relations.orchestration;
    const kind: CardKind = orchestration?.is_orchestrator ? "orchestrator" : dispatchers.has(placedCardKey(card)) ? "dispatcher" : orchestration ? "role" : "agent";
    return [placedCardKey(card), kind];
  }));
}

/** The agent's type in the words the dashboard's filter uses; one this build does not know, as it came. */
export function agentTypeText(agentType: string | null): string | undefined {
  if (!agentType) return undefined;
  return DASHBOARD_AGENT_TYPES.find((type) => type.id === agentType)?.label ?? agentType.replace(/_/g, " ");
}

/** Where each client remembers which cards are open. */
export const CARDS_OPEN_STORAGE_KEY = "dot-agent-deck.cards.open";

/** How many open cards are remembered at most; the newest are kept. */
const MAX_REMEMBERED_OPEN = 500;

/** The remembered open cards (placed keys); none when nothing usable is stored or storage is unavailable. */
export function readOpenCards(): Set<string> {
  try {
    const parsed: unknown = JSON.parse(window.localStorage.getItem(CARDS_OPEN_STORAGE_KEY) ?? "[]");
    return new Set(Array.isArray(parsed) ? parsed.filter((item): item is string => typeof item === "string") : []);
  } catch {
    return new Set();
  }
}

/** Remember `open`; a storage that refuses the write keeps the choice for this session only. */
export function writeOpenCards(open: ReadonlySet<string>): void {
  try {
    window.localStorage.setItem(CARDS_OPEN_STORAGE_KEY, JSON.stringify([...open].slice(-MAX_REMEMBERED_OPEN)));
  } catch {
    // Unavailable or full storage: the open cards still apply until the window closes.
  }
}

/**
 * The open cards worth remembering: for a deck whose cards are on screen, only
 * those its daemon still lists (a card that closed, or a daemon that was
 * replaced, drops them); a deck not on screen keeps what it had, since it may
 * only be disconnected for now. `listed` is every deck's placed cards.
 */
export function pruneOpenCards(open: ReadonlySet<string>, listed: readonly PlacedCard[]): ReadonlySet<string> {
  const decks = new Set(listed.map((card) => card.deckId));
  const keys = new Set(listed.map(placedCardKey));
  const kept = [...open].filter((key) => !decks.has(key.split("\u0000")[0]) || keys.has(key));
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

/** `https://github.com/<owner>/<repo>/issues/<number>` and nothing else — the same rule as `pr_browser::is_issue_url`. */
export function isIssueUrl(value: string): boolean {
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return false;
  }
  if (url.protocol !== "https:" || url.hostname.toLowerCase() !== "github.com" || url.port !== "" || url.username !== "" || url.password !== "") return false;
  const [owner, repo, issues, number] = url.pathname.split("/").slice(1);
  return Boolean(owner) && Boolean(repo) && issues === "issues" && /^[0-9]+$/.test(number ?? "");
}

/** The address a checked reference opens, when it is a github.com pull request or issue page. */
export function referenceUrl(ref: Pick<AgentCardCheckedRef, "url">): string | undefined {
  return ref.url !== undefined && (isPullRequestUrl(ref.url) || isIssueUrl(ref.url)) ? ref.url : undefined;
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
