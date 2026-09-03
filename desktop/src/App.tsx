import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  Activity,
  AlertTriangle,
  ArrowRight,
  Blocks,
  BookMarked,
  Bot,
  Check,
  CheckCircle2,
  ChevronRight,
  CircleStop,
  Command,
  FolderGit2,
  Gauge,
  GitBranch,
  HelpCircle,
  History,
  Keyboard,
  LayoutList,
  Network,
  PanelRight,
  Pause,
  Play,
  RefreshCw,
  Search,
  Send,
  Settings2,
  ShieldAlert,
  SlidersHorizontal,
  Sparkles,
  SquareTerminal,
  X,
  Zap,
} from "lucide-react";
import { AgentOverview } from "./components/AgentOverview";
import { AgentTile } from "./components/AgentTile";
import { ConfirmDialog, type ConfirmState } from "./components/ConfirmDialog";
import { HandoffRail } from "./components/HandoffRail";
import { ProfilesPanel, ProjectsPanel, PromptLibraryPanel, WorkflowPanel } from "./components/ConfigurationPanels";
import { useAgentProfiles } from "./hooks/useAgentProfiles";
import { useDeckRuntime } from "./hooks/useDeckRuntime";
import { useProjects } from "./hooks/useProjects";
import { usePromptLibrary } from "./hooks/usePromptLibrary";
import { desktopWorkflowPlatformIssue } from "./lib/platform";
import type { DeckAction, DeckActionResult, DeckRuntimeState, DeckView, EvidenceItem, PanelTab, WorkflowLaunchConfig } from "./types";
import { modeScopedKey } from "./lib/bridge";

const WORKFLOW_STORAGE_KEY = modeScopedKey("dot-agent-deck.desktop.workflow-preview.v1");
const DESKTOP_EVIDENCE_QUERY = "(min-width: 1260px)";

function evidenceOpenOnFirstLoad(): boolean {
  if (typeof window === "undefined") return true;
  if (typeof window.matchMedia === "function") return window.matchMedia(DESKTOP_EVIDENCE_QUERY).matches;
  return window.innerWidth >= 1260;
}

export default function App() {
  return <DeckShell runtime={useDeckRuntime()} />;
}

/**
 * Owns which top-level surface is mounted. The overview renders *instead of*
 * the deck, so the state belongs above `ControlDeck` rather than as one more
 * boolean inside it — none of the deck's own `useState` booleans is a view, and
 * five of the rail's six buttons are overlay toggles over an always-mounted
 * deck. `DeckView` is a discriminated union from the start so PRD #745
 * iteration 3's group and single-agent views arrive as added variants.
 *
 * The deck stays the default: launching the app lands exactly where it does
 * today.
 */
export function DeckShell({ runtime, workflowPlatformIssue, initialView = { kind: "deck" } }: { runtime: DeckRuntimeState; workflowPlatformIssue?: string; initialView?: DeckView }) {
  const [view, setView] = useState<DeckView>(initialView);
  if (view.kind === "overview") return <AgentOverview runtime={runtime} onNavigate={setView} />;
  return <ControlDeck runtime={runtime} workflowPlatformIssue={workflowPlatformIssue} onNavigate={setView} />;
}

export function ControlDeck({ runtime, workflowPlatformIssue = desktopWorkflowPlatformIssue(), onNavigate }: { runtime: DeckRuntimeState; workflowPlatformIssue?: string; onNavigate?: (view: DeckView) => void }) {
  const { snapshot, mode, setShownTerminals } = runtime;
  const [selectedAgentId, setSelectedAgentId] = useState("");
  const [tabs, setTabs] = useState<Record<string, PanelTab>>({});
  const [selectedEvidenceId, setSelectedEvidenceId] = useState("");
  const [evidenceOpen, setEvidenceOpen] = useState(evidenceOpenOnFirstLoad);
  const [projectsOpen, setProjectsOpen] = useState(false);
  const [selectedProjectId, setSelectedProjectId] = useState("");
  const [profilesOpen, setProfilesOpen] = useState(false);
  const [promptsOpen, setPromptsOpen] = useState(false);
  const [selectedPromptId, setSelectedPromptId] = useState("");
  const [composerFocus, setComposerFocus] = useState<{ agentId: string; token: number }>();
  const [workflowOpen, setWorkflowOpen] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [helpOpen, setHelpOpen] = useState(false);
  const [notice, setNotice] = useState<string>();
  const [confirm, setConfirm] = useState<ConfirmState>();
  const { profiles, updateProfile, resetProfiles } = useAgentProfiles(snapshot.profiles);
  const { projects, activeId: activeProjectId, activeProject, setActiveId: setActiveProjectId, addProject, updateProject, removeProject } = useProjects(snapshot.worktree, snapshot.repo);
  const { prompts, addPrompt, updatePrompt, removePrompt } = usePromptLibrary();
  const [profileOrder, setProfileOrder] = useState<string[]>([]);

  useEffect(() => {
    if (!selectedAgentId || !snapshot.agents.some((agent) => agent.id === selectedAgentId)) {
      setSelectedAgentId(snapshot.agents[0]?.id ?? "");
    }
  }, [selectedAgentId, snapshot.agents]);

  useEffect(() => {
    if (!selectedEvidenceId || !snapshot.evidence.some((item) => item.id === selectedEvidenceId)) {
      setSelectedEvidenceId(snapshot.evidence[0]?.id ?? "");
    }
  }, [selectedEvidenceId, snapshot.evidence]);

  useEffect(() => {
    if (!selectedProjectId || !projects.some((project) => project.id === selectedProjectId)) {
      setSelectedProjectId(activeProjectId || projects[0]?.id || "");
    }
  }, [activeProjectId, projects, selectedProjectId]);

  useEffect(() => {
    if (!selectedPromptId || !prompts.some((prompt) => prompt.id === selectedPromptId)) {
      setSelectedPromptId(prompts[0]?.id ?? "");
    }
  }, [prompts, selectedPromptId]);

  useEffect(() => {
    if (!profiles.length || profileOrder.length) return;
    try {
      const stored = JSON.parse(window.localStorage.getItem(WORKFLOW_STORAGE_KEY) ?? "null") as { order?: string[] } | null;
      setProfileOrder(stored?.order?.length ? stored.order : profiles.map((profile) => profile.id));
    } catch {
      setProfileOrder(profiles.map((profile) => profile.id));
    }
  }, [profileOrder.length, profiles]);

  useEffect(() => {
    if (!profileOrder.length) return;
    try {
      window.localStorage.setItem(WORKFLOW_STORAGE_KEY, JSON.stringify({ order: profileOrder }));
    } catch {
      // Keep the workflow preview usable when storage is unavailable.
    }
  }, [profileOrder]);

  /**
   * Every tile currently rendering a terminal (PRD #745 M7). `AgentTile` mounts
   * a terminal whenever its tab is `"terminal"`, which is also the default, so
   * the derivation has to repeat that `?? "terminal"` fallback exactly.
   */
  const shownTerminals = snapshot.agents
    .filter((agent) => (tabs[agent.id] ?? "terminal") === "terminal")
    .map((agent) => agent.id);

  /**
   * The same ids joined into one dependency so the effect below fires when the
   * SET changes rather than on every render. It is a dependency KEY and nothing
   * else — never split back apart. Agent ids are raw daemon identities, not
   * display strings, so an id containing a newline would come back out of a
   * `split` as two shown agents: two attach paths from one tile, and one
   * invalid identity turned into several valid-looking ones behind
   * `validate_agent_id`, which rejects control characters only when it is
   * handed the id whole.
   */
  const shownTerminalKey = shownTerminals.join("\n");
  /**
   * The array the effect actually passes on, held in a ref because the effect
   * must key on the joined string (stable across renders that change nothing)
   * while carrying the ids themselves (a fresh array every render).
   */
  const shownTerminalsRef = useRef(shownTerminals);
  shownTerminalsRef.current = shownTerminals;

  /**
   * ONE call per render commit carrying ALL the shown ids, never one call per
   * tile: `setShownTerminals` is declarative, so nine tiles declaring
   * themselves one at a time would leave eight of the nine in the warm set and
   * evict five of them. Deleting this effect does not fail a bridge test — it
   * silently leaves the deck with no attached terminals at all.
   */
  useEffect(() => {
    void setShownTerminals(shownTerminalsRef.current);
  }, [setShownTerminals, shownTerminalKey]);

  const orderedStages = snapshot.stages;
  const selectedAgent = snapshot.agents.find((agent) => agent.id === selectedAgentId);
  const selectedEvidence = snapshot.evidence.find((item) => item.id === selectedEvidenceId);
  const canControlDaemon = snapshot.connection.status === "connected" || snapshot.connection.daemonDetected === true;

  const coordinator = snapshot.agents.find((agent) => agent.isStartRole);

  const perform = async (action: DeckAction, success?: string) => {
    try {
      await runtime.runAction(action);
      if (success) setNotice(success);
    } catch (cause) {
      setNotice(cause instanceof Error ? cause.message : String(cause));
    }
  };

  // Returned to the composer rather than swallowed here: only the composer can
  // show a per-message delivered/failed state next to the text that produced it.
  const submitText = async (agentId: string, text: string): Promise<DeckActionResult> =>
    runtime.runAction({ type: "submit_text", agentId, text });

  const renameAgent = async (agentId: string, displayName: string) => {
    await perform({ type: "rename_agent", agentId, displayName }, `Agent renamed to ${displayName}.`);
  };

  const focusComposer = (agentId: string) => {
    setSelectedAgentId(agentId);
    setTabs((current) => ({ ...current, [agentId]: "terminal" }));
    setComposerFocus((current) => ({ agentId, token: (current?.token ?? 0) + 1 }));
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const editing = target?.matches("input, textarea, select, [contenteditable='true'], .xterm-helper-textarea");
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setPaletteOpen((open) => !open);
        return;
      }
      if (event.key === "Escape") {
        setPaletteOpen(false); setHelpOpen(false); setProjectsOpen(false); setProfilesOpen(false); setPromptsOpen(false); setWorkflowOpen(false); setConfirm(undefined);
        return;
      }
      if (editing) return;
      if (event.key === "?") { event.preventDefault(); setHelpOpen(true); return; }
      if (/^[1-4]$/.test(event.key)) {
        const agent = snapshot.agents[Number(event.key) - 1];
        if (agent) setSelectedAgentId(agent.id);
      }
      if ((event.key === "j" || event.key === "k") && snapshot.evidence.length) {
        const current = Math.max(0, snapshot.evidence.findIndex((item) => item.id === selectedEvidenceId));
        const delta = event.key === "j" ? 1 : -1;
        const next = (current + delta + snapshot.evidence.length) % snapshot.evidence.length;
        setSelectedEvidenceId(snapshot.evidence[next].id);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selectedEvidenceId, snapshot.agents, snapshot.evidence]);

  const moveStage = (id: string, direction: -1 | 1) => {
    setProfileOrder((current) => {
      const all = current.length ? current : profiles.map((profile) => profile.id);
      const index = all.indexOf(id);
      const target = index + direction;
      if (index < 0 || target < 0 || target >= all.length) return all;
      const next = [...all];
      [next[index], next[target]] = [next[target], next[index]];
      return next;
    });
  };

  const requestStop = () => {
    if (!selectedAgent) {
      if (!canControlDaemon) return;
      const liveAgents = snapshot.connection.runningAgentCount;
      setConfirm({
        title: "Stop the local daemon?",
        body: liveAgents && liveAgents > 0
          ? `The daemon reports ${liveAgents} live agent${liveAgents === 1 ? "" : "s"}. This safe stop will be refused until those agents are stopped individually.`
          : "This stops the local Agent Deck daemon. No live agents are reported, so this only shuts down the control service.",
        label: "Stop daemon",
        busyLabel: "Stopping…",
        action: async () => { await perform({ type: "stop_daemon" }, "Local daemon stopped."); },
      });
      return;
    }
    setConfirm({
      title: `Stop ${selectedAgent.role}?`,
      body: `This sends a stop request to ${selectedAgent.displayName}. Unsaved terminal work may be interrupted.`,
      label: "Stop agent",
      busyLabel: "Stopping…",
      action: async () => { await perform({ type: "stop_agent", agentId: selectedAgent.id }, `${selectedAgent.role} stop requested.`); },
    });
  };

  const requestRestartDaemon = () => {
    if (!snapshot.connection.daemonDetected || snapshot.connection.runningAgentCount !== 0) return;
    setConfirm({
      title: "Replace the incompatible daemon?",
      body: "The old daemon reports no live agents. Agent Deck will stop it and start the exact daemon build bundled with this desktop app.",
      label: "Replace daemon",
      busyLabel: "Replacing…",
      action: async () => { await perform({ type: "restart_daemon" }, "Matching daemon started and reconnected."); },
    });
  };

  /**
   * Issue #801. Offered ONLY when the desktop crate says the mismatch is
   * stamp-only — the wire protocol agreed on both sides and the git-describe
   * stamps did not. A genuine protocol mismatch never sets that flag, so this
   * button never appears for one, and the crate would refuse it anyway: the
   * protocol check runs first and the allowance cannot reach it.
   *
   * Unlike Replace daemon this is offered whatever the live-agent count, which
   * is the entire point — replacement is correctly refused while agents are
   * live, and that left an upgraded app with nine running agents no way in at
   * all.
   */
  const requestConnectAnyway = () => {
    if (!snapshot.connection.buildStampMismatchOnly) return;
    setConfirm({
      title: "Connect to a differently-built daemon?",
      body: "The wire protocol matched on both sides, so this daemon and this app agree on the shape of everything they exchange. They were built from different commits, and a stamp difference can still mean divergent behaviour behind an identical wire — a field whose meaning changed while its shape did not. Agent Deck will connect and keep the mismatch on screen for the rest of this session; nothing is remembered after you quit the app.",
      label: "Connect anyway",
      busyLabel: "Connecting…",
      action: async () => {
        try {
          await runtime.runAction({ type: "allow_build_mismatch" });
          // The allowance is read by the NEXT handshake, so the reconnect is
          // what actually connects; the crate caches no verdict.
          await runtime.reconnect();
          setNotice("Connected to the differently-built daemon. The mismatch stays in the connection banner for this session.");
        } catch (cause) {
          setNotice(cause instanceof Error ? cause.message : String(cause));
        }
      },
    });
  };

  const requestStartDaemon = () => {
    setConfirm({
      title: "Start the local daemon?",
      body: "Agent Deck will start its local daemon process and reconnect this control room. No agent is launched until you explicitly launch a workflow.",
      label: "Start daemon",
      busyLabel: "Starting…",
      action: async () => {
        try {
          await runtime.runAction({ type: "start_daemon" });
          setNotice("Local daemon started and control channel reconnected.");
        } catch (cause) {
          setNotice(cause instanceof Error ? cause.message : String(cause));
        }
      },
    });
  };

  const requestLaunch = (config: WorkflowLaunchConfig) => {
    const generatedCount = config.roles.length - config.customCommandCount;
    const commandCopy = config.customCommandCount > 0
      ? `${generatedCount} commands are generated from their current profile fields; ${config.customCommandCount} explicit custom command override${config.customCommandCount === 1 ? " bypasses" : "s bypass"} those fields. Custom commands may carry arbitrary permissions and are not covered by structured permission claims.`
      : `All ${generatedCount} commands are generated from the current provider, CLI, model, effort, and permission fields.`;
    const accessCopy = config.generatedFullAccessCount > 0
      ? ` Among generated commands, ${config.generatedFullAccessCount} role${config.generatedFullAccessCount === 1 ? " runs" : "s run"} unrestricted — Claude Code with bypassPermissions, or Codex with no sandbox — so ${config.generatedFullAccessCount === 1 ? "it acts" : "they act"} without asking.`
      : "";
    setConfirm({
      title: `Launch ${config.name}?`,
      body: `This starts ${config.roles.length} live CLI agents in ${config.cwd} and sends your task prompt to the coordinator. ${commandCopy}${accessCopy} This launch does not rewrite project TOML.`,
      label: "Launch live loop",
      busyLabel: "Launching…",
      action: async () => {
        try {
          const { customCommandCount: _customCommandCount, generatedFullAccessCount: _generatedFullAccessCount, ...launch } = config;
          await runtime.runAction({ type: "start_workflow", ...launch });
          setWorkflowOpen(false);
          await runtime.reconnect();
          setNotice(`${config.name} launched with ${config.roles.length} configured roles.`);
        } catch (cause) {
          setNotice(cause instanceof Error ? cause.message : String(cause));
        }
      },
    });
  };

  const commandItems = [
    ...(coordinator ? [{ label: "Message coordinator…", hint: `Send text to ${coordinator.displayName}`, icon: Send, run: () => focusComposer(coordinator.id) }] : []),
    { label: "Manage projects", hint: "Choose repositories & workflows", icon: FolderGit2, run: () => setProjectsOpen(true) },
    { label: "Open prompt library", hint: "Reusable launch and message prompts", icon: BookMarked, run: () => setPromptsOpen(true) },
    { label: "Open agent profiles", hint: "Configure models & permissions", icon: Bot, run: () => setProfilesOpen(true) },
    { label: "Edit workflow order", hint: "Enable, skip, or reorder roles", icon: Network, run: () => setWorkflowOpen(true) },
    { label: evidenceOpen ? "Hide evidence drawer" : "Show evidence drawer", hint: "Toggle transition evidence", icon: PanelRight, run: () => setEvidenceOpen((open) => !open) },
    ...snapshot.agents.map((agent, index) => ({ label: `Focus ${agent.role}`, hint: `Shortcut ${index + 1}`, icon: SquareTerminal, run: () => setSelectedAgentId(agent.id) })),
    ...(mode === "fixture" ? [{ label: "Advance fixture", hint: "Move the deterministic loop one node", icon: Zap, run: () => { void perform({ type: "advance_fixture" }); } }] : []),
  ];

  return (
    <div className={`control-deck ${evidenceOpen ? "with-evidence" : ""}`}>
      <aside className="rail" aria-label="Primary navigation">
        <div className="brand-mark" aria-label="Agent Deck"><span>AD</span><i aria-hidden="true" /></div>
        <nav>
          <RailButton icon={FolderGit2} label="Projects" active={projectsOpen} onClick={() => setProjectsOpen(true)} testId="open-projects" />
          <RailButton icon={Activity} label="Runs" active={!projectsOpen && !workflowOpen && !profilesOpen && !promptsOpen} onClick={() => { setProjectsOpen(false); setWorkflowOpen(false); setProfilesOpen(false); setPromptsOpen(false); }} />
          {/* The one rail button that is a real view rather than an overlay toggle. */}
          <RailButton icon={LayoutList} label="Overview" onClick={() => onNavigate?.({ kind: "overview" })} testId="open-overview" />
          <RailButton icon={BookMarked} label="Prompts" active={promptsOpen} onClick={() => setPromptsOpen(true)} testId="open-prompts" />
          <RailButton icon={Network} label="Workflows" active={workflowOpen} onClick={() => setWorkflowOpen(true)} />
          <RailButton icon={Bot} label="Agent Profiles" active={profilesOpen} onClick={() => setProfilesOpen(true)} testId="open-agent-profiles" />
          <RailButton icon={Settings2} label="Settings" onClick={() => setNotice("Runtime settings will use the same local configuration seam.")} />
        </nav>
        <div className="rail-bottom">
          <button aria-label="Keyboard shortcuts" title="Keyboard shortcuts" onClick={() => setHelpOpen(true)}><Keyboard size={18} /></button>
          <span className={`connection-lamp connection-${snapshot.connection.status}`} title={snapshot.connection.message} />
        </div>
      </aside>

      <main className="deck-main">
        <header className="topbar">
          <div className="repo-context">
            <div className="repo-line"><FolderGit2 size={15} /><strong>{activeProject?.name || snapshot.repo}</strong><ChevronRight size={13} /><span>{snapshot.runId}</span></div>
            {/*
              PRD #745 M8: the branch chip appears only when there IS a branch.
              Nothing daemon-side tracks one, so live mode reports none and the
              line carries the working directory alone rather than printing the
              literal "Unavailable" where a branch name belongs.
            */}
            <div className="branch-line">{snapshot.branch && <><GitBranch size={12} /><span>{snapshot.branch}</span><i /> </>}<span title={activeProject?.cwd || snapshot.worktree}>{activeProject?.cwd || snapshot.worktree}</span></div>
          </div>
          <div className="run-instruments">
            <Instrument label="HEALTH" testId="run-health"><span className={`health-value health-${snapshot.health}`}><i />{snapshot.health}</span></Instrument>
            <Instrument label="NODE"><strong>{String(snapshot.currentNode).padStart(2, "0")}<em>/{String(snapshot.totalNodes).padStart(2, "0")}</em></strong></Instrument>
            {/* Em dash, this deck's established "not known": no daemon tracks an attempt count (PRD #745 M8). */}
            <Instrument label="ATTEMPT"><strong>{snapshot.currentAttempt === undefined ? "—" : String(snapshot.currentAttempt).padStart(2, "0")}</strong></Instrument>
            <Instrument label="ELAPSED"><strong>{snapshot.elapsed}</strong></Instrument>
            <Instrument label="SPEND"><strong>{mode === "fixture" ? `$${snapshot.spend.toFixed(2)}` : "—"}</strong></Instrument>
          </div>
          <div className="top-actions">
            <button className="command-trigger" onClick={() => setPaletteOpen(true)}><Search size={14} /><span>Command</span><kbd>⌘ K</kbd></button>
            <button
              className="button secondary compact"
              data-testid="pause-run"
              disabled={mode === "live" || snapshot.connection.status !== "connected"}
              title={mode === "live" ? "Whole-run pause is not yet exposed by the daemon" : snapshot.paused ? "Resume fixture run" : "Pause fixture run"}
              onClick={() => void perform({ type: snapshot.paused ? "resume_run" : "pause_run" }, snapshot.paused ? "Fixture resumed." : "Fixture paused.")}
            >{snapshot.paused ? <Play size={14} /> : <Pause size={14} />}<span>{snapshot.paused ? "Resume" : "Pause"}</span></button>
            <button
              className="button danger compact"
              data-testid="stop-run"
              aria-label={selectedAgent ? `Stop ${selectedAgent.role}` : "Stop daemon"}
              title={selectedAgent ? `Stop ${selectedAgent.role}` : canControlDaemon ? "Stop local daemon" : "Daemon is not connected"}
              disabled={!selectedAgent && !canControlDaemon}
              onClick={requestStop}
            ><CircleStop size={14} /><span>Stop</span></button>
          </div>
        </header>

        {mode === "fixture" && (
          <div className="fixture-bar">
            <span><Sparkles size={13} /> DEMO DATA</span>
            <p>Deterministic run fixture · no external agents or files are being changed.</p>
            <button data-testid="fixture-advance" onClick={() => void perform({ type: "advance_fixture" })}>Advance fixture <ArrowRight size={13} /></button>
          </div>
        )}

        {/*
          The banner also stays up while CONNECTED with a build-stamp caveat
          (issue #801). Accepting the mismatch is not the same as it going away:
          the desktop crate deliberately keeps it in the connection message on
          the bypass path, and a banner keyed on `status` alone threw that away
          at the exact moment it started to matter — the session where the two
          builds actually differ.
        */}
        {(snapshot.connection.status !== "connected" || snapshot.connection.buildStampMismatchOnly) && (
          <div className={`connection-banner connection-${snapshot.connection.status}`} role="alert">
            {snapshot.connection.status === "loading" ? <RefreshCw className="spin" size={16} /> : <ShieldAlert size={16} />}
            <div><strong>{snapshot.connection.status === "loading" ? "Establishing control channel" : snapshot.connection.status === "connected" ? "Connected to a differently-built daemon" : snapshot.connection.status === "error" ? "Desktop bridge error" : "Daemon disconnected"}</strong><span>{snapshot.connection.message}</span></div>
            {snapshot.connection.status !== "loading" && <div className="connection-actions">{mode === "live" && snapshot.connection.status === "disconnected" && <button className="button primary compact" data-testid="start-daemon" onClick={requestStartDaemon}><Play size={13} /> Start daemon</button>}{mode === "live" && snapshot.connection.daemonDetected && snapshot.connection.status === "error" && snapshot.connection.runningAgentCount === 0 && <button className="button primary compact" data-testid="replace-daemon" onClick={requestRestartDaemon}><RefreshCw size={13} /> Replace daemon</button>}{mode === "live" && snapshot.connection.status === "error" && snapshot.connection.buildStampMismatchOnly && <button className="button primary compact" data-testid="connect-anyway" onClick={requestConnectAnyway}><ShieldAlert size={13} /> Connect anyway</button>}<button className="button secondary compact" onClick={() => void runtime.reconnect()}><RefreshCw size={13} /> Reconnect</button></div>}
          </div>
        )}

        <section className="workflow-strip" aria-labelledby="workflow-title">
          <header><div><span className="section-kicker">RUN GRAPH</span><h1 id="workflow-title">Visible deterministic loop</h1></div><button onClick={() => setWorkflowOpen(true)}><SlidersHorizontal size={13} /> Edit loop</button></header>
          <div className="workflow-track">
            {orderedStages.length ? orderedStages.map((stage, index) => (
              <div className={`workflow-node node-${stage.status} ${stage.enabled ? "" : "is-disabled"}`} key={stage.id} data-testid={`workflow-node-${stage.id}`}>
                <div className="node-glyph">{stage.status === "passed" ? <Check size={14} /> : stage.status === "failed" ? <X size={14} /> : <span>{String(index + 1).padStart(2, "0")}</span>}</div>
                <div><strong>{stage.label}</strong><small>{stage.enabled ? (stage.attempt === undefined ? stage.status : `${stage.status} · att ${stage.attempt}`) : "skipped"}</small></div>
                {index < orderedStages.length - 1 && <i className="workflow-link" aria-hidden="true" />}
              </div>
            )) : <div className="workflow-empty">No workflow nodes reported. Open <button onClick={() => setWorkflowOpen(true)}>Edit loop</button> to inspect configuration.</div>}
          </div>
        </section>

        <HandoffRail handoffs={snapshot.handoffs} />

        <section className="workspace-section" aria-label="Agent terminals">
          <header className="workspace-header">
            <div><span className="section-kicker">AGENT DECK</span><h2>Live work surfaces</h2></div>
            <div className="workspace-tools"><span>{snapshot.agents.length} agents</span><span>{snapshot.agents.filter((agent) => agent.status === "running").length} active</span><button className={evidenceOpen ? "is-active" : ""} onClick={() => setEvidenceOpen((open) => !open)}><PanelRight size={14} /> Evidence</button></div>
          </header>
          {snapshot.connection.status === "loading" && !snapshot.agents.length ? <LoadingDeck /> : snapshot.agents.length ? (
            <div className="agent-grid">
              {snapshot.agents.map((agent) => (
                <AgentTile
                  key={agent.id}
                  agent={agent}
                  mode={mode}
                  selected={agent.id === selectedAgentId}
                  tab={tabs[agent.id] ?? "terminal"}
                  terminalFeed={runtime.terminalFeed}
                  evidence={snapshot.evidence}
                  prompts={prompts}
                  composerFocusToken={composerFocus?.agentId === agent.id ? composerFocus.token : 0}
                  onSelect={() => setSelectedAgentId(agent.id)}
                  onTabChange={(tab) => setTabs((current) => ({ ...current, [agent.id]: tab }))}
                  onTerminalInput={runtime.sendTerminalInput}
                  onTerminalResize={runtime.resizeTerminal}
                  onEvidenceSelect={(id) => { setSelectedEvidenceId(id); setEvidenceOpen(true); }}
                  onSubmitText={submitText}
                  onRename={mode === "live" ? renameAgent : undefined}
                />
              ))}
            </div>
          ) : <EmptyDeck onReconnect={() => void runtime.reconnect()} onProfiles={() => setProfilesOpen(true)} />}
        </section>
      </main>

      {evidenceOpen && <EvidenceDrawer evidence={snapshot.evidence} selected={selectedEvidence} onSelect={setSelectedEvidenceId} onClose={() => setEvidenceOpen(false)} />}

      <ProjectsPanel
        open={projectsOpen}
        projects={projects}
        activeId={activeProjectId}
        selectedId={selectedProjectId}
        onSelect={setSelectedProjectId}
        onClose={() => setProjectsOpen(false)}
        onAdd={() => setSelectedProjectId(addProject())}
        onUpdate={updateProject}
        onRemove={(id) => { removeProject(id); setNotice("Project entry removed. The repository folder was not changed."); }}
        onActivate={(id) => { setActiveProjectId(id); setNotice("Active project updated. Open Workflows when you are ready to launch its agents."); }}
        onConfigureWorkflow={() => { setProjectsOpen(false); setWorkflowOpen(true); }}
      />
      <PromptLibraryPanel
        open={promptsOpen}
        prompts={prompts}
        selectedId={selectedPromptId}
        onSelect={setSelectedPromptId}
        onClose={() => setPromptsOpen(false)}
        onAdd={() => setSelectedPromptId(addPrompt())}
        onUpdate={updatePrompt}
        onRemove={(id) => { removePrompt(id); setNotice("Prompt removed from this device's library."); }}
      />
      <ProfilesPanel open={profilesOpen} profiles={profiles} onClose={() => setProfilesOpen(false)} onUpdate={updateProfile} onReset={resetProfiles} onSaved={() => setNotice("Agent profile draft saved locally. Project TOML is unchanged.")} />
      <WorkflowPanel key={activeProject?.id ?? "runtime-workflow"} open={workflowOpen} profiles={profiles} order={profileOrder} mode={mode} defaultName={activeProject?.workflowName ?? "dot-agent-deck"} defaultCwd={activeProject?.cwd || snapshot.worktree} onClose={() => setWorkflowOpen(false)} onToggle={(id) => { const profile = profiles.find((item) => item.id === id); if (profile) updateProfile(id, { enabled: !profile.enabled }); }} onMove={moveStage} onLaunch={requestLaunch} platformIssue={workflowPlatformIssue} prompts={prompts} />
      {paletteOpen && <CommandPalette commands={commandItems} onClose={() => setPaletteOpen(false)} />}
      {helpOpen && <ShortcutHelp onClose={() => setHelpOpen(false)} />}
      {confirm && <ConfirmDialog state={confirm} onClose={() => setConfirm(undefined)} />}
      {(notice || runtime.error) && <div className="toast" role="status"><AlertTriangle size={15} /><span>{notice ?? runtime.error}</span><button aria-label="Dismiss message" onClick={() => setNotice(undefined)}><X size={14} /></button></div>}
    </div>
  );
}

function RailButton({ icon: Icon, label, active, onClick, testId }: { icon: typeof Activity; label: string; active?: boolean; onClick: () => void; testId?: string }) {
  return <button className={active ? "is-active" : ""} aria-current={active ? "page" : undefined} title={label} onClick={onClick} data-testid={testId}><Icon size={18} /><span>{label}</span></button>;
}

function Instrument({ label, children, testId }: { label: string; children: ReactNode; testId?: string }) {
  return <div className="instrument" data-testid={testId}><span>{label}</span>{children}</div>;
}

function EvidenceDrawer({ evidence, selected, onSelect, onClose }: { evidence: EvidenceItem[]; selected?: EvidenceItem; onSelect: (id: string) => void; onClose: () => void }) {
  return (
    <aside className="evidence-drawer" data-testid="evidence-drawer" aria-label="Transition evidence">
      <header><div><span className="section-kicker">EVENT LEDGER</span><h2>Transition evidence</h2></div><button aria-label="Close evidence drawer" onClick={onClose}><X size={16} /></button></header>
      <div className="evidence-filter"><button className="is-active">All <span>{evidence.length}</span></button><button>Failures <span>{evidence.filter((item) => item.verdict === "FIX" || item.verdict === "ERROR").length}</span></button></div>
      <div className="evidence-list" role="listbox" aria-label="Run evidence">
        {evidence.length ? evidence.map((item) => (
          <button key={item.id} role="option" aria-selected={item.id === selected?.id} className={item.id === selected?.id ? "is-active" : ""} onClick={() => onSelect(item.id)}>
            <span className={`verdict verdict-${item.verdict.toLowerCase()}`}>{item.verdict}</span>
            <div><strong>{item.title}</strong><small>{item.to ? <>{item.from} <ArrowRight size={10} /> {item.to}</> : item.from}</small></div>
            <time>{item.at}</time>
          </button>
        )) : <div className="evidence-empty"><History size={20} /><strong>No events yet</strong><span>Live hook and handoff events appear here as agents work — delegations, deliveries, failures, and work-done reports included.</span></div>}
      </div>
      {selected && (
        <div className="evidence-detail">
          <div className="evidence-detail-head"><span className={`verdict verdict-${selected.verdict.toLowerCase()}`}>{selected.verdict}</span><span>{selected.acknowledged ? <><CheckCircle2 size={13} /> acknowledged</> : "unread"}</span></div>
          <h3>{selected.title}</h3>
          <p>{selected.summary}</p>
          <dl><div><dt>SENDER</dt><dd>{selected.from}</dd></div><div><dt>RECEIVER</dt><dd>{selected.to || "—"}</dd></div>{selected.command && <div className="wide"><dt>COMMAND</dt><dd><code>{selected.command}</code></dd></div>}{selected.exitCode !== undefined && <div><dt>EXIT</dt><dd>{selected.exitCode}</dd></div>}</dl>
          <div className="why-edge"><Gauge size={14} /><div><strong>{selected.to ? "Why this edge opened" : "Where this came from"}</strong><p>{selected.reason}</p></div></div>
        </div>
      )}
      <footer><span><i className="status-running" /> append-only run ledger</span><kbd>J</kbd><kbd>K</kbd></footer>
    </aside>
  );
}

function LoadingDeck() {
  return <div className="loading-grid" aria-label="Loading agents">{[0, 1, 2, 3].map((item) => <div key={item}><span /><i /><i /><b /></div>)}</div>;
}

function EmptyDeck({ onReconnect, onProfiles }: { onReconnect: () => void; onProfiles: () => void }) {
  return <div className="empty-deck"><Blocks size={28} /><h3>No active agent surfaces</h3><p>Connect to a running daemon or prepare agent profiles before starting the loop.</p><div><button className="button secondary" onClick={onProfiles}><Bot size={14} /> Configure agents</button><button className="button primary" onClick={onReconnect}><RefreshCw size={14} /> Reconnect</button></div></div>;
}

function CommandPalette({ commands, onClose }: { commands: { label: string; hint: string; icon: typeof Bot; run: () => void }[]; onClose: () => void }) {
  const [query, setQuery] = useState("");
  const filtered = commands.filter((item) => `${item.label} ${item.hint}`.toLowerCase().includes(query.toLowerCase()));
  return <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section className="command-palette" role="dialog" aria-modal="true" aria-label="Command menu" onMouseDown={(event) => event.stopPropagation()}><label><Search size={17} /><input autoFocus placeholder="Search controls, agents, and views…" value={query} onChange={(event) => setQuery(event.target.value)} /><kbd>ESC</kbd></label><div>{filtered.map(({ label, hint, icon: Icon, run }) => <button key={label} onClick={() => { run(); onClose(); }}><Icon size={16} /><span><strong>{label}</strong><small>{hint}</small></span><ChevronRight size={14} /></button>)}{!filtered.length && <p>No matching controls.</p>}</div><footer><span><Command size={12} /> local control surface</span><span><kbd>↵</kbd> select</span></footer></section></div>;
}

function ShortcutHelp({ onClose }: { onClose: () => void }) {
  const shortcuts = [["⌘ K", "Command menu"], ["1 — 4", "Focus agent"], ["J / K", "Move through evidence"], ["?", "Shortcut guide"], ["ESC", "Close overlay"]];
  return <div className="dialog-backdrop" role="presentation" onMouseDown={onClose}><section className="shortcut-dialog" role="dialog" aria-modal="true" aria-labelledby="shortcut-title" onMouseDown={(event) => event.stopPropagation()}><header><div><HelpCircle size={18} /><h2 id="shortcut-title">Control keys</h2></div><button aria-label="Close shortcut guide" onClick={onClose}><X size={16} /></button></header>{shortcuts.map(([keys, label]) => <div key={keys}><span>{label}</span><kbd>{keys}</kbd></div>)}</section></div>;
}
