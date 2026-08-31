import { createFixtureSnapshot, DEFAULT_PROFILES } from "../data/fixture";
import { applyHandoffEvent, mapDaemonEvent, MAX_LIVE_EVIDENCE } from "./daemonEvents";
import type { HandoffEdge,
  AgentSession,
  AgentStatus,
  DeckAction,
  DeckActionResult,
  DeckSnapshot,
  EvidenceItem,
  RuntimeMode,
  TerminalChunk,
  WorkflowStage,
} from "../types";

/** Exact DTO returned by the Tauri `desktop_get_snapshot` command. */
export interface DesktopSnapshotDto {
  connection: {
    status: "connected" | "disconnected" | "incompatible";
    socketPath: string;
    error?: string;
    clientProtocolVersion: number;
    serverProtocolVersion?: number;
    clientBuildVersion: string;
    daemonBuildVersion?: string;
    daemonVersion?: string;
    runningAgentCount?: number;
  };
  agents: DesktopAgentDto[];
  projectCwd?: string;
  protocolVersion: number;
  source: "daemon";
}

export interface DesktopAgentDto {
  id: string;
  paneId?: string;
  displayName?: string;
  cwd?: string;
  rows: number;
  cols: number;
  agentType: "claude_code" | "open_code" | "pi" | "codex" | "devin" | "none";
  status: "running" | "thinking" | "working" | "compacting" | "waiting_for_input" | "idle" | "error" | "unknown";
  activeTool?: { name: string; detail?: string };
  toolCount: number;
  tab:
    | { kind: "dashboard" }
    | { kind: "mode"; name: string }
    | { kind: "orchestration"; name: string; roleIndex: number; roleName: string; isStartRole: boolean; displayTitle?: string; orchestrationId?: string };
}

/** Result returned after the ordered Tauri output channel is registered. */
export interface TerminalAttachResult {
  sessionId: string;
  agentId: string;
  generation: number;
  reused: boolean;
}

/**
 * The subset of the Tauri `desktop_run_action` reply the frontend reads. The
 * command also returns the refreshed snapshot, which arrives separately on
 * `desktop://snapshot`.
 */
export interface DesktopActionResultDto {
  ok: boolean;
  sendResult?: import("../types").SendResult;
  message?: string;
}

/** Low-volume lifecycle payload emitted as `desktop://terminal-state`. */
export interface DesktopTerminalStateDto {
  sessionId: string;
  agentId: string;
  generation: number;
  state: "attached" | "end" | "error";
  message?: string;
}

/** Exact tagged payload accepted by the Tauri `desktop_run_action` command. */
export type DesktopRunActionDto =
  | { type: "refresh" }
  | { type: "bootstrap"; startIfMissing?: boolean }
  | { type: "start_agent"; command?: string; cwd?: string; displayName?: string; rows?: number; cols?: number }
  | { type: "stop_agent"; agentId: string }
  | { type: "rename_agent"; agentId: string; displayName: string }
  | { type: "attach_terminal"; agentId: string; onOutput: import("@tauri-apps/api/core").Channel<ArrayBuffer> }
  | { type: "detach_terminal"; sessionId: string }
  | { type: "submit_text"; agentId: string; text: string }
  | { type: "start_workflow"; name: string; cwd: string; taskPrompt: string; roles: { role: string; command: string; start: boolean }[]; rows?: number; cols?: number }
  | { type: "stop_daemon"; force?: boolean }
  | { type: "restart_daemon" };

type SnapshotListener = (snapshot: DeckSnapshot) => void;
type TerminalListener = (event: TerminalChunk) => void;
type Unsubscribe = () => void;

interface PendingTerminalAttachment {
  lifecycle: number;
  channel: import("@tauri-apps/api/core").Channel<ArrayBuffer>;
  output: Uint8Array[];
  stateEvents: DesktopTerminalStateDto[];
  session?: TerminalAttachResult;
  activated: boolean;
}

export interface DeckBridge {
  readonly mode: RuntimeMode;
  connect(): Promise<DeckSnapshot>;
  subscribe(onSnapshot: SnapshotListener, onTerminal: TerminalListener): Promise<Unsubscribe>;
  runAction(action: DeckAction): Promise<DeckActionResult>;
  sendTerminalInput(agentId: string, data: string): Promise<void>;
  resizeTerminal(agentId: string, cols: number, rows: number): Promise<void>;
  dispose(): Promise<void>;
}

/**
 * The daemon's closed status vocabulary (src-tauri `session_status_name`),
 * mapped exhaustively — PR #416 review B1/M3. The record makes a NEW daemon
 * status a visible fallthrough here instead of a silent one, and the
 * fallthrough itself is "waiting", never a terminal state: the daemon's own
 * `SessionStatus::Unknown` doc says it must be "rendered neutrally (like
 * Idle) so it never masquerades as an active state" — and a status this
 * build has never heard of gets the same treatment, because per PRD #162 a
 * newer daemon can add one without a protocol bump. The old substring
 * matcher sent "unknown" to "stopped", which locked a LIVE agent's terminal
 * read-only.
 */
const DAEMON_STATUS: Record<string, AgentStatus> = {
  thinking: "running",
  working: "running",
  compacting: "running",
  waiting_for_input: "waiting",
  idle: "waiting",
  error: "failed",
  unknown: "waiting",
};

function statusFromDaemon(status: string): AgentStatus {
  return DAEMON_STATUS[status.toLowerCase()] ?? "waiting";
}

function roleFromAgent(agent: DesktopAgentDto, index: number): string {
  const value = agent.tab.kind === "orchestration"
    ? agent.tab.roleName
    : agent.agentType.replaceAll("_", " ").trim();
  return value ? value.charAt(0).toUpperCase() + value.slice(1) : `Agent ${index + 1}`;
}

function agentFromDto(agent: DesktopAgentDto, index: number): AgentSession {
  const status = statusFromDaemon(agent.status);
  const role = roleFromAgent(agent, index);
  const orchestration = agent.tab.kind === "orchestration" ? agent.tab : undefined;
  return {
    id: agent.id,
    paneId: agent.paneId,
    role,
    displayName: agent.displayName || role,
    cli: agent.agentType || "agent",
    model: "Unavailable",
    status,
    task: agent.activeTool ? `Active tool: ${agent.activeTool.name}${agent.activeTool.detail ? ` · ${agent.activeTool.detail}` : ""}` : "Task metadata unavailable from daemon",
    cwd: agent.cwd ?? "Unavailable",
    attempt: 1,
    duration: "—",
    tokens: 0,
    cost: 0,
    contextPercent: 0,
    worktree: "Unavailable",
    writeLease: "unknown",
    rows: agent.rows,
    cols: agent.cols,
    activeTool: agent.activeTool?.name,
    toolCount: agent.toolCount,
    transcript: "",
    diff: [],
    checks: [],
    handoffIds: [],
    artifacts: [],
    inOrchestration: Boolean(orchestration),
    isStartRole: orchestration?.isStartRole ?? false,
  };
}

/**
 * PR #416 review M1: every persisted-preferences key is scoped by runtime
 * mode. Fixture sessions used to write projects/profiles/prompts under the
 * SAME keys live mode read back — so one fixture visit could hand a real
 * workflow launch a working directory that never existed.
 */
export function modeScopedKey(base: string): string {
  return `${base}.${selectRuntimeMode()}`;
}

export function mapDesktopSnapshot(dto: DesktopSnapshotDto, previous?: DeckSnapshot, evidence?: EvidenceItem[], handoffs?: HandoffEdge[]): DeckSnapshot {
  const agents = dto.agents.map(agentFromDto);
  const cwd = agents.find((agent) => agent.cwd !== "Unavailable")?.cwd
    ?? dto.projectCwd
    ?? (previous?.worktree?.startsWith("/") ? previous.worktree : undefined)
    ?? "No active project";
  const repo = cwd.split("/").filter(Boolean).at(-1) ?? cwd;
  const stages: WorkflowStage[] = agents.map((agent, index) => ({
    id: `agent-${agent.id}`,
    label: agent.role,
    agentId: agent.id,
    status: agent.status === "running" ? "active" : agent.status === "passed" ? "passed" : agent.status === "failed" ? "failed" : "queued",
    attempt: agent.attempt,
    enabled: true,
  }));

  return {
    runId: previous?.runId ?? "live-daemon",
    repo,
    branch: previous?.branch ?? "Unavailable",
    worktree: cwd,
    connection: {
      status: dto.connection.status === "incompatible" ? "error" : dto.connection.status,
      socketPath: dto.connection.socketPath,
      message: dto.connection.error ?? (dto.connection.status === "connected" ? "Daemon responding" : dto.connection.status === "incompatible" ? `Protocol mismatch: desktop v${dto.connection.clientProtocolVersion}, daemon v${dto.connection.serverProtocolVersion ?? "unknown"}` : "Daemon unavailable"),
      daemonDetected: dto.connection.status === "connected" || dto.connection.status === "incompatible",
      runningAgentCount: dto.connection.runningAgentCount,
    },
    health: dto.connection.status === "incompatible" ? "failed" : dto.connection.status === "disconnected" ? "idle" : agents.some((agent) => agent.status === "failed") ? "failed" : "healthy",
    elapsed: previous?.elapsed ?? "—",
    spend: previous?.spend ?? 0,
    currentNode: Math.max(1, agents.findIndex((agent) => agent.status === "running") + 1),
    totalNodes: agents.length,
    currentAttempt: 1,
    paused: false,
    stages,
    agents: agents.map((agent) => {
      const old = previous?.agents.find((candidate) => candidate.id === agent.id);
      return old ? { ...agent, transcript: old.transcript } : agent;
    }),
    evidence: evidence ?? previous?.evidence ?? [],
    handoffs: handoffs ?? previous?.handoffs ?? [],
    profiles: previous?.profiles ?? DEFAULT_PROFILES.map((profile) => ({ ...profile })),
  };
}

class FixtureDeckBridge implements DeckBridge {
  readonly mode = "fixture" as const;
  private snapshot: DeckSnapshot;
  private snapshotListeners = new Set<SnapshotListener>();
  private terminalListeners = new Set<TerminalListener>();
  private fixtureStep = 0;

  constructor() {
    const requestedState = new URLSearchParams(window.location.search).get("state");
    const state = requestedState === "disconnected" || requestedState === "error" || requestedState === "empty"
      ? requestedState
      : "connected";
    this.snapshot = createFixtureSnapshot(state);
  }

  async connect(): Promise<DeckSnapshot> {
    await Promise.resolve();
    return structuredClone(this.snapshot);
  }

  async subscribe(onSnapshot: SnapshotListener, onTerminal: TerminalListener): Promise<Unsubscribe> {
    this.snapshotListeners.add(onSnapshot);
    this.terminalListeners.add(onTerminal);
    return () => {
      this.snapshotListeners.delete(onSnapshot);
      this.terminalListeners.delete(onTerminal);
    };
  }

  private emitSnapshot(): void {
    const value = structuredClone(this.snapshot);
    this.snapshotListeners.forEach((listener) => listener(value));
  }

  async runAction(action: DeckAction): Promise<DeckActionResult> {
    if (action.type === "pause_run" || action.type === "resume_run") {
      this.snapshot.paused = action.type === "pause_run";
    } else if (action.type === "approve_run") {
      this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === "approve" ? { ...stage, status: "passed" } : stage);
      this.snapshot.health = "healthy";
    } else if (action.type === "retry_stage") {
      this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === action.stageId ? { ...stage, status: "active", attempt: stage.attempt + 1 } : stage);
    } else if (action.type === "stop_agent") {
      this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === action.agentId ? { ...agent, status: "stopped" } : agent);
    } else if (action.type === "rename_agent") {
      this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === action.agentId ? { ...agent, displayName: action.displayName } : agent);
    } else if (action.type === "submit_text") {
      this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === action.agentId ? { ...agent, transcript: `${agent.transcript}\r\n> ${action.text}\r\n` } : agent);
      this.terminalListeners.forEach((listener) => listener({ agentId: action.agentId, data: new TextEncoder().encode(`\r\n> ${action.text}\r\n`), stream: "output", operation: "append" }));
    } else if (action.type === "advance_fixture") {
      this.fixtureStep = (this.fixtureStep + 1) % 3;
      if (this.fixtureStep === 1) {
        this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === "review" ? { ...stage, status: "passed" } : stage.id === "test" ? { ...stage, status: "active" } : stage);
        this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === "reviewer" ? { ...agent, status: "passed" } : agent.id === "tester" ? { ...agent, status: "running", transcript: `${agent.transcript}\u001b[36mACTIVE\u001b[0m running browser smoke…\r\n` } : agent);
        this.snapshot.currentNode = 5;
      } else if (this.fixtureStep === 2) {
        this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === "test" ? { ...stage, status: "passed" } : stage.id === "approve" ? { ...stage, status: "waiting" } : stage);
        this.snapshot.agents = this.snapshot.agents.map((agent) => agent.id === "tester" ? { ...agent, status: "passed", transcript: `${agent.transcript}\u001b[32mPASS\u001b[0m browser · a11y · PTY fixture\r\n` } : agent);
        this.snapshot.currentNode = 6;
      } else {
        this.snapshot = createFixtureSnapshot("connected");
      }
    }
    this.emitSnapshot();
    return { ok: true, sendResult: action.type === "submit_text" ? "applied" : undefined };
  }

  async sendTerminalInput(agentId: string, data: string): Promise<void> {
    this.terminalListeners.forEach((listener) => listener({ agentId, data: new TextEncoder().encode(data), stream: "output", operation: "append" }));
    await Promise.resolve();
  }

  async resizeTerminal(): Promise<void> {
    await Promise.resolve();
  }

  async dispose(): Promise<void> {
    this.snapshotListeners.clear();
    this.terminalListeners.clear();
  }
}

export class TauriDeckBridge implements DeckBridge {
  readonly mode = "live" as const;
  private attached = new Set<string>();
  private sessions = new Map<string, TerminalAttachResult>();
  private terminalChannels = new Map<string, import("@tauri-apps/api/core").Channel<ArrayBuffer>>();
  private pendingAttachments = new Map<string, PendingTerminalAttachment>();
  private pendingTerminal = new Map<string, TerminalChunk[]>();
  private pendingResizes = new Map<string, { cols: number; rows: number }>();
  private resizeFrames = new Map<string, number>();
  private resizeInFlight = new Set<string>();
  private terminalListener?: TerminalListener;
  private invoke?: typeof import("@tauri-apps/api/core")["invoke"];
  private lifecycle = 0;
  /** Newest-first ring of mapped hook events, capped at MAX_LIVE_EVIDENCE. */
  private evidence: EvidenceItem[] = [];
  private handoffs: HandoffEdge[] = [];
  private evidenceSequence = 0;
  private agentIndex: AgentSession[] = [];

  /**
   * Resolves a hook event's agent by registry id first, then by the pane id the
   * daemon tagged the pane with — hook payloads from external agents carry only
   * one or the other.
   */
  private resolveAgent = (agentId?: string, paneId?: string): { id: string; role: string } | undefined => {
    const match = this.agentIndex.find((agent) => (agentId && agent.id === agentId) || (paneId && agent.paneId === paneId));
    return match ? { id: match.id, role: match.role } : undefined;
  };

  private recordDaemonEvent(payload: unknown): boolean {
    const edges = applyHandoffEvent(this.handoffs, payload);
    const edgesChanged = edges !== this.handoffs;
    this.handoffs = edges;
    const item = mapDaemonEvent(payload, this.evidenceSequence, this.resolveAgent);
    if (!item) return edgesChanged;
    this.evidenceSequence += 1;
    this.evidence = [item, ...this.evidence].slice(0, MAX_LIVE_EVIDENCE);
    return true;
  }

  private async getInvoke(): Promise<typeof import("@tauri-apps/api/core")["invoke"]> {
    if (!this.invoke) this.invoke = (await import("@tauri-apps/api/core")).invoke;
    return this.invoke;
  }

  private async attachAgents(agents: DesktopAgentDto[], expectedLifecycle = this.lifecycle): Promise<void> {
    if (expectedLifecycle !== this.lifecycle) return;
    const invoke = await this.getInvoke();
    if (expectedLifecycle !== this.lifecycle) return;
    const { Channel } = await import("@tauri-apps/api/core");
    if (expectedLifecycle !== this.lifecycle) return;
    await Promise.allSettled(agents.filter((agent) => !this.attached.has(agent.id)).map(async (agent) => {
      const lifecycle = expectedLifecycle;
      this.attached.add(agent.id);
      const onOutput = new Channel<ArrayBuffer>();
      const attempt: PendingTerminalAttachment = {
        lifecycle,
        channel: onOutput,
        output: [],
        stateEvents: [],
        activated: false,
      };
      onOutput.onmessage = (chunk) => {
        if (
          lifecycle !== this.lifecycle
          || this.terminalChannels.get(agent.id) !== onOutput
        ) return;
        const data = new Uint8Array(chunk);
        if (!attempt.activated) {
          attempt.output.push(data);
          return;
        }
        if (attempt.session) this.deliverOutput(agent.id, data, attempt.session.generation);
      };
      this.terminalChannels.set(agent.id, onOutput);
      this.pendingAttachments.set(agent.id, attempt);
      try {
        const session = await invoke<TerminalAttachResult>("desktop_terminal_attach", { agentId: agent.id, onOutput });
        if (
          lifecycle !== this.lifecycle
          || this.terminalChannels.get(agent.id) !== onOutput
          || this.pendingAttachments.get(agent.id) !== attempt
        ) {
          await invoke("desktop_terminal_detach", { sessionId: session.sessionId }).catch(() => undefined);
          return;
        }
        attempt.session = session;
        this.sessions.set(agent.id, session);
        this.pendingAttachments.delete(agent.id);

        const replayLength = attempt.output.reduce((total, chunk) => total + chunk.byteLength, 0);
        const replay = new Uint8Array(replayLength);
        let replayOffset = 0;
        for (const chunk of attempt.output) {
          replay.set(chunk, replayOffset);
          replayOffset += chunk.byteLength;
        }
        attempt.output = [];
        this.deliverTerminal({
          agentId: agent.id,
          data: replay,
          stream: "output",
          operation: "replace",
          generation: session.generation,
        });
        attempt.activated = true;
        attempt.stateEvents.forEach((event) => this.handleTerminalState(event));
        if (this.pendingResizes.has(agent.id)) this.scheduleResize(agent.id);
      } catch (cause) {
        if (lifecycle === this.lifecycle) {
          if (this.pendingAttachments.get(agent.id) === attempt) this.pendingAttachments.delete(agent.id);
          this.attached.delete(agent.id);
          if (this.terminalChannels.get(agent.id) === onOutput) this.terminalChannels.delete(agent.id);
          this.deliverTerminal({
            agentId: agent.id,
            data: new Uint8Array(),
            stream: "error",
            operation: "append",
            message: "Terminal attach failed. The agent is still running; reconnect to retry its terminal.",
          });
        }
        throw cause;
      }
    }));
  }

  private deliverOutput(agentId: string, data: Uint8Array, generation: number): void {
    this.deliverTerminal({ agentId, data, stream: "output", operation: "append", generation });
  }

  private deliverTerminal(event: TerminalChunk): void {
    if (this.terminalListener) {
      this.terminalListener(event);
      return;
    }
    const pending = this.pendingTerminal.get(event.agentId) ?? [];
    pending.push(event);
    this.pendingTerminal.set(event.agentId, pending);
  }

  private handleTerminalState(event: DesktopTerminalStateDto): void {
    if (event.state === "attached") return;
    const session = this.sessions.get(event.agentId);
    if (
      !session
      || session.sessionId !== event.sessionId
      || session.generation !== event.generation
    ) return;

    this.sessions.delete(event.agentId);
    this.attached.delete(event.agentId);
    this.terminalChannels.delete(event.agentId);
    this.deliverTerminal({
      agentId: event.agentId,
      data: new Uint8Array(),
      stream: event.state,
      operation: "append",
      generation: event.generation,
      message: event.message,
    });
  }

  async connect(): Promise<DeckSnapshot> {
    const lifecycle = this.lifecycle;
    const invoke = await this.getInvoke();
    const dto = await invoke<DesktopSnapshotDto>("desktop_bootstrap", { options: { startIfMissing: false } });
    await this.attachAgents(dto.agents, lifecycle);
    const snapshot = mapDesktopSnapshot(dto, undefined, this.evidence, this.handoffs);
    this.agentIndex = snapshot.agents;
    return snapshot;
  }

  async subscribe(onSnapshot: SnapshotListener, onTerminal: TerminalListener): Promise<Unsubscribe> {
    const { listen } = await import("@tauri-apps/api/event");
    this.terminalListener = onTerminal;
    this.pendingTerminal.forEach((events) => events.forEach((event) => onTerminal(event)));
    this.pendingTerminal.clear();
    let latest: DeckSnapshot | undefined;
    const emit = (dto: DesktopSnapshotDto) => {
      latest = mapDesktopSnapshot(dto, latest, this.evidence, this.handoffs);
      this.agentIndex = latest.agents;
      onSnapshot(latest);
    };
    const stopSnapshot = await listen<DesktopSnapshotDto>("desktop://snapshot", (event) => {
      void this.attachAgents(event.payload.agents);
      emit(event.payload);
    });
    // The daemon emits a coalesced snapshot after each event, but not every hook
    // event produces one within the coalescing window; republishing the last
    // mapped snapshot keeps the drawer current without waiting for the next.
    const stopDaemonEvent = await listen<unknown>("desktop://daemon-event", (event) => {
      if (!this.recordDaemonEvent(event.payload) || !latest) return;
      latest = { ...latest, evidence: this.evidence, handoffs: this.handoffs };
      onSnapshot(latest);
    });
    const stopTerminalState = await listen<DesktopTerminalStateDto>("desktop://terminal-state", (event) => {
      if (event.payload.state === "attached") return;
      const session = this.sessions.get(event.payload.agentId);
      if (
        session
        && session.sessionId === event.payload.sessionId
        && session.generation === event.payload.generation
      ) {
        this.handleTerminalState(event.payload);
        return;
      }
      const pending = this.pendingAttachments.get(event.payload.agentId);
      if (pending && pending.lifecycle === this.lifecycle) pending.stateEvents.push(event.payload);
    });
    return () => {
      stopSnapshot();
      stopDaemonEvent();
      stopTerminalState();
      if (this.terminalListener === onTerminal) this.terminalListener = undefined;
    };
  }

  async runAction(action: DeckAction): Promise<DeckActionResult> {
    const invoke = await this.getInvoke();
    if (action.type === "stop_agent" || action.type === "rename_agent" || action.type === "submit_text" || action.type === "start_workflow" || action.type === "stop_daemon" || action.type === "restart_daemon") {
      // `desktop_run_action` resolves with `ok: false` for a non-delivered
      // send rather than raising, so the result must be returned, not dropped.
      const result = await invoke<DesktopActionResultDto>("desktop_run_action", { action: action satisfies DesktopRunActionDto });
      if (action.type === "stop_daemon" || action.type === "restart_daemon") {
        this.sessions.clear();
        this.attached.clear();
        this.terminalChannels.clear();
        this.lifecycle += 1;
      }
      return { ok: result?.ok !== false, sendResult: result?.sendResult, message: result?.message };
    }
    if (action.type === "start_daemon") {
      const dto = await invoke<DesktopSnapshotDto>("desktop_bootstrap", { options: { startIfMissing: true } });
      if (dto.connection.status !== "connected") {
        throw new Error(dto.connection.error ?? "The local daemon did not become connected.");
      }
      await this.attachAgents(dto.agents);
      return { ok: true };
    }
    throw new Error("This orchestration control is available in the fixture preview but is not yet exposed by the live daemon.");
  }

  async sendTerminalInput(agentId: string, data: string): Promise<void> {
    const invoke = await this.getInvoke();
    const session = this.sessions.get(agentId);
    if (!session) throw new Error(`Terminal for ${agentId} is not attached.`);
    await invoke("desktop_terminal_write", { sessionId: session.sessionId, data: Array.from(new TextEncoder().encode(data)) });
  }

  async resizeTerminal(agentId: string, cols: number, rows: number): Promise<void> {
    if (cols < 1 || rows < 1) return;
    this.pendingResizes.set(agentId, { cols, rows });
    this.scheduleResize(agentId);
    await Promise.resolve();
  }

  private scheduleResize(agentId: string): void {
    if (this.resizeFrames.has(agentId) || this.resizeInFlight.has(agentId)) return;
    const frame = window.requestAnimationFrame(() => {
      this.resizeFrames.delete(agentId);
      void this.flushResize(agentId);
    });
    this.resizeFrames.set(agentId, frame);
  }

  private async flushResize(agentId: string): Promise<void> {
    if (this.resizeInFlight.has(agentId)) return;
    const lifecycle = this.lifecycle;
    const size = this.pendingResizes.get(agentId);
    const session = this.sessions.get(agentId);
    if (!size || !session) return;
    this.pendingResizes.delete(agentId);
    this.resizeInFlight.add(agentId);
    try {
      const invoke = await this.getInvoke();
      await invoke("desktop_terminal_resize", { sessionId: session.sessionId, cols: size.cols, rows: size.rows });
    } finally {
      if (lifecycle === this.lifecycle) {
        this.resizeInFlight.delete(agentId);
        if (this.pendingResizes.has(agentId)) this.scheduleResize(agentId);
      }
    }
  }

  async dispose(): Promise<void> {
    this.lifecycle += 1;
    const invoke = this.invoke;
    const sessions = Array.from(this.sessions.values());
    this.resizeFrames.forEach((frame) => window.cancelAnimationFrame(frame));
    this.attached.clear();
    this.sessions.clear();
    this.terminalChannels.clear();
    this.pendingAttachments.clear();
    this.pendingTerminal.clear();
    this.pendingResizes.clear();
    this.resizeFrames.clear();
    this.resizeInFlight.clear();
    this.terminalListener = undefined;
    if (!invoke) return;
    await Promise.allSettled(sessions.map((session) => invoke("desktop_terminal_detach", { sessionId: session.sessionId })));
  }
}

export function selectRuntimeMode(): RuntimeMode {
  const params = new URLSearchParams(window.location.search);
  const configured = import.meta.env.VITE_DECK_TRANSPORT;
  if (params.get("fixture") === "1" || configured === "fixture") return "fixture";
  if (params.get("live") === "1" || configured === "live") return "live";
  return window.__TAURI_INTERNALS__ ? "live" : "fixture";
}

export function createDeckBridge(mode = selectRuntimeMode()): DeckBridge {
  return mode === "live" ? new TauriDeckBridge() : new FixtureDeckBridge();
}
