import { useSyncExternalStore } from "react";
import type { AgentStatus, AgentTab, AgentTypeId } from "../types";
import { modeScopedKey, statusFromDaemon, type DesktopAgentDto } from "./bridge";

/**
 * Issue #1496 — the agent dashboard's filter: which agents it shows.
 *
 * Five facets. An agent is shown when it matches EVERY active facet; within a
 * multi-select facet the values are alternatives. An empty list, or blank
 * text, leaves that facet unrestricted, so the empty filter shows everything.
 *
 * The filter reads the facts the dashboard already has for each agent — its
 * tab, the daemon's own status word, its type, its daemon and the names it is
 * shown by — and keeps no list of agents of its own: it decides which of the
 * agents it is handed stay, in the order they were handed, as the same objects.
 */
export interface DashboardFilter {
  kinds: DashboardKind[];
  statuses: DashboardStatus[];
  agentTypes: AgentTypeId[];
  /** Daemon ids — `AgentSession.daemonId`, the deck's own id. */
  daemonIds: string[];
  text: string;
}

/**
 * What kind of agent a row is: an orchestration role, one of the three
 * authoring kinds (dispatcher, schedule, schedule-issues), or a single agent.
 * The authoring kinds share their ids with the daemon's
 * `AgentRecord.authoring_kind`; `schedule-issues` is not `schedule`.
 */
export type DashboardKind = "orchestration" | "single" | "dispatcher" | "schedule" | "schedule-issues";

/**
 * The daemon's own status words the filter offers. They are kept apart here
 * even though the dashboard's status column merges Working with Thinking and
 * Idle with Waiting for input, because "show only agents waiting for input" is
 * not a request for idle ones.
 */
export type DashboardStatus = "working" | "thinking" | "waiting_for_input" | "idle" | "blocked" | "error";

export type DashboardFilterFacet = keyof DashboardFilter;

/** The facts an agent is filtered on, in the daemon's vocabulary. */
export type DashboardFilterFacts = Pick<DesktopAgentDto, "id" | "displayName" | "cwd" | "lastUserPrompt" | "authoringKind"> & {
  tab: AgentTab;
  daemonId: string;
  status?: DesktopAgentDto["status"];
  agentType?: AgentTypeId;
};

/** In the order the filter menu lists them, with the label the filter line uses. */
export const DASHBOARD_KINDS: readonly { id: DashboardKind; label: string; mode?: string }[] = [
  { id: "orchestration", label: "Orchestration roles" },
  { id: "single", label: "Single agents" },
  { id: "dispatcher", label: "Dispatchers", mode: "dispatcher" },
  { id: "schedule", label: "Schedule", mode: "schedule" },
  { id: "schedule-issues", label: "Schedule: issues", mode: "schedule: issues" },
];

export const DASHBOARD_STATUSES: readonly { id: DashboardStatus; label: string }[] = [
  { id: "working", label: "Working" },
  { id: "thinking", label: "Thinking" },
  { id: "waiting_for_input", label: "Waiting for input" },
  { id: "idle", label: "Idle" },
  { id: "blocked", label: "Blocked" },
  { id: "error", label: "Error" },
];

export const DASHBOARD_AGENT_TYPES: readonly { id: AgentTypeId; label: string }[] = [
  { id: "claude_code", label: "Claude Code" },
  { id: "codex", label: "Codex" },
  { id: "open_code", label: "OpenCode" },
  { id: "pi", label: "Pi" },
  { id: "devin", label: "Devin" },
];

/** A fresh filter that shows everything. */
export function clearDashboardFilter(): DashboardFilter {
  return { kinds: [], statuses: [], agentTypes: [], daemonIds: [], text: "" };
}

/** Whether any facet restricts the dashboard. */
export function dashboardFilterActive(filter: DashboardFilter): boolean {
  return activeFacets(filter).length > 0;
}

/** `filter` with `facet` alone reset; the other facets keep restricting. */
export function removeDashboardFilterFacet(filter: DashboardFilter, facet: DashboardFilterFacet): DashboardFilter {
  return { ...filter, [facet]: facet === "text" ? "" : [] };
}

/**
 * The kind of `agent`, from the facts in order: an orchestration role; else the
 * authoring kind the daemon recorded when it accepted the start, which is how
 * a dispatcher or schedule agent started by any current client arrives — an
 * ordinary dashboard pane; else the name of a legacy mode tab, which only an
 * older TUI sends; else a single agent.
 */
function kindOf(agent: DashboardFilterFacts): DashboardKind | undefined {
  const tab = agent.tab;
  if (tab.kind === "orchestration") return "orchestration";
  const authoring = DASHBOARD_KINDS.find((kind) => kind.id === agent.authoringKind);
  if (authoring) return authoring.id;
  if (tab.kind === "mode") {
    const mode = normalize(tab.name);
    return DASHBOARD_KINDS.find((kind) => kind.mode === mode)?.id;
  }
  return "single";
}

/** The filter status for each status column, for a daemon word the filter does not offer by name. */
const COLUMN_FILTER_STATUS: Partial<Record<AgentStatus, DashboardStatus>> = {
  running: "working",
  waiting: "idle",
  failed: "error",
  blocked: "blocked",
};

/**
 * The filter status an agent's status word counts as. A word the filter offers
 * is itself, in any case; any other — `running` for an agent with no hook
 * state yet, `compacting`, `unknown`, a word a newer daemon adds — counts as
 * the status of the column the dashboard shows it in, so a filter never
 * leaves out an agent its column shows.
 */
export function dashboardStatusOf(status: string | undefined): DashboardStatus | undefined {
  if (status === undefined) return undefined;
  const word = normalize(status);
  return DASHBOARD_STATUSES.find((option) => option.id === word)?.id ?? COLUMN_FILTER_STATUS[statusFromDaemon(word)];
}

function normalize(text: string): string {
  return text.trim().toLowerCase();
}

/** The facts the text facet searches: the label, the role, the orchestration, the directory and the last prompt. */
function searchable(agent: DashboardFilterFacts): string[] {
  const tab = agent.tab;
  return [
    agent.displayName,
    tab.kind === "orchestration" ? tab.roleName : undefined,
    tab.kind === "orchestration" ? tab.name : undefined,
    tab.kind === "orchestration" ? tab.displayTitle : undefined,
    agent.cwd,
    agent.lastUserPrompt,
  ].filter((fact): fact is string => typeof fact === "string" && fact.length > 0);
}

/** Whether `agent` matches every active facet of `filter`. */
export function matchesDashboardFilter(agent: DashboardFilterFacts, filter: DashboardFilter): boolean {
  if (filter.kinds.length) {
    const kind = kindOf(agent);
    if (!kind || !filter.kinds.includes(kind)) return false;
  }
  if (filter.statuses.length) {
    const status = dashboardStatusOf(agent.status);
    if (!status || !filter.statuses.includes(status)) return false;
  }
  if (filter.agentTypes.length && !filter.agentTypes.some((agentType) => agentType === agent.agentType)) return false;
  if (filter.daemonIds.length && !filter.daemonIds.includes(agent.daemonId)) return false;
  const text = normalize(filter.text);
  if (text && !searchable(agent).some((fact) => fact.toLowerCase().includes(text))) return false;
  return true;
}

/**
 * The agents of `agents` that match `filter`, in their order and as the same
 * objects. `factsOf` reads an agent's facts when it is not already shaped as
 * {@link DashboardFilterFacts} — the dashboard's rows, whose `status` is the
 * merged column value.
 */
export function filterDashboardAgents<T extends DashboardFilterFacts>(agents: readonly T[], filter: DashboardFilter): T[];
export function filterDashboardAgents<T>(agents: readonly T[], filter: DashboardFilter, factsOf: (agent: T) => DashboardFilterFacts): T[];
export function filterDashboardAgents<T>(agents: readonly T[], filter: DashboardFilter, factsOf?: (agent: T) => DashboardFilterFacts): T[] {
  if (!dashboardFilterActive(filter)) return [...agents];
  const facts = factsOf ?? ((agent: T) => agent as unknown as DashboardFilterFacts);
  return agents.filter((agent) => matchesDashboardFilter(facts(agent), filter));
}

export interface DashboardFilterChip {
  facet: DashboardFilterFacet;
  /** What the filter line says for this facet: "Working", "Dispatchers or Schedule". */
  label: string;
  /** The facet's name in a sentence — the chip's remove button is "Remove {noun} filter". */
  noun: string;
}

export interface DashboardFilterHeader {
  /** "Showing 4 of 11 agents · Working · Dispatchers". */
  summary: string;
  /** Whether Show all is offered: whenever any facet is active. */
  showAll: boolean;
  chips: DashboardFilterChip[];
}

function labelled<T extends string>(ids: readonly T[], options: readonly { id: T; label: string }[]): string {
  return ids.map((id) => options.find((option) => option.id === id)?.label ?? id).join(" or ");
}

function activeFacets(filter: DashboardFilter): DashboardFilterFacet[] {
  const facets: DashboardFilterFacet[] = [];
  if (filter.statuses.length) facets.push("statuses");
  if (filter.kinds.length) facets.push("kinds");
  if (filter.agentTypes.length) facets.push("agentTypes");
  if (filter.daemonIds.length) facets.push("daemonIds");
  if (filter.text.trim()) facets.push("text");
  return facets;
}

/**
 * The dashboard header's statement of the filter. The counts beside it in the
 * header keep describing the whole fleet; this line is the one that says how
 * much of it is showing. Status leads, then kind, type, daemon and text.
 * `daemonName` names a daemon by the label the dashboard shows for it.
 */
export function buildDashboardFilterHeader(filter: DashboardFilter, visibleCount: number, fleetCount: number, daemonName: (id: string) => string = (id) => id): DashboardFilterHeader {
  const chips = activeFacets(filter).map((facet): DashboardFilterChip => {
    switch (facet) {
      case "statuses": return { facet, label: labelled(filter.statuses, DASHBOARD_STATUSES), noun: "status" };
      case "kinds": return { facet, label: labelled(filter.kinds, DASHBOARD_KINDS), noun: "kind" };
      case "agentTypes": return { facet, label: labelled(filter.agentTypes, DASHBOARD_AGENT_TYPES), noun: "agent type" };
      case "daemonIds": return { facet, label: filter.daemonIds.map(daemonName).join(" or "), noun: "daemon" };
      case "text": return { facet, label: `“${filter.text.trim()}”`, noun: "text" };
    }
  });
  const showing = `Showing ${visibleCount} of ${fleetCount} ${fleetCount === 1 ? "agent" : "agents"}`;
  return {
    summary: [showing, ...chips.map((chip) => chip.label)].join(" · "),
    showAll: chips.length > 0,
    chips,
  };
}

/**
 * The key voice gives the Daemon selector's All daemons entry (`voice::ALL_DECKS_ID`
 * in the desktop crate). It is every daemon, so as a dashboard daemon it is no
 * daemon facet: no agent's daemon has that id.
 */
const VOICE_ALL_DAEMONS_ID = "all-daemons";

/** The voice dispatch's facets, by the row's param names (`commands.toml`'s `filter_dashboard`). */
export function dashboardFilterFromParams(params: readonly { name: string; value: string }[]): DashboardFilter {
  const value = (name: string) => params.find((param) => param.name === name)?.value;
  const one = <T extends string>(name: string, options: readonly { id: T }[]): T[] => {
    const found = options.find((option) => option.id === value(name));
    return found ? [found.id] : [];
  };
  const daemon = value("daemon");
  return {
    kinds: one("kind", DASHBOARD_KINDS),
    statuses: one("status", DASHBOARD_STATUSES),
    agentTypes: one("agent_type", DASHBOARD_AGENT_TYPES),
    daemonIds: daemon && daemon !== VOICE_ALL_DAEMONS_ID ? [daemon] : [],
    text: value("text") ?? "",
  };
}

// -- the window session's filter -------------------------------------------

/**
 * The filter lives as long as the window session: kept in `sessionStorage`,
 * so it survives leaving the dashboard and coming back, and a reload of the
 * window, and is gone after an app restart. Where storage cannot be used, it
 * lives in memory for as long as the page does.
 */
const DASHBOARD_FILTER_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.dashboard-filter.v1");

/** The longest search text the filter keeps; anything past it is cut off when stored and when read back. */
export const DASHBOARD_FILTER_TEXT_MAX = 200;

function capped(text: string): string {
  return text.slice(0, DASHBOARD_FILTER_TEXT_MAX);
}

let memory: string | null = null;
/** Set once a write to storage failed, after which `memory` is the filter. */
let storageFailed = false;
let cached: { raw: string | null; filter: DashboardFilter } | undefined;
const listeners = new Set<() => void>();

function readRaw(): string | null {
  if (storageFailed) return memory;
  try {
    return window.sessionStorage.getItem(DASHBOARD_FILTER_STORAGE_KEY);
  } catch {
    return memory;
  }
}

function strings(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : [];
}

/** A stored filter, or the empty one for anything unreadable. */
export function readStoredDashboardFilter(raw: string | null): DashboardFilter {
  if (!raw) return clearDashboardFilter();
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return clearDashboardFilter();
    const stored = parsed as Record<string, unknown>;
    const known = <T extends string>(values: string[], options: readonly { id: T }[]) => values.filter((entry): entry is T => options.some((option) => option.id === entry));
    return {
      kinds: known(strings(stored.kinds), DASHBOARD_KINDS),
      statuses: known(strings(stored.statuses), DASHBOARD_STATUSES),
      agentTypes: known(strings(stored.agentTypes), DASHBOARD_AGENT_TYPES),
      daemonIds: strings(stored.daemonIds),
      text: typeof stored.text === "string" ? capped(stored.text) : "",
    };
  } catch {
    return clearDashboardFilter();
  }
}

/** The window session's filter now. */
export function currentDashboardFilter(): DashboardFilter {
  const raw = readRaw();
  if (cached?.raw !== raw) cached = { raw, filter: readStoredDashboardFilter(raw) };
  return cached.filter;
}

/**
 * Replace the window session's filter, and tell every screen showing it. The
 * search text is kept as typed, so a leading space stays in the box even
 * though the match is on the trimmed text and a blank one restricts nothing.
 */
export function setDashboardFilter(filter: DashboardFilter): void {
  const kept = { ...filter, text: capped(filter.text) };
  const raw = dashboardFilterActive(kept) || kept.text ? JSON.stringify(kept) : null;
  memory = raw;
  try {
    if (raw === null) window.sessionStorage.removeItem(DASHBOARD_FILTER_STORAGE_KEY);
    else window.sessionStorage.setItem(DASHBOARD_FILTER_STORAGE_KEY, raw);
  } catch {
    // Storage is unavailable or full: `memory` holds it for this page instead.
    storageFailed = true;
  }
  for (const listener of listeners) listener();
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/** The window session's filter, re-rendering whenever it changes. */
export function useDashboardFilter(): DashboardFilter {
  return useSyncExternalStore(subscribe, currentDashboardFilter, currentDashboardFilter);
}
