import { useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { Blocks, Boxes, Columns3, LayoutList, Layers, Network, RefreshCw, RotateCcw, ShieldAlert, Sparkles, SquareTerminal, Wrench } from "lucide-react";
import type { AgentSession, AgentStatus, ConnectionView, DeckRuntimeState, DeckView } from "../types";
import { modeScopedKey } from "../lib/bridge";
import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";
import { DISPLAY_LIMITS, displayActivity, displayIdentity, displayPath, displayText, displayTitle, displayUptime, domIdentity, rendersBlank } from "../lib/displayText";

/**
 * The honest subset of `AgentSession`: every field a daemon genuinely reports
 * AND the overview actually renders, and nothing else. The overview renders
 * from THIS and never from `AgentSession`, so reaching for a value the daemon
 * cannot supply — `model`, `tokens`, `cost`, `contextPercent`, `worktree`,
 * `attempt`, `duration` — is a compile error rather than a thing to remember.
 * `duration` stays out while `lastActivityMs` and `spawnedAtMs` are in, and the
 * line between them is honesty rather than taste. `duration` is a fixture
 * string; the two instants are daemon observations that read ABSENT when the
 * daemon cannot vouch for them (PRD #745 M9, M11). Note the screen DOES show a
 * duration now — it is computed here from `spawnedAtMs`, the instant the daemon
 * forked the process, rather than from `SessionState.started_at`, which is
 * event-derived and invented as `now` on hydration.
 * `role` is deliberately absent even though it is honest: a row shows
 * `displayName` and, inside an orchestration, `tab.roleName`, so carrying
 * `role` here would claim a consumption that does not exist. The field-by-field
 * reasoning lives on `AgentSession` itself; PRD #745's "Columns" table is the
 * decision.
 */
export type OverviewAgent = Pick<
  AgentSession,
  "id" | "daemonId" | "displayName" | "cli" | "status" | "activeTool" | "activeToolDetail" | "toolCount" | "tab" | "lastUserPrompt" | "lastActivityMs" | "spawnedAtMs"
> & {
  /**
   * HONEST, and optional exactly as `AgentSession.cwd` is. It was optional here
   * FIRST, because the `Pick<>` above closes dishonest field NAMES but cannot
   * close a dishonest sentinel inside an allowed one: `agentFromDto` used to
   * substitute `UNREPORTED` for a `cwd` the daemon did not report, so the
   * screen's guarantee that nothing on it reads "Unavailable" was violable
   * through this very field, and this boundary reversed the substitution. The
   * M8 audit found the reversal erasing a REAL directory called "Unavailable",
   * so absence now travels as absence the whole way and there is nothing left
   * to reverse. Rendered as nothing at all — not as a placeholder, and not as a
   * dash, which would be one more thing to read that says less than blank.
   */
  cwd?: string;
  /**
   * HONEST as of M8, and now the ONLY field whose deck-side sentinel is
   * reversed here rather than carried onto the screen: `AgentSession.writeLease`
   * says `"unknown"` when the daemon declared no live target. Unlike the `cwd`
   * sentinel it retired, this one cannot collide with daemon data — the desktop
   * crate emits only the three mapped strings or omits the key — so reversing
   * it can only ever remove a placeholder, never a fact.
   */
  writeLease?: "read" | "write" | "none";
};

export function toOverviewAgent(agent: AgentSession): OverviewAgent {
  const { id, daemonId, displayName, cli, status, cwd, activeTool, activeToolDetail, toolCount, tab, lastUserPrompt, lastActivityMs, spawnedAtMs, writeLease } = agent;
  return {
    id,
    daemonId,
    displayName,
    cli,
    status,
    cwd,
    activeTool,
    activeToolDetail,
    toolCount,
    tab,
    lastUserPrompt,
    lastActivityMs,
    spawnedAtMs,
    writeLease: writeLease === "unknown" ? undefined : writeLease,
  };
}

/**
 * Agents are keyed by the composite `(daemonId, agentId)` from day one. Agent
 * ids are per-daemon monotonic integers starting at 1, so two daemons both mint
 * `"1"` and any bare-id key is wrong the moment #742 connects a second daemon.
 * `encodeURIComponent` escapes `:`, so the join stays unambiguous for a daemon
 * id that is itself a socket path.
 *
 * This is built from RAW values, never from the sanitised display copies: two
 * names that differ only in a stripped character must stay two agents.
 */
export function agentKey(agent: Pick<OverviewAgent, "daemonId" | "id">): string {
  return `${encodeURIComponent(agent.daemonId)}:${encodeURIComponent(agent.id)}`;
}

/**
 * The copy of `agentKey` that reaches React — the row's key and its
 * `data-testid`. Identical to `agentKey` for anything a healthy daemon reports,
 * and BOUNDED for anything else: `DesktopAgentDto.id` and `daemonId` carry no
 * frontend clamp at all, so an unbounded copy in a DOM attribute is a way for a
 * malformed daemon to freeze the webview. `agentKey` itself stays raw, because
 * it is the identity two agents must never share.
 */
export function agentDomKey(agent: Pick<OverviewAgent, "daemonId" | "id">): string {
  return domIdentity(agentKey(agent));
}

export type OverviewGroupKind = "orchestration" | "mode" | "standalone";

export interface OverviewGroup {
  /**
   * The group's identity WITHIN its kind, raw and unescaped: an orchestration
   * id, a mode name, or the literal `"standalone"`. Kept raw because it is the
   * daemon-side identity a future drill-in navigates by; it is never a React
   * key and never a DOM id — see `key`.
   *
   * ABSENT for an orchestration whose agents reported no `orchestrationId`,
   * because there is then no daemon-side identity to carry and inventing one is
   * the same class of lie as an `Unavailable` cwd: a drill-in would navigate to
   * a group the daemon has never heard of. Such a group still renders, keyed on
   * itself — see `anonymousOrchestrationKey`.
   */
  id?: string;
  /**
   * Unique across every kind, and safe as an HTML id. Orchestration ids, mode
   * names and the standalone literal shared ONE key space, so a mode named
   * `standalone`, or one whose name equalled an orchestration id, produced
   * duplicate sibling React keys and duplicate `data-testid`s. And an id
   * interpolated raw into `aria-labelledby` breaks the IDREF silently the
   * moment a daemon-supplied name contains a space: the label association just
   * stops working, with no error anywhere. `encodeURIComponent` leaves no
   * whitespace and escapes the `:` separator, so the join stays unambiguous
   * and the result is always a legal id.
   *
   * Bounded, too: it reaches React as a key, a `data-testid`, a DOM `id` and an
   * `aria-labelledby` IDREF, and a daemon-supplied name has no length limit
   * below the protocol frame. Bounding happens HERE and never on the grouping
   * key, so no clamp can merge two groups.
   */
  key: string;
  kind: OverviewGroupKind;
  title: string;
  /** The orchestration's own name, shown only when `title` is a display title. */
  subtitle?: string;
  /**
   * The working directory MOST of this group's members share, set only when at
   * least two of them share it. The group states it once and those rows stay
   * blank, which turns the column into a differences column: what a row prints
   * is what makes it unlike its neighbours. A group whose members all differ
   * has no common value and prints every row; so does a group of one, which
   * needs no hoist because there is no repetition to remove.
   */
  commonCwd?: string;
  /**
   * The orchestration TAB's own directory, as the daemon states it on every
   * role pane's membership (`AgentTab.cwd`, PRD #745 M8) — not derived from the
   * members the way `commonCwd` is, and preferred over it when both exist. The
   * two differ in kind: a stated tab cwd is true of the orchestration even when
   * no two role panes share a per-pane directory, and it is what a role pane
   * that sits somewhere else is a difference FROM. Only orchestration groups
   * ever have one, and only when the daemon reported it.
   */
  orchestrationCwd?: string;
  agents: OverviewAgent[];
}

/**
 * The one directory a group states in its header. Rows print their own only
 * when they differ from this, so the per-row column stays a differences column
 * whichever of the two sources answered.
 */
export function hoistedCwdOf(group: Pick<OverviewGroup, "orchestrationCwd" | "commonCwd">): string | undefined {
  return group.orchestrationCwd ?? group.commonCwd;
}

/**
 * Groups by the daemon's own tab buckets: one group per orchestration with its
 * roles in `roleIndex` order, one per mode name, and one standalone bucket for
 * dashboard panes. Standalone leads, because the TUI opens on the dashboard tab
 * and always keeps it first — an overview that buried the same agents at the
 * bottom would be describing a different deck than the one next to it.
 * Orchestrations follow, then modes, each in first-appearance order.
 */
export function groupAgents(agents: OverviewAgent[]): OverviewGroup[] {
  const orchestrations = new Map<string, OverviewGroup>();
  const modes = new Map<string, OverviewGroup>();
  const standalone: OverviewAgent[] = [];

  for (const agent of agents) {
    if (agent.tab.kind === "orchestration") {
      // `orchestrationId` is optional on the wire. Falling back to the name
      // would merge two distinct orchestrations that happen to share one into a
      // single card with colliding role indexes, so an id-less agent keys on
      // itself instead: worst case it reads as its own group, which is honest,
      // rather than as somebody else's role. The two cases are kept in
      // DISJOINT key spaces — an explicit id can never reach the anonymous one,
      // whatever string a daemon reports — because they used to share one.
      const id = agent.tab.orchestrationId;
      const bucket = id === undefined ? `self:${agentKey(agent)}` : `id:${id}`;
      const group = orchestrations.get(bucket) ?? {
        id,
        key: id === undefined ? anonymousOrchestrationKey(agent) : groupKey("orchestration", id),
        kind: "orchestration" as const,
        title: agent.tab.displayTitle || agent.tab.name,
        subtitle: agent.tab.displayTitle ? agent.tab.name : undefined,
        agents: [],
      };
      group.agents.push(agent);
      orchestrations.set(bucket, group);
    } else if (agent.tab.kind === "mode") {
      const group = modes.get(agent.tab.name) ?? { id: agent.tab.name, key: groupKey("mode", agent.tab.name), kind: "mode" as const, title: agent.tab.name, agents: [] };
      group.agents.push(agent);
      modes.set(agent.tab.name, group);
    } else {
      standalone.push(agent);
    }
  }

  for (const group of orchestrations.values()) {
    group.agents.sort((left, right) => roleIndexOf(left) - roleIndexOf(right));
  }

  const groups = [
    ...(standalone.length ? [{ id: "standalone", key: groupKey("standalone", "standalone"), kind: "standalone" as const, title: "Standalone agents", agents: standalone }] : []),
    ...orchestrations.values(),
    ...modes.values(),
  ];
  for (const group of groups) {
    group.orchestrationCwd = statedOrchestrationCwdOf(group.agents);
    group.commonCwd = commonCwdOf(group.agents);
  }
  return groups;
}

/**
 * A group's key: its kind and its raw identity, escaped and bounded. See
 * `OverviewGroup.key` for why all three are load-bearing.
 *
 * Note the daemon's PRE-ID orchestration identity is `(name, cwd)`, so a
 * name-only fallback would merge two unrelated orchestrations. `groupAgents`
 * has no such fallback — an id-less orchestration agent keys on itself — and
 * anything that reintroduces one has to carry cwd with the name.
 */
export function groupKey(kind: OverviewGroupKind, id: string): string {
  return domIdentity(`${kind}:${encodeURIComponent(id)}`);
}

/**
 * The `orchestration` prefix `groupKey` emits, and the one it CANNOT emit.
 * They differ at the fourteenth character — `:` against `-` — and
 * `encodeURIComponent` never produces either from an id, so no orchestration
 * id a daemon reports can land in the anonymous space or vice versa.
 */
const ANONYMOUS_ORCHESTRATION_PREFIX = "orchestration-anonymous:";

/**
 * The key of an orchestration whose agents reported no `orchestrationId`.
 *
 * This used to be `agent:<agentKey>` fed back through the ordinary key space,
 * with a comment claiming the result was "unique to itself". It was not: a
 * different orchestration reporting that exact string as its EXPLICIT
 * `orchestrationId` landed in the same map entry, so the two rendered as one
 * card whose title came from whichever arrived first, with colliding role
 * indexes and a misleading agent count — and both halves of the string are
 * knowable, since it is built from a socket path and a daemon-minted agent id.
 * Kind-namespacing did not close it, because the collision happened in the map
 * before `groupKey` was ever applied.
 */
export function anonymousOrchestrationKey(agent: Pick<OverviewAgent, "daemonId" | "id">): string {
  return domIdentity(`${ANONYMOUS_ORCHESTRATION_PREFIX}${agentKey(agent)}`);
}

/**
 * The working directory the largest number of members share, or `undefined`
 * when no two of them share one. Ties go to whichever appears first, so the
 * answer does not depend on map iteration order.
 */
/**
 * The orchestration cwd the daemon stated on this group's memberships. Every
 * role pane of one tab carries the same value, so the first one that reports it
 * answers for the group; a group with no orchestration members has none.
 */
function statedOrchestrationCwdOf(agents: OverviewAgent[]): string | undefined {
  for (const agent of agents) {
    if (agent.tab.kind === "orchestration" && agent.tab.cwd) return agent.tab.cwd;
  }
  return undefined;
}

function commonCwdOf(agents: OverviewAgent[]): string | undefined {
  const counts = new Map<string, number>();
  for (const agent of agents) {
    if (agent.cwd) counts.set(agent.cwd, (counts.get(agent.cwd) ?? 0) + 1);
  }
  let best: string | undefined;
  let bestCount = 1;
  for (const [cwd, count] of counts) {
    if (count > bestCount) {
      best = cwd;
      bestCount = count;
    }
  }
  return best;
}

function roleIndexOf(agent: OverviewAgent): number {
  return agent.tab.kind === "orchestration" ? agent.tab.roleIndex : 0;
}

/** Statuses in the order an operator scans them: what needs attention first. */
const STATUS_ORDER: AgentStatus[] = ["running", "waiting", "failed", "queued", "passed", "stopped"];

export function countByStatus(agents: OverviewAgent[]): { status: AgentStatus; count: number }[] {
  return STATUS_ORDER
    .map((status) => ({ status, count: agents.filter((agent) => agent.status === status).length }))
    .filter((entry) => entry.count > 0);
}

const GROUP_KICKER: Record<OverviewGroupKind, string> = {
  orchestration: "ORCHESTRATION",
  mode: "MODE TAB",
  standalone: "NO TAB",
};

const GROUP_ICON: Record<OverviewGroupKind, typeof Network> = {
  orchestration: Network,
  mode: Layers,
  standalone: Boxes,
};

/**
 * Every column this screen can show, in grid order, **keyed by the
 * `OverviewAgent` field it renders** (PRD #745 M12).
 *
 * The key is the honesty guarantee, not a naming convention. `OverviewAgent` is
 * a `Pick<>` of the fields a daemon genuinely reports, so `satisfies readonly
 * (keyof OverviewAgent)[]` makes a column named `model`, `cost`, `tokens`,
 * `contextPercent`, `worktree`, `attempt` or `duration` a **compile error** —
 * the picker below derives its options from this list and therefore inherits
 * the guarantee, and can never offer a column the daemon cannot fill. That is
 * the whole reason the ids are field names rather than free-form slugs.
 *
 * The order here is the order columns appear on screen, whatever order the user
 * selected them in: a stored set is rendered through {@link orderedColumns},
 * so the layout is a property of the screen rather than of a click sequence.
 * Uptime sits immediately after Last activity because the two temporal columns
 * answer one question between them — how long this agent has been alive, and
 * how much of that it has spent doing nothing — and reading them apart is the
 * point of putting them together.
 *
 * **`status` is ONE column, not the two this screen used to carry.** There was
 * a coloured status mark in the first track and a textual State column in the
 * third, both rendered from `agent.status` and neither saying anything the
 * other did not. Two picker entries for one field would have been a question
 * with no right answer, so the mark and the label now share one cell: the
 * colour is still the thing you scan and the word is still the thing you read.
 * Row-level status signalling is unaffected either way — `data-status` on the
 * `<tr>` is what tints a failed row, and it is not a column.
 */
const COLUMN_FIELDS = [
  "status",
  "displayName",
  "lastActivityMs",
  "spawnedAtMs",
  "cli",
  "activeTool",
  "toolCount",
  "cwd",
  "lastUserPrompt",
] as const satisfies readonly (keyof OverviewAgent)[];

export type OverviewColumnId = (typeof COLUMN_FIELDS)[number];

/** Every column, in grid order — the picker's option list. */
export const ALL_OVERVIEW_COLUMNS: readonly OverviewColumnId[] = COLUMN_FIELDS;

interface OverviewColumnSpec {
  /** The `<th scope="col">` a screen reader announces, and the picker's label. */
  label: string;
  /** The visible legend's own short form. */
  legend: string;
  /**
   * This column's grid track. Every flexible track carries a fixed px MINIMUM
   * rather than `minmax(0, …)`, which is what makes the table region
   * horizontally scrollable at all: the grid's min-content width is then the
   * sum of these minimums, so `min-width: min-content` on the scroll track
   * gives a real overflow instead of columns crushed to nothing. A `minmax(0,
   * …)` track would shrink to zero and the scrollbar would never appear.
   */
  track: string;
}

const OVERVIEW_COLUMNS: Record<OverviewColumnId, OverviewColumnSpec> = {
  status: { label: "Status", legend: "STATUS", track: "82px" },
  displayName: { label: "Agent", legend: "AGENT", track: "minmax(150px, 1.3fr)" },
  lastActivityMs: { label: "Last activity", legend: "LAST ACTIVITY", track: "58px" },
  spawnedAtMs: { label: "Uptime", legend: "UPTIME", track: "48px" },
  cli: { label: "CLI", legend: "CLI", track: "78px" },
  activeTool: { label: "Active tool", legend: "ACTIVE TOOL", track: "minmax(130px, 1.3fr)" },
  toolCount: { label: "Tools", legend: "TOOLS", track: "40px" },
  cwd: { label: "Working directory", legend: "WORKING DIRECTORY", track: "minmax(130px, 1.3fr)" },
  lastUserPrompt: { label: "Last prompt", legend: "LAST PROMPT", track: "minmax(150px, 1.7fr)" },
};

/**
 * The one column that cannot be removed. A row with no name is not a shorter
 * row, it is an anonymous one — on the screen whose whole job is telling agents
 * apart — so the picker renders its checkbox permanently checked and disabled,
 * and {@link orderedColumns} puts it back whatever a stored value says.
 */
export const PERMANENT_COLUMN: OverviewColumnId = "displayName";

/**
 * What the screen shows before anyone chooses: name, status, working directory
 * and uptime — who it is, whether it is healthy, where it is working, and how
 * long it has been at it. Everything else is one click away and remembered.
 */
export const DEFAULT_OVERVIEW_COLUMNS: readonly OverviewColumnId[] = ["status", "displayName", "spawnedAtMs", "cwd"];

/**
 * A chosen set, rendered in grid order and never without the permanent column.
 * Selection is a SET and the layout is a property of this list, so the columns
 * do not reshuffle according to the order somebody happened to tick them.
 */
export function orderedColumns(selected: Iterable<OverviewColumnId>): OverviewColumnId[] {
  const chosen = new Set(selected);
  chosen.add(PERMANENT_COLUMN);
  return COLUMN_FIELDS.filter((column) => chosen.has(column));
}

/** The `grid-template-columns` a selection produces — one template, shared. */
export function gridTemplateFor(columns: readonly OverviewColumnId[]): string {
  return columns.map((column) => OVERVIEW_COLUMNS[column].track).join(" ");
}

function isOverviewColumnId(value: unknown): value is OverviewColumnId {
  return typeof value === "string" && (COLUMN_FIELDS as readonly string[]).includes(value);
}

/**
 * Where the column choice is remembered. Mode-scoped for the reason every
 * persisted key on this app is (`modeScopedKey`): a fixture visit must not
 * hand live mode a layout, and a live layout must not follow you into the
 * demo data.
 */
export const OVERVIEW_COLUMNS_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.overview-columns.v1");

/**
 * The columns a stored value asks for, or the defaults when it cannot ask for
 * anything usable.
 *
 * Everything here is about a value written by an OLDER build of this app, which
 * is the case that breaks silently a release later rather than loudly today:
 *
 * - **Absent, unparseable, or not the shape we wrote** — including a bare
 *   string, a number, or an object with no `columns` array — falls back to the
 *   defaults. There is nothing to salvage and a thrown `SyntaxError` on mount
 *   would take the whole screen down.
 * - **A column that no longer exists is DROPPED**, not carried through. A
 *   renamed or retired id would otherwise render as a `<th>` with no cell under
 *   it and one dead grid track down every card — the kind of fault that looks
 *   like a layout bug and is really a migration one.
 * - **Nothing recognisable left** — every stored id unknown, or an empty array
 *   — is indistinguishable from garbage, so it takes the defaults rather than
 *   collapsing the screen to the single permanent column. A user cannot reach
 *   that state by unticking, because the permanent column has no checkbox to
 *   untick.
 *
 * Whatever survives goes through {@link orderedColumns}, so the permanent
 * column is present and the order is the screen's rather than the file's.
 */
export function readStoredColumns(raw: string | null): OverviewColumnId[] {
  let stored: unknown;
  try {
    stored = JSON.parse(raw ?? "null");
  } catch {
    return [...DEFAULT_OVERVIEW_COLUMNS];
  }
  const columns = (stored as { columns?: unknown } | null | undefined)?.columns;
  if (!Array.isArray(columns)) return [...DEFAULT_OVERVIEW_COLUMNS];
  const known = columns.filter(isOverviewColumnId);
  if (!known.length) return [...DEFAULT_OVERVIEW_COLUMNS];
  return orderedColumns(known);
}

/**
 * How often the screen re-reads the clock so its two relative-time columns keep
 * counting between daemon events (PRD #745 M12).
 *
 * **Ten seconds, and the number follows from what the columns print.** Both
 * `displayActivity` and `displayUptime` render one unit, largest that fits,
 * floored — so above a minute the smallest change either cell can make is a
 * whole minute, and below it the label is a fixed `just now` / `<1m`. The only
 * moment a faster tick buys anything is the crossing INTO the first minute
 * bucket, and 10s bounds the lag on that crossing to a tenth of the unit being
 * crossed while costing six repaints a minute of a screen that mounts no
 * terminal. A one-second tick would be sixty repaints a minute to change a
 * label on one of them.
 *
 * It is a re-render and NOTHING else: no snapshot request, no reconnect, no
 * daemon call. The screen's whole design property is one RPC plus one already
 * open event stream (M7), and a polling loop here would quietly reinstate the
 * per-agent cost that milestone removed.
 */
export const OVERVIEW_CLOCK_TICK_MS = 10_000;

/**
 * The instant the overview's relative cells are measured against, re-read on an
 * interval so they keep counting while no snapshot arrives.
 *
 * This exists because both time columns were FROZEN between daemon events.
 * Snapshots are event-driven off the daemon watch stream and nothing polls, so
 * an agent emitting nothing produced no re-render: `Last activity` stayed at
 * `3m ago` while the truth became `30m ago`, and `Uptime` — a duration, and so
 * inherently continuous — stopped counting altogether. It failed exactly
 * backwards, because any OTHER agent's event repaints the whole screen: a busy
 * fleet masked it, and the idle case, where "quiet for two hours" is the most
 * valuable thing on the screen, is precisely where it stopped updating.
 *
 * One clock for the whole screen rather than one `Date.now()` per row, so every
 * cell on a repaint is relative to the SAME moment rather than to fifteen
 * slightly different ones.
 *
 * **Paused while the document is hidden, and it catches up on the way back.**
 * A backgrounded window repainting six times a minute forever is pure waste,
 * and browsers throttle the timer anyway — but a paused ticker means the cells
 * are stale by however long the window was away, so becoming visible re-reads
 * the clock IMMEDIATELY and only then restarts the interval. Waiting a whole
 * interval before correcting would show a time that is knowably wrong at the
 * one moment the user is looking straight at it.
 */
export function useOverviewClock(intervalMs: number = OVERVIEW_CLOCK_TICK_MS): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    let timer: number | undefined;
    const read = () => setNow(Date.now());
    const start = () => {
      if (timer === undefined) timer = window.setInterval(read, intervalMs);
    };
    const stop = () => {
      if (timer === undefined) return;
      window.clearInterval(timer);
      timer = undefined;
    };
    const onVisibilityChange = () => {
      if (document.visibilityState === "hidden") {
        stop();
        return;
      }
      read();
      start();
    };
    if (document.visibilityState !== "hidden") start();
    document.addEventListener("visibilitychange", onVisibilityChange);
    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibilityChange);
    };
  }, [intervalMs]);
  return now;
}

/** What a reported write lease says on hover, in the daemon's own terms. */
const WRITE_LEASE_TITLE: Record<"read" | "write" | "none", string> = {
  write: "The daemon holds a live, writable target for this agent — input typed on the deck reaches it.",
  read: "History-only: the daemon can replay this session but cannot deliver input to it.",
  none: "View-only: the daemon has no handle it can write to or resume.",
};

/**
 * The fleet at a glance: every agent the desktop can see, grouped the way the
 * daemon groups them, described only by things that are actually true — and
 * with no terminal anywhere on it. "Shows no output" and "opens no PTY" are
 * separate properties (PRD #745); this component owns the first, and mounting
 * no `TerminalViewport` is what its tests assert.
 *
 * Every daemon-supplied string on this screen goes through `lib/displayText`
 * before it reaches React — rendered text, `title` attributes, and the identity
 * values behind `data-*`, DOM ids, IDREFs and React keys alike — while
 * grouping, sorting and the `(daemonId, agentId)` identity keep the raw values.
 * The identity path goes through `domIdentity` rather than `displayText`,
 * because bounding is what it needs and truncating a key is not: the raw value
 * stays the key, and only the copy React sees is clamped.
 */
export function AgentOverview({ runtime, onNavigate }: { runtime: DeckRuntimeState; onNavigate: (view: DeckView) => void }) {
  const { snapshot, mode, setShownTerminals } = runtime;
  /**
   * The screen's whole claim, stated to the bridge rather than merely printed in
   * its own header (PRD #745 M7): this screen shows no terminal, so it opens no
   * PTY. Declaring the empty set also flushes the warm set to zero, which is
   * what makes the claim true when you arrive here from a nine-tile deck rather
   * than only on a cold start.
   *
   * What it cannot claim is that every socket a previous screen opened is
   * already gone by the time this renders: the declaration is fire-and-forget,
   * and an attach command still outstanding is cancelled by marking, so its
   * daemon-side tear-down completes afterwards. The copy below says exactly
   * that rather than the stronger thing.
   */
  useEffect(() => {
    void setShownTerminals([]);
  }, [setShownTerminals]);
  const connection = snapshot.connection;
  /**
   * The fleet is only knowable while the daemon is answering. A reconnect
   * failure replaces the connection and KEEPS the previous snapshot's agents
   * (`hooks/useDeckRuntime.ts`), and both the `disconnected` and `error`
   * fixtures ship the default four — so deriving the instruments from
   * `snapshot.agents` unconditionally printed `AGENTS 4 · GROUPS 1` above a
   * body correctly saying the fleet cannot be read. The body suppressed the
   * list and the header contradicted it.
   */
  const connected = connection.status === "connected";
  /*
    Re-read on a timer so the relative cells keep counting between daemon
    events. It re-renders and refetches nothing — see `useOverviewClock`.
  */
  const now = useOverviewClock();
  /**
   * The columns this operator chose, restored from the last visit (PRD #745
   * M12). Read once, through `readStoredColumns`, which is where every way a
   * stored value can be unusable is turned back into the defaults.
   */
  const [columns, setColumns] = useState<OverviewColumnId[]>(() => readStoredColumns(readColumnsStorage()));
  useEffect(() => {
    try {
      window.localStorage.setItem(OVERVIEW_COLUMNS_STORAGE_KEY, JSON.stringify({ columns }));
    } catch {
      // Storage can be unavailable or full. The screen keeps working; the
      // choice simply does not outlive the session, which is the right way
      // round for a display preference.
    }
  }, [columns]);
  const agents = useMemo(() => snapshot.agents.map(toOverviewAgent), [snapshot.agents]);
  const groups = useMemo(() => groupAgents(agents), [agents]);
  const counts = useMemo(() => countByStatus(agents), [agents]);
  const countOf = (status: AgentStatus) => counts.find((entry) => entry.status === status)?.count ?? 0;
  const openDeck = () => onNavigate({ kind: "deck" });
  const socketPath = connection.socketPath;
  const daemonMessage = connection.message ? displayText(connection.message, DISPLAY_LIMITS.message) : undefined;
  /**
   * Whether the connection message says anything the lamp beside it does not
   * (PRD #745). A healthy connection's message is literally `Daemon
   * responding`, which is the lamp restated in words — two renderings of one
   * bit, and the screen narrating its own state.
   *
   * The test is the flag rather than the wording, because the wording is not
   * the point and a string match would rot. There are exactly two ways to be
   * `connected`: the ordinary one, where the desktop crate reported no message
   * at all and the webview synthesised the restatement, and the one where a
   * build-stamp mismatch was bypassed — which is the case whose caveat issue
   * #801 requires to stay on screen for the whole session. `buildStampMismatchOnly`
   * is the same flag Connect anyway is gated on, and the crate sets it on
   * exactly the branch that puts a message in `error`. So a naive "hide when
   * connected" is what this deliberately is not.
   */
  const messageSaysSomethingNew = !connected || connection.buildStampMismatchOnly === true;
  /**
   * Issue #801. Since the crate stopped refusing a daemon that names the same
   * release, the ordinary case is two builds from different commits connecting
   * with nothing on screen — which is the point, but it also means the
   * difference had nowhere left to be seen. A `title` is the whole trace:
   * available on hover, absent from the layout, and deliberately NOT an alert.
   * A real compatibility break still gets the banner and Connect anyway.
   */
  const buildStampsCaveat = connection.clientBuildVersion && connection.daemonBuildVersion
    && connection.clientBuildVersion !== connection.daemonBuildVersion
    ? `Built from different commits — desktop ${connection.clientBuildVersion}, daemon ${connection.daemonBuildVersion}.`
    : undefined;
  /**
   * Everything hover can say about WHICH daemon this is: its socket path, and
   * the two build stamps when they differ.
   *
   * The socket path used to be on screen, shortened to its last segment — a
   * label whose stated purpose was keeping a uid or a username out of
   * screenshots, and which on the default socket reads
   * `dot-agent-deck-attach-501.sock`, so it leaked the very uid it was meant to
   * hide and told the reader nothing actionable either way. The path is
   * genuinely diagnostic, so it stays here, where it costs no layout;
   * `data-daemon-id` on the section still carries the identity for tests and a
   * future drill-in.
   *
   * The stamps hover moved here with it, and had to: it used to hang off the
   * connection message, which for a healthy connection no longer renders. On
   * the daemon's own name it is more discoverable than it was — a reader hovers
   * a thing they can see.
   */
  const daemonFacts = [socketPath, buildStampsCaveat].filter((fact): fact is string => Boolean(fact));
  // Joined rather than stacked: a sanitised `title` cannot carry a newline —
  // `displayText` strips category `Cc`, and `\n` is in it.
  const daemonIdentityTitle = daemonFacts.length ? displayTitle(daemonFacts.join(" · ")) : undefined;
  const [confirm, setConfirm] = useState<ConfirmState>();
  const [overrideError, setOverrideError] = useState<string>();
  /**
   * Issue #801. Daemon LIFECYCLE actions still live on the deck — this starts,
   * stops and replaces nothing. It relaxes this app's own build-stamp
   * comparison for this session, which is a judgement about what the user is
   * looking at, so it belongs on the screen that is refusing to show it. Gated
   * on `buildStampMismatchOnly`, so a genuine protocol mismatch never offers
   * it.
   */
  const requestConnectAnyway = () => {
    if (mode !== "live" || !connection.buildStampMismatchOnly) return;
    setConfirm({
      title: "Connect to a differently-built daemon?",
      body: "The wire protocol matched on both sides, so this daemon and this app agree on the shape of everything they exchange. They were built from different commits, and a stamp difference can still mean divergent behaviour behind an identical wire — a field whose meaning changed while its shape did not. Agent Deck will connect and keep the mismatch on screen for the rest of this session; nothing is remembered after you quit the app.",
      label: "Connect anyway",
      busyLabel: "Connecting…",
      action: async () => {
        setOverrideError(undefined);
        try {
          await runtime.runAction({ type: "allow_build_mismatch" });
          // The allowance is read by the NEXT handshake, so the reconnect is
          // what actually connects; the crate caches no verdict.
          await runtime.reconnect();
        } catch (cause) {
          setOverrideError(cause instanceof Error ? cause.message : String(cause));
        }
      },
    });
  };

  return (
    <div className="control-deck overview-screen">
      <aside className="rail" aria-label="Primary navigation">
        <div className="brand-mark" aria-label="Agent Deck"><span>AD</span><i aria-hidden="true" /></div>
        <nav>
          <OverviewRailButton icon={SquareTerminal} label="Deck" onClick={openDeck} testId="open-deck" />
          <OverviewRailButton icon={LayoutList} label="Overview" active onClick={() => onNavigate({ kind: "overview" })} testId="open-overview" />
        </nav>
        <div className="rail-bottom">
          <span className={`connection-lamp connection-${connection.status}`} title={daemonMessage} />
        </div>
      </aside>

      <main className="deck-main">
        <header className="topbar">
          <div className="repo-context">
            <div className="repo-line"><LayoutList size={15} /><strong>Agent overview</strong></div>
          </div>
          <div className="run-instruments">
            <OverviewInstrument label="AGENTS" testId="overview-count-agents"><OverviewCount known={connected} value={agents.length} /></OverviewInstrument>
            <OverviewInstrument label="RUNNING" testId="overview-count-running"><OverviewCount known={connected} value={countOf("running")} className="count-running" /></OverviewInstrument>
            <OverviewInstrument label="WAITING" testId="overview-count-waiting"><OverviewCount known={connected} value={countOf("waiting")} className="count-waiting" /></OverviewInstrument>
            <OverviewInstrument label="FAILED" testId="overview-count-failed"><OverviewCount known={connected} value={countOf("failed")} className="count-failed" /></OverviewInstrument>
            <OverviewInstrument label="GROUPS" testId="overview-count-groups"><OverviewCount known={connected} value={groups.length} /></OverviewInstrument>
          </div>
          <div className="top-actions">
            <button className="button secondary compact" data-testid="overview-open-deck" onClick={openDeck}><SquareTerminal size={14} /><span>Open deck</span></button>
            <OverviewColumnPicker columns={columns} onChange={setColumns} />
            <button className="button secondary compact" data-testid="overview-refresh" onClick={() => void runtime.reconnect()}><RefreshCw size={14} /><span>Refresh</span></button>
          </div>
        </header>

        {mode === "fixture" && (
          <div className="fixture-bar">
            <span><Sparkles size={13} /> DEMO DATA</span>
            <p>Deterministic fixture · no daemon is attached and no agent is running.</p>
          </div>
        )}

        <section className="overview-body" aria-label="Agent overview">
          {/*
            The daemon group is the OUTER unit even though there is exactly one
            daemon. With one it is minimal chrome; #742's second daemon becomes a
            sibling here and changes no inner component.
          */}
          <section className="daemon-group" data-testid="daemon-group" data-daemon-id={socketPath === undefined ? "" : domIdentity(socketPath)} aria-labelledby="daemon-group-title">
            <header className="daemon-group-header">
              <span className={`connection-lamp connection-${connection.status}`} aria-hidden="true" />
              <div className="daemon-identity">
                {/*
                  The socket filename is gone from the layout and lives on
                  hover — see `daemonIdentityTitle`, which is also where the
                  build stamps disclose themselves.
                */}
                <strong id="daemon-group-title" title={daemonIdentityTitle} data-testid="daemon-identity">Local daemon</strong>
              </div>
              {/*
                Only when it says something the lamp does not. The element is
                not deleted — in the disconnected, incompatible and
                connected-anyway states it carries the only explanation on the
                header, including the build-mismatch caveat issue #801 requires
                to survive the whole session.
              */}
              {messageSaysSomethingNew && <p className="daemon-state" data-testid="daemon-state">{daemonMessage ?? connection.status}</p>}
              {connected && agents.length > 0 && (
                <div className="daemon-pips">{counts.map((entry) => (
                  <span className={`status-label status-${entry.status}`} key={entry.status}>{entry.count} {entry.status}</span>
                ))}</div>
              )}
            </header>

            <div className="daemon-group-body">
              <DaemonBody
                agents={agents}
                groups={groups}
                now={now}
                columns={columns}
                connection={connection}
                message={daemonMessage}
                overrideError={overrideError}
                onOpenDeck={openDeck}
                onReconnect={() => void runtime.reconnect()}
                onConnectAnyway={mode === "live" && connection.buildStampMismatchOnly ? requestConnectAnyway : undefined}
              />
            </div>
          </section>
        </section>
      </main>
      {confirm && <ConfirmDialog state={confirm} onClose={() => setConfirm(undefined)} />}
    </div>
  );
}

function DaemonBody({ agents, groups, now, columns, connection, message, overrideError, onOpenDeck, onReconnect, onConnectAnyway }: {
  agents: OverviewAgent[];
  groups: OverviewGroup[];
  /** The one instant every relative cell on this screen is measured against. */
  now: number;
  /** The chosen columns, in grid order — one list for every card. */
  columns: OverviewColumnId[];
  connection: ConnectionView;
  message?: string;
  overrideError?: string;
  onOpenDeck: () => void;
  onReconnect: () => void;
  /** Absent unless the mismatch is stamp-only — see `requestConnectAnyway`. */
  onConnectAnyway?: () => void;
}) {
  if (connection.status === "loading") {
    return (
      <OverviewNote testId="overview-loading" icon={<RefreshCw className="spin" size={24} />} title="Establishing control channel">
        <p>Reading the daemon's agent list. Nothing is attached while this runs.</p>
      </OverviewNote>
    );
  }

  if (connection.status === "disconnected") {
    return (
      <OverviewNote testId="overview-disconnected" icon={<ShieldAlert size={24} />} title="Daemon disconnected">
        <p>{message ?? "No dot-agent-deck daemon is listening on the configured socket."}</p>
        <p className="overview-note-hint">Nothing can be said about the fleet until a daemon answers, so this list is blank rather than stale. Start a daemon from the deck, then reconnect.</p>
        <div>
          <button className="button secondary" onClick={onOpenDeck}><SquareTerminal size={14} /> Open deck</button>
          <button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button>
        </div>
      </OverviewNote>
    );
  }

  if (connection.status === "error") {
    return (
      <OverviewNote testId="overview-incompatible" icon={<ShieldAlert size={24} />} title="Incompatible daemon">
        <p>{message ?? "A daemon answered but this build cannot speak to it."}</p>
        <p className="overview-note-hint">
          {connection.runningAgentCount === undefined
            ? "A daemon answered the handshake, but this build cannot read its agent list. Nothing is listed rather than guessed."
            : `A daemon answered the handshake and reports ${connection.runningAgentCount} running ${connection.runningAgentCount === 1 ? "agent" : "agents"}, but this build cannot read them. Nothing is listed rather than guessed.`}
          {" "}Daemon lifecycle actions live on the deck.
          {onConnectAnyway && " Only the build stamps differ — the wire protocol agreed — so you can connect to this daemon as it is."}
        </p>
        {overrideError && <p className="overview-note-hint" data-testid="overview-connect-anyway-error">{overrideError}</p>}
        <div>
          <button className="button secondary" onClick={onOpenDeck}><SquareTerminal size={14} /> Open deck</button>
          {onConnectAnyway && <button className="button primary" data-testid="overview-connect-anyway" onClick={onConnectAnyway}><ShieldAlert size={14} /> Connect anyway</button>}
          <button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button>
        </div>
      </OverviewNote>
    );
  }

  if (!agents.length) {
    return (
      <OverviewNote testId="overview-first-run" icon={<Blocks size={26} />} title="No agents are running yet">
        <p>The daemon is healthy and owns nothing. This is what a fresh install looks like — not a failure.</p>
        <p className="overview-note-hint">Launch a workflow from the deck's Workflows panel, or start an agent from the CLI in a project directory. Whatever the daemon adopts shows up here on the next snapshot.</p>
        <div>
          <button className="button primary" onClick={onOpenDeck}><SquareTerminal size={14} /> Open deck</button>
        </div>
      </OverviewNote>
    );
  }

  return (
    /*
      ONE scroll region for the legend and every group card together, and that
      is a correctness property rather than a layout preference (PRD #745 M12).
      All the cards share one `grid-template-columns`, which is what makes the
      whole fleet read as a single table across card boundaries; scrolling them
      individually would let two cards sit at different horizontal offsets and
      the columns would stop lining up — visibly, and only once the chosen set
      is wide enough to overflow, which is exactly when a reader is relying on
      the alignment. The template is published once here as a custom property
      and inherited by the legend and every row inside.
    */
    <div className="overview-table-region" data-testid="overview-table-region">
      <div className="overview-table-track" style={{ "--overview-grid": gridTemplateFor(columns) } as CSSProperties}>
        {/*
          Decoration, not structure: one legend for the whole fleet so the group
          cards read as one table. The header association a screen reader needs
          comes from each group's own `<thead>`, which is visually hidden rather
          than repeated four times down the page.
        */}
        <div className="overview-legend" aria-hidden="true">
          {columns.map((column) => <span key={column}>{OVERVIEW_COLUMNS[column].legend}</span>)}
        </div>
        <div className="overview-groups">
          {groups.map((group) => <OverviewGroupCard key={group.key} group={group} now={now} columns={columns} />)}
        </div>
      </div>
    </div>
  );
}

/**
 * The column picker: what the screen shows, chosen from what the daemon
 * reports and nothing else (PRD #745 M12).
 *
 * The options come from {@link ALL_OVERVIEW_COLUMNS}, whose ids are
 * `OverviewAgent` field names — so this list cannot offer `model`, `cost` or
 * `tokens` even by mistake, because those are not fields of `OverviewAgent` and
 * naming one would not compile. The picker inherits the screen's honesty
 * guarantee rather than restating it.
 *
 * It lives in the top bar, so choosing columns never leaves the overview.
 */
function OverviewColumnPicker({ columns, onChange }: { columns: OverviewColumnId[]; onChange: (columns: OverviewColumnId[]) => void }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const chosen = new Set(columns);
  const toggle = (column: OverviewColumnId) => {
    const next = new Set(chosen);
    if (next.has(column)) next.delete(column);
    else next.add(column);
    onChange(orderedColumns(next));
  };
  /*
    Dismiss on a click anywhere else (PRD #745). Three details are the whole
    thing:

    `pointerdown`, not `click`. A click fires after focus has already moved, so
    a menu that closes on it closes AFTER whatever was clicked has taken focus
    — the ordering a user reads as the menu lagging behind them. Pointer-down is
    the moment the intent is expressed.

    Anything inside the picker's root is ignored, and the TRIGGER lives inside
    that root. That is what stops the close-then-reopen: were the trigger
    outside, its pointer-down would close the menu and its click would toggle it
    straight back open, so the button would appear not to work at all.

    The listener is bound only while the menu is open and removed when it
    closes, not merely on unmount — `open` is in the dependency list, so React
    runs the cleanup on the same transition that hides the menu.
  */
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      setOpen(false);
    };
    document.addEventListener("pointerdown", dismiss);
    return () => document.removeEventListener("pointerdown", dismiss);
  }, [open]);
  return (
    <div
      className="overview-columns-picker"
      ref={root}
      onKeyDown={(event) => {
        if (event.key !== "Escape") return;
        setOpen(false);
        event.stopPropagation();
      }}
    >
      <button
        className="button secondary compact"
        data-testid="overview-columns-toggle"
        aria-expanded={open}
        aria-haspopup="true"
        title="Choose which columns this screen shows. Your choice is remembered."
        onClick={() => setOpen((wasOpen) => !wasOpen)}
      >
        <Columns3 size={14} /><span>Columns</span>
      </button>
      {open && (
        <div className="overview-columns-menu" data-testid="overview-columns-menu" role="group" aria-label="Columns">
          <p>Every column the daemon reports. There is nothing else to show.</p>
          {ALL_OVERVIEW_COLUMNS.map((column) => {
            const permanent = column === PERMANENT_COLUMN;
            return (
              <label key={column} className={permanent ? "is-permanent" : undefined}>
                <input
                  type="checkbox"
                  data-testid={`overview-column-${column}`}
                  checked={chosen.has(column)}
                  disabled={permanent}
                  onChange={() => toggle(column)}
                />
                <span>{OVERVIEW_COLUMNS[column].label}</span>
                {permanent && <em title="A row with no name is not a shorter row, it is an anonymous one.">always</em>}
              </label>
            );
          })}
          {/*
            The way back (PRD #745). Without it, the only route out of a set
            somebody unticked their way into is remembering which four the
            screen opened on. It persists through exactly the path every other
            change does — `onChange` up to the screen, whose effect writes the
            selection — so there is no second way for a choice to be saved.
          */}
          <button
            type="button"
            className="overview-columns-reset"
            data-testid="overview-columns-reset"
            onClick={() => onChange(orderedColumns(DEFAULT_OVERVIEW_COLUMNS))}
          >
            <RotateCcw size={12} /><span>Restore defaults</span>
          </button>
        </div>
      )}
    </div>
  );
}

/** The stored column choice, or `null` when storage cannot be read at all. */
function readColumnsStorage(): string | null {
  try {
    return window.localStorage.getItem(OVERVIEW_COLUMNS_STORAGE_KEY);
  } catch {
    return null;
  }
}

function OverviewGroupCard({ group, now, columns }: { group: OverviewGroup; now: number; columns: OverviewColumnId[] }) {
  const Icon = GROUP_ICON[group.kind];
  const counts = countByStatus(group.agents);
  const titleId = `overview-group-title-${group.key}`;
  const hoistedCwd = hoistedCwdOf(group);
  // Shown only when it says something: a subtitle that renders to nothing is an
  // empty `<code>` chip next to the heading, which reads as a rendering fault.
  const subtitle = group.subtitle ? displayText(group.subtitle, DISPLAY_LIMITS.name) : undefined;
  return (
    <article
      className="overview-group"
      data-testid={`overview-group-${group.key}`}
      data-group-id={group.id === undefined ? undefined : domIdentity(group.id)}
      data-group-kind={group.kind}
      aria-labelledby={titleId}
    >
      <header className="overview-group-header">
        <Icon size={14} aria-hidden="true" />
        <div className="overview-group-identity">
          <span className="section-kicker">{GROUP_KICKER[group.kind]}</span>
          <h3 id={titleId}>{displayIdentity(group.title, DISPLAY_LIMITS.name, unnamedGroupLabel(group))}</h3>
        </div>
        {subtitle && !rendersBlank(subtitle) && <code className="overview-group-subtitle">{subtitle}</code>}
        {hoistedCwd && (
          <code
            className="overview-group-cwd"
            data-cwd-source={group.orchestrationCwd ? "orchestration" : "shared"}
            title={displayText(
              group.orchestrationCwd
                // Stated by the daemon rather than inferred, so the hover says
                // so: this is the orchestration's own directory, and a role
                // pane elsewhere is a genuine difference from it.
                ? `This orchestration runs in ${group.orchestrationCwd} — a row prints its own directory only when it differs`
                : `Most of this group works in ${hoistedCwd} — a row prints its own directory only when it differs`,
              DISPLAY_LIMITS.title,
            )}
          >
            {displayPath(hoistedCwd)}
          </code>
        )}
        <span className="overview-group-count">{group.agents.length} {group.agents.length === 1 ? "agent" : "agents"}</span>
        <div className="overview-group-pips">{counts.map((entry) => (
          <span className={`status-label status-${entry.status}`} key={entry.status}>{entry.count} {entry.status}</span>
        ))}</div>
      </header>
      {/*
        A real `<table>`, because this screen IS a table and `<th scope="col">`
        is how a cell gets its column header. The grid layout is kept by
        `display: grid` on the rows, which strips a table's implicit ARIA
        semantics in browsers — so the roles are restated explicitly rather than
        left to the element names.
      */}
      <table className="overview-table" role="table" aria-labelledby={titleId}>
        <thead className="overview-sr-only" role="rowgroup">
          <tr role="row">
            {columns.map((column) => <th key={column} role="columnheader" scope="col">{OVERVIEW_COLUMNS[column].label}</th>)}
          </tr>
        </thead>
        <tbody className="overview-rows" role="rowgroup">
          {group.agents.map((agent) => <OverviewRow key={agentDomKey(agent)} agent={agent} hoistedCwd={hoistedCwd} now={now} columns={columns} />)}
        </tbody>
      </table>
    </article>
  );
}

/**
 * What an identity cell says when the daemon's own value renders as nothing.
 *
 * Honest — it does not invent a name — and identifying, which is the property
 * that matters: two agents whose names are both invisible still read as two
 * different rows, because the daemon's agent id distinguishes them and is what
 * every other surface (the TUI, the deck, the CLI) calls them by. The voice
 * matches the screen's other absences: "no active tool", "socket path not
 * reported".
 */
function unnamedAgentLabel(agent: OverviewAgent): string {
  const id = displayText(agent.id, DISPLAY_LIMITS.toolName);
  return rendersBlank(id) ? "unnamed agent" : `unnamed agent ${id}`;
}

/** The same, for a group whose title renders as nothing. */
function unnamedGroupLabel(group: OverviewGroup): string {
  const noun = group.kind === "mode" ? "mode tab" : "orchestration";
  const id = group.id === undefined ? "" : displayText(group.id, DISPLAY_LIMITS.toolName);
  return rendersBlank(id) ? `unnamed ${noun}` : `unnamed ${noun} ${id}`;
}

function OverviewRow({ agent, hoistedCwd, now, columns }: { agent: OverviewAgent; hoistedCwd?: string; now: number; columns: OverviewColumnId[] }) {
  const orchestration = agent.tab.kind === "orchestration" ? agent.tab : undefined;
  // Only an orchestration's role name says something the other columns do not.
  // Outside one, the daemon derives the role from the agent type, so showing it
  // here would just restate the CLI column.
  const roleLabel = orchestration?.roleName;
  const roleName = roleLabel ? displayText(roleLabel, DISPLAY_LIMITS.name) : undefined;
  /*
    A display name made ENTIRELY of retained default-ignorable characters
    (`U+200B`, `U+200C`, `U+200D`, `U+FEFF`) renders as a blank cell, and two
    names differing only by one of them render identically — on the one screen
    whose entire purpose is telling agents apart. It is not fixed by stripping
    those characters, which are load-bearing in emoji sequences and in Persian,
    Arabic and Indic orthography and whose retention this seam shares with
    `src/untrusted_text.rs`; it is fixed by saying something visible instead.
  */
  const name = displayIdentity(agent.displayName, DISPLAY_LIMITS.name, unnamedAgentLabel(agent));
  // ONE instant for the whole screen, ticked by `useOverviewClock` so these two
  // cells keep counting between daemon events. Passed down rather than read
  // here so every row on a repaint is relative to the same moment rather than
  // to fifteen slightly different ones.
  const activity = displayActivity(agent.lastActivityMs, now);
  const uptime = displayUptime(agent.spawnedAtMs, now);
  /**
   * One cell, built only if its column is on screen. A `switch` rather than a
   * record of every cell, because two of them sanitise and clamp a string the
   * daemon supplies with no length limit below the protocol frame — building a
   * 64 KiB prompt cell for a column nobody selected would be work done fifteen
   * times a row and repeated on every clock tick. The `never` arm is what makes
   * adding a column to `COLUMN_FIELDS` without a cell a compile error.
   */
  const cell = (column: OverviewColumnId): ReactNode => {
    switch (column) {
      case "status":
        // The mark and the word, in ONE cell: they render the same field, and
        // splitting them across two columns would have made the picker ask a
        // question with no right answer (see `COLUMN_FIELDS`). The mark stays
        // `aria-hidden` — the label beside it is already the readable value.
        return (
          <td className="overview-state" role="cell" key={column}>
            <span className={`agent-state-mark status-${agent.status}`} aria-hidden="true" />
            <span className={`status-label status-${agent.status}`}>{agent.status}</span>
          </td>
        );
      case "displayName":
        return (
          <td className="overview-agent-name" role="cell" key={column}>
            {orchestration && <em className="overview-role-index" title={`Role ${orchestration.roleIndex} of this orchestration`}>{String(orchestration.roleIndex + 1).padStart(2, "0")}</em>}
            <strong>{name}</strong>
            {orchestration?.isStartRole && <span className="coordinator-badge" title="Orchestration start role — the agent an operator messages">COORDINATOR</span>}
            {/*
              The write lease the daemon reported (PRD #745 M8). Shown whenever it
              IS reported, including the ordinary writable case, so the rule a
              reader learns is the simple one — a chip means the daemon said
              something — rather than "no chip means writable, unless it means the
              daemon said nothing". A daemon that declared no live target renders
              nothing here: `toOverviewAgent` reversed its sentinel to absent, and
              absence is NOT read as read-only.
            */}
            {agent.writeLease && <span className={`overview-lease lease-${agent.writeLease}`} title={WRITE_LEASE_TITLE[agent.writeLease]}>{agent.writeLease}</span>}
            {roleName && !rendersBlank(roleName) && roleLabel?.toLowerCase() !== agent.displayName.toLowerCase() && <em className="overview-role-name">{roleName}</em>}
          </td>
        );
      case "lastActivityMs":
        /*
          How long ago the daemon last saw this one do anything (PRD #745 M9).
          Blank — no dash, no placeholder — for every case `displayActivity`
          cannot honestly express: the daemon reported no instant (which is every
          agent under a restarted daemon, since it keeps no session state), the
          value is not a finite number, or it sits more than a minute in the
          future. That last one is the clock-skew rule: the instant comes from
          whichever hook process stamped the event, so ordinary skew reads "just
          now", while a stamp genuinely ahead of the webview's clock is not
          rewritten into one — a negative "ago" is a bug a user sees, and a
          fabricated "just now" for a far-future stamp is the same lie in nicer
          clothes.
        */
        return <td className="overview-activity" role="cell" key={column} title={activity && `Last activity reported by the daemon: ${activity.title}`}>{activity?.label ?? ""}</td>;
      case "spawnedAtMs":
        /*
          How long this agent's process has been running (PRD #745 M11) — the
          daemon's own spawn instant, not a session start, so it is present for an
          agent that has never emitted a hook event. Blank on exactly the same
          terms as Last activity beside it: `displayUptime` shares that column's
          clock-skew and usability policy and forks only the wording, so the one
          rule a reader learns holds for both cells.

          What the number MEANS needs no flag. A restarted orchestration worker is
          a fresh spawn with a fresh registry record, so it reads as the age of
          its current iteration; a role nobody has restarted keeps its original
          record, so it reads as its whole lifetime.
        */
        return <td className="overview-uptime" role="cell" key={column} title={uptime && `Spawned by the daemon at: ${uptime.title}`}>{uptime?.label ?? ""}</td>;
      case "cli":
        /*
          The BINARY this agent runs, resolved from the agent registry rather
          than from the wire identity (PRD #745). It used to print the
          serialised enum, so Claude Code read `claude_code` and OpenCode read
          `open_code` — neither of them a name anybody would type — while
          `codex` happened to be right. The hover is the full value and nothing
          else: the column header already says what it is, and a sentence
          restating it would be the screen describing itself.
        */
        return <td className="overview-cli" role="cell" key={column} title={displayTitle(agent.cli)}>{displayText(agent.cli, DISPLAY_LIMITS.name)}</td>;
      case "activeTool":
        return (
          <td className="overview-tool" role="cell" key={column}>
            {agent.activeTool ? (
              <>
                <Wrench size={11} aria-hidden="true" />
                <strong>{displayText(agent.activeTool, DISPLAY_LIMITS.toolName)}</strong>
                {/*
                  For a shell tool this detail is the command line the agent ran, so
                  it is bounded short: a full command line on a fifteen-row overview
                  is unreadable, and it is the kind of thing that ends up in a
                  screenshot. Bounded, not redacted — a length limit is honest,
                  where a secret-matching heuristic would only look like assurance.
                */}
                {agent.activeToolDetail && <em title={displayTitle(agent.activeToolDetail)}>{displayText(agent.activeToolDetail, DISPLAY_LIMITS.toolDetail)}</em>}
              </>
            ) : <span className="overview-tool-idle">no active tool</span>}
          </td>
        );
      case "toolCount":
        return <td className="overview-tool-count" role="cell" key={column} title={`${agent.toolCount} tool calls reported`}>{agent.toolCount}</td>;
      case "cwd":
        /*
          Blank for a directory the group header already states, and blank again
          when the daemon reported none — an empty cell says "nothing to add
          here" in both cases, which is exactly what is true.
        */
        return <td className="overview-cwd" role="cell" key={column} title={agent.cwd ? displayTitle(agent.cwd) : undefined}>{!agent.cwd || agent.cwd === hoistedCwd ? "" : displayPath(agent.cwd)}</td>;
      case "lastUserPrompt":
        /*
          The last prompt the operator sent this agent — the daemon's own answer
          to what it was asked to do, and the honest replacement for the
          placeholder live mode used to print. Blank when the daemon reported
          none, with no hover either: there is nothing to say, and a dash would be
          one more thing to read that says less.

          The most attacker-shaped string on this screen — free-form text an
          agent's own output can steer — so it is sanitised and clamped to
          `DISPLAY_LIMITS.prompt` before React sees it, and pinned to one line by
          `.overview-prompt` so no length or content can push a row taller than
          its neighbours.
        */
        return (
          <td className="overview-prompt" role="cell" key={column} title={agent.lastUserPrompt ? displayTitle(agent.lastUserPrompt) : undefined}>
            {agent.lastUserPrompt ? displayText(agent.lastUserPrompt, DISPLAY_LIMITS.prompt) : ""}
          </td>
        );
      default: {
        const unreachable: never = column;
        return unreachable;
      }
    }
  };
  return (
    <tr className="overview-row" role="row" data-testid={`overview-agent-${agentDomKey(agent)}`} data-status={agent.status}>
      {columns.map(cell)}
    </tr>
  );
}

function OverviewNote({ testId, icon, title, children }: { testId: string; icon: ReactNode; title: string; children: ReactNode }) {
  return <div className="overview-note" data-testid={testId}>{icon}<h3>{title}</h3>{children}</div>;
}

function OverviewRailButton({ icon: Icon, label, active, onClick, testId }: { icon: typeof LayoutList; label: string; active?: boolean; onClick: () => void; testId: string }) {
  return <button className={active ? "is-active" : ""} aria-current={active ? "page" : undefined} title={label} onClick={onClick} data-testid={testId}><Icon size={18} /><span>{label}</span></button>;
}

function OverviewInstrument({ label, children, testId }: { label: string; children: ReactNode; testId?: string }) {
  return <div className="instrument" data-testid={testId}><span>{label}</span>{children}</div>;
}

/**
 * A fleet total, or an explicit "not known". The em dash is this codebase's
 * established no-value marker and is deliberately not a zero: "no agents" and
 * "we cannot see the agents" are different statements, and only one of them is
 * true when the daemon is unreachable.
 */
function OverviewCount({ known, value, className }: { known: boolean; value: number; className?: string }) {
  if (!known) return <strong className="count-unknown" title="Not known — the daemon is not answering, so nothing can be counted.">—</strong>;
  return <strong className={className}>{value}</strong>;
}
