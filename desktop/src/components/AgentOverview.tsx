import { Children, cloneElement, createContext, isValidElement, useCallback, useContext, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type KeyboardEvent as ReactKeyboardEvent, type MouseEvent, type ReactNode } from "react";
import { Blocks, Boxes, CircleArrowUp, CircleStop, Columns3, Ellipsis, Filter, LayoutList, Layers, Network, Play, Plus, RefreshCw, RotateCcw, Server, ShieldAlert, Sparkles, SquareTerminal, Wrench, X } from "lucide-react";
import { desktopFeaturesOf } from "../types";
import { worktreePreview } from "../lib/worktreeClose";
import { type AgentCardEntry, type AgentSession, type AgentStatus, type CardTabSection, type ConnectionView, type DeckRuntimeState, type DeckView, type EndedRole, type GenerateConfigResult } from "../types";
import { STATUS_WORD_ORDER, statusWord, statusWordLabel, type StatusWord } from "../lib/statusWords";
import { modeScopedKey, type DesktopAgentDto } from "../lib/bridge";
import { buildDashboardFilterHeader, clearDashboardFilter, DASHBOARD_AGENT_TYPES, DASHBOARD_FILTER_TEXT_MAX, DASHBOARD_KINDS, DASHBOARD_STATUSES, dashboardFilterActive, filterDashboardAgents, removeDashboardFilterFacet, setDashboardFilter, useDashboardFilter, type DashboardFilter, type DashboardFilterFacts } from "../lib/dashboardFilter";
import { VOICE_ACTIONS, type AgentViewTarget, type DashboardScroll, type NewAgentVoice, type NewAgentVoiceChannel, type VoiceActionContext, type VoiceDispatchTarget, type VoiceOverviewChannel } from "../lib/voiceActions";
import { DECK_STATE_FALLBACK, deckUnavailableReason, isNewAgentShortcut } from "../lib/newAgent";
import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";
import { UpgradeDialog, type UpgradeTarget } from "./UpgradeDialog";
import { UpgradeAllDialog } from "./UpgradeAllDialog";
import { upgradeAllOffered } from "../lib/upgradeAll";
import { appFirst, LocalUpgradeFlow, type LocalUpgradeRequest } from "./LocalUpgradeFlow";
import { upgradeKindOf, upgradeOffered } from "../lib/upgrade";
import { cliBehind, type SelfUpgradeApi, type SelfUpgradeCheck } from "../lib/selfUpgrade";
import { ConnectionDetail } from "./ConnectionDetail";
import { HostMetricsButton } from "./HostMetricsPanel";
import { DashboardCounters } from "./DashboardCounters";
import { chooseHostDeck } from "../lib/hostMetrics";
import { CONNECT_ANYWAY_BODY, disconnectedDetails, incompatibleRemedy, startDaemonConfirmCopy } from "../lib/connectionRemedy";
import { StartDaemonError } from "../lib/actionError";
import { stopDaemonConfirmCopy, stopDaemonOffer } from "../lib/stopDaemon";
import { NewAgentDialog, NO_DIALOG_FOR_DECK, NO_DIALOG_TO_DISCARD, NO_DIRECTORY_BROWSER, NO_NEW_AGENT_DIALOG, NO_NEW_AGENT_FORM, type NewAgentRuntime } from "./NewAgentDialog";
import type { NewAgentDraft } from "../lib/newAgentDraft";
import { DeckSelector } from "./DeckSelector";
import { AddDaemonOpener } from "./AddDaemonDialog";
import type { DesktopSettingsState } from "../hooks/useDesktopSettings";
import { VoiceNumber } from "./VoiceNumber";
import { PullRequestBadge } from "./PullRequestBadge";
import { OpenCardReference, OpenPullRequest } from "../lib/prBrowser";
import { useNumberedList, useNumbersShown } from "../hooks/useVoiceNumbers";
import { numberKey, type VoiceNumberedEntryDto, type VoiceNumberedSectionDto } from "../lib/voiceNumbers";
import { agentSpokenName, orchestrationTitle } from "../lib/agentNames";
import { DISPLAY_LIMITS, deckName, displayActivity, displayIdentity, displayPath, displayText, displayTitle, displayUptime, domIdentity, rendersBlank } from "../lib/displayText";
import { HookBinaryNotices } from "./HookBinaryNotice";
import { InterruptedWorkBanner, InterruptedWorkDialog } from "./InterruptedWork";
import { INTERRUPTED_COVERED, listedCount, mayHaveInterrupted, NOTHING_INTERRUPTED, shownInterruptedDialog, type InterruptedDialogControls } from "../lib/interruptedWork";
import { sameIncarnation } from "../lib/agentKey";
import { AGENT_GONE_AFTER_RESTART } from "../lib/agentIdentity";
import { inboxAttentionWords, INBOX_WORDS } from "../lib/inboxWords";
import { ENDED_ROLE_WORDS, endedRoleOfCard, endedRolesOf } from "../lib/endedRoles";
import { DeckAttention } from "./DeckAttention";
import { useAgentCardsView, type AgentCardsView } from "../hooks/useAgentCardsView";
import { needsYouSentence, placedCardKey, type PlacedCard } from "../lib/agentCards";
import { OverviewModeSwitch } from "./OverviewModeSwitch";
import { FleetViewMock } from "./FleetViewMock";
import { AgentViewMock } from "./AgentViewMock";
import { fleetMockComposer, fleetMockState, fleetMockView } from "../data/fleetMock";
import { FIXTURE_DAEMON_ID } from "../data/fixture";
import { AgentCardsDeckBody, AgentCardsNotes, AgentCardsSelectionBar, NO_UNCOUNTED, needsYouCountDescription, type SelectionBarEditor, type UncountedDaemons } from "./AgentCardsGrid";
import { needsYouRowKeys } from "../lib/agentCardsTree";
import { MoreMenu, type MoreMenuItem } from "./MoreMenu";
import { AgentCardReviewDialog } from "./AgentCardReviewDialog";
import type { PaneCardsChannel, PaneCardsView } from "../lib/paneCards";
import { AgentCardBatchDialog } from "./AgentCardBatchDialog";
import { AgentCardActionsEditor } from "./AgentCardActionsEditor";
import { useCardActionsEditor, type CardActionsDaemon } from "../hooks/useCardActionsEditor";
import { OverviewRowCard } from "./OverviewRowMenu";

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
  /* Issue #1496 — read by the dashboard filter, not rendered. */
  | "agentType" | "daemonStatus" | "authoringKind"
  /* PRD #1401 — the badge beside the agent's name. */
  | "pullRequest"
  /* PRD #1666 part 2 — sent back with a stop, so it reaches the daemon process that listed the agent. */
  | "incarnation" | "paneId"
  /* PRD #2119 — Generate config reads Update config when the daemon says the project already has one. */
  | "projectConfigExists"
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
  /**
   * Issue #2387: the words the row's `held` marker gives on hover, while the
   * deck holds a message for the agent (`inboxAttentionWords`).
   */
  holdWords?: string;
};

export function toOverviewAgent(agent: AgentSession): OverviewAgent {
  const { id, daemonId, displayName, cli, status, cwd, activeTool, activeToolDetail, toolCount, tab, lastUserPrompt, lastActivityMs, spawnedAtMs, writeLease, agentType, daemonStatus, authoringKind, pullRequest, incarnation, paneId, projectConfigExists } = agent;
  return {
    id,
    daemonId,
    displayName,
    cli,
    status,
    agentType,
    daemonStatus,
    authoringKind,
    cwd,
    activeTool,
    activeToolDetail,
    toolCount,
    tab,
    lastUserPrompt,
    lastActivityMs,
    spawnedAtMs,
    writeLease: writeLease === "unknown" ? undefined : writeLease,
    ...(pullRequest ? { pullRequest } : {}),
    ...(incarnation ? { incarnation } : {}),
    ...(paneId ? { paneId } : {}),
    ...(projectConfigExists !== undefined ? { projectConfigExists } : {}),
    ...withHoldWords(inboxAttentionWords(agent.inbox)),
  };
}

const withHoldWords = (holdWords: string | undefined) => (holdWords === undefined ? {} : { holdWords });

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

/**
 * Issue #1496 — a row's facts as the dashboard filter reads them. The daemon's
 * own status word where there is one; in fixture mode, whose agents carry
 * only the merged column value, the word that value stands for.
 */
const FIXTURE_DAEMON_STATUS: Partial<Record<AgentStatus, DesktopAgentDto["status"]>> = {
  running: "working",
  needs_input: "waiting_for_input",
  waiting: "waiting",
  idle: "idle",
  failed: "error",
  blocked: "blocked",
};

export function overviewFilterFacts(agent: OverviewAgent): DashboardFilterFacts {
  return { ...agent, status: agent.daemonStatus ?? FIXTURE_DAEMON_STATUS[agent.status] };
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
  /**
   * Issue #1965 — this orchestration's roles whose agent exited, which
   * `agents` no longer lists: drawn after the live rows, saying which role it
   * was and why, as the TUI's card for the same role does. Only orchestration
   * groups have them.
   */
  endedRoles?: EndedRole[];
}

/**
 * Issue #1965 — `groups` with each orchestration's exited roles attached, from
 * the deck's own reply.
 */
function withEndedRoles(groups: OverviewGroup[], endedRoles: readonly EndedRole[] | undefined): OverviewGroup[] {
  if (!endedRoles?.length) return groups;
  const withRows = groups.map((group) => {
    if (group.kind !== "orchestration") return group;
    const ended = endedRolesOf(endedRoles, group.id, new Set(group.agents.map(roleIndexOf)));
    return ended.length ? { ...group, endedRoles: ended } : group;
  });
  // An orchestration every role of which has exited has no live agent to
  // build its group from, so it gets one of its own, headed as the TUI's tab
  // is, after the orchestrations that still have live roles.
  const covered = new Set(groups.flatMap((group) => (group.kind === "orchestration" && group.id !== undefined ? [group.id] : [])));
  const orphaned = new Map<string, OverviewGroup>();
  for (const role of endedRoles) {
    const id = role.orchestrationId;
    if (id === undefined || covered.has(id) || orphaned.has(id)) continue;
    orphaned.set(id, {
      id,
      key: groupKey("orchestration", id),
      kind: "orchestration",
      title: orchestrationTitle({ name: role.orchestrationName, displayTitle: role.displayTitle }),
      subtitle: role.displayTitle ? role.orchestrationName : undefined,
      agents: [],
      endedRoles: endedRolesOf(endedRoles, id, new Set()),
    });
  }
  if (!orphaned.size) return withRows;
  const lastOrchestration = withRows.map((group) => group.kind).lastIndexOf("orchestration");
  const at = lastOrchestration >= 0 ? lastOrchestration + 1 : withRows.filter((group) => group.kind === "standalone").length;
  return [...withRows.slice(0, at), ...orphaned.values(), ...withRows.slice(at)];
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
        title: orchestrationTitle(agent.tab),
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

/**
 * How many agents show each status word, in `STATUS_WORD_ORDER` (issue #2411:
 * the daemon's words, so Thinking, Working and Compacting are counted apart,
 * as the TUI's stats bar counts them).
 */
export function countByStatus(agents: OverviewAgent[]): { status: StatusWord; count: number }[] {
  const words = agents.map(statusWord);
  return STATUS_WORD_ORDER
    .map((status) => ({ status, count: words.filter((word) => word === status).length }))
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
   * The fixed widths are sized for the type scale in `styles.css` (issue
   * #2312), so a status badge or a legend at that size fits its column; the
   * flexible minimums stayed where they were, so nine columns still fit a
   * 1280px window.
   */
  track: string;
}

const OVERVIEW_COLUMNS: Record<OverviewColumnId, OverviewColumnSpec> = {
  status: { label: "Status", legend: "STATUS", track: "120px" },
  displayName: { label: "Agent", legend: "AGENT", track: "minmax(150px, 1.3fr)" },
  lastActivityMs: { label: "Last activity", legend: "LAST ACTIVITY", track: "84px" },
  spawnedAtMs: { label: "Uptime", legend: "UPTIME", track: "70px" },
  cli: { label: "CLI", legend: "CLI", track: "110px" },
  activeTool: { label: "Active tool", legend: "ACTIVE TOOL", track: "minmax(130px, 1.3fr)" },
  toolCount: { label: "Tools", legend: "TOOLS", track: "56px" },
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
function useOverviewClock(intervalMs: number = OVERVIEW_CLOCK_TICK_MS): number {
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

/**
 * What a reported write lease says on hover, in the daemon's own terms — for
 * the two leases the overview shows. `write` has no entry because it has no
 * chip: see `shownWriteLease`.
 */
const WRITE_LEASE_TITLE: Record<"read" | "none", string> = {
  read: "History-only: the daemon can replay this session but cannot deliver input to it.",
  none: "View-only: the daemon has no handle it can write to or resume.",
};

/**
 * Issue #2353 — the write lease the overview prints for an agent: `read` or
 * `none`, when input typed on the screen would NOT reach the agent, and
 * nothing otherwise. `write` is the ordinary case for every agent, but only a
 * session that declares a live target reports it (today the stdout wrapper,
 * so Codex alone), so a `write` chip marked one agent type for being normal.
 * The TUI says the same: nothing on a live card, a `history` / `view-only`
 * marker on one that is not (CLAUDE.md rule 22). Absence — a daemon that
 * reported no lease — prints nothing, and is not read as read-only.
 */
function shownWriteLease(writeLease: OverviewAgent["writeLease"]): "read" | "none" | undefined {
  return writeLease === "read" || writeLease === "none" ? writeLease : undefined;
}

/**
 * PRD #1105 M5 — how a row's open control reaches the navigation callback.
 *
 * A context rather than one more prop on each of `DeckGroup`, `DaemonBody`,
 * `OverviewGroupCard` and `OverviewRow`, and the reason is the same one
 * `lib/settingsBridge` gives: three of those four would only FORWARD it. They
 * are layout components about decks, groups and columns, and a callback about
 * views threaded through their signatures is noise every later reader has to
 * re-derive as noise.
 *
 * The default is `undefined` rather than a no-op, and the row renders no
 * control at all when it reads one: a row outside {@link AgentOverview} has no
 * view to navigate, and a control that silently does nothing is worse than an
 * absent one. {@link AgentOverview} is the only provider and always supplies a
 * callback, so today that branch is a guarantee about the type rather than a
 * state the app reaches.
 *
 * # Every listed agent is openable, including one on a deck that is not
 * selected
 *
 * This screen merges every observed deck (PRD [#742](https://github.com/vfarcic/dot-agent-deck/issues/742))
 * and offers **every one of its rows** the same control. If an agent is on the
 * screen there is a way into its pane; a listed row with no way to open it is a
 * dead row, and which deck an agent happens to be on is not something a reader
 * can see from the row they are pointing at.
 *
 * The pane it opens has a LIVE terminal whichever deck the agent is on: it
 * carries the composite `(deckId, agentId)` through attach, output, input and
 * resize, and the crate resolves that deck's own link through `DaemonLinks`
 * (`App.tsx`'s `OverviewAgentPane`, `lib/bridge.ts`, `src-tauri/terminal.rs`).
 * That is the desktop app's reason to exist over the TUI — one place from which
 * any agent on any deck can be worked with (PRD
 * [#802](https://github.com/vfarcic/dot-agent-deck/issues/802)) — and nothing
 * here moves the selected deck to achieve it.
 *
 * What the deck decides is what the PANE can do, not whether it opens. A deck
 * with no live link — disconnected, not yet reporting, no address — has nothing
 * to attach over, and the pane says so instead of showing a black rectangle
 * (`unreachableDeckTerminalState` in `lib/terminalInput.ts`). That is the
 * honest division: the overview knows nothing about attach, and the pane is
 * where the attach either happens or is explained.
 */
const OpenAgentContext = createContext<((agent: OverviewAgent) => void) | undefined>(undefined);

/**
 * PRD #1223 U4 — closing what the New agent flow creates, from the screen it
 * creates it on: one agent from its row, a whole orchestration from its card.
 *
 * Each asks the same {@link ConfirmDialog} the deck screen's stop asks, then
 * sends a deck-scoped action — `stop_agent`, or `stop_orchestration` over every
 * role the card lists — to the deck the agent is ON, never the selected one.
 * A failure is filed by the runtime under its global error, which the overview's
 * toast shows with the cleanup warning for any role whose stop the deck could
 * not confirm.
 *
 * No control is disabled for its deck's state: rows and cards render only for
 * a connected deck (`DaemonBody`), and a deck that drops between render and
 * confirmation refuses the action, which the toast reports. While a stop is in
 * flight the confirmation's own busy latch is what stops a second one.
 */
interface OverviewStopControls {
  stopAgent: (agent: OverviewAgent) => void;
  closeOrchestration: (group: OverviewGroup) => void;
}

const StopControlsContext = createContext<OverviewStopControls | undefined>(undefined);

/**
 * PR #1451 round 3, change 3 — the number each row shows while voice is on,
 * by {@link agentKey}: one sequence across every deck's rows, in the order
 * they render. Absent while voice is off, and while a pane or the New agent
 * dialog covers the rows.
 */
const RowNumbersContext = createContext<ReadonlyMap<string, number> | undefined>(undefined);
/** PRD #2374 — the cards view, while the runtime offers cards: each row's needs-you marker and ⋯ menu read it. */
const RowCardsContext = createContext<AgentCardsView | undefined>(undefined);

/** Overlap kept between two screens of a spoken scroll, so the row at the edge stays in sight (issue #1492). */
const DASHBOARD_SCROLL_OVERLAP = 48;

/** The least a spoken scroll moves, for a window too short to measure a screen from. */
const DASHBOARD_SCROLL_MIN = 120;

/** How far an arrow key moves the dashboard when the keyboard is forwarded to it, as a browser's arrow step. */
const DASHBOARD_ARROW_STEP = 40;

/**
 * Where keys stay with what they were typed into rather than scroll the
 * dashboard: fields, terminals, dialogs, lists and menus, which use the same
 * keys themselves, and the dashboard's own region, which scrolls natively
 * once it has focus.
 */
const DASHBOARD_KEYS_STAY = "input, textarea, select, [contenteditable='true'], .xterm, [role='dialog'], [role='alertdialog'], [role='listbox'], [role='menu'], [role='menubar'], [role='slider'], .overview-body";

/**
 * Issue #1492 — the scroll a key asks of the dashboard while focus is outside
 * its region, as the window took those keys before the dashboard scrolled in
 * a region of its own: Page Up/Down, Home and End wherever they are not
 * someone else's, and the arrows and Space only with nothing focused, since a
 * focused control may use them.
 */
function dashboardKeyScroll(key: string, shift: boolean, nothingFocused: boolean): DashboardScroll | number | undefined {
  if (shift && key !== " ") return undefined;
  if (key === "PageDown") return "down";
  if (key === "PageUp") return "up";
  if (key === "Home") return "top";
  if (key === "End") return "bottom";
  if (!nothingFocused) return undefined;
  if (key === " ") return shift ? "up" : "down";
  if (key === "ArrowDown") return DASHBOARD_ARROW_STEP;
  if (key === "ArrowUp") return -DASHBOARD_ARROW_STEP;
  return undefined;
}

/**
 * Whether a modal is open over the dashboard — Settings, a confirmation, any
 * sheet that declares itself one — wherever focus is. Opening Settings from
 * the rail leaves focus on the rail button, outside the sheet, so where a key
 * was pressed does not tell whether the dashboard is covered.
 */
export function modalOpen(): boolean {
  return document.querySelector("[aria-modal='true']") !== null;
}

/**
 * PR #2431 round 5 — whether a modal other than the Interrupted work dialog
 * is open. A request to show interrupted work may turn that dialog to another
 * daemon's list, so it never blocks one; Settings, a confirmation, or any
 * modal drawn over the dialog still does.
 */
function modalOtherThanInterruptedOpen(): boolean {
  return [...document.querySelectorAll("[aria-modal='true']")].some((modal) => !modal.classList.contains("interrupted-dialog"));
}

/**
 * The most dashboard rows that carry a number: the most items a numbered list
 * may declare to voice (`MAX_VOICE_NUMBERED_ENTRIES` in `src-tauri/src/lib.rs`,
 * which refuses a longer list and with it every voice command on the screen).
 * Rows past it are still listed and still chosen by name.
 */
const MAX_NUMBERED_ROWS = 1_000;

/** Already at the bottom, for "scroll down". */
const DASHBOARD_AT_BOTTOM = "The dashboard is already at the bottom.";
/** Already at the top, for "scroll up". */
const DASHBOARD_AT_TOP = "The dashboard is already at the top.";
/** A menu or dialog is in front of the dashboard, such as the open Daemon selector. */
export const DASHBOARD_COVERED = "Something is open over the dashboard, so it was not scrolled. Close it first.";
/** Nothing to scroll: every daemon and agent fits in the window. */
const DASHBOARD_FITS = "The whole dashboard is already on screen, so there is nothing to scroll.";

/**
 * Issue #2306 — where the dashboard's own smooth scroll is heading, by region:
 * the position it started from and the one it was sent to.
 */
const dashboardHeading = new WeakMap<HTMLElement, { from: number; to: number }>();

/** The regions {@link watchDashboardHeading} already listens on. */
const dashboardWatched = new WeakSet<HTMLElement>();

/**
 * Issue #2306 — forget where the dashboard's own smooth scroll was heading as
 * soon as it gets there, and when the user starts moving the region
 * themselves: a wheel, a pointer press or a touch on it, or a key pressed
 * inside it. Without this a destination outlived its scroll, and a Page Down
 * after End and then a wheel halfway back up did nothing.
 */
function watchDashboardHeading(region: HTMLElement): void {
  if (dashboardWatched.has(region)) return;
  dashboardWatched.add(region);
  const forget = () => dashboardHeading.delete(region);
  region.addEventListener("scroll", () => {
    const heading = dashboardHeading.get(region);
    if (heading && Math.abs(region.scrollTop - heading.to) <= 1) forget();
  }, { passive: true });
  for (const type of ["wheel", "pointerdown", "touchstart", "keydown"]) region.addEventListener(type, forget, { passive: true });
}

/**
 * Issue #2306 — the dashboard's position for the next scroll: where its own
 * smooth scroll is heading while that scroll is still on the way there, and
 * `scrollTop` otherwise. Until a smooth scroll has drawn a frame or two,
 * `scrollTop` still reads where it started, so a Page Up pressed straight
 * after a Page Down read "already at the top" and was dropped, and the Page
 * Down then finished a screen down. A scroll counts as on the way while
 * `scrollTop` lies between its start and its destination, has not reached the
 * destination, and the destination is still within the region's room (a
 * resize can shrink it); otherwise `scrollTop` is the position again.
 */
function dashboardPosition(region: HTMLElement, room: number): number {
  const at = region.scrollTop;
  const heading = dashboardHeading.get(region);
  if (heading && heading.to <= room + 1 && Math.abs(at - heading.to) > 1 && at >= Math.min(heading.from, heading.to) - 1 && at <= Math.max(heading.from, heading.to) + 1) return heading.to;
  dashboardHeading.delete(region);
  return at;
}

/**
 * Issue #2306 — the dashboard was moved by something other than one of its
 * own smooth scrolls (an arrow key's step), so where the last one was heading
 * no longer says where it is.
 */
function forgetDashboardHeading(region: HTMLElement): void {
  dashboardHeading.delete(region);
}

/**
 * Issue #1492 — scroll the dashboard by voice. The daemons scroll in their own
 * region, `.overview-body`, between the top bar and the voice row. A screen is
 * that region's height less a little overlap, so the row that was at the
 * bottom edge is still in sight at the top after "scroll down". Answers
 * `undefined` when it scrolled, or why it did not. A scroll asked for while
 * the last one is still moving counts from where that one is heading (issue
 * #2306, `dashboardPosition`).
 */
function scrollDashboard(region: HTMLElement | null, move: DashboardScroll): string | undefined {
  const room = region ? region.scrollHeight - region.clientHeight : 0;
  if (!region || room <= 1) return DASHBOARD_FITS;
  const behavior: ScrollBehavior = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth";
  const at = dashboardPosition(region, room);
  if ((move === "down" || move === "bottom") && at >= room - 1) return DASHBOARD_AT_BOTTOM;
  if ((move === "up" || move === "top") && at <= 1) return DASHBOARD_AT_TOP;
  const screen = Math.max(DASHBOARD_SCROLL_MIN, region.clientHeight - DASHBOARD_SCROLL_OVERLAP);
  const to = move === "top" ? 0 : move === "bottom" ? room : Math.min(room, Math.max(0, at + (move === "down" ? screen : -screen)));
  const from = region.scrollTop;
  region.scrollTo({ top: to, behavior });
  if (behavior === "smooth" && Math.abs(from - to) > 1) {
    watchDashboardHeading(region);
    dashboardHeading.set(region, { from, to });
  } else forgetDashboardHeading(region);
  return undefined;
}

/**
 * Whether `DaemonBody` renders a deck's agent rows rather than a note about
 * the deck: connected, reported, and configured. Its early returns, as one
 * question, so the numbered rows are exactly the rows on screen.
 */
function rendersAgentRows(connection: ConnectionView): boolean {
  return connection.status === "connected" && !connection.pending && !connection.unconfigured;
}


/*
  PRD #802 D5 — why a spoken stop opened no confirmation. Each says nothing
  was stopped, because nothing was.
*/
/** The agent a spoken stop named is no longer on the overview. */
export const STOP_TARGET_GONE = "That agent is not on the dashboard any more, so nothing was closed.";
/**
 * PRD #1666 part 2 (G-fix7) — the agent a spoken stop or close named is gone
 * with its deck's restart: the deck now lists an unrelated agent under its id,
 * and nothing is offered for that one. The TUI's sentence, then what happened.
 */
export const STOP_TARGET_RESTARTED = `${AGENT_GONE_AFTER_RESTART[0].toUpperCase()}${AGENT_GONE_AFTER_RESTART.slice(1)}, so nothing was closed.`;
/** The orchestration a spoken close named is no longer on the overview. */
const ORCHESTRATION_GONE = "That orchestration is not on the dashboard any more, so nothing was closed.";
/** A confirmation is already open; a second would replace it under the user's pointer. */
export const CONFIRMATION_ALREADY_OPEN = "A confirmation is already open — answer it first. Nothing else was stopped.";
/** The New agent dialog is modal, so a stop's confirmation could not be reached behind it. */
export const STOP_BEHIND_NEW_AGENT = "The New agent dialog is open — close it first. Nothing was stopped.";

/** What a stop or close confirmation calls one agent — the sanitised identity its row shows, or, in an orchestration, its role. */
function stopTargetName(agent: OverviewAgent): string {
  const raw = agent.tab.kind === "orchestration" && agent.tab.roleName ? agent.tab.roleName : agent.displayName;
  return displayIdentity(raw, DISPLAY_LIMITS.name, unnamedAgentLabel(agent));
}

/**
 * Issue #2436 — the orchestration `agent` is a role of, as its card on the
 * dashboard lists it, from the agents its deck reports. `undefined` for an
 * agent outside an orchestration, and for a role whose deck reported no
 * `orchestrationId`: `groupAgents` puts such a role in a group of its own, so
 * its other roles cannot be told apart and "close the orchestration" would
 * stop this role alone (Qodo, PR #2437). That role's dialog keeps the
 * single-role warning instead.
 */
export function orchestrationOf(agent: OverviewAgent, deckAgents: readonly OverviewAgent[]): OverviewGroup | undefined {
  if (agent.tab.kind !== "orchestration" || agent.tab.orchestrationId === undefined) return undefined;
  const key = agentKey(agent);
  return groupAgents([...deckAgents]).find((group) => group.kind === "orchestration" && group.agents.some((member) => agentKey(member) === key));
}

/** A `stop_orchestration` role: the agent the deck closes, the name its report uses, and the daemon process that listed it. */
type OrchestrationStopRole = { agentId: string; name: string; incarnation?: string };

/**
 * What closing a whole orchestration stops and how it is worded: the group's
 * own Close and an orchestration role's Close dialog (issue #2436) read the
 * same copy, so both name the orchestration and every role it stops.
 */
export function orchestrationCloseOf(group: OverviewGroup, connection: ConnectionView): { title: string; body: string; label: string; roles: OrchestrationStopRole[] } {
  const roles = group.agents.map((agent) => ({ agentId: agent.id, name: stopTargetName(agent), ...(agent.incarnation ? { incarnation: agent.incarnation } : {}) }));
  const title = displayIdentity(group.title, DISPLAY_LIMITS.name, unnamedGroupLabel(group));
  const count = roles.length === 1 ? "its 1 role" : `all ${roles.length} of its roles`;
  return {
    title,
    roles,
    label: roles.length === 1 ? "Close 1 role" : `Close all ${roles.length} roles`,
    body: `This stops every role of this orchestration on ${deckName(connection)} — ${count}: ${roles.map((role) => role.name).join(", ")}. Unsaved terminal work in any of them may be interrupted.`,
  };
}

/**
 * One-agent close flow shared by the dashboard row and full-window pane (issue #1402). The target is captured when the question opens, including its daemon process.
 *
 * Issue #2436: for a role of an orchestration (`orchestration`, from
 * {@link orchestrationOf}), the dialog also offers closing the whole
 * orchestration — the group Close's `stop_orchestration` over the same roles.
 * On the orchestrator that is the choice the dialog leads with, since closing
 * it alone leaves the workers with nothing coordinating them; on any other
 * role closing only that agent leads. Every role of an orchestration shares
 * one worktree, so the agent's own worktree preview answers for both closes,
 * and the "Keep the worktree" box applies to whichever is pressed.
 */
export function stopAgentConfirmation(agent: OverviewAgent, connection: ConnectionView, runAction: DeckRuntimeState["runAction"], onStopped?: () => void, previewWorktreeClose?: DeckRuntimeState["previewWorktreeClose"], orchestration?: OverviewGroup): ConfirmState {
  const name = stopTargetName(agent);
  const settle = async (action: Parameters<DeckRuntimeState["runAction"]>[0]) => {
    try {
      const result = await runAction(action);
      if (result.ok) onStopped?.();
    } catch {
      // The runtime files the refusal under its global error; keep the agent's view.
    }
  };
  const base = {
    title: `Close ${name}?`,
    label: "Close agent",
    busyLabel: "Stopping…",
    worktreePreview: worktreePreview({ previewWorktreeClose }, agent.daemonId, [agent]),
    action: ({ keepWorktree }: { keepWorktree: boolean }) => settle({ type: "stop_agent", deckId: agent.daemonId, agentId: agent.id, incarnation: agent.incarnation, ...(keepWorktree ? { keepWorktree: true } : {}) }),
    // Issue #1958: the dialog offers the box only when the preview names a worktree the deck can keep.
    keepWorktree: true,
  };
  const interrupted = "Unsaved terminal work may be interrupted.";
  const group = orchestration?.agents.some((member) => agentKey(member) === agentKey(agent)) ? orchestration : undefined;
  if (!group) {
    const roleWarning = agent.tab.kind === "orchestration"
      ? " This closes only this role, leaving the orchestration one role short. Other roles keep running."
      : "";
    return { ...base, body: `This sends a stop request to ${name} on ${deckName(connection)}. ${interrupted}${roleWarning}` };
  }
  const whole = orchestrationCloseOf(group, connection);
  const count = whole.roles.length === 1 ? "its 1 role" : `all ${whole.roles.length} of its roles`;
  const everyRole = `Close orchestration stops every role of ${whole.title} on ${deckName(connection)} — ${count}: ${whole.roles.map((role) => role.name).join(", ")}.`;
  const orchestrator = agent.tab.kind === "orchestration" && agent.tab.isStartRole;
  const onlyThis = orchestrator
    ? `Close agent stops only ${name}, the orchestrator, leaving the other roles with nothing coordinating them.`
    : `Close agent stops only ${name}, leaving ${whole.title} one role short; its other roles keep running.`;
  return {
    ...base,
    body: orchestrator ? `${everyRole} ${onlyThis} ${interrupted}` : `${onlyThis} ${everyRole} ${interrupted}`,
    alternative: {
      label: "Close orchestration",
      primary: orchestrator,
      action: ({ keepWorktree }) => settle({ type: "stop_orchestration", deckId: agent.daemonId, roles: whole.roles, ...(keepWorktree ? { keepWorktree: true } : {}) }),
    },
  };
}

/*
  PRD #2119 — Generate config: the TUI's question, in its words, answered
  Yes/No. It asks an agent to write a file the user can review, so it is drawn
  neutral rather than as a stop.
*/
const GENERATE_CONFIG_TITLE = "Generate .dot-agent-deck.toml";
const GENERATE_CONFIG_BODY = "No orchestration config found for this project. Want to instruct your agent to analyze the project and create one?";
/** PRD #2119 — the same question when the daemon reports the project already has a config: the TUI's words again. */
const UPDATE_CONFIG_TITLE = "Update .dot-agent-deck.toml";
const UPDATE_CONFIG_BODY = "This project already has an orchestration config. Want to instruct your agent to analyze the project and update the existing config?";
/**
 * PR #2362 review — the question when the daemon could not tell whether the project has a config (or did not say):
 * it claims neither, since the request the daemon then sends is the conditional one. Titled Generate, like the
 * button. The TUI's words again (`render_config_gen_prompt`, CLAUDE.md rule 22).
 */
const UNKNOWN_CONFIG_BODY = "Want to instruct your agent to check this project for .dot-agent-deck.toml and create it, or propose updates to it if it already exists?";

/** PRD #2119 — the confirmation's body for what the daemon reported: an existing config, none, or unknown. */
function configConfirmationBody(agent: Pick<OverviewAgent, "projectConfigExists">): string {
  if (agent.projectConfigExists === true) return UPDATE_CONFIG_BODY;
  if (agent.projectConfigExists === false) return GENERATE_CONFIG_BODY;
  return UNKNOWN_CONFIG_BODY;
}

/** PRD #2119 — the config action's verb: Update only when the daemon reported an existing config, else Generate (the TUI footer's `config_action_label`). */
export function configActionVerb(agent: Pick<OverviewAgent, "projectConfigExists">): "Update" | "Generate" {
  return agent.projectConfigExists === true ? "Update" : "Generate";
}
/** The agent offers no pane the daemon could post the request to. */
export const GENERATE_CONFIG_NO_PANE = "That agent has no pane the daemon can send the request to, so nothing was sent.";
/** Generate config's refusals for an agent that went away, in its own words: nothing was SENT, not closed. */
export const GENERATE_TARGET_GONE = "That agent is not on the dashboard any more, so nothing was sent.";
export const GENERATE_TARGET_RESTARTED = `${AGENT_GONE_AFTER_RESTART[0].toUpperCase()}${AGENT_GONE_AFTER_RESTART.slice(1)}, so nothing was sent.`;
/** A confirmation is already open; a second would replace it under the user's pointer. */
export const GENERATE_CONFIRMATION_ALREADY_OPEN = "A confirmation is already open — answer it first. Nothing was sent.";
/** The New agent dialog is modal, so the question could not be reached behind it. */
const GENERATE_BEHIND_NEW_AGENT = "The New agent dialog is open — close it first. Nothing was sent.";
/** Prefix of the failure sentence, the TUI's (`daemon_client::CONFIG_GEN_FAILED_PREFIX`), for a request the app itself could not hand over. */
const GENERATE_CONFIG_FAILED_PREFIX = "Failed to send config prompt";

/** The pane-side reply to a voice request, in a pane, that names an agent other than the one shown. */
export const GENERATE_FOR_THIS_AGENT = "To generate config for this agent, say 'generate config' or press Generate config.";

/**
 * PR #2362 review — the latest Generate config attempt per agent (deck and id),
 * shared by the dashboard row and the pane because both report into the same
 * pane notice. Each Yes takes the next token for its agent, and only the
 * attempt still holding the latest one is reported: an older request that
 * settles after a newer one cannot overwrite the newer result.
 */
const latestGeneration = new Map<string, number>();

/**
 * PRD #2119 — the Generate config confirmation shared by the dashboard row and
 * the agent's pane. The binding — deck, agent, pane and the daemon process that
 * listed it — is captured HERE, when the question opens, so a Yes pressed after
 * the row refreshed from a successor process still names the agent the user was
 * asked about, and the daemon refuses it if that process is gone. `onAnswer`
 * hears every outcome of the latest attempt for that agent, a failure to reach
 * the app's backend included; an attempt superseded by a newer Yes for the same
 * agent is not reported.
 */
export function generateConfigConfirmation(agent: Pick<OverviewAgent, "daemonId" | "id" | "paneId" | "incarnation" | "projectConfigExists">, paneId: string, generateConfig: NonNullable<DeckRuntimeState["generateConfig"]>, onAnswer: (result: GenerateConfigResult) => void): ConfirmState {
  const { daemonId, id, incarnation } = agent;
  const update = configActionVerb(agent) === "Update";
  return {
    title: update ? UPDATE_CONFIG_TITLE : GENERATE_CONFIG_TITLE,
    body: configConfirmationBody(agent),
    label: "Yes",
    busyLabel: "Sending…",
    cancelLabel: "No",
    neutral: true,
    action: async () => {
      const key = `${daemonId}\u0000${id}`;
      const attempt = (latestGeneration.get(key) ?? 0) + 1;
      latestGeneration.set(key, attempt);
      let result: GenerateConfigResult;
      try {
        result = await generateConfig(daemonId, id, paneId, incarnation);
      } catch (cause) {
        result = { outcome: "failed", message: `${GENERATE_CONFIG_FAILED_PREFIX}: ${cause instanceof Error ? cause.message : String(cause)}` };
      }
      if (latestGeneration.get(key) === attempt) onAnswer(result);
    },
  };
}

/**
 * PRD #1790 stage 2 — the dashboard's answer when several daemons have
 * interrupted work and a key or spoken request named none. `definite` is
 * false when any of them only answered with an incomplete empty list, which
 * says it may have work, not that it has (verification finding N4).
 */
function interruptedSeveral(names: string[], definite: boolean, then: string): string {
  return `Several daemons ${definite ? "have" : "may have"} interrupted work: ${names.join(", ")}. ${then}`;
}

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
/**
 * `settings` is OPTIONAL, and its absence is what removes the Deck selector
 * rather than rendering a broken one (PRD #741 M9).
 *
 * The alternative — falling back to a `useDesktopSettings` of this screen's own
 * — is the trap `ControlDeck`'s doc comment describes: two live settings
 * instances in one tree, one of them rendered by nothing, so a save against the
 * wrong one writes state no screen reads. `DeckShell` owns the one instance and
 * passes it to whichever view is mounted; a caller that renders this screen
 * standalone gets everything except the control that needs a document.
 */
export function AgentOverview({ runtime, settings, onNavigate, agentPaneOpen = false, agentPane, paneCards, voiceChannel, newAgentVoice, onConfirmationChange, onNewAgentChange, notice, selfUpgrade, onConfigRequestSent }: {
  runtime: DeckRuntimeState;
  /**
   * PRD #2119 — told when Generate config was sent from this screen, after it
   * navigated to that agent's pane. A shell that draws the pane over this
   * screen shows the sentence there, where it is not behind the pane; without
   * this prop the sentence is shown on this screen.
   */
  onConfigRequestSent?: (target: { deckId: string; agentId: string; incarnation?: string }, message: string) => void;
  settings?: DesktopSettingsState;
  onNavigate: (view: DeckView) => void;
  /**
   * PRD #1260 — told whenever a D5 confirmation opens or closes here, by
   * click or by voice. The voice panel does not own the confirmation and
   * learns of it only through this: an opening one pre-empts the dictation
   * mode, which does not resume when it is answered.
   */
  onConfirmationChange?: (open: boolean) => void;
  /** PRD #2174: told whether the New agent dialog is open, so the shell's star reminder waits behind it. */
  onNewAgentChange?: (open: boolean) => void;
  /**
   * An agent's pane is open over this screen (PRD #1105), which is where the
   * keyboard belongs — so the New agent shortcut stands down: `Ctrl+N` is
   * readline's next-history key, and an agent's terminal may be waiting for it.
   */
  agentPaneOpen?: boolean;
  /**
   * PR #2491 decision 2026-11-25 — the agent whose pane is open over this
   * screen: where the pane header's previous and next move from.
   */
  agentPane?: { deckId: string; agentId: string; incarnation?: string };
  /**
   * PR #2491 decision 2026-11-25 — where this screen publishes the cards the
   * agent pane's Card tab draws; absent where no pane is drawn beside it.
   */
  paneCards?: PaneCardsChannel;
  /**
   * PRD #1223 U5 — where this screen publishes what a voice dispatch may need
   * from it: `closeNewAgent`, only while the New agent dialog is open. Absent
   * where nothing dispatches voice (a standalone render).
   */
  voiceChannel?: VoiceOverviewChannel;
  /**
   * PRD #1223 — the New agent dialog's voice slot, handed straight to it: the
   * dialog writes what its directory browser shows and its three moves, the
   * voice surface reads the first and this screen serves the second.
   */
  newAgentVoice?: NewAgentVoiceChannel;
  /** Issue #1635 — the app's newer-release banner, shown at the top of the dashboard. */
  notice?: ReactNode;
  /**
   * Issue #1753 — the app's newer-release check and the calls that upgrade
   * this machine's CLI, for the Local daemon section's Upgrade. `onClosed`
   * runs when that flow ends, so the app checks again.
   */
  selfUpgrade?: { check?: SelfUpgradeCheck; api?: SelfUpgradeApi; onClosed?: () => void };
}) {
  const { fleet, snapshot, mode } = runtime;
  /*
   * This screen shows no terminal and opens no PTY (PRD #745 M7), and it no
   * longer says so to the bridge itself.
   *
   * It used to: `setShownTerminals([])` on mount, which is also what flushes
   * the warm set to zero and makes the claim true when you arrive here from a
   * nine-tile deck rather than only on a cold start. PRD #1105 M4 moved that
   * declaration UP to `DeckShell` without weakening it — the empty set is still
   * declared for this screen, by the only component that can also see the agent
   * pane when one is open over it.
   *
   * The move is the whole of M4 and not tidying. `setShownTerminals` must be
   * called once per render commit with the whole shown set; an overview that
   * declares `[]` for itself while a pane above it needs `[agentId]` is two
   * declarations in one commit, and which one the bridge saw last decided
   * whether that pane had a terminal. Neither outcome failed loudly.
   *
   * So: nothing here may declare a shown set again. A caller that renders this
   * screen standalone — every test that does, since `DeckShell` is the only
   * production caller — declares nothing, which is a statement about the
   * harness rather than about the screen.
   */
  /**
   * The SELECTED deck's connection — the Deck selector's.
   *
   * It is `fleet[0]`'s and deliberately not an aggregate: `selectionFallback`
   * describes THE SELECTION rather than a deck ("the deck you chose is gone"),
   * so it has exactly one true value however many decks are observed, and the
   * control that renders it is the control that made the selection.
   */
  const connection = snapshot.connection;
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
  /**
   * One rendered group per observed deck, each carrying its OWN agents, its own
   * grouping and its own connection (PRD #742 M4).
   *
   * `groupAgents` is untouched and is called once per deck: it buckets WITHIN a
   * deck by tab membership, which is a different question from which deck an
   * agent is on, and merging the fleet before calling it would put two
   * machines' roles in one orchestration card.
   */
  /**
   * Issue #1496 — the window session's filter. It decides which rows each
   * deck SHOWS (`sections`), and nothing else: `agents`, `groups` and
   * `counts` stay the whole deck, so the header's instruments keep describing
   * the whole fleet and a voice stop still finds an agent the filter hides.
   */
  const filter = useDashboardFilter();
  const filtering = dashboardFilterActive(filter);
  const decks = useMemo(() => fleet.map((deck, at): FleetDeck => {
    const deckAgents = deck.agents.map(toOverviewAgent);
    const groups = withEndedRoles(groupAgents(deckAgents), deck.endedRoles);
    const matchingAgents = filtering ? filterDashboardAgents(deckAgents, filter, overviewFilterFacts) : deckAgents;
    // Issue #1965: an exited role has no agent for a filter to match, so a
    // filtered view leaves the exited rows out; Show all brings them back.
    const shown = filtering ? groupAgents(matchingAgents) : groups;
    return {
      key: fleetMemberKey(deck.connection.deckId, at),
      snapshot: deck,
      agents: deckAgents,
      groups,
      /* Every group in its place, shown with its matching agents or, when
         none match, as one line saying so — never dropped without a word. */
      sections: groups.map((group) => {
        const matching = shown.find((candidate) => candidate.key === group.key);
        return matching ? { group: matching, matching: true } : { group, matching: false };
      }),
      shownCount: shown.reduce((total, group) => total + group.agents.length, 0),
      matchingAgents,
      counts: countByStatus(deckAgents),
      connected: deck.connection.status === "connected",
    };
  }), [filter, filtering, fleet]);
  /**
   * The fleet aggregate, over CONNECTED decks only (PRD #742 M4).
   *
   * Summing over every deck is the failure this exists to avoid: a deck that is
   * not answering has an UNKNOWN number of agents, and adding zero for it
   * silently under-counts the fleet by however many agents that deck is running
   * — a wrong number that looks exactly like a right one. Its group says so in
   * its own header instead, and `decksUp` below is what tells the reader the
   * total is partial.
   */
  const aggregate = useMemo(() => {
    const visible = decks.filter((deck) => deck.connected);
    const agents = visible.flatMap((deck) => deck.agents);
    return {
      agents,
      shown: visible.reduce((total, deck) => total + deck.shownCount, 0),
      groups: visible.reduce((total, deck) => total + deck.groups.length, 0),
      counts: countByStatus(agents),
      decksUp: visible.length,
    };
  }, [decks]);
  /**
   * Whether anything can be counted at all. With one deck this is exactly the
   * old `connection.status === "connected"`; with N it is "at least one deck is
   * answering", because one reachable deck makes the instruments a true
   * statement about a knowable part of the fleet rather than a guess.
   */
  const known = aggregate.decksUp > 0;
  // PRD #1666 part 3, round 5-wake (Q6); issue #2387: who the deck holds the
  // doorbell for, over the decks that answered, as the counters beside it are.
  const heldOnDecks = useMemo(() => decks.filter((deck) => deck.connected).flatMap((deck) => deck.snapshot.agents), [decks]);
  const countOf = (status: StatusWord) => aggregate.counts.find((entry) => entry.status === status)?.count ?? 0;
  /**
   * PRD #802 M2 — the overview's "Open deck" controls and
   * its row-level open all dispatch through the action registry. The context is
   * one member wide because that is all this screen can serve: the overlays, the
   * selection and the fixture loop belong to `DeckSurface`.
   */
  /**
   * PRD #1223 M4 — the New agent flow, and the runtime it runs against.
   *
   * `undefined` for a runtime without the two queries the flow needs — several
   * render-only test runtimes are that shape — which removes every entry point
   * rather than rendering a dialog that cannot load a directory.
   */
  const newAgentRuntime = useMemo<NewAgentRuntime | undefined>(() => (
    runtime.listDirectories && runtime.newAgentOptions
      ? {
        fleet: runtime.fleet,
        runAction: runtime.runAction,
        clearError: runtime.clearError,
        listDirectories: runtime.listDirectories,
        newAgentOptions: runtime.newAgentOptions,
        // PRD #1223 M6 — optional: a runtime without it offers no orchestration chips.
        ...(runtime.newAgentOrchestrations ? { newAgentOrchestrations: runtime.newAgentOrchestrations } : {}),
        // PRD #1885 — the review before an orchestration starts. A runtime
        // without them starts no orchestration: the dialog says why.
        ...(runtime.projectTrustState ? { projectTrustState: runtime.projectTrustState } : {}),
        ...(runtime.approveProjectTrust ? { approveProjectTrust: runtime.approveProjectTrust } : {}),
      }
      : undefined
  ), [runtime.approveProjectTrust, runtime.clearError, runtime.fleet, runtime.listDirectories, runtime.newAgentOptions, runtime.newAgentOrchestrations, runtime.projectTrustState, runtime.runAction]);
  /** The open flow and the deck it preselects; `undefined` while it is closed. */
  const [newAgent, setNewAgent] = useState<{ deckId?: string }>();
  /** The open dialog's own close — every route it has, blocked while a start is in flight — published by the dialog (PRD #1223 U5). */
  const newAgentClose = useRef<(() => string | undefined) | undefined>(undefined);
  /** What the flow could not finish on screen: an agent the deck accepted and has not listed. */
  const [newAgentNotice, setNewAgentNotice] = useState<string>();
  /** PRD #2119 — Generate config's outcome, or why it opened no confirmation. */
  const [generationNotice, setGenerationNotice] = useState<string>(); // voice-registry-exempt: Generate config's sentence in a banner, reporting what the request did
  /** Issue #1073 — what the last Stop daemon did, in the crate's own sentence, naming the daemon. */
  const [daemonNotice, setDaemonNotice] = useState<string>();
  /** The stop whose result the notice may show: a stop that settles after a later one was confirmed writes nothing. */
  const latestDaemonStop = useRef(0);
  /**
   * Issue #1247 — the form the dialog handed back when it was last closed,
   * replayed by the next open. Here rather than in the dialog because closing
   * is the dialog's unmount; cleared by the dialog's Discard (which hands back
   * nothing) and by a start the deck accepted. It lives as long as this screen
   * does, so leaving the overview for the deck screen loses it — see
   * `lib/newAgentDraft.ts` for the whole keep/discard list.
   */
  const [newAgentDraft, setNewAgentDraft] = useState<NewAgentDraft>();
  const newAgentAvailable = newAgentRuntime !== undefined;
  const openNewAgent = useCallback((deckId?: string) => {
    if (!newAgentAvailable) return;
    setNewAgentNotice(undefined);
    setNewAgent({ deckId });
  }, [newAgentAvailable]);
  /*
    PRD #802 D5 — the two stops by voice, each opening EXACTLY the
    confirmation its manual control opens (`stopControls`, U4) and nothing
    else: no deck action runs until the user presses that confirmation's
    button. The agent is looked up in the fleet as it is NOW — the dispatch
    names it by the deck and id Rust resolved against — and each refuses in
    words rather than acting on a stale target:

    - an agent or orchestration no longer on the overview;
    - another confirmation already open, which a second one would silently
      REPLACE under the user's pointer — the one mistake a confirmation must
      not invite;
    - the New agent dialog open, which is modal: a confirmation behind it
      would be unreachable, and the user is doing something else.
  */
  /*
    PRD #2119 — Generate config by voice or by the row's control, opening the
    same neutral confirmation and nothing else: no request is sent until the
    user presses Yes. Refused in words, as the stops are, for an agent that is
    gone or restarted, behind the New agent dialog, or over another
    confirmation.
  */
  const generateConfig = runtime.generateConfig;
  const confirmGenerateConfig = (target: VoiceDispatchTarget): string | undefined => {
    if (newAgent) return GENERATE_BEHIND_NEW_AGENT;
    if (confirm) return GENERATE_CONFIRMATION_ALREADY_OPEN;
    const deck = decks.find((candidate) => candidate.connected && candidate.snapshot.connection.deckId === target.deckId);
    const agent = deck?.agents.find((candidate) => candidate.id === target.agentId);
    if (!agent || !generateConfig) return GENERATE_TARGET_GONE;
    if ("incarnation" in target && !sameIncarnation(target.incarnation, agent.incarnation)) return GENERATE_TARGET_RESTARTED;
    if (!agent.paneId) return GENERATE_CONFIG_NO_PANE;
    setGenerationNotice(undefined);
    const question = generateConfigConfirmation(agent, agent.paneId, generateConfig, (result) => {
      if (result.outcome !== "sent") {
        setGenerationNotice(result.message);
        return;
      }
      /* Dismissed while it was sending: the outcome is still recorded, but
         the user has moved on, so the screen stays where they took it. */
      if (shownConfirm.current === question) {
        onNavigate({ kind: "agent", deckId: agent.daemonId, agentId: agent.id, from: "overview", ...(agent.incarnation ? { incarnation: agent.incarnation } : {}) });
      }
      /* The process the question was asked about, so the pane shows it only for that incarnation. */
      if (onConfigRequestSent) onConfigRequestSent({ deckId: agent.daemonId, agentId: agent.id, ...(agent.incarnation ? { incarnation: agent.incarnation } : {}) }, result.message);
      else setGenerationNotice(result.message);
    });
    setConfirm(question);
    return undefined;
  };
  const voiceStops = {
    confirmGenerateConfig,
    confirmStopAgent: (target: VoiceDispatchTarget): string | undefined => {
      if (newAgent) return STOP_BEHIND_NEW_AGENT;
      if (confirm) return CONFIRMATION_ALREADY_OPEN;
      const deck = decks.find((candidate) => candidate.connected && candidate.snapshot.connection.deckId === target.deckId);
      const agent = deck?.agents.find((candidate) => candidate.id === target.agentId);
      if (!agent) return STOP_TARGET_GONE;
      /* PRD #1666 part 2 (G-fix7): the agent the utterance named, as the
         daemon process that listed it then. A restarted deck's agent under
         the same id is not it, and no confirmation is opened for it. */
      if ("incarnation" in target && !sameIncarnation(target.incarnation, agent.incarnation)) return STOP_TARGET_RESTARTED;
      stopControls.stopAgent(agent);
      return undefined;
    },
    confirmCloseOrchestration: (target: VoiceDispatchTarget): string | undefined => {
      if (newAgent) return STOP_BEHIND_NEW_AGENT;
      if (confirm) return CONFIRMATION_ALREADY_OPEN;
      const deck = decks.find((candidate) => candidate.connected && candidate.snapshot.connection.deckId === target.deckId);
      const group = deck?.groups.find((candidate) => candidate.kind === "orchestration" && candidate.agents.some((agent) => agent.id === target.orchestrationAgentId));
      if (!group) return ORCHESTRATION_GONE;
      /* G-fix7: the card's member as the process that listed it then. */
      const member = group.agents.find((agent) => agent.id === target.orchestrationAgentId);
      if ("orchestrationIncarnation" in target && !sameIncarnation(target.orchestrationIncarnation, member?.incarnation)) return STOP_TARGET_RESTARTED;
      stopControls.closeOrchestration(group);
      return undefined;
    },
  };
  /*
    PR #1672 — which daemon's Host popover is open, by deck id: at most one,
    as before, now held here so the button and the spoken "show host" open it
    through one registry action (`showHostMetrics`).
  */
  const [hostOpen, setHostOpen] = useState<string | undefined>();
  /* Issue #2312 follow-up — whether the counters popover is open: held here
     so the button and the spoken "show the counters" open it through one
     registry action (`showDashboardCounters`), as the Host popover is. */
  const [countersOpen, setCountersOpen] = useState(false);
  const showDashboardCounters = useCallback(() => setCountersOpen(true), []);
  const showHostMetrics = useCallback((deckId?: string): string | undefined => {
    const choice = chooseHostDeck(decks.map((deck) => ({
      deckId: deck.snapshot.connection.deckId,
      name: deckName(deck.snapshot.connection),
      connected: deck.connected,
      hasFigures: deck.snapshot.hostMetrics !== undefined,
    })), deckId);
    if ("refused" in choice) return choice.refused;
    setHostOpen(choice.open);
    return undefined;
  }, [decks]);
  const closeHostMetrics = useCallback((deckId: string) => setHostOpen((open) => (open === deckId ? undefined : open)), []);
  /*
    PRD #1790 stage 2 — each deck's interrupted work: its banner while its
    daemon lists any, and the "Interrupted work" dialog, whose state the
    runtime holds so the restart notice can open it too. The open dialog
    publishes its calls here, for its buttons' registry entries and voice.
  */
  const interrupted = runtime.interruptedWork;
  const interruptedControls = useRef<InterruptedDialogControls | undefined>(undefined);
  /*
    Final reviewer 2 — `I`, the restart notice's Show and "show interrupted
    work" never decide from a list held from before: with one daemon to ask,
    its dialog opens and asks it again, as the TUI's `I` does, even when its
    last answer listed nothing or failed; with several, each is asked again
    first. "No interrupted work" is said only when every one of them answered
    that with a complete list.
  */
  /*
    PR #2431 round 4 (Qodo) — which request to show interrupted work is the
    newest. Asking several daemons again takes a round trip, and by its end
    the person may have moved on: asked again, opened or closed a dialog,
    left the dashboard. Each of those moves this on, and an answer to an
    older request opens nothing and says nothing.
  */
  const showRequest = useRef(0);
  useEffect(() => () => { showRequest.current += 1; }, []);
  const openInterruptedDialog = interrupted?.dialog;
  useEffect(() => { showRequest.current += 1; }, [openInterruptedDialog]);
  const showInterruptedWork = useCallback((deckId?: string): string | undefined | Promise<string | undefined> => {
    const request = ++showRequest.current;
    if (!interrupted) return NOTHING_INTERRUPTED;
    if (deckId !== undefined) return interrupted.openDialog(deckId) ? undefined : NOTHING_INTERRUPTED;
    const held = interrupted.decks;
    const asked = decks.filter((deck) => {
      const id = deck.snapshot.connection.deckId;
      return deck.connected && id !== undefined && held[id] !== undefined && !held[id].unsupported;
    });
    if (asked.length === 0) return NOTHING_INTERRUPTED;
    if (asked.length === 1) return interrupted.openDialog(asked[0].snapshot.connection.deckId as string) ? undefined : NOTHING_INTERRUPTED;
    const { openDialog, refresh, reveal } = interrupted;
    return Promise.all(asked.map((deck) => {
      const id = deck.snapshot.connection.deckId as string;
      return refresh(id, false).then((found) => ({ deck, id, found }));
    })).then((answers) => {
      /*
        The person moved on, or another modal now covers the dashboard: this
        answer is not theirs any more. An Interrupted work dialog open since
        the request is the one it asked for and may be turned to another
        daemon (PR #2431 round 5); one opened or closed meanwhile moved the
        request on above.
      */
      if (showRequest.current !== request || modalOtherThanInterruptedOpen()) return undefined;
      /*
        A daemon that could not be asked again still counts the items its last
        answer listed. One whose list is incomplete may have work it could not
        read, so it is opened or named like one with work, never taken for none
        (verification finding C); its dialog says the list is incomplete.
      */
      const listing = answers.filter(({ id, found }) => (found.outcome === "listed" ? found.count > 0 || found.incomplete : found.outcome !== "unsupported" && mayHaveInterrupted(held[id]?.list)));
      if (listing.length === 1) return openDialog(listing[0].id) ? undefined : NOTHING_INTERRUPTED;
      /*
        Each of them has a banner to open it by, an incomplete empty list's
        included (N4); one its Hide put away is brought back first, so the
        banner the answer points at is on screen for a keyboard or mouse
        user, not only reachable by voice (verification R2).
      */
      const definite = listing.every(({ id, found }) => (found.outcome === "listed" ? found.count : listedCount(held[id]?.list)) > 0);
      if (listing.length > 1) {
        reveal(listing.map(({ id }) => id));
        return interruptedSeveral(listing.map(({ deck }) => deckName(deck.snapshot.connection)), definite, "Open one from its banner, or say which one.");
      }
      const unknown = answers.filter(({ found }) => found.outcome === "failed" || found.outcome === "superseded");
      if (unknown.length > 0) return `Could not ask ${unknown.map(({ deck }) => deckName(deck.snapshot.connection)).join(", ")} for its interrupted work; try again, or say which daemon.`;
      return NOTHING_INTERRUPTED;
    });
  }, [decks, interrupted]);
  const hideInterruptedWork = useCallback((deckId: string) => interrupted?.hide(deckId), [interrupted]);
  /*
    What voice reaches: the open dialog's calls, refused while no dialog is
    open (reviewer 9: an action's own `undefined` answer is its success, never
    taken for that), and while another modal is drawn over the dialog
    (auditor 2). The utterance's binding to the dialog it was said to is the
    voice surface's gate (`shownInterruptedToken`).
  */
  const interruptedDialog = useMemo<InterruptedDialogControls>(() => {
    const first = "Open the interrupted work first: say “show interrupted work”.";
    const reach = <T,>(act: (controls: InterruptedDialogControls) => T): T | string => {
      const open = interruptedControls.current;
      if (!open) return first;
      if (shownInterruptedDialog()?.covered()) return INTERRUPTED_COVERED;
      return act(open);
    };
    return {
      reclaim: (said) => reach((open) => open.reclaim(said)),
      confirmReclaim: () => reach((open) => open.confirmReclaim()),
      dismiss: () => reach((open) => open.dismiss()),
      askDismiss: (said) => reach((open) => open.askDismiss(said)),
      confirmDismiss: () => reach((open) => open.confirmDismiss()),
      dismissAll: () => reach((open) => open.dismissAll()),
      confirmDismissAll: () => reach((open) => open.confirmDismissAll()),
      select: (id) => { reach((open) => open.select(id)); },
      openDetails: (which) => reach((open) => open.openDetails(which)),
      showReports: (said) => reach((open) => open.showReports(said)),
      nextReports: () => reach((open) => open.nextReports()),
      previousReports: () => reach((open) => open.previousReports()),
      askRestore: (said) => reach((open) => open.askRestore(said)),
      restoreAndContinue: () => reach((open) => open.restoreAndContinue()),
      restoreOnly: () => reach((open) => open.restoreOnly()),
      startOver: () => reach((open) => open.startOver()),
      back: () => { reach((open) => open.back()); },
      close: () => { reach((open) => open.close()); },
    };
  }, []);
  const interruptedVoice = useMemo(() => ({ showInterruptedWork, hideInterruptedWork }), [hideInterruptedWork, showInterruptedWork]);
  /*
    PR #1672 — the open popover goes with its button. A daemon that
    disconnects or stops sending figures loses its Host button, and the
    popover is closed with it, so it does not reappear unasked when they come
    back.
  */
  useEffect(() => {
    if (hostOpen === undefined) return;
    const shown = decks.some((deck) => deck.connected && deck.snapshot.connection.deckId === hostOpen && deck.snapshot.hostMetrics !== undefined);
    if (!shown) setHostOpen(undefined);
  }, [decks, hostOpen]);
  /*
    Issue #1492 — the dashboard scrolls in its own region, and a row acted on
    by number or by name is scrolled into view first, so it is on screen
    behind the pane that opens over it, and on screen when that pane closes.
  */
  const bodyRef = useRef<HTMLElement>(null);
  const revealAgent = useCallback((deckId: string, agentId: string) => {
    const testId = `overview-agent-${agentDomKey({ daemonId: deckId, id: agentId })}`;
    const row = Array.from(bodyRef.current?.querySelectorAll(".overview-row") ?? []).find((candidate) => candidate.getAttribute("data-testid") === testId);
    row?.scrollIntoView?.({ block: "nearest" });
  }, []);
  /*
    PRD #1223 U5 — publish `closeNewAgent` only while the dialog is open, so
    `closeTopmost` can read its presence the way it reads the voice overlay's.
    And `openNewAgent` — the `open_new_agent` row — only while it is CLOSED and
    this runtime can serve the flow at all, the same two conditions the
    `Ctrl+N` shortcut stands down on: a dispatch the overview cannot serve is
    then refused against the entry's `needs` rather than reopening a dialog
    mid-form. No dependency array, for the voice surface's reason: the slot
    must hold the last committed closure, and the cleanup clears it on unmount.
  */
  useEffect(() => {
    if (!voiceChannel) return;
    /* PRD #1223 — the directory browser's three moves, served ALWAYS on this
       screen and resolved against the dialog's slot at call time. Rust has
       already refused them when no browser was declared; what reaches here is
       a dispatch whose browser may have gone during the round trip, and that
       is a sentence, not a `needs` miss. */
    const move = (pick: (slot: NewAgentVoice) => string | undefined) => {
      const slot = newAgentVoice?.current;
      return slot ? pick(slot) : NO_DIRECTORY_BROWSER;
    };
    /* The form's three fills, the same way: served always, resolved against
       the dialog's slot at call time, refusing in words when it has gone. */
    const fill = (pick: (slot: NewAgentVoice) => string | undefined) => {
      const slot = newAgentVoice?.current;
      return slot ? pick(slot) : NO_NEW_AGENT_FORM;
    };
    const directoryMoves = {
      openDirectory: (target: VoiceDispatchTarget) => move((slot) => slot.openDirectory(target)),
      goToParentDirectory: (target: VoiceDispatchTarget) => move((slot) => slot.goToParentDirectory(target)),
      useThisDirectory: (target: VoiceDispatchTarget) => move((slot) => slot.useThisDirectory(target)),
      filterDirectories: (target: VoiceDispatchTarget) => move((slot) => slot.filterDirectories(target)),
      clearDirectoryFilter: (target: VoiceDispatchTarget) => move((slot) => slot.clearDirectoryFilter(target)),
      chooseNewAgentMode: (target: VoiceDispatchTarget) => fill((slot) => slot.chooseNewAgentMode(target)),
      chooseNewAgentType: (target: VoiceDispatchTarget) => fill((slot) => slot.chooseNewAgentType(target)),
      nameNewAgent: (target: VoiceDispatchTarget) => fill((slot) => slot.nameNewAgent(target)),
      setNewAgentCommand: (target: VoiceDispatchTarget) => fill((slot) => slot.setNewAgentCommand(target)),
      /* #1263 and #1247 — callable whenever the dialog is open, so a slot that
         has gone during the round trip means the dialog closed, not that the
         form has no deck. The slot is tested, not the call's answer: a
         choice that WORKED answers `undefined`, and `?? NO_DIALOG_FOR_DECK`
         on it reported every successful choice as "not open" (found on
         PR #1340, once a refusal stopped rendering beside a success). */
      chooseNewAgentDeck: (target: VoiceDispatchTarget) => {
        const slot = newAgentVoice?.current;
        return slot ? slot.chooseNewAgentDeck(target) : NO_DIALOG_FOR_DECK;
      },
      discardNewAgent: (target: VoiceDispatchTarget) => {
        const slot = newAgentVoice?.current;
        return slot ? slot.discardNewAgent(target) : NO_DIALOG_TO_DISCARD;
      },
      /* The dialog's own start, from its slot (PRD #1223). */
      startNewAgent: (target: VoiceDispatchTarget) => {
        const slot = newAgentVoice?.current;
        return slot ? slot.startNewAgent(target) : NO_NEW_AGENT_DIALOG;
      },
      ...voiceStops,
      /* Issue #1492 — the dashboard's own scroll, served always: Rust offers
         the scroll rows only while the New agent dialog is closed, and a
         scroll that lands under a pane moves only the screen behind it. */
      scrollDashboard: (move: DashboardScroll) => scrollDashboard(bodyRef.current, move),
      revealAgent,
      /* PR #1672 — a daemon's Host figures, served always: Rust offers the
         row only while the New agent dialog is closed. */
      showHostMetrics,
      /* And its close, published only while a popover is open, so
         `closeTopmost` reads its presence the way it reads `closeNewAgent`'s. */
      ...(hostOpen !== undefined ? { closeHostMetrics: () => setHostOpen(undefined) } : {}),
      /* The counters popover: its opening withheld while the New agent
         dialog is open, so a voice result judged before the dialog opened
         cannot open the list behind it (PR #2317 review); its close
         published only while it is open. */
      ...(newAgent ? {} : { showDashboardCounters }),
      ...(countersOpen ? { closeDashboardCounters: () => setCountersOpen(false) } : {}),
      /* PRD #1790 stage 2 — interrupted work: opening a daemon's dialog, and
         the open dialog's reclaim and dismissals, while the runtime offers
         it; withheld while the New agent dialog is open over the dashboard. */
      ...(interrupted && !newAgent ? { showInterruptedWork, hideInterruptedWork, interruptedDialog } : {}),
      /* PRD #2374 — the cards' members, only while the runtime offers cards;
         `closeCardReview` only while a review is open. */
      ...(cards.shown ? cards.voice : {}),
      /* PRD #2374 decision 2026-11-24 — the Card actions editor's members,
         while the cards are shown and a daemon on screen has them. */
      ...(cards.shown && cardActionsEditor.shown ? cardActionsEditor.controls : {}),
    };
    /* PR #2491 (2026-11-25) — the dashboard's view and Needs you only as
       they stand, while the runtime offers cards: the voice panel declares
       them with each utterance on the dashboard, so the command model reads
       "show all cards" as the filter's off switch while the filter is on. */
    const dashboardState = cards.shown ? { dashboardState: () => ({ view: cards.mode, needsYouOnly: cards.needsYouOnly }) } : {};
    voiceChannel.current = newAgent
      ? { closeNewAgent: () => newAgentClose.current?.(), ...directoryMoves, ...dashboardState }
      : newAgentAvailable ? { openNewAgent, ...directoryMoves, ...dashboardState } : { ...directoryMoves, ...dashboardState };
    return () => { voiceChannel.current = undefined; };
  });
  const voiceContext = useMemo(() => ({ navigate: onNavigate, openNewAgent, revealAgent, showHostMetrics, showDashboardCounters }), [onNavigate, openNewAgent, revealAgent, showHostMetrics, showDashboardCounters]);
  /* Issue #1198 — the deck is an experimental surface, so every "Open deck"
     door on this screen exists only while it is shown. `undefined` removes
     the top bar's button and each deck group's alike. */
  const openDeck = desktopFeaturesOf(runtime).showDeck ? () => VOICE_ACTIONS.openDeck.run(voiceContext) : undefined;
  /*
    PRD #2274 — the empty-agents note offers Add a daemon only while the local
    daemon is the only one configured: one prompt where a new user already
    looks, and nothing extra for someone who has added a daemon already. The
    Daemon selector's Add a daemon… entry is there for everyone, always.
  */
  const openAddDaemon = useContext(AddDaemonOpener);
  /* `chosen`, not `loaded`: a settings read that failed is loaded with the
     defaults standing in, and those say nothing about which daemons the user
     has (Qodo, PR #2319). */
  const onlyLocalDaemon = settings?.chosen === true && !settings.settings.endpoints?.remote.length;
  const addDaemon = openAddDaemon && onlyLocalDaemon ? () => VOICE_ACTIONS.addDaemon.run({ openAddDaemon }) : undefined;
  /**
   * `Ctrl+N` / `Cmd+N`, the TUI's `Ctrl+n` (PRD #1223 M4) — on this screen only,
   * because it is the screen the flow lives on.
   *
   * It stands down wherever the key belongs to something else: in a text field
   * or xterm's helper textarea (the same selector the deck's own shortcuts
   * skip), while an agent's pane is open over the screen, and while the flow
   * is already open.
   */
  useEffect(() => {
    if (!newAgentAvailable || agentPaneOpen || newAgent) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (!isNewAgentShortcut(event)) return;
      const target = event.target;
      if (target instanceof Element && target.matches("input, textarea, select, [contenteditable='true'], .xterm-helper-textarea")) return;
      event.preventDefault();
      VOICE_ACTIONS.openNewAgent.run(voiceContext);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [agentPaneOpen, newAgent, newAgentAvailable, voiceContext]);
  /*
    PRD #1790 stage 2 (reviewer 8, CLAUDE.md rule 22) — `I`, the TUI's
    Interrupted work key: opens the dialog of the daemon that lists items, as
    its banner's Show does, asking again first (final reviewer 2); with
    several, the dashboard says so, as voice's "show interrupted work" does.
    `X` hides a banner. Both stand down where the Ctrl+N shortcut does, and
    under any modal.

    A layout effect, not a passive one: the listener reads the banners'
    state from its closure, and a passive effect re-registers it only in a
    task React schedules after the commit, so a key pressed once the banners
    were drawn but before that task ran was handled against the state before
    them (`X` with two banners up did nothing). A layout effect runs inside
    the commit, so the listener always matches what is drawn.
  */
  useLayoutEffect(() => {
    if (!interrupted || agentPaneOpen || newAgent) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.key !== "I" && event.key !== "X") || event.ctrlKey || event.metaKey || event.altKey || event.defaultPrevented) return;
      const target = event.target;
      if (target instanceof Element && target.matches("input, textarea, select, [contenteditable='true'], .xterm-helper-textarea")) return;
      if (modalOpen()) return;
      if (event.key === "I") {
        event.preventDefault();
        VOICE_ACTIONS.showInterruptedWork.run({ showInterruptedWork, reportRefused: setDaemonNotice }, {});
        return;
      }
      /*
        Final reviewer 3 — `X`, the TUI's Hide: hides the one banner on the
        dashboard, as its Hide button does, and dismisses nothing. With
        several banners up it hides none and says so, as `I` does, since no
        key says which one was meant.
      */
      const shown = decks.flatMap((deck) => {
        const id = deck.snapshot.connection.deckId;
        const list = id !== undefined ? interrupted.decks[id]?.list : undefined;
        return id !== undefined && mayHaveInterrupted(list) && !interrupted.hidden(id) ? [{ id, name: deckName(deck.snapshot.connection), count: listedCount(list) }] : [];
      });
      if (shown.length === 0) return;
      event.preventDefault();
      if (shown.length > 1) setDaemonNotice(interruptedSeveral(shown.map(({ name }) => name), shown.every(({ count }) => count > 0), "Hide one with its banner's Hide."));
      else VOICE_ACTIONS.hideInterruptedWork.run({ hideInterruptedWork }, shown[0].id);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [agentPaneOpen, decks, hideInterruptedWork, interrupted, newAgent, showInterruptedWork]);
  /**
   * PRD #1105 M5 — the overview's entry point into an agent's pane.
   *
   * The composite `(daemonId, id)` goes into the view rather than the bare id,
   * because this screen merges every observed deck and agent ids are per-daemon
   * monotonic: `"1"` names one agent here and a different one on the deck
   * beside it. Everything downstream — the pane's own lookup, its shown
   * declaration, its terminal state — addresses that composite, so the bare id
   * is not a shorter spelling of it.
   *
   * `from: "overview"` is what closing reads, and it is the whole of the back
   * behaviour — no history, no stack.
   *
   * **Every agent on this screen reaches this**, whichever deck it is on. This
   * screen makes no attach decision and asks no question about the selected
   * deck; see {@link OpenAgentContext}.
   */
  const openAgent = useCallback((agent: OverviewAgent) => {
    VOICE_ACTIONS.openAgent.run(voiceContext, { deckId: agent.daemonId, agentId: agent.id, from: "overview", incarnation: agent.incarnation });
  }, [voiceContext]);
  /*
    PRD #2374 — the agent cards: the Table |
    Cards switch, the cards grid, the single-action review and the table's needs-you
    marker and row menu. A card's `open` action opens the agent exactly as a
    row does.
  */
  const cardDeckList = useMemo(() => decks.map((deck) => ({ deckId: deck.snapshot.connection.deckId, connected: deck.connected })), [decks]);
  /* A card opens its agent's pane through the target the agent's table row
     opens (issue #2418, D12), so the two cannot open different things. The
     listed agent is the card's only when the daemon process that listed it is
     the card's too (PR #2491 review, Qodo 5): agent ids are per-process, so
     after a restart an older card keeps its own identity instead of opening
     the newer agent that took its id. */
  const cardOpenTarget = useCallback((card: PlacedCard): AgentViewTarget => {
    const agent = decks.find((deck) => deck.snapshot.connection.deckId === card.deckId)?.agents.find((candidate) => cardIsAgent(card.entry, candidate));
    return agent
      ? { deckId: agent.daemonId, agentId: agent.id, from: "overview", incarnation: agent.incarnation }
      : { deckId: card.deckId, agentId: card.entry.agent_id, from: "overview", ...(card.entry.instance_id ? { incarnation: card.entry.instance_id } : {}) };
  }, [decks]);
  const openCardAgent = useCallback((card: PlacedCard, options?: { cardTab?: boolean; cardSection?: CardTabSection }) => {
    VOICE_ACTIONS.openAgent.run(voiceContext, { ...cardOpenTarget(card), ...(options?.cardTab ? { cardTab: true } : {}), ...(options?.cardSection ? { cardSection: options.cardSection } : {}) });
  }, [cardOpenTarget, voiceContext]);
  /* PRD #2374 — in the Cards view, the cards of a deck whose daemon has them,
     after the dashboard filter; `undefined` for a deck that keeps its usual
     body (the Table view, or a daemon with no cards or not answering). */
  const cardsOnScreen = (deck: (typeof decks)[number]): PlacedCard[] | undefined => (cards.mode === "cards" ? cardsOfDeck(deck) : undefined);
  /* The cards of a deck whose daemon has them, after the dashboard filter,
     whichever view is on screen: what Needs you only and the top bar's
     needs-you count read in the Table view too (PR #2491 decision 2026-11-25). */
  function cardsOfDeck(deck: (typeof decks)[number]): PlacedCard[] | undefined {
    const deckId = deck.snapshot.connection.deckId;
    if (!deck.connected || deckId === undefined || !cards.decksWithCards.has(deckId)) return undefined;
    return cards.placed.filter((card) => card.deckId === deckId && (!filtering || cardMatchesFilter(card, deck.matchingAgents)));
  }
  /* PRD #1790 stage 2 — a deck's banner, while its daemon lists interrupted work and it was not hidden for this process. */
  const interruptedBannerFor = (deck: (typeof decks)[number]): ReactNode | undefined => {
    const deckId = deck.snapshot.connection.deckId;
    if (!interrupted || deckId === undefined) return undefined;
    const list = interrupted.decks[deckId]?.list;
    if (!mayHaveInterrupted(list) || interrupted.hidden(deckId)) return undefined;
    return <InterruptedWorkBanner deckId={deckId} count={listedCount(list)} controls={interruptedVoice} />;
  };
  /* The open dialog, mounted per daemon process: another process's answers never reach it. */
  const interruptedOpen = interrupted?.dialog;
  const interruptedDeck = interruptedOpen ? interrupted?.decks[interruptedOpen.deckId] : undefined;
  const interruptedDeckName = interruptedOpen ? decks.find((deck) => deck.snapshot.connection.deckId === interruptedOpen.deckId) : undefined;
  const interruptedDialogView = interrupted && interruptedOpen && interruptedDeck && interruptedDeck.instance === interruptedOpen.instance
    ? <InterruptedWorkDialog key={`${interruptedOpen.deckId}:${interruptedOpen.instance}`} deckLabel={interruptedDeckName ? deckName(interruptedDeckName.snapshot.connection) : ""} deck={interruptedDeck} target={interruptedOpen} runtime={interrupted} controlsRef={interruptedControls} />
    : undefined;
  const cardsBodyFor = (deck: (typeof decks)[number]): ReactNode | undefined => {
    const shown = cardsOnScreen(deck);
    const deckId = deck.snapshot.connection.deckId;
    if (shown === undefined || deckId === undefined) return undefined;
    return <AgentCardsDeckBody deckId={deckId} cards={shown} tree={cards.treeFor(deckId)} openCards={cards.openCards} needsYouOnly={cards.needsYouOnly} controls={cardControls} results={cards.resultsFor} endedRoles={deck.snapshot.endedRoles} selection={cards.batchFor(deckId) ? cards.selection : undefined} />;
  };
  const openCardReference = useContext(OpenCardReference);
  /* Issue #2418 — whether the dashboard filter keeps a card off the Cards
     view, so a spoken open of its tree can say so. */
  const cardHiddenByFilter = useMemo(() => {
    if (!filtering) return undefined;
    return (card: PlacedCard) => {
      const deck = decks.find((candidate) => candidate.snapshot.connection.deckId === card.deckId);
      return deck !== undefined && !cardMatchesFilter(card, deck.matchingAgents);
    };
  }, [decks, filtering]);
  const cards = useAgentCardsView({ runtime, decks: cardDeckList, openAgent: openCardAgent, pane: agentPane, openReference: openCardReference, hiddenByFilter: cardHiddenByFilter });
  /* The cards on screen across the daemons, whichever view shows: the top
     bar's needs-you count and the "nothing needs you" note read them. */
  /* `cardsOfDeck` reads the cards, the decks and the filter, which are the memo's inputs. */
  const cardsShown = useMemo(() => decks.flatMap((deck) => cardsOfDeck(deck) ?? []), [cards.decksWithCards, cards.placed, decks, filtering]);
  const needYouCount = cardsShown.filter((card) => card.entry.needs_you).length;
  /* PR #2491 review S1 — the daemons on screen the cards do not speak for: a
     connected one that reports no cards (and has an agent on screen), and one
     that is not answering. While any is listed, neither the note above the
     daemons nor the ⋯ says that nothing needs the user. */
  const uncountedDecks = useMemo<UncountedDaemons>(() => {
    const notReporting = new Set<string>();
    const notAnswering = new Set<string>();
    for (const deck of decks) {
      if (cardsOfDeck(deck) !== undefined) continue;
      if (!deck.connected) notAnswering.add(deckName(deck.snapshot.connection));
      else if (deck.matchingAgents.length > 0) notReporting.add(deckName(deck.snapshot.connection));
    }
    return notReporting.size === 0 && notAnswering.size === 0 ? NO_UNCOUNTED : { notReporting: [...notReporting], notAnswering: [...notAnswering] };
  }, [cards.decksWithCards, cards.placed, decks, filtering]);
  const needYouDescription = needsYouCountDescription(needYouCount, uncountedDecks);
  /*
    PR #2491 decision 2026-11-25 — Needs you only in the Table view: each
    daemon that has cards shows only the rows whose card needs the user and
    the rows of the agents above them in its card tree, the path the Cards
    view keeps bright; the rest are hidden rather than faded, since a table
    row has no tree to keep it in place. A group with none of those rows is
    left out, and a daemon with none says so in one line. A daemon without
    cards keeps its rows: it marks nothing as needing the user. The rows
    numbered for voice are the rows shown, as under the dashboard filter.
  */
  const needsYouRows = useMemo(() => (cards.mode === "table" && cards.needsYouOnly ? needsYouRowKeys(cardsShown, cards.treeFor) : undefined), [cards.mode, cards.needsYouOnly, cards.treeFor, cardsShown]);
  const shownDecks = useMemo(() => (needsYouRows === undefined ? decks : decks.map((deck): FleetDeck => {
    const deckId = deck.snapshot.connection.deckId;
    if (deckId === undefined || !deck.connected || !cards.decksWithCards.has(deckId)) return deck;
    const kept = deck.matchingAgents.filter((agent) => {
      const card = cards.cardFor(deckId, agent.id, agent.incarnation);
      return card !== undefined && needsYouRows.has(placedCardKey(card));
    });
    const shown = groupAgents(kept);
    return {
      ...deck,
      sections: deck.groups.flatMap((group) => {
        const matching = shown.find((candidate) => candidate.key === group.key);
        return matching ? [{ group: matching, matching: true }] : [];
      }),
      shownCount: kept.length,
      needsYouOnly: true,
    };
  })), [cards.cardFor, cards.decksWithCards, decks, needsYouRows]);
  /* PRD #2374 decision 2026-11-24 — the Card actions editor, over the daemons
     whose cards the dashboard shows, in the dashboard's order. Each daemon's
     ⋯ menu opens it bound to that daemon (PR #2491 decision 2026-11-25). */
  const cardActionDaemons = useMemo<CardActionsDaemon[]>(() => decks.flatMap((deck) => {
    const deckId = deck.snapshot.connection.deckId;
    const snapshot = deckId !== undefined ? runtime.agentCards?.[deckId] : undefined;
    return deckId !== undefined && snapshot ? [{ deckId, name: deckName(deck.snapshot.connection), editable: snapshot.catalogEdit === true, instanceId: snapshot.instanceId }] : [];
  }), [decks, runtime.agentCards]);
  /* PR #2491 (the maintainer, 2026-11-25) — the daemons the selected cards
     are on, in the dashboard's order: the selection bar's Edit card actions
     opens one of their editors, and with a selection on screen on exactly one
     daemon, a spoken "open card actions" naming none opens that daemon's. */
  const selectionDaemons = useMemo(() => {
    const deckIds = cards.mode === "cards" && cards.selection && cards.selection.selected.size > 0 ? cards.selection.deckIds : [];
    return cardActionDaemons.filter((daemon) => deckIds.includes(daemon.deckId));
  }, [cardActionDaemons, cards.mode, cards.selection]);
  /* PR #2491 review T1 — with an agent's pane open over the dashboard, an
     opening that names no daemon (a spoken "open card actions" there) means
     the pane's daemon, never the selection's behind it. */
  const cardActionsEditor = useCardActionsEditor({ runtime, daemons: cardActionDaemons, preferredDeckId: selectionDaemons.length === 1 ? selectionDaemons[0].deckId : undefined, paneDeckId: agentPaneOpen ? agentPane?.deckId : undefined });
  /* The editor's opening, stable across renders, for the surfaces outside the
     overview's own controls (the selection bar, the agent pane's Card tab):
     it always reaches the editor as it stands now. */
  const latestEditor = useRef(cardActionsEditor);
  latestEditor.current = cardActionsEditor;
  const openCardActionsEditor = useCallback((deckId?: string) => latestEditor.current.controls.openCardActionsEditor(deckId), []);
  const { shown: editorShown, offeredFor: editorOfferedFor } = cardActionsEditor;
  const selectionEditor = useMemo<SelectionBarEditor | undefined>(() => (editorShown
    ? { daemons: selectionDaemons.filter((daemon) => editorOfferedFor(daemon.deckId)), controls: { openCardActionsEditor, reportRefused: cards.clickControls.reportRefused } }
    : undefined), [cards.clickControls.reportRefused, editorOfferedFor, editorShown, openCardActionsEditor, selectionDaemons]);
  /* PR #2491 decision 2026-11-25 — the items of a daemon's ⋯ menu: Card
     actions, opening the editor bound to that daemon, while it shows cards
     (read-only, with the reason, on a daemon whose release cannot edit them).
     Later items join this list. */
  const deckMoreItems = (deck: FleetDeck): MoreMenuItem[] | undefined => {
    const deckId = deck.snapshot.connection.deckId;
    if (!cards.shown || deckId === undefined || !cardActionsEditor.offeredFor(deckId)) return undefined;
    const editor = { ...cardActionsEditor.controls, reportRefused: cards.clickControls.reportRefused };
    return [{
      key: "card-actions",
      label: "Card actions",
      testId: "daemon-card-actions",
      title: "Add, change, hide or remove the actions this daemon's agents offer on their cards",
      onSelect: () => VOICE_ACTIONS.openCardActionsEditor.run(editor, deckId),
    }];
  };
  const needYouCountId = useId();
  /* Issue #1965 — the exited role a card is for, from its own deck's reply: the Card tab says why in full. */
  const endedForCard = useCallback((card: PlacedCard) => endedRoleOfCard(decks.find((deck) => deck.snapshot.connection.deckId === card.deckId)?.snapshot.endedRoles, card.entry), [decks]);
  /* Issue #2418 (D12) — a card's name opens its agent through `openAgent`
     with the same members a table row's click uses, so the two cannot open
     different things. */
  const cardControls = useMemo(() => ({ ...cards.clickControls, navigate: voiceContext.navigate, revealAgent: voiceContext.revealAgent }), [cards.clickControls, voiceContext]);
  /* PR #2491 decision 2026-11-25 — what the agent pane's Card tab draws,
     published for the pane beside this screen; withdrawn on unmount. */
  const paneCardsView = useMemo<PaneCardsView | undefined>(() => (cards.shown
    ? {
      shownFor: (deckId) => cards.decksWithCards.has(deckId),
      cardFor: cards.cardFor,
      resultsFor: cards.resultsFor,
      treeFor: cards.treeFor,
      endedFor: endedForCard,
      notice: cards.notice,
      /* PR #2491 (the maintainer, 2026-11-25) — the panel's Edit, beside its
         Actions heading, opens the editor of the agent's daemon. */
      cardActionsFor: editorOfferedFor,
      controls: { ...cards.clickControls, openCardActionsEditor },
    }
    : undefined), [editorOfferedFor, openCardActionsEditor, cards.cardFor, cards.clickControls, cards.decksWithCards, cards.notice, cards.resultsFor, cards.shown, cards.treeFor, endedForCard]);
  useLayoutEffect(() => { paneCards?.set(paneCardsView); }, [paneCards, paneCardsView]);
  useEffect(() => () => { paneCards?.set(undefined); }, [paneCards]);
  /* Issue #2399 review — the batch dialog names each agent's daemon the way
     the dashboard's section header does, when a batch reaches more than one. */
  const batchDaemonName = useCallback((deckId: string) => {
    const deck = decks.find((candidate) => candidate.snapshot.connection.deckId === deckId);
    return deck ? deckName(deck.snapshot.connection) : deckId;
  }, [decks]);
  const [confirm, setConfirm] = useState<ConfirmState>();
  /* The confirmation on screen as of the last render, read by an answer that
     arrives after it: a Generate config the user dismissed with No must not
     take them to its agent when its request settles late. `undefined` once
     this screen is gone, as `ConfirmDialog`'s own `shown` is: a request still
     pending when the user navigated away (voice "open daemons", say) must not
     take them back, and a reopened dashboard is a new instance whose
     confirmations are not this one's. */
  const shownConfirm = useRef<ConfirmState | undefined>(confirm);
  shownConfirm.current = confirm;
  useEffect(() => {
    shownConfirm.current = confirm;
    return () => { shownConfirm.current = undefined; };
  }, [confirm]);
  /** PRD #1487 M5 — the Upgrade dialog, for the deck whose card it was pressed on. */
  const [upgrade, setUpgrade] = useState<UpgradeTarget>();
  /** Issue #1600 — the Upgrade all dialog, opened from the top bar. */
  const [upgradeAllOpen, setUpgradeAllOpen] = useState(false); // voice-registry-exempt: the Upgrade all dialog (issue #1600) — opened only by its top-bar button, kept off voice like Upgrade, and it asks before stopping anything
  /** Issue #1753 — Upgrade on the Local daemon section: the CLI, the daemon, or both. */
  const [localUpgrade, setLocalUpgrade] = useState<LocalUpgradeRequest>();
  /* The CLI's plan while it is behind and this app can upgrade it. */
  const cliPlan = selfUpgrade?.api ? cliBehind(selfUpgrade.check) : undefined;
  /**
   * PR #1451 round 3, change 3 — the rows, numbered: every deck's, in the
   * order they render, as one sequence. Declared whenever they are on screen
   * — not while a pane or the New agent dialog covers them — and numbered
   * visibly only while voice is on.
   */
  const rowsShown = !agentPaneOpen && !newAgent;
  /*
    Issue #1492 — every row is drawn and numbered, voice on or off: the
    dashboard scrolls rather than splitting into pages, so a daemon or an
    orchestration is never missing because voice is on, and a row's number
    means the same row wherever the dashboard is scrolled. (PR #1451 round 3,
    change 4 used to page it while voice was on and draw only the page
    showing.)
  */
  const numberedAgents = useMemo(() => (
    !rowsShown
      ? undefined
      /* Issue #1496 — the rows SHOWING, so a number names a row on screen. */
      : shownDecks.flatMap((deck) => (rendersAgentRows(deck.snapshot.connection) ? deck.sections.flatMap((section) => (section.matching ? section.group.agents : [])) : [])).slice(0, MAX_NUMBERED_ROWS)
  ), [shownDecks, rowsShown]);
  const numberedEntries = useMemo(() => numberedAgents?.map((agent): VoiceNumberedEntryDto => {
    /* Issue #1668 — an orchestration's row by its run's title and its name,
       as Reading names it, so two runs' coders are two names in a choice. */
    const label = agentSpokenName(agent);
    return {
      kind: "agent",
      value: agent.id,
      deckId: agent.daemonId,
      label,
      /* The names a spoken number can also be: its own name, the role and the
         CLI. Not the id, which nobody reads off the row and which often ends
         in a digit. */
      names: [...new Set([agent.displayName, agent.tab.kind === "orchestration" ? agent.tab.roleName : undefined, agent.cli])].filter((name): name is string => Boolean(name) && name !== label),
    };
  }), [numberedAgents]);
  /* One section across every daemon group (round 4, D7). */
  const numberedSections = useMemo<VoiceNumberedSectionDto[] | undefined>(() => numberedEntries && [{ kind: "agent", entries: numberedEntries }], [numberedEntries]);
  useNumberedList("screen", numberedSections);
  const voiceOn = useNumbersShown("screen");
  const rowNumbers = useMemo(() => (
    voiceOn && numberedAgents ? new Map(numberedAgents.map((agent, at) => [agentKey(agent), at + 1])) : undefined
  ), [numberedAgents, voiceOn]);
  /* The Upgrade dialog counts as a confirmation for voice and for the
     dashboard's own keys: it asks whether to stop agents (PRD #1487 review). */
  const confirmationOpen = confirm !== undefined || upgrade !== undefined || localUpgrade !== undefined || upgradeAllOpen;
  /**
   * What a deck's Upgrade does, and what its button says. A remote deck's
   * covers its daemon, which runs that host's CLI. The local deck's covers the
   * app when it can upgrade itself or waits for a relaunch, the CLI on this
   * machine and the local daemon (issues #1753, #1959 and #1962), so its label
   * and tooltip name what is behind.
   */
  const deckUpgrade = (connection: ConnectionView): { onUpgrade?: () => void; upgradeLabel?: string; upgradeTitle?: string } => {
    const daemon: UpgradeTarget | undefined = runtime.upgradeDaemon && upgradeOffered(connection) && connection.deckId !== undefined
      ? { deckId: connection.deckId, deckName: deckName(connection), kind: upgradeKindOf(connection), offer: connection.upgradeOffer }
      : undefined;
    const daemonTitle = connection.upgradeOffer?.kind === "offered" ? `Its daemon runs ${connection.upgradeOffer.from}; this app is ${connection.upgradeOffer.to}.` : undefined;
    if (connection.deckKind === "remote") return daemon ? { onUpgrade: () => setUpgrade(daemon), upgradeTitle: daemonTitle } : {};
    const check = selfUpgrade?.check;
    if (!check || !selfUpgrade.api || (!cliPlan && !(daemon && appFirst(check)))) return daemon ? { onUpgrade: () => setUpgrade(daemon), upgradeTitle: daemonTitle } : {};
    const copies = { check, api: selfUpgrade.api };
    const withApp = appFirst(check);
    const targets = [withApp ? "Agent Deck" : undefined, cliPlan ? "the CLI" : undefined, daemon ? "the daemon" : undefined].filter((target): target is string => Boolean(target));
    const targetLabel = targets.length === 3 ? `${targets[0]}, ${targets[1]} and ${targets[2]}` : targets.join(" and ");
    return {
      onUpgrade: () => setLocalUpgrade({ daemon, copies }),
      upgradeLabel: `Upgrade ${targetLabel} on this machine`,
      upgradeTitle: [
        withApp ? displayText(check.notice ?? check.app.headline, DISPLAY_LIMITS.message) : undefined,
        cliPlan ? displayText(cliPlan.headline, DISPLAY_LIMITS.message) : undefined,
        daemon ? daemonTitle : undefined,
      ].filter(Boolean).join(" "),
    };
  };
  /* Issue #1492 — the scroll keys, while the rows are on screen and focus is
     outside the dashboard's region (see `dashboardKeyScroll`). */
  useEffect(() => {
    if (!rowsShown || confirmationOpen) return;
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey || modalOpen()) return;
      const target = event.target;
      const nothingFocused = !(target instanceof Element) || target === document.body || target === document.documentElement;
      if (!nothingFocused && target.closest(DASHBOARD_KEYS_STAY)) return;
      const scroll = dashboardKeyScroll(event.key, event.shiftKey, nothingFocused);
      const region = bodyRef.current;
      if (scroll === undefined || !region) return;
      event.preventDefault();
      if (typeof scroll === "number") {
        forgetDashboardHeading(region);
        region.scrollBy({ top: scroll });
      } else scrollDashboard(region, scroll);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [confirmationOpen, rowsShown]);
  /* The number keys, while the rows are numbered: a digit opens the row
     showing it, as saying it would. Nothing else on this screen takes a bare
     digit, and a field or terminal keeps its own. A modal over the dashboard —
     this screen's own confirmations, or one the app opens, such as the
     newer-release dialog (issue #1635) — keeps them, as it keeps the scroll
     keys above. */
  useEffect(() => {
    if (!rowNumbers || !numberedAgents || confirmationOpen) return;
    const onKeyDown = (event: KeyboardEvent) => {
      // A modal this screen does not own covers it too, such as the Upgrade
      // dialog the shell opens on its own (issue #1636): a digit pressed in it
      // must not open a row hidden behind it.
      if (modalOpen()) return;
      const number = numberKey(event);
      const agent = number === undefined ? undefined : numberedAgents[number - 1];
      if (!agent) return;
      event.preventDefault();
      openAgent(agent);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [confirmationOpen, numberedAgents, openAgent, rowNumbers]);
  const confirmationChanged = useRef(onConfirmationChange);
  confirmationChanged.current = onConfirmationChange;
  useEffect(() => { confirmationChanged.current?.(confirmationOpen); }, [confirmationOpen]);
  /* An overview unmounted with a confirmation up takes the confirmation with it. */
  useEffect(() => () => { confirmationChanged.current?.(false); }, []);
  const newAgentChanged = useRef(onNewAgentChange);
  newAgentChanged.current = onNewAgentChange;
  const newAgentOpen = newAgent !== undefined && newAgentRuntime !== undefined;
  useEffect(() => { newAgentChanged.current?.(newAgentOpen); }, [newAgentOpen]);
  useEffect(() => () => { newAgentChanged.current?.(false); }, []);
  /**
   * Why Connect anyway did not connect, and on WHICH deck (issue #1472). Keyed
   * by the deck it was pressed on, because that is where the reader is looking:
   * a refused deck in a fleet is rarely the selected one, and an error filed
   * under the selection was rendered on a group nobody had clicked.
   */
  const [overrideError, setOverrideError] = useState<{ deckId: string | undefined; message: string }>();
  /*
    The reason describes one attempt, so a newer state of that deck supersedes
    it: once the deck reports connected the sentence is stale, and it must not
    come back on a later disconnected note as if it explained that one.
    Reconnect clears it too, below.
  */
  const overrideDeckConnected = overrideError !== undefined
    && fleet.some((deck) => deck.connection.deckId === overrideError.deckId && deck.connection.status === "connected");
  useEffect(() => {
    if (overrideDeckConnected) setOverrideError(undefined);
  }, [overrideDeckConnected]);
  /**
   * PRD #1223 U4 — see {@link StopControlsContext}. The deck an agent is on is
   * `daemonId`, the wire id every deck-targeted action names; the connection
   * is read from the fleet as it is NOW, not from a copy the row captured.
   */
  const stopControls = useMemo<OverviewStopControls>(() => {
    const deckOf = (deckId: string) => fleet.find((deck) => deck.connection.deckId === deckId);
    const run = async (action: Parameters<DeckRuntimeState["runAction"]>[0]) => {
      try {
        await runtime.runAction(action);
      } catch {
        // Filed by the runtime under its global error — with the roles it
        // could not confirm beside it — which the overview's toast renders.
      }
    };
    return {
      stopAgent: (agent) => {
        const deck = deckOf(agent.daemonId);
        if (!deck) return;
        // Issue #2436: the whole deck's roles, not only the rows a dashboard filter shows.
        setConfirm(stopAgentConfirmation(agent, deck.connection, runtime.runAction, undefined, runtime.previewWorktreeClose, orchestrationOf(agent, deck.agents.map(toOverviewAgent))));
      },
      closeOrchestration: (group) => {
        const deckId = group.agents[0]?.daemonId;
        const deck = deckId === undefined ? undefined : deckOf(deckId);
        if (deckId === undefined || !deck) return;
        const { title, body, label, roles } = orchestrationCloseOf(group, deck.connection);
        setConfirm({
          title: `Close ${title}?`,
          body,
          label,
          busyLabel: "Stopping…",
          worktreePreview: worktreePreview(runtime, deckId, group.agents),
          // Issue #1958: the keep goes on every role's stop; the crate fans it out.
          action: ({ keepWorktree }) => run({ type: "stop_orchestration", deckId, roles, ...(keepWorktree ? { keepWorktree: true } : {}) }),
          keepWorktree: true,
        });
      },
    };
  }, [fleet, runtime]);
  /**
   * Issue #801. Daemon LIFECYCLE actions still live on the deck — this starts,
   * stops and replaces nothing. It relaxes this app's own build-stamp
   * comparison for this session, which is a judgement about what the user is
   * looking at, so it belongs on the screen that is refusing to show it. Gated
   * on `buildStampMismatchOnly`, so a genuine protocol mismatch never offers
   * it.
   *
   * The allowance is this APP's, not one deck's — `allow_build_mismatch` sets a
   * process-wide flag every later handshake reads. What is per deck is the
   * refusal the user pressed it on, so the gate reads THAT deck's connection
   * and the action names it (issue #1472): the crate connects to that deck and
   * rejects with the reason when it still cannot. Reading the SELECTED deck's
   * connection here is what made the button do nothing at all on a refused
   * deck beside a healthy, selected local one.
   */
  const requestConnectAnyway = (refused: ConnectionView) => {
    if (mode !== "live" || !refused.buildStampMismatchOnly) return;
    setConfirm({
      title: "Connect to a daemon from a different version?",
      body: CONNECT_ANYWAY_BODY,
      label: "Connect anyway",
      busyLabel: "Connecting…",
      action: async () => {
        setOverrideError(undefined);
        try {
          // The crate handshakes the deck again and emits its snapshot, so the
          // group turns into the connected deck on its own; no reconnect, which
          // would spend a second handshake on a deck this one already redid.
          await runtime.runAction({ type: "allow_build_mismatch", ...(refused.deckId === undefined ? {} : { deckId: refused.deckId }) });
        } catch (cause) {
          setOverrideError({ deckId: refused.deckId, message: cause instanceof Error ? cause.message : String(cause) });
        }
      },
    });
  };

  /**
   * Issue #1490 — Start daemon on THIS deck's card, local or remote. The
   * request names the deck it was pressed on, never the selection, and a
   * rejection lands on that deck's note in the crate's own sentence, with its
   * technical detail behind a disclosure.
   *
   * One error PER DECK (PR #1623 review): a failed start on one deck must not
   * replace another's, and only that deck's own connection, Reconnect or
   * Start clears it.
   */
  const [startErrors, setStartErrors] = useState<ReadonlyMap<string, StartErrorView>>(() => new Map());
  const setStartError = (deckId: string | undefined, error: StartErrorView | undefined) => {
    setStartErrors((current) => {
      const key = startErrorKey(deckId);
      if (error === undefined && !current.has(key)) return current;
      const next = new Map(current);
      if (error === undefined) next.delete(key);
      else next.set(key, error);
      return next;
    });
  };
  const connectedStartErrorKeys = fleet
    .filter((deck) => deck.connection.status === "connected" && startErrors.has(startErrorKey(deck.connection.deckId)))
    .map((deck) => startErrorKey(deck.connection.deckId))
    .join("\n");
  useEffect(() => {
    if (connectedStartErrorKeys === "") return;
    const connected = new Set(connectedStartErrorKeys.split("\n"));
    setStartErrors((current) => new Map([...current].filter(([key]) => !connected.has(key))));
  }, [connectedStartErrorKeys]);
  const requestStartDaemon = (target: ConnectionView) => {
    const reason = target.disconnectedReason;
    if (mode !== "live" || reason?.action !== "start-daemon") return;
    const deckId = target.deckId;
    setConfirm({
      ...startDaemonConfirmCopy(reason.host),
      label: "Start daemon",
      busyLabel: "Starting…",
      action: async () => {
        setStartError(deckId, undefined);
        try {
          // The crate resolves once the deck is connected and emits its
          // snapshot, so the card turns into the connected deck on its own.
          await runtime.runAction({ type: "start_daemon", ...(deckId === undefined ? {} : { deckId }) });
        } catch (cause) {
          setStartError(deckId, {
            message: cause instanceof Error ? cause.message : String(cause),
            ...(cause instanceof StartDaemonError && cause.detail !== undefined ? { detail: cause.detail } : {}),
          });
        }
      },
    });
  };

  /**
   * Issue #1073 — Stop daemon on THIS deck's card, local or remote, which need
   * not be the selected deck. The request names the deck it was pressed on, and
   * the crate resolves that deck and refuses one it does not observe. A refusal
   * (agents still running) lands in the runtime's error, as a refused agent
   * close does; a stopped deck's card turns into the disconnected deck on its
   * own.
   */
  const requestStopDaemon = (target: ConnectionView) => {
    const offer = stopDaemonOffer(target);
    const deckId = target.deckId;
    if (mode !== "live" || !offer.offered || offer.reason !== undefined || deckId === undefined) return;
    setConfirm({
      ...stopDaemonConfirmCopy(target),
      label: "Stop daemon",
      busyLabel: "Stopping…",
      action: async () => {
        const stop = ++latestDaemonStop.current;
        setDaemonNotice(undefined);
        try {
          // The crate's own sentence: "stopped" only for a daemon it saw go,
          // and "still shutting down" for one that accepted and had not gone
          // when it stopped waiting, whose card may still read connected.
          const result = await runtime.runAction({ type: "stop_daemon", deckId });
          if (stop === latestDaemonStop.current) setDaemonNotice(`${deckName(target)}: ${result.message ?? "Daemon stopped."}`);
        } catch {
          // The runtime files the refusal under its global error.
        }
      },
    });
  };

  /** What a deck's card offers for Stop daemon: the handler, or a disabled control with the reason. */
  const deckStop = (target: ConnectionView): { onStopDaemon?: () => void; stopDaemonReason?: string } => {
    const offer = stopDaemonOffer(target);
    if (mode !== "live" || !offer.offered) return {};
    if (offer.reason !== undefined) return { stopDaemonReason: offer.reason };
    return { onStopDaemon: () => requestStopDaemon(target) };
  };

  /**
   * Issue #1073 — Replace daemon on the LOCAL deck's card when its daemon
   * answered and was refused, whichever deck is selected. It is the deck
   * screen's Replace daemon (PRD #1487 D10): the shared upgrade procedure,
   * which asks before stopping anything. Only the local deck: Replace starts
   * the daemon that came with this app, on this machine, and a remote deck's
   * remedy is Upgrade.
   */
  const replaceOffered = (target: ConnectionView) => mode === "live" && Boolean(runtime.upgradeDaemon) && target.deckKind !== "remote"
    && target.status === "error" && target.daemonDetected === true && target.deckId !== undefined;
  const requestReplaceDaemon = (target: ConnectionView) => {
    const deckId = target.deckId;
    if (!replaceOffered(target) || deckId === undefined) return;
    setUpgrade({ deckId, deckName: deckName(target), kind: "replace" });
  };

  /* Issue #1496 — the daemons the filter can name, and the line stating it. */
  const filterDaemons = useMemo(() => decks.flatMap((deck) => {
    const id = deck.snapshot.connection.deckId;
    return id === undefined ? [] : [{ id, label: filterDaemonName(deck.snapshot.connection) }];
  }), [decks]);
  const filterHeader = buildDashboardFilterHeader(filter, aggregate.shown, aggregate.agents.length, (id) => filterDaemons.find((daemon) => daemon.id === id)?.label ?? id);

  /*
    Named rather than returned directly, so the provider below can wrap it
    without re-indenting eighty lines of screen for one line of plumbing.
  */
  const overviewScreen = (
    <div className="control-deck overview-screen">
      {/* The rail is the shell's — one rail, rendered once, beside every screen (#1197). */}
      <main className="deck-main">
        <header className="topbar overview-topbar">
          <div className="repo-context">
            <div className="repo-line"><LayoutList size={15} /><strong>Agent dashboard</strong></div>
            {/* PRD #741 M9: the same control, in the same block, as the deck's. */}
            {settings && <DeckSelector settings={settings} connection={connection} />}
          </div>
          <div className="top-actions">
            {/* Issue #2387: the held agents, inline; each name opens its agent. */}
            <DeckAttention agents={heldOnDecks} from="overview" context={voiceContext} />
            {/*
              Issue #2312 follow-up: the counters are one button that names
              only what needs someone (needs input, error, daemons not
              answering) and opens the full set on hover, click or voice.
              Issue #1757: WAITING is counted beside IDLE because the TUI's
              stats bar counts it (CLAUDE.md rule 22).
            */}
            <DashboardCounters
              summary={{ known, agents: aggregate.agents.length, needsInput: countOf("needs_input"), errors: countOf("error"), decksUp: aggregate.decksUp, decksTotal: decks.length }}
              open={countersOpen}
              onOpen={() => VOICE_ACTIONS.showDashboardCounters.run(voiceContext)}
              onClose={() => setCountersOpen(false)}
            >
              <OverviewInstrument label="AGENTS" testId="overview-count-agents"><OverviewCount known={known} value={aggregate.agents.length} /></OverviewInstrument>
              {/* Issue #2411: the daemon's words, each counted apart, as the TUI's stats bar counts them. */}
              <OverviewInstrument label="WORKING" testId="overview-count-working"><OverviewCount known={known} value={countOf("working")} className="count-working" /></OverviewInstrument>
              <OverviewInstrument label="THINKING" testId="overview-count-thinking"><OverviewCount known={known} value={countOf("thinking")} className="count-thinking" /></OverviewInstrument>
              <OverviewInstrument label="COMPACTING" testId="overview-count-compacting"><OverviewCount known={known} value={countOf("compacting")} className="count-compacting" /></OverviewInstrument>
              <OverviewInstrument label="NEEDS INPUT" testId="overview-count-needs-input"><OverviewCount known={known} value={countOf("needs_input")} className="count-needs-input" /></OverviewInstrument>
              <OverviewInstrument label="ERROR" testId="overview-count-error"><OverviewCount known={known} value={countOf("error")} className="count-error" /></OverviewInstrument>
              <OverviewInstrument label="WAITING" testId="overview-count-waiting"><OverviewCount known={known} value={countOf("waiting")} className="count-waiting" /></OverviewInstrument>
              <OverviewInstrument label="IDLE" testId="overview-count-idle"><OverviewCount known={known} value={countOf("idle")} /></OverviewInstrument>
              <OverviewInstrument label="GROUPS" testId="overview-count-groups"><OverviewCount known={known} value={aggregate.groups} /></OverviewInstrument>
              {/*
                PRD #742 M4. The one counter that is ALWAYS known, and it is
                what makes the others readable: they are computed over the
                daemons that answered, so "how many answered" is the caveat
                that belongs next to them.
              */}
              <OverviewInstrument label="DAEMONS" testId="overview-count-decks">
                <strong className={aggregate.decksUp === decks.length ? undefined : "count-failed"} title={decksUpTitle(aggregate.decksUp, decks.length)}>{aggregate.decksUp}/{decks.length}</strong>
              </OverviewInstrument>
            </DashboardCounters>
            {newAgentAvailable && <button className="button primary compact" data-testid="overview-new-agent" aria-label="New agent" title="New agent (Ctrl+N / ⌘N)" onClick={() => VOICE_ACTIONS.openNewAgent.run(voiceContext)}><Plus size={14} /><span>New agent</span></button>}
            {/* Issue #1600: shown while a remote deck runs an older daemon than this app, as each deck's own Upgrade is. */}
            {runtime.upgradeAll && runtime.decideUpgrade && upgradeAllOffered(decks.map((deck) => deck.snapshot.connection)) && <button type="button" className="button secondary compact" data-testid="overview-upgrade-all" aria-label="Upgrade all daemons" title="Upgrade every remote daemon older than this app" onClick={() => setUpgradeAllOpen(true)}><CircleArrowUp size={14} /><span>Upgrade all</span></button>}
            {/* Icon-only (issue #1308): with its label the top bar ran past 1440px on CI's font. The rail's Daemons entry is the labelled door. */}
            {openDeck && <button className="button secondary compact" data-testid="overview-open-deck" aria-label="Open daemons" title="Open daemons" onClick={openDeck}><SquareTerminal size={14} /></button>}
            {cards.shown && <OverviewModeSwitch mode={cards.mode} controls={cards.controls} consumeCards={runtime.consumeAgentCards} />}
            <DashboardFilterControls filter={filter} daemons={filterDaemons} />
            {/* PR #2491 decision 2026-11-25 — while Needs you only is on, a chip beside Filter says so, and one press anywhere on it turns it off. */}
            {cards.shown && cards.needsYouOnly && (
              <button
                type="button"
                className="button secondary compact needs-you-chip"
                data-testid="needs-you-chip"
                aria-label="Needs you only: show every agent"
                title="Needs you only is on. Press to show every agent again."
                onClick={() => VOICE_ACTIONS.toggleNeedsYouFilter.run(cards.clickControls, { needsYouOnly: false })}
              ><span>Needs you only</span><X size={12} aria-hidden="true" /></button>
            )}
            {cards.mode === "table" && <OverviewColumnPicker columns={columns} onChange={setColumns} />}
            <button className="button secondary compact" data-testid="overview-refresh" onClick={() => void runtime.reconnect()}><RefreshCw size={14} /><span>Refresh</span></button>
            {/*
              PR #2491 decision 2026-11-25 — the Dashboard's ⋯ menu, right of
              Refresh: Needs you only, checkable, for both views. The count on
              it is how many agents on screen need the user, which the line
              above the cards used to say.
            */}
            {cards.shown && (
              <>
                <MoreMenu
                  label="More"
                  menuLabel="More"
                  testId="overview-more"
                  className="more-menu"
                  menuTestId="overview-more-menu"
                  buttonClassName="button secondary compact more-menu-button"
                  icon={<Ellipsis size={14} aria-hidden="true" />}
                  describedBy={needYouDescription !== undefined ? needYouCountId : undefined}
                  badge={needYouCount > 0 ? <span className="more-menu-count" data-testid="overview-needs-you-count" aria-hidden="true">{needYouCount}</span> : undefined}
                  items={[{
                    key: "needs-you-only",
                    label: "Needs you only",
                    testId: "overview-needs-you-only",
                    checked: cards.needsYouOnly,
                    title: cards.needsYouOnly ? "Show every agent again" : "Show only the agents that need you, with the agents they sit under",
                    onSelect: () => VOICE_ACTIONS.toggleNeedsYouFilter.run(cards.clickControls, { needsYouOnly: !cards.needsYouOnly }),
                  }]}
                />
                {needYouDescription !== undefined && <span id={needYouCountId} hidden>{needYouDescription}</span>}
              </>
            )}
          </div>
        </header>

        {/*
          Issue #1496 — what the filter shows, on a line of its own: the counts
          above keep describing the whole fleet, and this says how much of it
          is on screen. Show all is here whenever any facet is set.
        */}
        {filterHeader.showAll && (
          <div className="dashboard-filter-line" data-testid="dashboard-filter-line" role="status">
            <span className="dashboard-filter-summary">{filterHeader.summary}</span>
            <span className="dashboard-filter-chips">
              {filterHeader.chips.map((chip) => (
                <span className="dashboard-filter-chip" key={chip.facet}>
                  <span>{displayText(chip.label, DISPLAY_LIMITS.name)}</span>
                  <button type="button" aria-label={`Remove ${chip.noun} filter`} title={`Remove ${chip.noun} filter`} onClick={() => setDashboardFilter(removeDashboardFilterFacet(filter, chip.facet))}><X size={12} /></button>
                </span>
              ))}
            </span>
            <button type="button" className="button secondary compact" data-testid="dashboard-filter-show-all" onClick={() => VOICE_ACTIONS.clearDashboardFilter.run(voiceContext)}><RotateCcw size={13} /><span>Show all</span></button>
          </div>
        )}

        {mode === "fixture" && (
          <div className="fixture-bar">
            <span><Sparkles size={13} /> DEMO DATA</span>
            <p>Deterministic fixture · no daemon is attached and no agent is running.</p>
          </div>
        )}

        {/* Focusable (issue #1492): it is the dashboard's scroll region, and a region the keyboard cannot reach cannot be scrolled from it. */}
        <section className="overview-body" aria-label="Agent dashboard" ref={bodyRef} tabIndex={0}>
          {notice}
          {newAgentNotice && (
            <div className="overview-banner" role="status" data-testid="overview-new-agent-notice">
              <span>{newAgentNotice}</span>
              <button type="button" aria-label="Dismiss" onClick={() => setNewAgentNotice(undefined)}><X size={13} /></button>
            </div>
          )}
          {generationNotice && (
            <div className="overview-banner" role="status" data-testid="overview-config-notice">
              <span>{displayText(generationNotice, DISPLAY_LIMITS.message)}</span>
              <button type="button" aria-label="Dismiss" onClick={() => setGenerationNotice(undefined)}><X size={13} /></button>
            </div>
          )}
          {cards.notice && (
            <div className="overview-banner" role="status" data-testid="overview-card-notice">
              <span>{displayText(cards.notice, DISPLAY_LIMITS.message)}</span>
              <button type="button" aria-label="Dismiss" onClick={cards.dismissNotice}><X size={13} /></button>
            </div>
          )}
          {daemonNotice && (
            <div className="overview-banner" role="status" data-testid="overview-daemon-notice">
              <span>{displayText(daemonNotice, DISPLAY_LIMITS.message)}</span>
              <button type="button" aria-label="Dismiss" onClick={() => setDaemonNotice(undefined)}><X size={13} /></button>
            </div>
          )}
          {/*
            One `daemon-group` per observed deck (PRD #742 M4). The section was
            already the outer unit when there was exactly one, and the comment
            that stood here said #742's second deck "becomes a sibling here and
            changes no inner component" — which is what happened: this is a
            `.map`, and `DaemonBody` below took a `fleetSize` for its note
            density and nothing else.
          */}
          {cards.shown && <AgentCardsNotes cards={cardsShown} needsYouOnly={cards.needsYouOnly} cardsView={cards.mode === "cards"} uncounted={uncountedDecks} />}
          {/* PRD #2374 — in the Cards view every deck keeps its own section and
              header, with the daemon's own controls; a deck whose daemon has
              cards shows them as the section's body, and one with no cards (or
              that is not answering) keeps its usual body. */}
          {fleetMockState() && fleetMockView() === "agent" ? <AgentViewMock deckId={FIXTURE_DAEMON_ID} composer={fleetMockComposer()} /> : fleetMockState() && fleetMockView() === "fleet" ? <FleetViewMock state={fleetMockState()!} deckId={FIXTURE_DAEMON_ID} /> : shownDecks.map((deck) => (
            <DeckGroup
              /*
                PRD #742 M5: the KEY, not the label. `socketPath` is
                `Endpoint::describe()`, which two daemons on one host share — so
                two sibling sections took one React key and React kept one of
                them. `deck.key`, because a daemon that has not reported an id
                yet still needs a key of its own.
              */
              key={deck.key}
              deck={deck}
              cardsBody={cardsBodyFor(deck)}
              now={now}
              columns={columns}
              filtering={filtering}
              fleetSize={decks.length}
              onOpenDeck={openDeck}
              onAddDaemon={addDaemon}
              onReconnect={() => {
                setOverrideError(undefined);
                setStartError(deck.snapshot.connection.deckId, undefined);
                // Issue #1994: this deck's section, so this deck alone.
                void runtime.reconnect(deck.snapshot.connection.deckId);
              }}
              onStartDaemon={mode === "live" && deck.snapshot.connection.disconnectedReason?.action === "start-daemon" ? () => requestStartDaemon(deck.snapshot.connection) : undefined}
              startError={startErrors.get(startErrorKey(deck.snapshot.connection.deckId))}
              overrideError={overrideError && deck.snapshot.connection.deckId === overrideError.deckId ? overrideError.message : undefined}
              onConnectAnyway={mode === "live" && deck.snapshot.connection.buildStampMismatchOnly ? () => requestConnectAnyway(deck.snapshot.connection) : undefined}
              /*
                PRD #1487 D8/D9: Upgrade on THIS deck's card, offered when the
                crate says its daemon runs an older release than the app — on
                the local deck too since issue #1636, as a restart onto this
                app's build — and, on the local deck since issue #1753, when
                this machine's CLI is behind, whatever the daemon runs.
              */
              {...deckUpgrade(deck.snapshot.connection)}
              {...deckStop(deck.snapshot.connection)}
              onReplaceDaemon={replaceOffered(deck.snapshot.connection) ? () => requestReplaceDaemon(deck.snapshot.connection) : undefined}
              onNewAgent={newAgentAvailable && deck.connected && deck.snapshot.connection.deckId !== undefined && deckUnavailableReason(deck.snapshot.connection) === undefined ? () => VOICE_ACTIONS.openNewAgent.run(voiceContext, { preselectDeckId: deck.snapshot.connection.deckId }) : undefined}
              hostOpen={deck.snapshot.connection.deckId !== undefined && hostOpen === deck.snapshot.connection.deckId}
              onOpenHost={() => VOICE_ACTIONS.showHostMetrics.run(voiceContext, { preselectDeckId: deck.snapshot.connection.deckId })}
              onCloseHost={() => { if (deck.snapshot.connection.deckId !== undefined) closeHostMetrics(deck.snapshot.connection.deckId); }}
              interruptedBanner={interruptedBannerFor(deck)}
              moreItems={deckMoreItems(deck)}
            />
          ))}
          {/* PR #2491 review — last in the scroll region, so it sticks to the region's foot while cards are selected and, scrolled to the end, sits below the last card rather than over it. */}
          {cards.mode === "cards" && cards.selection && <AgentCardsSelectionBar selection={cards.selection} controls={cards.clickControls} editor={selectionEditor} />}
        </section>
      </main>
      {cards.review && <AgentCardReviewDialog review={cards.review} controls={cards.clickControls} />}
      {cards.batch && <AgentCardBatchDialog view={cards.batch} controls={cards.clickControls} daemonName={batchDaemonName} />}
      {cardActionsEditor.view && <AgentCardActionsEditor view={cardActionsEditor.view} editor={cardActionsEditor} />}
      {interruptedDialogView}
      {confirm && <ConfirmDialog state={confirm} onClose={() => setConfirm(undefined)} />}
      {upgrade && <UpgradeDialog target={upgrade} runtime={runtime} onClose={() => setUpgrade(undefined)} />}
      {upgradeAllOpen && <UpgradeAllDialog runtime={runtime} onClose={() => setUpgradeAllOpen(false)} />}
      {localUpgrade && <LocalUpgradeFlow {...localUpgrade} runtime={runtime} onClose={() => { setLocalUpgrade(undefined); selfUpgrade?.onClosed?.(); }} />}
      {newAgent && newAgentRuntime && (
        <NewAgentDialog
          runtime={newAgentRuntime}
          initialDeckId={newAgent.deckId}
          draft={newAgentDraft}
          closeRequest={newAgentClose}
          voice={newAgentVoice}
          onClose={(draft) => {
            setNewAgentDraft(draft);
            setNewAgent(undefined);
          }}
          onAppeared={(target) => {
            setNewAgentDraft(undefined);
            setNewAgent(undefined);
            // PRD #1223 M5: only now — the deck lists the agent, so the pane's
            // retirement check has a record to find. `target` carries the
            // start's own process (G-fix7), so the view is bound to it rather
            // than to whichever record lists the id now.
            VOICE_ACTIONS.openAgent.run(voiceContext, { ...target, from: "overview" });
          }}
          onNotAppeared={({ deckName: onDeck, agentName }) => {
            setNewAgentDraft(undefined);
            setNewAgent(undefined);
            setNewAgentNotice(`Started ${agentName ? displayText(agentName, DISPLAY_LIMITS.name) : "an agent"} on ${onDeck}, but the daemon has not listed it yet.`);
          }}
        />
      )}
    </div>
  );
  return <OpenAgentContext.Provider value={openAgent}><StopControlsContext.Provider value={stopControls}><RowNumbersContext.Provider value={rowNumbers}><RowCardsContext.Provider value={cards.shown ? cards : undefined}>{overviewScreen}</RowCardsContext.Provider></RowNumbersContext.Provider></StopControlsContext.Provider></OpenAgentContext.Provider>;
}

/** One deck of the fleet, as {@link AgentOverview} prepares it for rendering. */
interface FleetDeck {
  /** Distinct per fleet member ({@link fleetMemberKey}): the React key. */
  key: string;
  snapshot: DeckRuntimeState["snapshot"];
  agents: OverviewAgent[];
  groups: OverviewGroup[];
  /** What the dashboard shows of {@link groups} under the filter (issue #1496). */
  sections: OverviewSection[];
  /** How many of {@link agents} the filter shows. */
  shownCount: number;
  /** The agents the filter shows (all of {@link agents} while none is set): the Cards view shows their cards. */
  matchingAgents: OverviewAgent[];
  /** PR #2491 decision 2026-11-25 — `sections` hold only the rows Needs you only keeps (the Table view). */
  needsYouOnly?: boolean;
  counts: { status: StatusWord; count: number }[];
  connected: boolean;
}

/**
 * Whether the dashboard filter (issue #1496) shows `card`: its agent is one of
 * the deck's `matching` agents. The filter is applied to the cards as it is to
 * the table rows, on the same facts; it changes which cards are shown, never
 * their order or what they say (PRD #2374 review).
 */
function cardMatchesFilter(card: PlacedCard, matching: readonly OverviewAgent[]): boolean {
  return matching.some((agent) => cardIsAgent(card.entry, agent));
}

/**
 * Whether `entry` is the card of `agent`: the same id, listed by the same
 * daemon process when both name one. Agent ids are per-process, so a card from
 * before a restart is not the card of the newer agent that took its id.
 */
function cardIsAgent(entry: Pick<AgentCardEntry, "agent_id" | "instance_id">, agent: Pick<OverviewAgent, "id" | "incarnation">): boolean {
  return agent.id === entry.agent_id
    && (agent.incarnation === undefined || entry.instance_id === "" || entry.instance_id === agent.incarnation);
}

/** One group as the filter leaves it: its matching agents, or a line saying none match. */
interface OverviewSection {
  group: OverviewGroup;
  matching: boolean;
}

/**
 * A fleet member's key: its daemon id, or — for a daemon that has not reported
 * one — its place in the fleet, so two id-less daemons never share a page unit
 * or a React key. The prefixes keep the two kinds from colliding.
 */
function fleetMemberKey(deckId: string | undefined, at: number): string {
  return deckId === undefined ? `at:${at}` : `id:${deckId}`;
}

/**
 * How many of the fleet's daemons are answering, as a sentence (PRD #742 M4).
 *
 * The instrument prints `2/3`, which is the compact reading; this is the hover,
 * and it is where the thing a ratio cannot say gets said — that the counts
 * beside it are over the daemons that answered and not over the fleet.
 */
function decksUpTitle(up: number, total: number): string {
  if (total === 1) return up === 1 ? "The daemon is answering." : "The daemon is not answering, so nothing can be counted.";
  if (up === total) return `All ${total} daemons are answering.`;
  if (up === 0) return `No daemon is answering, so nothing can be counted. ${total} are configured.`;
  return `${up} of ${total} daemons are answering. Every count beside this one is over those ${up}; the daemons that are not answering say so in their own group.`;
}

/**
 * One deck's section: its identity, its state, and either its fleet or the note
 * that says why there isn't one.
 *
 * A deck that is down renders a DEGRADED GROUP — its own header, its own lamp,
 * its own note — and never a group with zero agents, because "this deck runs
 * nothing" and "we cannot see what this deck runs" are different statements and
 * only the first is a number.
 */
/** Issue #1490 — one deck's failed Start daemon: the crate's sentence, and its technical detail. */
interface StartErrorView {
  message: string;
  detail?: string;
}

/** The key a deck's start error is kept under: its id, or `""` for a deck that has not reported one. */
function startErrorKey(deckId: string | undefined): string {
  return deckId ?? "";
}

function DeckGroup({ deck, cardsBody, now, columns, filtering, fleetSize, overrideError, startError, onOpenDeck, onAddDaemon, onReconnect, onStartDaemon, onConnectAnyway, onUpgrade, upgradeLabel, upgradeTitle, onStopDaemon, stopDaemonReason, onReplaceDaemon, onNewAgent, hostOpen, onOpenHost, onCloseHost, interruptedBanner, moreItems }: {
  /** PRD #1790 stage 2 — this deck's interrupted work banner, while its daemon lists any. */
  interruptedBanner?: ReactNode;
  /** PR #2491 decision 2026-11-25 — the items of this daemon's ⋯ menu; no menu while it has none. */
  moreItems?: readonly MoreMenuItem[];
  deck: FleetDeck;
  /** PRD #2374 — in the Cards view, this deck's cards, shown in place of its agent table. */
  cardsBody?: ReactNode;
  now: number;
  columns: OverviewColumnId[];
  /** Whether the dashboard filter is set (issue #1496). */
  filtering: boolean;
  /** How many decks are on screen — the note density, and nothing else. */
  fleetSize: number;
  overrideError?: string;
  /** Issue #1490 — why the last Start daemon on this deck failed. */
  startError?: StartErrorView;
  /** Issue #1490 — Start daemon on this deck. Absent unless its reason offers it. */
  onStartDaemon?: () => void;
  /** Absent while the deck is hidden (issue #1198), which removes this group's Open deck buttons. */
  onOpenDeck?: () => void;
  onReconnect: () => void;
  onConnectAnyway?: () => void;
  /** PRD #1487 — Upgrade this deck's daemon, and on the local deck this machine's CLI (issue #1753). Absent unless something is behind. */
  onUpgrade?: () => void;
  /** Issue #1753 — what Upgrade's button says to a screen reader, when it is not the daemon alone. */
  upgradeLabel?: string;
  /** What hovering Upgrade shows: what is behind. */
  upgradeTitle?: string;
  /** Issue #1073 — Stop this deck's daemon. Absent unless the deck answers and can be stopped. */
  onStopDaemon?: () => void;
  /** Issue #1073 — why Stop daemon cannot act on this deck; renders the control disabled with it. */
  stopDaemonReason?: string;
  /** Issue #1073 — Replace this (local) deck's refused daemon. */
  onReplaceDaemon?: () => void;
  /** Open the New agent flow with THIS deck preselected (PRD #1223 M4). Absent where the deck cannot take a spawn. */
  onNewAgent?: () => void;
  /** PRD #2274 — Add a daemon, in the empty-agents note. Absent unless the local daemon is the only one configured. */
  onAddDaemon?: () => void;
  /** PR #1672 — whether THIS deck's Host popover is the open one, and the two ways to change that. */
  hostOpen: boolean;
  onOpenHost: () => void;
  onCloseHost: () => void;
}) {
  const connection = deck.snapshot.connection;
  /*
    An incompatible deck's note carries Upgrade beside Connect anyway, with the
    sentence that explains it; every other state puts it on the header, so a
    card never shows two.
  */
  const upgradeInNote = connection.status === "error" && connection.daemonDetected === true && connection.deckKind === "remote";
  const socketPath = connection.socketPath;
  const titleId = useId();
  const daemonMessage = connection.message ? displayText(connection.message, DISPLAY_LIMITS.message) : undefined;
  /**
   * Whether the connection message says anything the lamp beside it does not
   * (PRD #745). A healthy connection's message is literally `Deck
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
  const messageSaysSomethingNew = !deck.connected || connection.buildStampMismatchOnly === true;
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
   * Everything hover can say about WHICH deck this is: its socket path, and
   * the two build stamps when they differ.
   *
   * The socket path used to be on screen, shortened to its last segment — a
   * label whose stated purpose was keeping a uid or a username out of
   * screenshots, and which on the default socket of the day read
   * `dot-agent-deck-attach-501.sock`, so it leaked the very uid it was meant to
   * hide and told the reader nothing actionable either way. (Issue #1121 later
   * moved the uid up into the directory — `dot-agent-deck-501/attach.sock` —
   * which would have made the same label read a bare `attach.sock`: no leak,
   * and no information at all.) The path is
   * genuinely diagnostic, so it stays here, where it costs no layout;
   * `data-daemon-id` on the section still carries the identity for tests and a
   * future drill-in.
   *
   * The stamps hover moved here with it, and had to: it used to hang off the
   * connection message, which for a healthy connection no longer renders. On
   * the deck's own name it is more discoverable than it was — a reader hovers
   * a thing they can see.
   */
  const daemonFacts = [socketPath, buildStampsCaveat].filter((fact): fact is string => Boolean(fact));
  // Joined rather than stacked: a sanitised `title` cannot carry a newline —
  // `displayText` strips category `Cc`, and `\n` is in it.
  const daemonIdentityTitle = daemonFacts.length ? displayTitle(daemonFacts.join(" · ")) : undefined;
  return (
    <section
      className={deck.connected ? "daemon-group" : "daemon-group is-degraded"}
      data-testid="daemon-group"
      /*
        The deck's KEY, so a test asserting which section it found cannot be
        fooled by two decks that describe identically (PRD #742 M5). The name a
        reader sees is `daemon-identity` below, which is still the label.
      */
      data-daemon-id={connection.deckId === undefined ? "" : domIdentity(connection.deckId)}
      data-deck-connected={deck.connected ? "yes" : "no"}
      aria-labelledby={titleId}
    >
      <header className="daemon-group-header">
        {/*
          Issue #2406: the row is its own box, so the connection message can
          sit on a line of its own below it instead of competing with the
          name, the Host button, the pips and the buttons for the row's width.
        */}
        <div className="daemon-group-header-row">
        <span className={`connection-lamp connection-${connection.status}`} aria-hidden="true" />
        <div className="daemon-identity">
          {/*
            The socket filename is gone from the layout and lives on
            hover — see `daemonIdentityTitle`, which is also where the
            build stamps disclose themselves.
          */}
          <strong id={titleId} title={daemonIdentityTitle} data-testid="daemon-identity">{deckName(connection)}</strong>
        </div>
        {/* PRD #1258: this deck's host, from its own daemon — a small button beside its name, coloured by the worst reading, that opens the numbers. Every deck in a fleet has its own. */}
        {deck.connected && deck.snapshot.hostMetrics && <HostMetricsButton report={deck.snapshot.hostMetrics} deckLabel={deckName(connection)} open={hostOpen} onOpen={onOpenHost} onClose={onCloseHost} />}
        {deck.connected && deck.agents.length > 0 && (
          <div className="daemon-pips">{deck.counts.map((entry) => (
            <span className={`status-label status-${entry.status}`} key={entry.status}>{entry.count} {statusWordLabel(entry.status)}</span>
          ))}</div>
        )}
        {/*
          PRD #742 M4: an em dash where the pips would be, so a deck whose
          fleet cannot be read reads as UNKNOWN on the same line its
          neighbours read as counted — never as a deck running nothing.
        */}
        {!deck.connected && <span className="daemon-unknown" data-testid="daemon-unknown" title={unknownPipsTitle(connection)}>—</span>}
        {onUpgrade && !upgradeInNote && <button type="button" className="button primary compact daemon-upgrade" data-testid="daemon-upgrade" aria-label={upgradeLabel ?? `Upgrade the daemon on ${connection.deckKind === "remote" ? deckName(connection) : "this machine"}`} title={upgradeTitle} onClick={onUpgrade}><CircleArrowUp size={13} /><span>Upgrade</span></button>}
        {(onStopDaemon || stopDaemonReason) && <button type="button" className="button secondary compact daemon-stop" data-testid="daemon-stop" aria-label={`Stop the daemon on ${connection.deckKind === "remote" ? deckName(connection) : "this machine"}`} title={stopDaemonReason ? displayTitle(stopDaemonReason) : undefined} disabled={!onStopDaemon} onClick={onStopDaemon}><CircleStop size={13} /><span>Stop daemon</span></button>}
        {/* PR #2491 decision 2026-11-25 — this daemon's ⋯ menu, just left of New agent. */}
        {moreItems && moreItems.length > 0 && (
          <MoreMenu
            label={`More for ${deckName(connection)}`}
            menuLabel={`More for ${deckName(connection)}`}
            testId="daemon-more"
            menuTestId="daemon-more-menu"
            className="more-menu daemon-more"
            buttonClassName="button secondary compact more-menu-button"
            icon={<Ellipsis size={13} aria-hidden="true" />}
            items={moreItems}
          />
        )}
        {onNewAgent && <button type="button" className="button secondary compact daemon-new-agent" data-testid="daemon-new-agent" aria-label={`New agent on ${deckName(connection)}`} onClick={onNewAgent}><Plus size={13} /><span>New agent</span></button>}
        </div>
        {/*
          Only when it says something the lamp does not. The element is
          not deleted — in the disconnected, incompatible and
          connected-anyway states it carries the only explanation on the
          header, including the build-mismatch caveat issue #801 requires
          to survive the whole session. It has a line of its own below the
          row and wraps rather than truncating, so the whole sentence reads
          and the row's controls keep their layout whatever its length
          (issue #2406); with no message there is no second line.
        */}
        {messageSaysSomethingNew && <p className="daemon-state" data-testid="daemon-state">{daemonMessage ?? connection.status}</p>}
      </header>

      {/* Issue #1637: this deck's agents' hooks run an older dot-agent-deck. */}
      {deck.connected && <HookBinaryNotices notices={connection.hookBinaryNotices} />}

      {/* PRD #1790 stage 2: what a restart of this deck's daemon left behind. */}
      {deck.connected && interruptedBanner}

      <div className="daemon-group-body">
        {cardsBody ?? <DaemonBody
          agents={deck.agents}
          sections={deck.sections}
          filtering={filtering}
          needsYouOnly={deck.needsYouOnly === true}
          now={now}
          columns={columns}
          connection={connection}
          message={daemonMessage}
          compactNote={fleetSize > 1}
          overrideError={overrideError}
          startError={startError}
          onOpenDeck={onOpenDeck}
          onReconnect={onReconnect}
          onStartDaemon={onStartDaemon}
          onConnectAnyway={onConnectAnyway}
          onUpgrade={upgradeInNote ? onUpgrade : undefined}
          onReplaceDaemon={onReplaceDaemon}
          onNewAgent={onNewAgent}
          onAddDaemon={onAddDaemon}
        />}
      </div>
    </section>
  );
}

/**
 * Why a deck's agent counts read as UNKNOWN (PRD #742 M4; M14 split the
 * pending case out).
 *
 * "Not answering" is a measurement, and for a deck that has simply not reported
 * yet nothing has been measured — the em dash is right either way, but the
 * sentence behind it is a different one. The unconfigured case keeps the
 * general wording: nothing is answering there either, and its own note says
 * what is actually missing.
 */
function unknownPipsTitle(connection: ConnectionView): string {
  if (connection.pending) return "Not known yet — this daemon has not reported, so its agents cannot be counted.";
  return "Not known — this daemon is not answering, so its agents cannot be counted.";
}

function DaemonBody({ agents, sections, filtering, needsYouOnly, now, columns, connection, message, compactNote, overrideError, startError, onOpenDeck, onReconnect, onStartDaemon, onConnectAnyway, onUpgrade, onReplaceDaemon, onNewAgent, onAddDaemon }: {
  agents: OverviewAgent[];
  sections: OverviewSection[];
  /** PR #2491 decision 2026-11-25 — `sections` are what Needs you only keeps in the Table view. */
  needsYouOnly: boolean;
  filtering: boolean;
  /** The one instant every relative cell on this screen is measured against. */
  now: number;
  /** The chosen columns, in grid order — one list for every card. */
  columns: OverviewColumnId[];
  connection: ConnectionView;
  message?: string;
  /**
   * Whether this deck is one of several on screen (PRD #742 M4). A note is the
   * WHOLE of a single-deck screen, so it is laid out as a full panel; in a
   * fleet it is one deck's line among its neighbours', and a 260px panel per
   * unreachable deck would push every healthy deck off the screen.
   */
  compactNote?: boolean;
  overrideError?: string;
  /** Issue #1490 — why the last Start daemon on this deck failed, in the crate's words, and its technical detail. */
  startError?: StartErrorView;
  /** Absent while the deck is hidden (issue #1198): no Open deck button, and no sentence sending the user to it. */
  onOpenDeck?: () => void;
  onReconnect: () => void;
  /** Issue #1490 — offered in place of Reconnect when the deck's reason says no daemon runs. */
  onStartDaemon?: () => void;
  /** Absent unless the mismatch is stamp-only — see `requestConnectAnyway`. */
  onConnectAnyway?: () => void;
  /** PRD #1487 — Upgrade, in the incompatible note. Absent unless offered. */
  onUpgrade?: () => void;
  /** Issue #1073 — Replace daemon, in the local deck's incompatible note. Absent unless offered. */
  onReplaceDaemon?: () => void;
  /** The New agent flow on this deck (PRD #1223 M4) — what the first-run note offers. */
  onNewAgent?: () => void;
  /** PRD #2274 — Add a daemon, which the first-run note offers while this is the only daemon configured. */
  onAddDaemon?: () => void;
}) {
  const noteClass = compactNote ? "overview-note is-compact" : "overview-note";
  /*
    PRD #742 M14, and BEFORE the `loading` branch below because it is a narrower
    case of the same status. That one is the APP establishing its control
    channel — the pre-connect seed, one deck, nothing on screen yet. This is one
    member of a fleet that is already on screen: the others have their tables,
    and this one is a group with a name, a place in the denominator and nothing
    inside it yet.

    It is stated as what it is rather than as a failure, in the register the
    overview's empty state uses, and it offers no button — unlike the
    disconnected note, there is nothing for the reader to do and Reconnect would
    only hurry one deck that is already coming.
  */
  if (connection.pending) {
    return (
      <OverviewNote className={noteClass} testId="overview-pending" icon={<RefreshCw className="spin" size={24} />} title="Waiting for this daemon">
        <p>{message ?? DECK_STATE_FALLBACK.pending}</p>
        <p className="overview-note-hint">It is counted in the fleet's total and not among the daemons that answered, because nothing has answered for it yet. Its agents appear here as soon as it reports.</p>
      </OverviewNote>
    );
  }

  if (connection.status === "loading") {
    return (
      <OverviewNote className={noteClass} testId="overview-loading" icon={<RefreshCw className="spin" size={24} />} title="Establishing control channel">
        <p>Reading the daemon's agent list. Nothing is attached while this runs.</p>
      </OverviewNote>
    );
  }

  /*
    PRD #742 M12, and BEFORE the disconnected branch because it is a narrower
    case of the same status. A configured deck with no socket path yet is not
    a deck that stopped answering — nothing was asked of it — so "no deck is
    listening on the configured socket" and "start one from the deck screen,
    then reconnect" are both false, and Reconnect is a button that cannot help.
    It says what is actually missing and points at the panel that fills it in,
    in the same words #741's selector uses for the same state.

    It is still a group, still in the fleet's denominator and still never in
    `decksUp` — which is the whole of what this milestone closes. Before it, a
    socketless row was absent from the overview entirely and three configured
    decks read as `2/2`.
  */
  if (connection.unconfigured) {
    return (
      <OverviewNote className={noteClass} testId="overview-unconfigured" icon={<ShieldAlert size={24} />} title="Daemon not configured">
        <p>{message ?? DECK_STATE_FALLBACK.unconfigured}</p>
        <p className="overview-note-hint">Nothing has been asked of this daemon, so it counts toward the fleet without counting as one that answered.</p>
      </OverviewNote>
    );
  }

  if (connection.status === "disconnected") {
    /*
      Issue #1490: ONE remedy, the one the deck's reason names — Start daemon
      when no daemon runs there, Reconnect otherwise. A deck without a reason
      (fixture data, an older snapshot) keeps Reconnect and the old hint.
    */
    const reason = connection.disconnectedReason;
    const offersStart = reason?.action === "start-daemon" && onStartDaemon !== undefined;
    return (
      <OverviewNote className={noteClass} testId="overview-disconnected" icon={<ShieldAlert size={24} />} title="Daemon disconnected">
        <p>{reason ? displayText(reason.message, DISPLAY_LIMITS.message) : message ?? DECK_STATE_FALLBACK.disconnected}</p>
        <p className="overview-note-hint">Nothing can be said about the fleet until a daemon answers, so this list is blank rather than stale.{reason ? "" : onOpenDeck ? " Start one from the Daemons screen, then reconnect." : " Start one, then reconnect."}</p>
        {/*
          PR #1623 review: the reason's sentence is the headline, and the
          connection's own error stays visible — on the card's header line
          (`daemon-state`), so the disclosure carries the details beside it.
        */}
        <ConnectionDetail detail={reason ? disconnectedDetails(connection, { message: false }) : undefined} />
        {/* Issue #1472: a deck that stopped answering during Connect anyway lands here, and the reason must land with it. */}
        {overrideError && <p className="overview-note-hint" data-testid="overview-connect-anyway-error">{overrideError}</p>}
        {startError && (
          <div role="alert" data-testid="overview-start-error">
            <p className="overview-note-hint">{startError.message}</p>
            <ConnectionDetail detail={startError.detail} />
          </div>
        )}
        <div>
          {onOpenDeck && <button className="button secondary" onClick={onOpenDeck}><SquareTerminal size={14} /> Open daemons</button>}
          {offersStart
            ? <button className="button primary" data-testid="start-daemon" onClick={onStartDaemon}><Play size={14} /> Start daemon</button>
            : <button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button>}
        </div>
      </OverviewNote>
    );
  }

  /*
    An `error` no daemon answered is the app's own bridge failing — a connect
    or subscription that failed before any handshake — not a daemon this build
    refused. Calling it incompatible, or advising a version update, would send
    the user after a mismatch that does not exist (Qodo on PR #1451). Same
    title as the Daemons-screen banner gives it.
  */
  if (connection.status === "error" && !connection.daemonDetected) {
    return (
      <OverviewNote className={noteClass} testId="overview-bridge-error" icon={<ShieldAlert size={24} />} title="Desktop bridge error">
        <p>{message ?? "The app could not open its connection to this daemon."}</p>
        <p className="overview-note-hint">The app could not read this daemon's agents, so nothing is listed rather than guessed. Reconnect tries again.</p>
        <div>
          <button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button>
        </div>
      </OverviewNote>
    );
  }

  if (connection.status === "error") {
    return (
      <OverviewNote className={noteClass} testId="overview-incompatible" icon={<ShieldAlert size={24} />} title="Incompatible daemon">
        <p>{message ?? DECK_STATE_FALLBACK.incompatible}</p>
        <p className="overview-note-hint">
          {connection.runningAgentCount === undefined
            ? "This app cannot show this daemon's agents until the two match, so nothing is listed rather than guessed."
            : `The daemon reports ${connection.runningAgentCount} running ${connection.runningAgentCount === 1 ? "agent" : "agents"}, but this app cannot show ${connection.runningAgentCount === 1 ? "it" : "them"} until the two match, so nothing is listed rather than guessed.`}
          {" "}
          {/* Names exactly the buttons rendered below, with what each does (CLAUDE.md rule 21). */}
          {incompatibleRemedy(connection, { upgrade: Boolean(onUpgrade), replaceDaemon: Boolean(onReplaceDaemon), openDaemons: Boolean(onOpenDeck), connectAnyway: Boolean(onConnectAnyway), reconnect: true })}
        </p>
        <ConnectionDetail detail={connection.detail} />
        {overrideError && <p className="overview-note-hint" data-testid="overview-connect-anyway-error">{overrideError}</p>}
        <div>
          {onOpenDeck && <button className="button secondary" onClick={onOpenDeck}><SquareTerminal size={14} /> Open daemons</button>}
          {onUpgrade && <button className="button primary" data-testid="overview-upgrade" onClick={onUpgrade}><CircleArrowUp size={14} /> Upgrade</button>}
          {onReplaceDaemon && <button className="button primary" data-testid="overview-replace" onClick={onReplaceDaemon}><RefreshCw size={14} /> Replace daemon</button>}
          {onConnectAnyway && <button className="button primary" data-testid="overview-connect-anyway" onClick={onConnectAnyway}><ShieldAlert size={14} /> Connect anyway</button>}
          <button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button>
        </div>
      </OverviewNote>
    );
  }

  /*
    Issue #1496 — a daemon none of whose agents the filter shows is one line.
    Before the first-run note, because a daemon with no agents is one the filter
    shows none of: while filtering it reads like every other empty daemon. The
    connection notes above stay first — they say why nothing can be listed.
  */
  /* PR #2491 decision 2026-11-25 — under Needs you only, a daemon none of whose agents needs the user is one line too. */
  if (needsYouOnly && !sections.length) {
    return <p className="overview-filter-empty" data-testid="overview-needs-you-empty">{filterDaemonName(connection)}: no agent needs you</p>;
  }
  if (filtering && !sections.some((section) => section.matching)) {
    return <p className="overview-filter-empty" data-testid="overview-filter-empty">{filterDaemonName(connection)}: no matching agents</p>;
  }

  // Issue #1965: a deck whose only orchestration roles have exited still has
  // their rows to show.
  if (!agents.length && !sections.some((section) => section.group.endedRoles?.length)) {
    /*
      One full-width row rather than a centred card (issue #2312 follow-up):
      a healthy daemon with nothing to show is not worth a third of the
      window, and the green lamp in the group's header already says it is
      healthy. The words are PRD #2274's plain ones, run together as one hint.
      No "Open daemons" here: it opened the Daemons screen for the SELECTED
      daemon, not this group's, so under All daemons it landed on a "choose a
      daemon" note; with experimental features on, the top bar and the rail
      keep that door. The Orchestrations panel is named only where the screen
      it lives on is shown (issue #1198); a sentence pointing at a hidden
      screen is a door that is not there.
    */
    const next = onNewAgent
      ? onOpenDeck ? "Create an agent on this daemon, or activate an orchestration from the Daemons screen's Orchestrations panel." : "Create an agent on this daemon."
      : onOpenDeck ? "Activate an orchestration from the Daemons screen's Orchestrations panel, or start an agent from the CLI in a project directory." : "Start an agent from the CLI in a project directory.";
    return (
      <div className="overview-note overview-note-row" data-testid="overview-first-run">
        <Blocks size={22} aria-hidden="true" />
        <div className="overview-note-row-text">
          <h3>No agents are running yet</h3>
          <p className="overview-note-hint">This daemon has no agents yet. {next} Agents appear here as soon as they start.</p>
          {onAddDaemon && <p className="overview-note-hint" data-testid="overview-first-run-add-daemon-hint">Running agents on another machine? Add it as a daemon and its agents show here too.</p>}
        </div>
        {onNewAgent && <button className="button primary compact" data-testid="overview-first-run-new-agent" onClick={onNewAgent}><Plus size={14} /> New agent</button>}
        {onAddDaemon && <button className="button secondary compact" data-testid="overview-first-run-add-daemon" onClick={onAddDaemon}><Server size={14} /> Add a daemon</button>}
      </div>
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
          {sections.map(({ group, matching }) => (matching
            ? <OverviewGroupCard key={group.key} group={group} now={now} columns={columns} />
            : <p key={group.key} className="overview-filter-empty" data-testid={`overview-group-empty-${group.key}`}>{displayIdentity(group.title, DISPLAY_LIMITS.name, unnamedGroupLabel(group))}: no matching agents</p>))}
        </div>
      </div>
    </div>
  );
}

/**
 * Issue #1496 — a daemon as the dashboard filter names it: by the name it was
 * given, where it has one, else as the dashboard's section heading names it.
 */
function filterDaemonName(connection: ConnectionView): string {
  return connection.name ? displayIdentity(connection.name, DISPLAY_LIMITS.name, deckName(connection)) : deckName(connection);
}

/**
 * Issue #1496 — the dashboard filter's controls in the header: a text box
 * matched against each agent's name, role, orchestration, directory and last
 * prompt, and a menu of the other four facets. Every change writes the window
 * session's filter, which the dashboard and voice both read.
 */
function DashboardFilterControls({ filter, daemons }: { filter: DashboardFilter; daemons: { id: string; label: string }[] }) {
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const toggle = <K extends "kinds" | "statuses" | "agentTypes" | "daemonIds">(facet: K, value: DashboardFilter[K][number]) => {
    const values = filter[facet] as string[];
    const next = values.includes(value) ? values.filter((entry) => entry !== value) : [...values, value];
    setDashboardFilter({ ...filter, [facet]: next });
  };
  /* Dismissed the way the Columns menu is, for the reasons given there. */
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      setOpen(false);
    };
    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") dismiss(event);
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", dismissOnEscape, true);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", dismissOnEscape, true);
    };
  }, [open]);
  const facets: { facet: "kinds" | "statuses" | "agentTypes" | "daemonIds"; title: string; options: readonly { id: string; label: string }[] }[] = [
    { facet: "kinds", title: "Kind", options: DASHBOARD_KINDS },
    { facet: "statuses", title: "Status", options: DASHBOARD_STATUSES },
    { facet: "agentTypes", title: "Agent type", options: DASHBOARD_AGENT_TYPES },
    { facet: "daemonIds", title: "Daemon", options: daemons },
  ];
  const chosen = filter.kinds.length + filter.statuses.length + filter.agentTypes.length + filter.daemonIds.length;
  return (
    <div
      className="dashboard-filter overview-columns-picker"
      ref={root}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || !open) return;
        setOpen(false);
        event.stopPropagation();
      }}
    >
      <input
        type="text"
        className="dashboard-filter-text"
        data-testid="dashboard-filter-text"
        aria-label="Filter agents"
        placeholder="Filter agents"
        maxLength={DASHBOARD_FILTER_TEXT_MAX}
        value={filter.text}
        onChange={(event) => setDashboardFilter({ ...filter, text: event.target.value })}
      />
      <button
        className="button secondary compact"
        data-testid="dashboard-filter-toggle"
        aria-expanded={open}
        aria-haspopup="true"
        title="Show only agents of a kind, status, agent type or daemon"
        onClick={() => setOpen((wasOpen) => !wasOpen)}
      >
        <Filter size={14} /><span>{chosen ? `Filter (${chosen})` : "Filter"}</span>
      </button>
      {open && (
        <div className="overview-columns-menu dashboard-filter-menu" data-testid="dashboard-filter-menu" role="group" aria-label="Filter">
          {facets.filter(({ options }) => options.length > 0).map(({ facet, title, options }) => (
            <fieldset key={facet}>
              <legend>{title}</legend>
              {options.map((option) => (
                <label key={option.id}>
                  <input
                    type="checkbox"
                    data-testid={`dashboard-filter-${facet}-${domIdentity(option.id)}`}
                    checked={(filter[facet] as string[]).includes(option.id)}
                    onChange={() => toggle(facet, option.id as never)}
                  />
                  <span>{option.label}</span>
                </label>
              ))}
            </fieldset>
          ))}
          <button type="button" className="overview-columns-reset" data-testid="dashboard-filter-clear" onClick={() => setDashboardFilter(clearDashboardFilter())}>
            <RotateCcw size={12} /><span>Clear filter</span>
          </button>
        </div>
      )}
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

    The listeners are bound only while the menu is open and removed when it
    closes, not merely on unmount — `open` is in the dependency list, so React
    runs the cleanup on the same transition that hides the menu.
  */
  /*
    Escape dismisses wherever the key was raised, for the same reason a pointer
    does (issue #957). The root's own `onKeyDown` below sees the key only while
    focus is INSIDE the picker, and what is supposed to put it there is the
    click that opened the menu — which is the engine-dependent part. Measured
    during the browser tier's bring-up, a real click leaves
    `document.activeElement` on the trigger in Chromium and in Playwright's
    WebKit. WKWebView, which is what the packaged macOS app renders in, was not
    measured either way, and Safari on macOS is documented not to focus a
    `<button>` on click; WebKitGTK under Tauri is the distribution's build and
    moves independently of Playwright's pinned one. #957 was filed as a RISK
    rather than a defect — no failure was seen while that tier was built, and
    none has been reported — and this listener removes the dependency rather
    than settling the question, which the manual macOS walkthrough in
    `docs/develop/desktop-gui.md` or the driver-level tier in #953 would.

    CAPTURE phase, and that is the difference between "wherever the key was
    raised" and "wherever nothing else reached it first". Sibling controls in
    this same top bar stop Escape at their own root — `DeckSelector` does it on
    every Escape, whether or not its own menu is open — so a bubble-phase
    listener here would never see a press made while focus sits inside one of
    them, and the menu would stay open with nothing but the mouse to shut it.
    Capturing at `document` runs this ahead of every element-level handler on
    the page.

    It reuses the pointer rule wholesale, INCLUDING the inside-the-root
    exemption, which is what keeps the two paths from fighting: an Escape
    raised inside the picker is left to the root handler below, which closes
    the menu and stops the key propagating so the app-wide Escape in `App.tsx`
    does not act on the same press. Outside the root the key is deliberately
    NOT stopped — focus is somewhere else, possibly in an overlay that wants
    that press, and a dismissal nobody can see must not eat it.
  */
  useEffect(() => {
    if (!open) return;
    const dismiss = (event: Event) => {
      if (event.target instanceof Node && root.current?.contains(event.target)) return;
      setOpen(false);
    };
    const dismissOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      dismiss(event);
    };
    document.addEventListener("pointerdown", dismiss);
    document.addEventListener("keydown", dismissOnEscape, true);
    return () => {
      document.removeEventListener("pointerdown", dismiss);
      document.removeEventListener("keydown", dismissOnEscape, true);
    };
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
  const stopControls = useContext(StopControlsContext);
  const groupName = displayIdentity(group.title, DISPLAY_LIMITS.name, unnamedGroupLabel(group));
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
          <span className={`status-label status-${entry.status}`} key={entry.status}>{entry.count} {statusWordLabel(entry.status)}</span>
        ))}</div>
        {/* PRD #1223 U4: the TUI's Ctrl+W on this orchestration's tab — every role, after a confirmation that names them. */}
        {/* Issue #1965: a group whose roles have all exited has no agent left to stop. */}
        {stopControls && group.kind === "orchestration" && group.agents.length > 0 && (
          <button
            type="button"
            className="button secondary compact overview-close-orchestration"
            data-testid="overview-close-orchestration"
            aria-label={`Close ${groupName} orchestration`}
            title="Close every role of this orchestration"
            onClick={() => stopControls.closeOrchestration(group)}
          ><X size={12} /><span>Close</span></button>
        )}
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
          {group.endedRoles?.map((role) => <EndedRoleRow key={`ended-${role.agentId}`} role={role} />)}
        </tbody>
      </table>
    </article>
  );
}

/**
 * Issue #1965 — an orchestration role whose agent exited: which role, the
 * `Exited` status, why, and the last lines its pane showed. A row with no
 * control: there is no terminal to open, since the role's agent is gone, so it
 * carries nothing to click, and the voice surfaces have nothing to reach
 * (CLAUDE.md rule 23). Spans every column, whichever are on screen.
 */
function EndedRoleRow({ role }: { role: EndedRole }) {
  const name = displayIdentity(role.roleName, DISPLAY_LIMITS.name, `role ${role.roleIndex + 1}`);
  const reason = displayText(role.reason, DISPLAY_LIMITS.title);
  return (
    <tr className="overview-row overview-row-ended" role="row" data-status="failed" data-testid="overview-ended-role" title={displayText(role.headline, DISPLAY_LIMITS.title)}>
      <td role="cell" className="overview-ended-cell">
        <div className="overview-ended-head">
          <strong className="overview-ended-name">{name}</strong>
          <span className="status-label status-failed">{ENDED_ROLE_WORDS.status}</span>
          <span className="overview-ended-reason">{reason}</span>
        </div>
        {role.lastOutput.length > 0 && (
          <div className="overview-ended-output">
            <span className="overview-ended-output-label">{ENDED_ROLE_WORDS.lastOutput}:</span>
            <pre>{role.lastOutput.map((line) => displayText(line, DISPLAY_LIMITS.title)).join("\n")}</pre>
          </div>
        )}
      </td>
    </tr>
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
  const shownLease = shownWriteLease(agent.writeLease);
  /*
    PRD #1105 M5 — the only thing this milestone adds to a card, and the limit
    is deliberate: #745's commitment is that these rows stay terminal-free, so
    the row gains a way to NAVIGATE — the row itself, by a click or by Enter
    or Space while it has focus (see `openFromRow`) — and gains nothing that
    renders output. The row's accessible name is built from `name` rather than
    from `agent.displayName`, so the sanitised, clamped copy is what a screen
    reader announces and a hostile display name cannot spell a different one.

    **Every row opens, whichever deck its agent is on.** This screen merges
    every observed deck, and a tile's terminal is always the SELECTED deck's —
    so a pane opened for a non-selected deck's agent attaches nothing. That is
    a fact about the pane and it is the pane that states it: it opens with an
    explicit no-terminal state naming the deck rather than with a blank
    terminal. Gating the open here instead was tried and reverted, because a
    listed agent with no way to open it is a dead row and nothing on the row
    tells the reader which deck they are pointing at. The deck SWITCH is the
    thing that stays withdrawn (see the PRD's decision 5), and attaching
    without switching is a wire change and is
    [#1073](https://github.com/vfarcic/dot-agent-deck/issues/1073).
  */
  const openAgent = useContext(OpenAgentContext);
  const stopControls = useContext(StopControlsContext);
  const openPullRequest = useContext(OpenPullRequest);
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
            <span className={`agent-state-mark status-${statusWord(agent)}`} aria-hidden="true" />
            <span className={`status-label status-${statusWord(agent)}`}>{statusWordLabel(statusWord(agent))}</span>
            {/* Issue #2387: the tile's marker, beside the status it never replaces. */}
            {agent.holdWords && <span className="held-marker" title={agent.holdWords}>{INBOX_WORDS.attentionHeld}</span>}
          </td>
        );
      case "displayName":
        return (
          <td className="overview-agent-name" role="cell" key={column}>
            {orchestration && <em className="overview-role-index" title={`Role ${orchestration.roleIndex} of this orchestration`}>{String(orchestration.roleIndex + 1).padStart(2, "0")}</em>}
            <strong>{name}</strong>
            {orchestration?.isStartRole && <span className="coordinator-badge" title="Orchestration start role — the agent an operator messages">ORCHESTRATOR</span>}
            {/*
              The write lease the daemon reported (PRD #745 M8), shown only when
              input typed here would not reach the agent (issue #2353,
              `shownWriteLease`). A `write` lease and a daemon that declared no
              live target both render nothing: `toOverviewAgent` reversed the
              latter's sentinel to absent, and absence is NOT read as read-only.
            */}
            {shownLease && <span className={`overview-lease lease-${shownLease}`} title={WRITE_LEASE_TITLE[shownLease]}>{shownLease}</span>}
            {roleName && !rendersBlank(roleName) && roleLabel?.toLowerCase() !== agent.displayName.toLowerCase() && <em className="overview-role-name">{roleName}</em>}
            {/*
              PRD #1401 — the same badge the agent's tile and screen show, and
              the same click: it opens the PR in the in-app browser directly,
              over this screen, rather than opening the agent first. It is a
              button, so a click on it never also opens the agent's pane (see
              `clickLandedOnRowControl`).
            */}
            {agent.pullRequest && (
              <PullRequestBadge
                pullRequest={agent.pullRequest}
                testId="overview-pr-badge"
                onOpen={openPullRequest && agent.pullRequest.url ? () => openPullRequest({ deckId: agent.daemonId, agentId: agent.id }) : undefined}
              />
            )}
            {/*
              Issue #2454 — the daemon's Needs you, beside the name rather than
              among the controls at the end of the cell: the name column is the
              table's permanent column, so the badge shows whichever columns are
              hidden, and the controls no longer move when it comes and goes. It
              is a label, not a control: a click on it opens the agent like the
              rest of the row.
            */}
            {rowCard?.entry.needs_you && <span className="overview-needs-you" data-testid="overview-needs-you" title={needsYouSentence(rowCard.entry)}>Needs you</span>}
            {(stopControls || (rowCard && rowCards)) && (
              <span className="overview-row-actions">
                {/*
                  PRD #1223 U4 — stop this one agent, on the deck it is on. On the
                  ROW rather than in the pane overlay: the row is where the overview
                  already puts an agent's one action, it is reachable without
                  opening anything, and stopping from inside the pane would close
                  the pane under the reader the moment the deck stopped listing it.
                */}
                {stopControls && (
                  <button
                    className="overview-stop-agent"
                    data-testid="overview-stop-agent"
                    aria-label={`Close ${name} agent`}
                    title={`Close ${name}`}
                    onClick={() => stopControls.stopAgent(agent)}
                  ><CircleStop size={12} /></button>
                )}
                {rowCard && rowCards && <OverviewRowCard card={rowCard} name={name} rowKey={agentDomKey(agent)} open={rowCards.rowMenu === agentDomKey(agent)} controls={rowCards.controls} />}
              </span>
            )}
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
          The BINARY this agent runs, as the DAEMON reported it (issue #856). It
          used to print the serialised wire identity, so Claude Code read
          `claude_code` and OpenCode read `open_code` — neither a name anybody
          would type — while `codex` happened to be right; PRD #745 fixed that
          by resolving the name from the agent registry, and #856 moved the
          resolution to the side of the wire that forked the process.

          Empty when the daemon named no binary, and deliberately NOT a word
          standing in for one — exactly what the uptime and activity cells above
          do with an unreported value. There is no local registry lookup to fall
          back to here, which is the point: a fallback would reinstate the
          divergence #856 closed.

          The hover is the full value and nothing else: the column header
          already says what it is, and a sentence restating it would be the
          screen describing itself.
        */
        return <td className="overview-cli" role="cell" key={column} title={agent.cli && displayTitle(agent.cli)}>{agent.cli ? displayText(agent.cli, DISPLAY_LIMITS.name) : ""}</td>;
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
  /*
    The whole row opens the pane: a click anywhere on it, or Enter or Space while
    the row has focus (issue #2454, which removed the row's separate Open icon).
    A click handler on the `<tr>` rather than a `<button>` around it, which would
    be invalid HTML with the row's own controls inside and would announce the
    entire row, controls and all, as one button name. It is also a click handler
    rather than a stretched link: a transparent layer stretched over the row
    would sit on top of every cell, so the working directory and prompt could no
    longer be selected, and each cell's `title` (the full path, the full prompt,
    the exact instants) would never show on hover. The row is ONE Tab stop per
    agent, named "Open <name> agent", as the icon was; a key pressed on one of
    the row's own controls (Close, the ⋯ menu, the PR badge) is that control's,
    and never also opens the row.
  */
  /* PR #1451 round 3, change 3 — the number this row shows while voice is
     on, first in the row and first in its accessible name. */
  const number = useContext(RowNumbersContext)?.get(agentKey(agent));
  /* PRD #2374 — this row's card, while the cards are shown. */
  const rowCards = useContext(RowCardsContext);
  const rowCard = rowCards?.cardFor(agent.daemonId, agent.id, agent.incarnation);
  const openFromRow = openAgent && ((event: MouseEvent<HTMLTableRowElement>) => {
    if (clickLandedOnRowControl(event) || clickFinishedSelection(event.currentTarget)) return;
    openAgent(agent);
  });
  const openFromRowKey = openAgent && ((event: ReactKeyboardEvent<HTMLTableRowElement>) => {
    if (!rowKeyOpens(event)) return;
    event.preventDefault();
    openAgent(agent);
  });
  const rowName = openAgent ? `Open ${name} agent` : name;
  return (
    <tr
      className={openAgent ? "overview-row is-openable" : "overview-row"}
      role="row"
      data-testid={`overview-agent-${agentDomKey(agent)}`}
      data-status={agent.status}
      tabIndex={openAgent ? 0 : undefined}
      aria-label={number === undefined ? (openAgent ? rowName : undefined) : `${number}. ${rowName}`}
      data-voice-number={number}
      onClick={openFromRow}
      onKeyDown={openFromRowKey}
    >
      {columns.map((column, at) => (at === 0 && number !== undefined ? numberedCell(cell(column), number) : cell(column)))}
    </tr>
  );
}

/** `cell` with the row's voice number before its content, so the number leads the row whichever column is first. */
function numberedCell(cell: ReactNode, number: number): ReactNode {
  if (!isValidElement<{ children?: ReactNode }>(cell)) return cell;
  return cloneElement(cell, undefined, <VoiceNumber key="voice-number" number={number} hidden />, ...Children.toArray(cell.props.children));
}

/**
 * Everything inside a row that can do a job of its own, so a click on it is
 * left to that job: Close, the ⋯ menu and the PR badge each do their own, and
 * letting the click reach the row as well would also open the pane. The
 * selector is wider than those controls on purpose: a control added to a row
 * later gets its own click without having to remember `stopPropagation`. The
 * row itself carries a `tabindex` and so matches too, which is why the match
 * must be a control INSIDE the row.
 */
const ROW_CONTROL_SELECTOR = "a[href], button, input, select, textarea, label, summary, [contenteditable], [tabindex], [role='button'], [role='link']";

/**
 * Issue #2454 — whether a key opens the row: Enter or Space pressed on the row
 * itself, with no modifier. A key pressed on one of the row's own controls
 * bubbles here too, and is that control's: the target is the row only when the
 * row itself has focus.
 */
function rowKeyOpens(event: ReactKeyboardEvent<HTMLTableRowElement>): boolean {
  if (event.target !== event.currentTarget) return false;
  if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey || event.repeat) return false;
  return event.key === "Enter" || event.key === " ";
}

function clickLandedOnRowControl(event: MouseEvent<HTMLTableRowElement>): boolean {
  if (!(event.target instanceof Element)) return false;
  const control = event.target.closest(ROW_CONTROL_SELECTOR);
  return control !== null && control !== event.currentTarget && event.currentTarget.contains(control);
}

/**
 * Whether this click is the mouse-up that finished a text selection in the row.
 * A drag across a working directory or a prompt still ends in a `click` on the
 * row, and someone copying a path is not asking to open a pane.
 *
 * It reads the selection rather than measuring the drag: the selection is the
 * thing that must survive, and a drag distance is only a guess at it, one that
 * misses a short selection and flags a long drag that selected nothing. A plain
 * click leaves a collapsed caret, which this does not count. The check is
 * scoped to ranges touching THIS row, so a selection somewhere else on the
 * screen cannot make a row refuse to open.
 *
 * Selecting by double-click is not covered: its first click is an ordinary
 * click and opens the pane before the second one arrives.
 */
function clickFinishedSelection(row: HTMLTableRowElement): boolean {
  const selection = window.getSelection();
  if (!selection || selection.isCollapsed) return false;
  for (let index = 0; index < selection.rangeCount; index += 1) {
    if (selection.getRangeAt(index).intersectsNode(row)) return true;
  }
  return false;
}

function OverviewNote({ className, testId, icon, title, children }: { className?: string; testId: string; icon: ReactNode; title: string; children: ReactNode }) {
  return <div className={className ?? "overview-note"} data-testid={testId}>{icon}<h3>{title}</h3>{children}</div>;
}

/** One counter in the dashboard's counters popover: its label and its value. */
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
