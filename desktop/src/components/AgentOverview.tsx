import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Blocks, Boxes, LayoutList, Layers, Network, RefreshCw, ShieldAlert, Sparkles, SquareTerminal, Wrench } from "lucide-react";
import type { AgentSession, AgentStatus, ConnectionView, DeckRuntimeState, DeckView } from "../types";
import { ConfirmDialog, type ConfirmState } from "./ConfirmDialog";
import { DISPLAY_LIMITS, displayActivity, displayIdentity, displayPath, displayText, displayTitle, domIdentity, rendersBlank, shortDaemonLabel } from "../lib/displayText";

/**
 * The honest subset of `AgentSession`: every field a daemon genuinely reports
 * AND the overview actually renders, and nothing else. The overview renders
 * from THIS and never from `AgentSession`, so reaching for a value the daemon
 * cannot supply — `model`, `tokens`, `cost`, `contextPercent`, `worktree`,
 * `attempt`, `duration` — is a compile error rather than a thing to remember.
 * `duration` stays out while `lastActivityMs` is in, and the line between them
 * is honesty rather than taste: `started_at` is invented as `now` on hydration
 * so a duration resets under a restarted daemon, whereas `last_activity` is a
 * high-water mark of observed event timestamps and reads ABSENT when the daemon
 * cannot vouch for it (PRD #745 M9).
 * `role` is deliberately absent even though it is honest: a row shows
 * `displayName` and, inside an orchestration, `tab.roleName`, so carrying
 * `role` here would claim a consumption that does not exist. The field-by-field
 * reasoning lives on `AgentSession` itself; PRD #745's "Columns" table is the
 * decision.
 */
export type OverviewAgent = Pick<
  AgentSession,
  "id" | "daemonId" | "displayName" | "cli" | "status" | "activeTool" | "activeToolDetail" | "toolCount" | "tab" | "lastUserPrompt" | "lastActivityMs"
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
  const { id, daemonId, displayName, cli, status, cwd, activeTool, activeToolDetail, toolCount, tab, lastUserPrompt, lastActivityMs, writeLease } = agent;
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
 * The column headers, in grid order. The visible legend mirrors these, and so do
 * the `nth-child` rules in `styles.css`'s two overview media queries — which are
 * INDEX-SENSITIVE, so inserting a column here means renumbering them. Nine
 * columns as of PRD #745 M9, which put Last activity fourth: it is read
 * alongside State, because "waiting" and "waiting, and quiet for two hours" are
 * different situations and a reader should not have to scan across the row to
 * tell them apart.
 */
const COLUMNS = ["Status", "Agent", "State", "Last activity", "CLI", "Active tool", "Tools", "Working directory", "Last prompt"];

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
  const agents = useMemo(() => snapshot.agents.map(toOverviewAgent), [snapshot.agents]);
  const groups = useMemo(() => groupAgents(agents), [agents]);
  const counts = useMemo(() => countByStatus(agents), [agents]);
  const countOf = (status: AgentStatus) => counts.find((entry) => entry.status === status)?.count ?? 0;
  const openDeck = () => onNavigate({ kind: "deck" });
  const socketPath = connection.socketPath;
  const daemonMessage = connection.message ? displayText(connection.message, DISPLAY_LIMITS.message) : undefined;
  /**
   * Issue #801. Since the crate stopped refusing a daemon that names the same
   * release, the ordinary case is two builds from different commits connecting
   * with nothing on screen — which is the point, but it also means the
   * difference had nowhere left to be seen. A `title` is the whole trace:
   * available on hover, absent from the layout, and deliberately NOT an alert.
   * A real compatibility break still gets the banner and Connect anyway.
   */
  const buildStampsTitle = connection.clientBuildVersion && connection.daemonBuildVersion
    && connection.clientBuildVersion !== connection.daemonBuildVersion
    ? displayTitle(`Built from different commits — desktop ${connection.clientBuildVersion}, daemon ${connection.daemonBuildVersion}.`)
    : undefined;
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
            <div className="branch-line"><span title="Every agent this desktop can see, grouped the way the daemon groups them.">every agent this desktop can see · this screen opens no terminal</span></div>
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
                <strong id="daemon-group-title">Local daemon</strong>
                {/*
                  A socket path routinely embeds a uid or a username, so the
                  header carries a short label and keeps the full path on hover.
                  `data-daemon-id` above carries the identity, sanitised and
                  bounded: it is a marker for tests and a future drill-in, not a
                  key, so nothing depends on it being byte-for-byte raw — and
                  `daemonId` has no length limit below the protocol frame.
                */}
                <code title={socketPath ? displayTitle(socketPath) : undefined}>
                  {socketPath ? shortDaemonLabel(socketPath) : "socket path not reported"}
                </code>
              </div>
              <p className="daemon-state" data-testid="daemon-state" title={buildStampsTitle}>{daemonMessage ?? connection.status}</p>
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
                connection={connection}
                message={daemonMessage}
                overrideError={overrideError}
                onOpenDeck={openDeck}
                onReconnect={() => void runtime.reconnect()}
                onConnectAnyway={mode === "live" && connection.buildStampMismatchOnly ? requestConnectAnyway : undefined}
              />
            </div>
          </section>

          <p className="overview-footnote">
            This screen reads one snapshot, opens no terminal of its own, and asks the bridge to release the ones the deck left
            attached — a socket a previous screen opened is torn down as that release lands, not the instant this line renders.
            Model, token, cost, context-window, branch, attempt and duration columns are absent because the daemon does not
            track them — see PRD #745.
          </p>
        </section>
      </main>
      {confirm && <ConfirmDialog state={confirm} onClose={() => setConfirm(undefined)} />}
    </div>
  );
}

function DaemonBody({ agents, groups, connection, message, overrideError, onOpenDeck, onReconnect, onConnectAnyway }: {
  agents: OverviewAgent[];
  groups: OverviewGroup[];
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
    <>
      {/*
        Decoration, not structure: one legend for the whole fleet so the group
        cards read as one table. The header association a screen reader needs
        comes from each group's own `<thead>`, which is visually hidden rather
        than repeated four times down the page.
      */}
      <div className="overview-legend" aria-hidden="true">
        <span />
        <span>AGENT</span>
        <span>STATE</span>
        <span>LAST ACTIVITY</span>
        <span>CLI</span>
        <span>ACTIVE TOOL</span>
        <span>TOOLS</span>
        <span>WORKING DIRECTORY</span>
        <span>LAST PROMPT</span>
      </div>
      <div className="overview-groups">
        {groups.map((group) => <OverviewGroupCard key={group.key} group={group} />)}
      </div>
    </>
  );
}

function OverviewGroupCard({ group }: { group: OverviewGroup }) {
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
            {COLUMNS.map((column) => <th key={column} role="columnheader" scope="col">{column}</th>)}
          </tr>
        </thead>
        <tbody className="overview-rows" role="rowgroup">
          {group.agents.map((agent) => <OverviewRow key={agentDomKey(agent)} agent={agent} hoistedCwd={hoistedCwd} />)}
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

function OverviewRow({ agent, hoistedCwd }: { agent: OverviewAgent; hoistedCwd?: string }) {
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
  // Read once per render against ONE `Date.now()`, so every row on a repaint is
  // relative to the same moment rather than to fifteen slightly different ones.
  const activity = displayActivity(agent.lastActivityMs);
  return (
    <tr className="overview-row" role="row" data-testid={`overview-agent-${agentDomKey(agent)}`} data-status={agent.status}>
      <td role="cell"><span className={`agent-state-mark status-${agent.status}`} aria-hidden="true" /></td>
      <td className="overview-agent-name" role="cell">
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
      <td role="cell"><span className={`status-label status-${agent.status}`}>{agent.status}</span></td>
      {/*
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
      */}
      <td className="overview-activity" role="cell" title={activity && `Last activity reported by the daemon: ${activity.title}`}>{activity?.label ?? ""}</td>
      <td className="overview-cli" role="cell" title={displayText(`Agent type reported by the daemon: ${agent.cli}`, DISPLAY_LIMITS.title)}>{displayText(agent.cli, DISPLAY_LIMITS.name)}</td>
      <td className="overview-tool" role="cell">
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
      <td className="overview-tool-count" role="cell" title={`${agent.toolCount} tool calls reported`}>{agent.toolCount}</td>
      {/*
        Blank for a directory the group header already states, and blank again
        when the daemon reported none — an empty cell says "nothing to add
        here" in both cases, which is exactly what is true.
      */}
      <td className="overview-cwd" role="cell" title={agent.cwd ? displayTitle(agent.cwd) : undefined}>{!agent.cwd || agent.cwd === hoistedCwd ? "" : displayPath(agent.cwd)}</td>
      {/*
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
      */}
      <td className="overview-prompt" role="cell" title={agent.lastUserPrompt ? displayTitle(agent.lastUserPrompt) : undefined}>
        {agent.lastUserPrompt ? displayText(agent.lastUserPrompt, DISPLAY_LIMITS.prompt) : ""}
      </td>
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
