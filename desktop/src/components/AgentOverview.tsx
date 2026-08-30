import { useMemo, type ReactNode } from "react";
import { Blocks, Boxes, LayoutList, Layers, Network, RefreshCw, ShieldAlert, Sparkles, SquareTerminal, Wrench } from "lucide-react";
import type { AgentSession, AgentStatus, ConnectionView, DeckRuntimeState, DeckView } from "../types";

/**
 * The honest subset of `AgentSession`: every field a daemon genuinely reports
 * and nothing else. The overview renders from THIS and never from
 * `AgentSession`, so reaching for a value the daemon cannot supply — `model`,
 * `tokens`, `cost`, `contextPercent`, `worktree`, `attempt`, `duration` — is a
 * compile error rather than a thing to remember. The field-by-field reasoning
 * lives on `AgentSession` itself; PRD #745's "Columns" table is the decision.
 */
export type OverviewAgent = Pick<
  AgentSession,
  "id" | "daemonId" | "role" | "displayName" | "cli" | "status" | "cwd" | "activeTool" | "activeToolDetail" | "toolCount" | "tab"
>;

export function toOverviewAgent(agent: AgentSession): OverviewAgent {
  const { id, daemonId, role, displayName, cli, status, cwd, activeTool, activeToolDetail, toolCount, tab } = agent;
  return { id, daemonId, role, displayName, cli, status, cwd, activeTool, activeToolDetail, toolCount, tab };
}

/**
 * Agents are keyed by the composite `(daemonId, agentId)` from day one. Agent
 * ids are per-daemon monotonic integers starting at 1, so two daemons both mint
 * `"1"` and any bare-id key is wrong the moment #742 connects a second daemon.
 * `encodeURIComponent` escapes `:`, so the join stays unambiguous for a daemon
 * id that is itself a socket path.
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
   * Set when every member shares one working directory, which is the common
   * case for an orchestration. The group then states it once instead of
   * repeating the same long path down six rows.
   */
  sharedCwd?: string;
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
      const id = agent.tab.orchestrationId ?? agent.tab.name;
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
  for (const group of groups) group.sharedCwd = sharedCwdOf(group.agents);
  return groups;
}

function sharedCwdOf(agents: OverviewAgent[]): string | undefined {
  const first = agents[0]?.cwd;
  return first && agents.every((agent) => agent.cwd === first) ? first : undefined;
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
 * The fleet at a glance: every agent the desktop can see, grouped the way the
 * daemon groups them, described only by things that are actually true — and
 * with no terminal anywhere on it. "Shows no output" and "opens no PTY" are
 * separate properties (PRD #745); this component owns the first, and mounting
 * no `TerminalViewport` is what its tests assert.
 */
export function AgentOverview({ runtime, onNavigate }: { runtime: DeckRuntimeState; onNavigate: (view: DeckView) => void }) {
  const { snapshot, mode } = runtime;
  const connection = snapshot.connection;
  const agents = useMemo(() => snapshot.agents.map(toOverviewAgent), [snapshot.agents]);
  const groups = useMemo(() => groupAgents(agents), [agents]);
  const counts = useMemo(() => countByStatus(agents), [agents]);
  const countOf = (status: AgentStatus) => counts.find((entry) => entry.status === status)?.count ?? 0;
  const openDeck = () => onNavigate({ kind: "deck" });

  return (
    <div className="control-deck overview-screen">
      <aside className="rail" aria-label="Primary navigation">
        <div className="brand-mark" aria-label="Agent Deck"><span>AD</span><i aria-hidden="true" /></div>
        <nav>
          <OverviewRailButton icon={SquareTerminal} label="Deck" onClick={openDeck} testId="open-deck" />
          <OverviewRailButton icon={LayoutList} label="Overview" active onClick={() => onNavigate({ kind: "overview" })} testId="open-overview" />
        </nav>
        <div className="rail-bottom">
          <span className={`connection-lamp connection-${connection.status}`} title={connection.message} />
        </div>
      </aside>

      <main className="deck-main">
        <header className="topbar">
          <div className="repo-context">
            <div className="repo-line"><LayoutList size={15} /><strong>Agent overview</strong></div>
            <div className="branch-line"><span title="Every agent this desktop can see, grouped the way the daemon groups them.">every agent this desktop can see · no terminals attached</span></div>
          </div>
          <div className="run-instruments">
            <OverviewInstrument label="AGENTS" testId="overview-count-agents"><strong>{agents.length}</strong></OverviewInstrument>
            <OverviewInstrument label="RUNNING"><strong className="count-running">{countOf("running")}</strong></OverviewInstrument>
            <OverviewInstrument label="WAITING"><strong className="count-waiting">{countOf("waiting")}</strong></OverviewInstrument>
            <OverviewInstrument label="FAILED"><strong className="count-failed">{countOf("failed")}</strong></OverviewInstrument>
            <OverviewInstrument label="GROUPS"><strong>{groups.length}</strong></OverviewInstrument>
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
          <section className="daemon-group" data-testid="daemon-group" data-daemon-id={connection.socketPath ?? ""} aria-labelledby="daemon-group-title">
            <header className="daemon-group-header">
              <span className={`connection-lamp connection-${connection.status}`} aria-hidden="true" />
              <div className="daemon-identity">
                <strong id="daemon-group-title">Local daemon</strong>
                <code>{connection.socketPath ?? "socket path not reported"}</code>
              </div>
              <p className="daemon-state">{connection.message ?? connection.status}</p>
              {connection.status === "connected" && agents.length > 0 && (
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

function DaemonBody({ agents, groups, connection, onOpenDeck, onReconnect }: {
  agents: OverviewAgent[];
  groups: OverviewGroup[];
  connection: ConnectionView;
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
        <p>{connection.message ?? "No dot-agent-deck daemon is listening on the configured socket."}</p>
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
        <p>{connection.message ?? "A daemon answered but this build cannot speak to it."}</p>
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
          <h3 id={titleId}>{group.title}</h3>
        </div>
        {group.subtitle && <code className="overview-group-subtitle">{group.subtitle}</code>}
        {group.sharedCwd && <code className="overview-group-cwd" title={`Every agent in this group is working in ${group.sharedCwd}`}>{group.sharedCwd}</code>}
        <span className="overview-group-count">{group.agents.length} {group.agents.length === 1 ? "agent" : "agents"}</span>
        <div className="overview-group-pips">{counts.map((entry) => (
          <span className={`status-label status-${entry.status}`} key={entry.status}>{entry.count} {entry.status}</span>
        ))}</div>
      </header>
      <ul className="overview-rows">
        {group.agents.map((agent) => <OverviewRow key={agentKey(agent)} agent={agent} sharedCwd={group.sharedCwd} />)}
      </ul>
    </article>
  );
}

function OverviewRow({ agent, sharedCwd }: { agent: OverviewAgent; sharedCwd?: string }) {
  const orchestration = agent.tab.kind === "orchestration" ? agent.tab : undefined;
  // Only an orchestration's role name says something the other columns do not.
  // Outside one, `role` is derived from the agent type, so showing it here
  // would just restate the CLI column.
  const roleLabel = orchestration?.roleName;
  return (
    <li className="overview-row" data-testid={`overview-agent-${agentKey(agent)}`} data-status={agent.status}>
      <span className={`agent-state-mark status-${agent.status}`} aria-hidden="true" />
      <span className="overview-agent-name">
        {orchestration && <em className="overview-role-index" title={`Role ${orchestration.roleIndex} of this orchestration`}>{String(orchestration.roleIndex + 1).padStart(2, "0")}</em>}
        <strong>{agent.displayName}</strong>
        {orchestration?.isStartRole && <span className="coordinator-badge" title="Orchestration start role — the agent an operator messages">COORDINATOR</span>}
        {roleLabel && roleLabel.toLowerCase() !== agent.displayName.toLowerCase() && <em className="overview-role-name">{roleLabel}</em>}
      </span>
      <span className={`status-label status-${agent.status}`}>{agent.status}</span>
      <span className="overview-cli" title={`Agent type reported by the daemon: ${agent.cli}`}>{agent.cli}</span>
      <span className="overview-tool">
        {agent.activeTool ? (
          <>
            <Wrench size={11} aria-hidden="true" />
            <strong>{agent.activeTool}</strong>
            {agent.activeToolDetail && <em title={agent.activeToolDetail}>{agent.activeToolDetail}</em>}
          </>
        ) : <span className="overview-tool-idle">no active tool</span>}
      </span>
      <span className="overview-tool-count" title={`${agent.toolCount} tool calls reported`}>{agent.toolCount}</span>
      {/* Stated once in the group header when the whole group shares it. */}
      <span className="overview-cwd" title={agent.cwd}>{agent.cwd === sharedCwd ? "" : agent.cwd}</span>
    </li>
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
