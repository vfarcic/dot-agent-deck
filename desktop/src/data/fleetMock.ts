/*
 * PRD 2557 milestone 1 — MOCK-UP DATA, never merged.
 *
 * The fleet view mock-up scene (`/?fixture=1&state=docs&cards=fleet-mock&mock=<state>`):
 *
 * - `fleetMock.live.json` is this box's live deck, captured read-only by
 *   `scripts/fleet-mock-capture.sh` at render time: `daemon status`, `card
 *   list` and the GitHub states of the pull requests and issues the
 *   dispatcher's view names. The Cards view of the scene is drawn from it.
 * - `DISPATCHER_VIEW` is the dispatcher's own composition (2026-11-27 ~13:00
 *   UTC), word for word, written as the A2UI v1.0 messages it would publish:
 *   a flat component list against the deck's small fleet catalog.
 */
import type { AgentCardCheckedRef, AgentCardEntry, AgentCardsSnapshot } from "../types";
import LIVE from "./fleetMock.live.json";
import DISPATCHER from "./fleetMock.dispatcher.json";

export type FleetMockState = "first-open" | "composed" | "unpredicted" | "delivered" | "safe-to-close" | "stale";
export const FLEET_MOCK_STATES: readonly FleetMockState[] = ["first-open", "composed", "unpredicted", "delivered", "safe-to-close", "stale"];

export function fleetMockState(): FleetMockState | undefined {
  if (typeof window === "undefined") return undefined;
  const params = new URLSearchParams(window.location.search);
  if (params.get("cards") !== "fleet-mock") return undefined;
  const mock = params.get("mock") as FleetMockState | null;
  return mock && FLEET_MOCK_STATES.includes(mock) ? mock : "composed";
}

export function fleetMockView(): "fleet" | "cards" {
  if (typeof window === "undefined") return "cards";
  return new URLSearchParams(window.location.search).get("view") === "fleet" ? "fleet" : "cards";
}

export interface LivePullRequest {
  number: number; title: string; state: string; review: string; mergeable: string; labels: string[]; author: string;
  mergedAt: string | null; mergeCommit: string | null;
  ci: { state: string; passing: number; failing: number; pending: number; skipped: number }; failing: string[];
}
export interface LiveIssue { number: number; title: string; state: string; labels: string[] }
interface LiveStatusRow { PANE: string; AGENT: string; STATUS: string; TOOL: string; WAKE: string; LABEL: string; CWD: string }
interface LiveDoc {
  captured_at: string;
  daemon_status: LiveStatusRow[];
  cards: { instance_id: string; revision: number; entries: AgentCardEntry[] };
  pull_requests: Record<string, LivePullRequest>;
  issues: Record<string, LiveIssue>;
  main_run: { databaseId: number; conclusion: string; headSha: string };
}
export const LIVE_DECK = LIVE as unknown as LiveDoc;

/** The instant the scene's clock is frozen at: when the live data was captured. */
export const FLEET_MOCK_NOW = LIVE_DECK.captured_at;

/** The wall clock, HH:MM UTC, `minutes` after the capture. */
export function clockAt(minutes = 0, at = FLEET_MOCK_NOW): string {
  const date = new Date(new Date(at).getTime() + minutes * 60_000);
  return date.toISOString().slice(11, 16);
}

const DISPATCHER_KEY = LIVE_DECK.cards.entries.find((entry) => entry.kind === "dispatcher")?.key ?? "dispatcher";

/* ------------------------------------------------------------------------ */
/* The deck's fleet catalog, and the dispatcher's composition in A2UI v1.0. */
/* ------------------------------------------------------------------------ */

/**
 * The catalog the deck would register (A2UI `catalogId`). Basic-catalog names
 * where the basic component fits as is (Text, Column, Row, List, Card); three
 * deck components where it does not: `PromptButton` (label and sent text are
 * one field, so a button can only send what it shows), `DeckRef` (the agent
 * names a pull request or issue; the deck fills in its state) and `AgentRef`
 * (the agent names an agent; the deck fills in status and its own actions).
 */
export const FLEET_CATALOG_ID = "https://agent-deck.devopstoolkit.ai/a2ui/fleet/v1/catalog.json";
export const FLEET_CATALOG = ["Text", "Column", "Row", "List", "Card", "PromptButton", "DeckRef", "AgentRef", "DeckAction"] as const;

export type A2uiComponent = { id: string; component: string } & Record<string, unknown>;
export type A2uiMessage =
  | { version: "1.0"; createSurface: { surfaceId: string; catalogId: string } }
  | { version: "1.0"; updateComponents: { surfaceId: string; components: A2uiComponent[] } }
  | { version: "1.0"; updateDataModel: { surfaceId: string; path: string; value: unknown } };

interface Item {
  id: string;
  title: string;
  text: string;
  refs: { kind: "pull_request" | "issue"; number: number }[];
  prompts: string[];
}

/* The dispatcher's composition, word for word (2026-11-27 ~13:00 UTC). Kept as JSON: the palette check reads a four-digit `#NNNN` in .ts as a colour. */
const NEEDS_YOU = DISPATCHER.needs_you as Item[];
const SAFE_TO_CLOSE = DISPATCHER.safe_to_close;
const WORKING = DISPATCHER.working;

function agentKey(name: string): string {
  return LIVE_DECK.cards.entries.find((entry) => entry.name === name)?.key ?? name;
}

/** The dispatcher's composition as the A2UI v1.0 messages it would publish into the view. */
export function dispatcherView(): A2uiMessage[] {
  const surfaceId = "fleet";
  const components: A2uiComponent[] = [
    { id: "root", component: "Column", children: ["needs", "close", "working"] },
    { id: "needs", component: "List", title: "Needs you", children: NEEDS_YOU.map((item) => item.id) },
  ];
  for (const item of NEEDS_YOU) {
    const refIds = item.refs.map((ref) => `${item.id}-${ref.kind}-${ref.number}`);
    const promptIds = item.prompts.map((_, at) => `${item.id}-p${at}`);
    components.push(
      { id: item.id, component: "Card", child: `${item.id}-col` },
      { id: `${item.id}-col`, component: "Column", children: [`${item.id}-title`, ...(refIds.length ? [`${item.id}-refs`] : []), `${item.id}-text`, `${item.id}-actions`] },
      { id: `${item.id}-title`, component: "Text", variant: "h4", text: item.title },
      { id: `${item.id}-text`, component: "Text", variant: "body", text: item.text },
      { id: `${item.id}-actions`, component: "Row", children: promptIds },
    );
    if (refIds.length) components.push({ id: `${item.id}-refs`, component: "Row", children: refIds });
    item.refs.forEach((ref, at) => components.push({ id: refIds[at], component: "DeckRef", kind: ref.kind, repo: "vfarcic/dot-agent-deck", number: ref.number }));
    item.prompts.forEach((prompt, at) => components.push({ id: promptIds[at], component: "PromptButton", agent: DISPATCHER_KEY, text: prompt }));
  }
  components.push(
    { id: "close", component: "List", title: "Safe to close", children: SAFE_TO_CLOSE.map((_, at) => `c${at}`), action: "close-all" },
    { id: "close-all", component: "DeckAction", operation: "close", agents: SAFE_TO_CLOSE.map((entry) => agentKey(entry.agent)) },
    ...SAFE_TO_CLOSE.map((entry, at) => ({ id: `c${at}`, component: "AgentRef", agent: agentKey(entry.agent), text: entry.text })),
    { id: "working", component: "List", title: "Working", collapsed: true, children: WORKING.map((_, at) => `w${at}`) },
    ...WORKING.map((entry, at) => ({ id: `w${at}`, component: "AgentRef", agent: agentKey(entry.agent), text: entry.text })),
  );
  return [
    { version: "1.0", createSurface: { surfaceId, catalogId: FLEET_CATALOG_ID } },
    { version: "1.0", updateComponents: { surfaceId, components } },
    { version: "1.0", updateDataModel: { surfaceId, path: "/composedAt", value: "2026-11-27T13:00:00Z" } },
  ];
}

/* ------------------------------------------------------------------------ */
/* The scene's moment, per state.                                            */
/* ------------------------------------------------------------------------ */

/** The button the `delivered` state shows pressed. */
export const DELIVERED = { item: "n3", prompt: NEEDS_YOU.find((item) => item.id === "n3")!.prompts[0], atMinutes: 2 } as const;

/** How the `stale` state's dispatcher went quiet: busy since `busyMinutes` before the capture moment it is drawn at. */
export const STALE = { shiftMinutes: 25, lastAnsweredMinutes: 2, askedMinutes: 20 } as const;

function shifted(minutes: number): string {
  return new Date(new Date(FLEET_MOCK_NOW).getTime() + minutes * 60_000).toISOString();
}

/** The live cards, as the deck sends them, adjusted to the state's moment (only the dispatcher's own status changes). */
export function fleetMockCards(deckId: string, state: FleetMockState): AgentCardsSnapshot {
  const entries = LIVE_DECK.cards.entries.map((entry): AgentCardEntry => {
    if (entry.kind !== "dispatcher") return { ...entry };
    if (state === "delivered" || state === "stale" || state === "first-open") return { ...entry, status: "working", updated_at: shifted(state === "stale" ? STALE.shiftMinutes : 0) };
    return { ...entry };
  });
  return { deckId, instanceId: LIVE_DECK.cards.instance_id, revision: LIVE_DECK.cards.revision, entries, catalogErrors: [], tree: true, batch: true };
}

/** The scene's clock for a state: the capture, moved on for the states that happen later. */
export function fleetMockNow(state: FleetMockState): string {
  if (state === "delivered") return shifted(DELIVERED.atMinutes);
  if (state === "stale") return shifted(STALE.shiftMinutes);
  return FLEET_MOCK_NOW;
}

/** The deck's own check of a reference, built from the live GitHub state (what the daemon's lookup would hold). */
export function deckCheck(kind: "pull_request" | "issue", number: number): AgentCardCheckedRef {
  const url = `https://github.com/vfarcic/dot-agent-deck/${kind === "pull_request" ? "pull" : "issues"}/${number}`;
  const base = { field: kind === "pull_request" ? "pull-request" : "issue", kind, claim: `#${number}`, repo: "vfarcic/dot-agent-deck", number, url };
  if (kind === "pull_request") {
    const pr = LIVE_DECK.pull_requests[String(number)];
    if (!pr) return { ...base, result: { status: "pending" } };
    return { ...base, result: { status: "checked", state: pr.state, ci: pr.ci as never, review: pr.review, title: pr.title, checked_at: FLEET_MOCK_NOW, stale: false } };
  }
  const issue = LIVE_DECK.issues[String(number)];
  if (!issue) return { ...base, result: { status: "pending" } };
  return { ...base, result: { status: "checked", state: issue.state, title: issue.title, checked_at: FLEET_MOCK_NOW, stale: false } };
}
