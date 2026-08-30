import { useMemo, type ReactNode } from "react";
import { Blocks, Boxes, LayoutList, Layers, Network, RefreshCw, ShieldAlert, Sparkles, SquareTerminal, Wrench } from "lucide-react";
import { UNREPORTED } from "../types";
import type { AgentSession, AgentStatus, ConnectionView, DeckRuntimeState, DeckView } from "../types";
import { DISPLAY_LIMITS, displayPath, displayText, displayTitle, shortDaemonLabel } from "../lib/displayText";

/**
 * The honest subset of `AgentSession`: every field a daemon genuinely reports
 * AND the overview actually renders, and nothing else. The overview renders
 * from THIS and never from `AgentSession`, so reaching for a value the daemon
 * cannot supply — `model`, `tokens`, `cost`, `contextPercent`, `worktree`,
 * `attempt`, `duration` — is a compile error rather than a thing to remember.
 * `role` is deliberately absent even though it is honest: a row shows
 * `displayName` and, inside an orchestration, `tab.roleName`, so carrying
 * `role` here would claim a consumption that does not exist. The field-by-field
 * reasoning lives on `AgentSession` itself; PRD #745's "Columns" table is the
 * decision.
 */
export type OverviewAgent = Pick<
  AgentSession,
  "id" | "daemonId" | "displayName" | "cli" | "status" | "activeTool" | "activeToolDetail" | "toolCount" | "tab"
> & {
  /**
   * HONEST, and OPTIONAL where `AgentSession.cwd` is not. The `Pick<>` above
   * closes dishonest field NAMES but cannot close a dishonest sentinel inside
   * an allowed one: `agentFromDto` substitutes `UNREPORTED` for a `cwd` the
   * daemon did not report, so the screen's guarantee that nothing on it reads
   * "Unavailable" was violable through this very field. Absence is preserved
   * here and rendered as nothing at all — not as a placeholder, and not as a
   * dash, which would be one more thing to read that says less than blank.
   */
  cwd?: string;
};

export function toOverviewAgent(agent: AgentSession): OverviewAgent {
  const { id, daemonId, displayName, cli, status, cwd, activeTool, activeToolDetail, toolCount, tab } = agent;
  return { id, daemonId, displayName, cli, status, cwd: cwd === UNREPORTED ? undefined : cwd, activeTool, activeToolDetail, toolCount, tab };
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

export type OverviewGroupKind = "orchestration" | "mode" | "standalone";

export interface OverviewGroup {
  id: string;
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
  agents: OverviewAgent[];
}

/**
 * Groups by the daemon's own tab buckets: one group per orchestration with its
 * roles in `roleIndex` order, one per mode name, and one standalone bucket for
 * dashboard panes. Orchestrations and modes keep first-appearance order;
 * standalone is always last because it is the bucket of things that belong to
 * nothing.
 */
export function groupAgents(agents: OverviewAgent[]): OverviewGroup[] {
  const orchestrations = new Map<string, OverviewGroup>();
  const modes = new Map<string, OverviewGroup>();
  const standalone: OverviewAgent[] = [];

  for (const agent of agents) {
    if (agent.tab.kind === "orchestration") {
      // `orchestrationId` is optional on the wire. Falling back to the name
      // would merge two distinct orchestrations that happen to share one into a
      // single card with colliding role indexes, so an id-less agent gets a key
      // unique to itself instead: worst case it reads as its own group, which
      // is honest, rather than as somebody else's role.
      const id = agent.tab.orchestrationId ?? `agent:${agentKey(agent)}`;
      const group = orchestrations.get(id) ?? {
        id,
        kind: "orchestration" as const,
        title: agent.tab.displayTitle || agent.tab.name,
        subtitle: agent.tab.displayTitle ? agent.tab.name : undefined,
        agents: [],
      };
      group.agents.push(agent);
      orchestrations.set(id, group);
    } else if (agent.tab.kind === "mode") {
      const group = modes.get(agent.tab.name) ?? { id: agent.tab.name, kind: "mode" as const, title: agent.tab.name, agents: [] };
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
    ...orchestrations.values(),
    ...modes.values(),
    ...(standalone.length ? [{ id: "standalone", kind: "standalone" as const, title: "Standalone agents", agents: standalone }] : []),
  ];
  for (const group of groups) group.commonCwd = commonCwdOf(group.agents);
  return groups;
}

/**
 * The working directory the largest number of members share, or `undefined`
 * when no two of them share one. Ties go to whichever appears first, so the
 * answer does not depend on map iteration order.
 */
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

/** The column headers, in grid order. The visible legend mirrors these. */
const COLUMNS = ["Status", "Agent", "State", "CLI", "Active tool", "Tools", "Working directory"];

/**
 * The fleet at a glance: every agent the desktop can see, grouped the way the
 * daemon groups them, described only by things that are actually true — and
 * with no terminal anywhere on it. "Shows no output" and "opens no PTY" are
 * separate properties (PRD #745); this component owns the first, and mounting
 * no `TerminalViewport` is what its tests assert.
 *
 * Every daemon-supplied string on this screen goes through `lib/displayText`
 * before it reaches React — rendered text and `title` attributes alike — while
 * grouping, sorting and keys keep the raw values.
 */
export function AgentOverview({ runtime, onNavigate }: { runtime: DeckRuntimeState; onNavigate: (view: DeckView) => void }) {
  const { snapshot, mode } = runtime;
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
            <div className="branch-line"><span title="Every agent this desktop can see, grouped the way the daemon groups them.">every agent this desktop can see · no terminals attached</span></div>
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
          <section className="daemon-group" data-testid="daemon-group" data-daemon-id={socketPath ?? ""} aria-labelledby="daemon-group-title">
            <header className="daemon-group-header">
              <span className={`connection-lamp connection-${connection.status}`} aria-hidden="true" />
              <div className="daemon-identity">
                <strong id="daemon-group-title">Local daemon</strong>
                {/*
                  A socket path routinely embeds a uid or a username, so the
                  header carries a short label and keeps the full path on hover.
                  `data-daemon-id` above stays the raw identity key.
                */}
                <code title={socketPath ? displayTitle(socketPath) : undefined}>
                  {socketPath ? shortDaemonLabel(socketPath) : "socket path not reported"}
                </code>
              </div>
              <p className="daemon-state">{daemonMessage ?? connection.status}</p>
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
                onOpenDeck={openDeck}
                onReconnect={() => void runtime.reconnect()}
              />
            </div>
          </section>

          <p className="overview-footnote">
            This screen reads one snapshot and attaches nothing. Model, token, cost, context-window, branch, attempt and duration
            columns are absent because the daemon does not track them — see PRD #745.
          </p>
        </section>
      </main>
    </div>
  );
}

function DaemonBody({ agents, groups, connection, message, onOpenDeck, onReconnect }: {
  agents: OverviewAgent[];
  groups: OverviewGroup[];
  connection: ConnectionView;
  message?: string;
  onOpenDeck: () => void;
  onReconnect: () => void;
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
        </p>
        <div>
          <button className="button secondary" onClick={onOpenDeck}><SquareTerminal size={14} /> Open deck</button>
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
        <span>CLI</span>
        <span>ACTIVE TOOL</span>
        <span>TOOLS</span>
        <span>WORKING DIRECTORY</span>
      </div>
      <div className="overview-groups">
        {groups.map((group) => <OverviewGroupCard key={group.id} group={group} />)}
      </div>
    </>
  );
}

function OverviewGroupCard({ group }: { group: OverviewGroup }) {
  const Icon = GROUP_ICON[group.kind];
  const counts = countByStatus(group.agents);
  const titleId = `overview-group-title-${group.id}`;
  return (
    <article className="overview-group" data-testid={`overview-group-${group.id}`} data-group-kind={group.kind} aria-labelledby={titleId}>
      <header className="overview-group-header">
        <Icon size={14} aria-hidden="true" />
        <div className="overview-group-identity">
          <span className="section-kicker">{GROUP_KICKER[group.kind]}</span>
          <h3 id={titleId}>{displayText(group.title, DISPLAY_LIMITS.name)}</h3>
        </div>
        {group.subtitle && <code className="overview-group-subtitle">{displayText(group.subtitle, DISPLAY_LIMITS.name)}</code>}
        {group.commonCwd && (
          <code className="overview-group-cwd" title={displayText(`Most of this group works in ${group.commonCwd} — a row prints its own directory only when it differs`, DISPLAY_LIMITS.title)}>
            {displayPath(group.commonCwd)}
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
          {group.agents.map((agent) => <OverviewRow key={agentKey(agent)} agent={agent} commonCwd={group.commonCwd} />)}
        </tbody>
      </table>
    </article>
  );
}

function OverviewRow({ agent, commonCwd }: { agent: OverviewAgent; commonCwd?: string }) {
  const orchestration = agent.tab.kind === "orchestration" ? agent.tab : undefined;
  // Only an orchestration's role name says something the other columns do not.
  // Outside one, the daemon derives the role from the agent type, so showing it
  // here would just restate the CLI column.
  const roleLabel = orchestration?.roleName;
  const name = displayText(agent.displayName, DISPLAY_LIMITS.name);
  return (
    <tr className="overview-row" role="row" data-testid={`overview-agent-${agentKey(agent)}`} data-status={agent.status}>
      <td role="cell"><span className={`agent-state-mark status-${agent.status}`} aria-hidden="true" /></td>
      <td className="overview-agent-name" role="cell">
        {orchestration && <em className="overview-role-index" title={`Role ${orchestration.roleIndex} of this orchestration`}>{String(orchestration.roleIndex + 1).padStart(2, "0")}</em>}
        <strong>{name}</strong>
        {orchestration?.isStartRole && <span className="coordinator-badge" title="Orchestration start role — the agent an operator messages">COORDINATOR</span>}
        {roleLabel && roleLabel.toLowerCase() !== agent.displayName.toLowerCase() && <em className="overview-role-name">{displayText(roleLabel, DISPLAY_LIMITS.name)}</em>}
      </td>
      <td role="cell"><span className={`status-label status-${agent.status}`}>{agent.status}</span></td>
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
      <td className="overview-cwd" role="cell" title={agent.cwd ? displayTitle(agent.cwd) : undefined}>{!agent.cwd || agent.cwd === commonCwd ? "" : displayPath(agent.cwd)}</td>
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
