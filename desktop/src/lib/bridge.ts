import { createFixtureSnapshot, DEFAULT_PROFILES, type FixtureState } from "../data/fixture";
import { applyHandoffEvent, mapDaemonEvent, MAX_LIVE_EVIDENCE } from "./daemonEvents";
import { DISPLAY_LIMITS, displayText } from "./displayText";
import { UNREPORTED } from "../types";
import type { HandoffEdge,
  AgentSession,
  AgentStatus,
  AgentTab,
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
    /**
     * Always emitted by the desktop crate, including as `false`. Optional here
     * only so a DTO literal in a test need not restate the common case; read it
     * as "an override exists", never as "something is wrong".
     */
    buildStampMismatchOnly?: boolean;
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
  /**
   * The binary the agent registry says this type runs — `claude`, `opencode`,
   * `pi`, `codex`, `devin` (PRD #745). `agentType` above is the wire IDENTITY
   * and is not a name anybody types: rendering it showed Claude Code as
   * `claude_code`, OpenCode as `open_code`, and `codex` correctly only by
   * coincidence.
   *
   * Absent — the key is omitted, never blank — when the daemon reported
   * `none` or no type at all. `none` is also the landing spot for a type this
   * build has never heard of, so absence here means "this build cannot name
   * the binary", and nothing invents one.
   */
  cliName?: string;
  status: "running" | "thinking" | "working" | "compacting" | "waiting_for_input" | "idle" | "error" | "unknown";
  activeTool?: { name: string; detail?: string };
  toolCount: number;
  /**
   * `SessionSnapshot.last_user_prompt`, surfaced by the desktop crate in M8.
   * Absent — the key is omitted, never null — when the agent has emitted no
   * prompt, when the record carries no live snapshot, or when the daemon
   * predates the field.
   */
  lastUserPrompt?: string;
  /**
   * `SessionSnapshot.live_target.writable`, projected by the desktop crate into
   * the deck's own vocabulary. Absent when the daemon declared no live target,
   * which is NOT the same as declaring a non-writable one.
   */
  writeLease?: "read" | "write" | "none";
  /**
   * `SessionSnapshot.last_activity_ms` (PRD #745 M9): when the daemon last saw
   * this agent do anything, as epoch milliseconds. Epoch milliseconds and not a
   * formatted string, so the relative wording stays a webview decision — see
   * `displayActivity` in `lib/displayText`, which also owns the clock-skew rule.
   *
   * Absent from a daemon that has no live session for the agent (a restarted
   * daemon has none at all) and from one that predates the field. A TYPE
   * ASSERTION, not a validated value: the DTO cannot stop a malformed daemon
   * sending a non-finite or out-of-range number, which is why the render seam
   * checks rather than trusts.
   */
  lastActivityMs?: number;
  /**
   * `AgentRecord.spawned_at_ms` (PRD #745 M11): when the daemon forked this
   * agent's process, as epoch milliseconds. Epoch milliseconds and not a
   * formatted string for the same reason `lastActivityMs` is — see
   * `displayUptime` in `lib/displayText`, which owns the wording and shares the
   * clock-skew rule.
   *
   * It comes off the registry RECORD rather than a live session, so unlike
   * `lastActivityMs` it is present for an agent that has never emitted a hook
   * event. Absent from a daemon that did not spawn the agent (an id-only
   * `ListAgents` reply) and from one that predates the field. A TYPE ASSERTION,
   * not a validated value, exactly like `lastActivityMs`: the render seam
   * checks rather than trusts.
   */
  spawnedAtMs?: number;
  /**
   * The desktop crate's `DesktopTab` is structurally identical to the app
   * model's `AgentTab`, so the DTO reuses it and `agentFromDto` copies the
   * value through rather than flattening it to a role string. If the IPC shape
   * ever diverges from the model, this is where the mapping function goes.
   */
  tab: AgentTab;
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
  | { type: "restart_daemon" }
  | { type: "allow_build_mismatch" };

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
  /**
   * States the WHOLE set of agents whose terminal is on screen right now
   * (PRD #745 M7). Attach follows this and nothing else — not `connect()`, not
   * a snapshot event — because an attach costs one daemon socket and one full
   * scrollback replay per agent, and "renders no output" and "opens no PTYs"
   * are different claims. Declarative, not imperative: the two facts the UI has
   * to state are "these nine tiles are showing a terminal" and "now none is",
   * and neither can be expressed by a per-agent show. Call it once per render
   * commit with every shown id.
   */
  setShownTerminals(agentIds: string[]): Promise<void>;
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
  // `running` is what the Rust side emits for an agent with no hook state yet
  // — `map_agent` falls back to it when `AgentRecord.live` is absent
  // (`desktop/src-tauri/src/dto.rs`, pinned by
  // `record_without_hook_state_is_still_running`). It is a documented member of
  // `DesktopAgentDto["status"]`, and its absence here sent a live, hookless
  // agent down the unknown-status fallthrough and labelled it "waiting".
  running: "running",
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

/**
 * The deck's assignment line, as a DISPLAY COPY — sanitised and clamped here
 * rather than at the tile that prints it.
 *
 * PRD #745 M8 made this line carry `lastUserPrompt`, which is free-form,
 * agent-influenceable text bounded only by the daemon's 64 KiB per-prompt
 * ceiling; before that it carried a hardcoded placeholder or a restatement of
 * the active tool. `AgentTile` renders `agent.task` straight into a DOM text
 * node and the deck is the screen the app opens on, so a `U+202E` in a prompt
 * reversed the assignment line — the daemon-side scrub removes category `Cc`
 * and bidi formatting characters are `Cf` — and fifteen agents put about a
 * megabyte of prompt text in the deck's DOM on every refreshed snapshot.
 *
 * Bounding it at the projection rather than at the tile is what makes the
 * property structural: `task` is display-only — nothing sorts, groups or keys
 * on it — so every consumer of it, present and future, gets the bounded copy
 * and no raw daemon text reaches a deck DOM node through this field at all.
 * The raw prompt stays on `lastUserPrompt` for the surfaces that need more of
 * it, each of which passes it through this same seam with its own budget.
 *
 * The active-tool restatement goes through it too, which closes the same hole
 * one field over: a tool detail is the agent's own command line and was never
 * sanitised on this path either.
 */
function taskLine(agent: DesktopAgentDto): string {
  // The daemon's own last user prompt is the honest answer to "what is this
  // agent doing", so it leads. The active-tool restatement is the fallback it
  // always was, and the placeholder is reached only when the daemon reported
  // neither (PRD #745 M8).
  const reported = agent.lastUserPrompt
    ?? (agent.activeTool ? `Active tool: ${agent.activeTool.name}${agent.activeTool.detail ? ` · ${agent.activeTool.detail}` : ""}` : undefined);
  return reported === undefined ? "Task metadata unavailable from daemon" : displayText(reported, DISPLAY_LIMITS.prompt);
}

function agentFromDto(agent: DesktopAgentDto, index: number, daemonId: string): AgentSession {
  const status = statusFromDaemon(agent.status);
  const role = roleFromAgent(agent, index);
  const orchestration = agent.tab.kind === "orchestration" ? agent.tab : undefined;
  return {
    id: agent.id,
    daemonId,
    paneId: agent.paneId,
    role,
    displayName: agent.displayName || role,
    // The BINARY, not the enum (PRD #745). `agentType` is the wire identity —
    // `claude_code`, `open_code` — and nobody types either of those. The name
    // is resolved daemon-side from the agent registry, which is where the deck
    // already keeps the command each agent launches, so a desktop-side lookup
    // table cannot drift away from it. `"agent"` is the fallback it always was,
    // reached now for the case it was written for: a type this build cannot
    // name a binary for.
    cli: agent.cliName || "agent",
    model: UNREPORTED,
    status,
    task: taskLine(agent),
    // Absent, not sentinel-encoded. The deck's own stand-in word is a legal
    // working directory (`src/agent_pty.rs` accepts any non-empty, bounded,
    // control-free `cwd`), so writing it here let an agent launched in a
    // directory called "Unavailable" have its real, reported directory erased
    // by `toOverviewAgent`'s reversal. Absence that cannot be spelled by the
    // daemon cannot collide with it; the one surface that still wants a word
    // for it supplies its own at its own render seam (`AgentTile`).
    cwd: agent.cwd,
    // No `attempt`: the daemon has no retry counter, and live mode used to
    // hardcode `1` here, which every tile then printed as `ATT 01` (PRD #745 M8).
    duration: "—",
    tokens: 0,
    cost: 0,
    contextPercent: 0,
    worktree: UNREPORTED,
    // `"unknown"` is this field's absence sentinel, not a third lease state:
    // the daemon declared no live target. `toOverviewAgent` reverses it.
    writeLease: agent.writeLease ?? "unknown",
    lastUserPrompt: agent.lastUserPrompt,
    lastActivityMs: agent.lastActivityMs,
    spawnedAtMs: agent.spawnedAtMs,
    rows: agent.rows,
    cols: agent.cols,
    activeTool: agent.activeTool?.name,
    activeToolDetail: agent.activeTool?.detail,
    toolCount: agent.toolCount,
    transcript: "",
    diff: [],
    checks: [],
    handoffIds: [],
    artifacts: [],
    tab: agent.tab,
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

/**
 * The message shown when the desktop crate sent none. It always does today, so
 * this is a belt-and-braces path — but it used to hardcode `Protocol mismatch`
 * for EVERY incompatible status, which is wrong for the far more common
 * build-stamp case and would have told a user to look at a protocol version
 * that matched (issue #801). It now says which of the two checks failed, using
 * the same flag the Connect anyway affordance is gated on.
 */
function fallbackConnectionMessage(connection: DesktopSnapshotDto["connection"]): string {
  if (connection.status === "connected") return "Daemon responding";
  if (connection.status !== "incompatible") return "Daemon unavailable";
  if (connection.buildStampMismatchOnly) {
    return `Build mismatch: desktop is ${connection.clientBuildVersion}, daemon is ${connection.daemonBuildVersion ?? "unreported"}.`;
  }
  return `Protocol mismatch: desktop v${connection.clientProtocolVersion}, daemon v${connection.serverProtocolVersion ?? "unknown"}`;
}

export function mapDesktopSnapshot(dto: DesktopSnapshotDto, previous?: DeckSnapshot, evidence?: EvidenceItem[], handoffs?: HandoffEdge[]): DeckSnapshot {
  // The socket path is the only per-daemon identity the handshake gives us, and
  // it is exactly what distinguishes one local daemon from another (PRD #745,
  // ahead of #742).
  const daemonId = dto.connection.socketPath;
  const agents = dto.agents.map((agent, index) => agentFromDto(agent, index, daemonId));
  const cwd = agents.find((agent) => agent.cwd)?.cwd
    ?? dto.projectCwd
    ?? (previous?.worktree?.startsWith("/") ? previous.worktree : undefined)
    ?? "No active project";
  const repo = cwd.split("/").filter(Boolean).at(-1) ?? cwd;
  const stages: WorkflowStage[] = agents.map((agent, index) => ({
    id: `agent-${agent.id}`,
    label: agent.role,
    agentId: agent.id,
    status: agent.status === "running" ? "active" : agent.status === "passed" ? "passed" : agent.status === "failed" ? "failed" : "queued",
    // No attempt: it was read straight off the hardcoded per-agent one, so
    // every live node claimed a retry count no daemon tracks (PRD #745 M8).
    enabled: true,
  }));

  return {
    runId: previous?.runId ?? "live-daemon",
    repo,
    // No branch: nothing daemon-side tracks one, and the literal "Unavailable"
    // this used to carry was a placeholder the topbar printed as if it were the
    // checked-out branch (PRD #745 M8).
    worktree: cwd,
    connection: {
      status: dto.connection.status === "incompatible" ? "error" : dto.connection.status,
      socketPath: dto.connection.socketPath,
      message: dto.connection.error ?? fallbackConnectionMessage(dto.connection),
      daemonDetected: dto.connection.status === "connected" || dto.connection.status === "incompatible",
      runningAgentCount: dto.connection.runningAgentCount,
      buildStampMismatchOnly: dto.connection.buildStampMismatchOnly,
      clientBuildVersion: dto.connection.clientBuildVersion,
      daemonBuildVersion: dto.connection.daemonBuildVersion,
    },
    health: dto.connection.status === "incompatible" ? "failed" : dto.connection.status === "disconnected" ? "idle" : agents.some((agent) => agent.status === "failed") ? "failed" : "healthy",
    elapsed: previous?.elapsed ?? "—",
    spend: previous?.spend ?? 0,
    currentNode: Math.max(1, agents.findIndex((agent) => agent.status === "running") + 1),
    totalNodes: agents.length,
    // No currentAttempt, for the same reason as the per-agent one.
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

/**
 * Every scenario `?state=` accepts. Keeping the accepted values in one list
 * next to `FixtureState` means adding a scenario cannot silently fail to be
 * reachable from the URL — the previous inline `||` chain had to be edited in
 * lockstep with the fixture and was not.
 */
const FIXTURE_STATES: readonly FixtureState[] = ["connected", "crowded", "disconnected", "error", "empty"];

class FixtureDeckBridge implements DeckBridge {
  readonly mode = "fixture" as const;
  private snapshot: DeckSnapshot;
  private snapshotListeners = new Set<SnapshotListener>();
  private terminalListeners = new Set<TerminalListener>();
  private fixtureStep = 0;

  constructor() {
    const requestedState = new URLSearchParams(window.location.search).get("state");
    const state = FIXTURE_STATES.find((candidate) => candidate === requestedState) ?? "connected";
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
      // Counting up from an absent attempt would invent one, so a stage with no
      // count keeps none. Every fixture stage has one; live mode has no retry
      // action at all (PRD #745 M8).
      this.snapshot.stages = this.snapshot.stages.map((stage) => stage.id === action.stageId ? { ...stage, status: "active", attempt: stage.attempt === undefined ? undefined : stage.attempt + 1 } : stage);
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

  /**
   * Fixture mode owns no PTYs, so there is nothing to attach or evict — but the
   * seam lives on `DeckBridge` rather than on `TauriDeckBridge` alone, so no
   * screen ever has to know which bridge it is holding.
   */
  async setShownTerminals(): Promise<void> {
    await Promise.resolve();
  }

  async dispose(): Promise<void> {
    this.snapshotListeners.clear();
    this.terminalListeners.clear();
  }
}

/**
 * How many terminals stay attached after you have left them. The bound governs
 * the WARM set alone — terminals currently on screen are never capped, because
 * the deck mounts a terminal on every tile and a bound over everything attached
 * would kill six of nine visible panes. Three keeps bouncing between the
 * handful of agents you are actually working with free of a scrollback replay
 * while holding the idle cost of a nine-agent fleet at three sockets instead of
 * nine (PRD #745 M7).
 */
export const MAX_WARM_TERMINALS = 3;

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
  /**
   * Agents whose terminal is on screen right now, as last declared by
   * `setShownTerminals`. Unbounded, and never an eviction candidate.
   */
  private shown = new Set<string>();
  /**
   * Agents whose terminal has been left but is still attached, insertion-ordered
   * least-recently-left first so eviction takes the head. Bounded by
   * `MAX_WARM_TERMINALS`. Membership is by agent id and does NOT require the
   * attach to have landed — a pending attach that is never a warm member is
   * never selected for eviction, and installs itself afterwards past the bound.
   */
  private warm = new Set<string>();
  /**
   * Agents with a `desktop_terminal_attach` invocation still outstanding —
   * added before the invoke, removed when it settles either way. It is what
   * stops a second invocation from starting behind the first: the Rust side
   * serialises every agent through one attach gate with no timeout and no
   * cancellation, so a daemon that never answers would otherwise collect one
   * more channel, closure, promise and queued command per hide/reshow cycle.
   *
   * Deliberately NOT `pendingAttachments`, and deliberately NOT cleared by
   * `evictTerminal`: eviction cancels an attach by *marking* it (deleting
   * `pendingAttachments` is what makes the post-await guard fail), and the
   * backend command it started keeps running whatever the frontend forgets.
   * A guard that eviction clears re-arms on every cycle and guards nothing.
   * The one place it IS cleared without settling is `clearAttachSuppression`,
   * so an attach that never answers cannot suppress its agent forever.
   */
  private attachInvocations = new Set<string>();
  /**
   * Agents whose attach the guard above suppressed, coalesced to at most one
   * request each. Replayed once when the outstanding invocation settles, so a
   * hide/reshow that raced an attach still ends with a live pane — suppressing
   * without this would trade an unbounded queue for a dead terminal.
   */
  private attachRequested = new Set<string>();
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

  /**
   * The single attach trigger (PRD #745 M7). Takes the whole shown set, diffs it
   * against the previous one, and does all four things in one pass: attach what
   * is newly shown, move what is newly hidden into the warm set, evict warm
   * overflow, and flush the warm set entirely when nothing is shown at all.
   *
   * It must be called ONCE per render commit with every shown id, never once
   * per tile: nine single-id calls would leave eight of the nine warm and evict
   * five of them, which is the same broken deck the bound exists to avoid.
   */
  async setShownTerminals(agentIds: string[]): Promise<void> {
    const next = new Set(agentIds);

    // Leaving a terminal does not detach it. It moves to the warm set, delete-
    // then-add so the tail is the most recently left and the head is the LRU.
    for (const agentId of this.shown) {
      if (next.has(agentId)) continue;
      this.warm.delete(agentId);
      this.warm.add(agentId);
    }
    // A shown terminal is never an eviction candidate, so showing a warm one
    // takes it back out of the warm set. It stays in `attached`, so coming back
    // costs no attach and produces no replay — the whole point of warm.
    for (const agentId of next) this.warm.delete(agentId);
    this.shown = next;

    // Bounded against `warm.size` ALONE — never against the shown or the
    // attached count, which is what would kill visible panes.
    const overflow = this.shown.size === 0
      // Flushed to ZERO rather than down to the bound, so "no terminals
      // attached" is true however you arrived at a screen that shows none.
      ? this.warm.size
      : Math.max(0, this.warm.size - MAX_WARM_TERMINALS);
    // `evictTerminal` is synchronous up to its `desktop_terminal_detach`, so
    // every evicted agent is out of `sessions` / `terminalChannels` /
    // `pendingAttachments` BEFORE the attach below writes its new entries.
    const evictions = Array.from(this.warm).slice(0, overflow).map((agentId) => this.evictTerminal(agentId));

    // Every shown id, not only the newly shown ones: `attachAgents` filters out
    // whatever is already attached, so re-declaring an unchanged set is a no-op
    // except where a shown terminal lost its session to a daemon `end`/`error`
    // state event and has to be brought back.
    const attaching = this.shown.size ? this.attachAgents(Array.from(this.shown)) : Promise.resolve();
    await Promise.all([...evictions, attaching]);
  }

  /**
   * Drops one terminal completely — client-side state first, then the daemon.
   *
   * Every map keyed by agent id has to be cleared here, not just `sessions`: a
   * surviving `terminalChannels` entry would leave the dead attach's channel
   * able to deliver output, and a surviving `pendingResizes` / `resizeFrames`
   * entry would push a size computed for the old pane at the next session
   * (`attachAgents` re-schedules a pending resize on re-attach).
   *
   * Dropping `pendingAttachments` and `terminalChannels` IS how an attach still
   * in flight gets cancelled: `attachAgents`' post-await guard then fails and
   * takes the orphan-detach branch instead of installing a session behind a
   * screen that no longer shows it. Cancellation is marking, never awaiting —
   * one slow attach must not freeze every later terminal switch. The marking is
   * also why `attachInvocations` is the ONE agent-keyed set deliberately left
   * alone here: the Tauri command an evicted attach started is still running,
   * and forgetting it is what would let a stalled daemon collect one queued
   * invocation per hide/reshow cycle.
   *
   * Per-agent, and deliberately NOT a `lifecycle` bump: `lifecycle` is a
   * whole-bridge generation, and bumping it here would also void every SHOWN
   * attach still in flight and leak `resizeInFlight` for any agent mid-resize.
   */
  private async evictTerminal(agentId: string): Promise<void> {
    const session = this.sessions.get(agentId);
    this.shown.delete(agentId);
    this.warm.delete(agentId);
    this.sessions.delete(agentId);
    this.attached.delete(agentId);
    this.terminalChannels.delete(agentId);
    this.pendingAttachments.delete(agentId);
    // Chunks buffered while no listener was installed belong to the session
    // being torn down here. Keeping them would replay a dead pane's scrollback
    // ahead of the live one at the next `subscribe` drain.
    this.pendingTerminal.delete(agentId);
    this.attachRequested.delete(agentId);
    this.pendingResizes.delete(agentId);
    const frame = this.resizeFrames.get(agentId);
    if (frame !== undefined) {
      window.cancelAnimationFrame(frame);
      this.resizeFrames.delete(agentId);
    }
    this.resizeInFlight.delete(agentId);
    // Nothing installed yet: the teardown above is the whole cancellation, and
    // the pending attach detaches its own late-arriving session.
    if (!session) return;
    const invoke = await this.getInvoke();
    await invoke("desktop_terminal_detach", { sessionId: session.sessionId }).catch(() => undefined);
  }

  /**
   * Drops the per-agent attach guard and everything queued behind it.
   *
   * The guard in `attachAgents` is cleared per invocation by its own `finally`,
   * which never runs for an attach the daemon accepts and never answers — and
   * `evictTerminal` deliberately leaves it alone, because forgetting a running
   * command is what lets a stalled daemon collect one more queued invocation
   * per hide/reshow cycle. Both are right in the steady state and together they
   * make one agent PERMANENTLY unattachable: every later declaration for it is
   * suppressed, while the replay that would undo the suppression is itself
   * waiting on the invocation that never settles.
   *
   * So it is cleared exactly where a whole-bridge restart happens — `connect()`
   * and `dispose()` — and nowhere else. Both mean the frontend is starting its
   * relationship with the daemon over, which is the only moment at which
   * forgetting an outstanding command is a fresh start rather than an
   * unbounded queue: it costs at most one extra queued command per Reconnect,
   * bounded by a deliberate user action, against a pane that is otherwise dead
   * until the app restarts.
   */
  private clearAttachSuppression(): void {
    this.attachInvocations.clear();
    this.attachRequested.clear();
  }

  private async attachAgents(agentIds: string[], expectedLifecycle = this.lifecycle): Promise<void> {
    if (expectedLifecycle !== this.lifecycle) return;
    const invoke = await this.getInvoke();
    if (expectedLifecycle !== this.lifecycle) return;
    const { Channel } = await import("@tauri-apps/api/core");
    if (expectedLifecycle !== this.lifecycle) return;
    // Re-read membership after the dynamic imports, not before them: an agent
    // shown when this call started can have been hidden and evicted while they
    // resolved, and attaching it then would leave a live PTY behind a screen
    // that shows no terminal at all.
    await Promise.allSettled(agentIds.filter((agentId) => {
      if (this.attached.has(agentId) || !(this.shown.has(agentId) || this.warm.has(agentId))) return false;
      // One outstanding invocation per agent, whatever the frontend has since
      // forgotten about it. Queue the declaration instead of starting a second
      // command — the settling invocation replays it.
      if (this.attachInvocations.has(agentId)) {
        this.attachRequested.add(agentId);
        return false;
      }
      return true;
    }).map(async (agentId) => {
      const lifecycle = expectedLifecycle;
      this.attached.add(agentId);
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
          || this.terminalChannels.get(agentId) !== onOutput
        ) return;
        const data = new Uint8Array(chunk);
        if (!attempt.activated) {
          attempt.output.push(data);
          return;
        }
        if (attempt.session) this.deliverOutput(agentId, data, attempt.session.generation);
      };
      this.terminalChannels.set(agentId, onOutput);
      this.pendingAttachments.set(agentId, attempt);
      this.attachInvocations.add(agentId);
      try {
        const session = await invoke<TerminalAttachResult>("desktop_terminal_attach", { agentId, onOutput });
        if (
          lifecycle !== this.lifecycle
          || this.terminalChannels.get(agentId) !== onOutput
          || this.pendingAttachments.get(agentId) !== attempt
        ) {
          await invoke("desktop_terminal_detach", { sessionId: session.sessionId }).catch(() => undefined);
          return;
        }
        attempt.session = session;
        this.sessions.set(agentId, session);
        this.pendingAttachments.delete(agentId);

        const replayLength = attempt.output.reduce((total, chunk) => total + chunk.byteLength, 0);
        const replay = new Uint8Array(replayLength);
        let replayOffset = 0;
        for (const chunk of attempt.output) {
          replay.set(chunk, replayOffset);
          replayOffset += chunk.byteLength;
        }
        attempt.output = [];
        this.deliverTerminal({
          agentId,
          data: replay,
          stream: "output",
          operation: "replace",
          generation: session.generation,
        });
        attempt.activated = true;
        attempt.stateEvents.forEach((event) => this.handleTerminalState(event));
        if (this.pendingResizes.has(agentId)) this.scheduleResize(agentId);
      } catch (cause) {
        if (lifecycle === this.lifecycle) {
          if (this.pendingAttachments.get(agentId) === attempt) this.pendingAttachments.delete(agentId);
          this.attached.delete(agentId);
          if (this.terminalChannels.get(agentId) === onOutput) this.terminalChannels.delete(agentId);
          this.deliverTerminal({
            agentId,
            data: new Uint8Array(),
            stream: "error",
            operation: "append",
            message: "Terminal attach failed. The agent is still running; reconnect to retry its terminal.",
          });
        }
        throw cause;
      } finally {
        this.attachInvocations.delete(agentId);
        // A declaration suppressed while this invocation was outstanding is
        // coalesced rather than dropped: replay exactly one, and only while the
        // agent is still wanted and still unattached. A failed attach queues no
        // request of its own, so this cannot become a retry loop.
        if (
          this.attachRequested.delete(agentId)
          && lifecycle === this.lifecycle
          && !this.attached.has(agentId)
          && (this.shown.has(agentId) || this.warm.has(agentId))
        ) {
          void this.attachAgents([agentId], lifecycle);
        }
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
    // Reconnect is the user's remedy for a wedged control room, so it has to be
    // able to remedy this too. `useDeckRuntime` memoizes the bridge on `mode`
    // alone and `reconnect()` calls straight into here, so nothing is disposed
    // and nothing is recreated in between.
    this.clearAttachSuppression();
    const invoke = await this.getInvoke();
    const dto = await invoke<DesktopSnapshotDto>("desktop_bootstrap", { options: { startIfMissing: false } });
    // PRD #745 M7: connecting attaches NOTHING. It used to attach every agent
    // the daemon owns, so a nine-agent fleet cost nine sockets and nine
    // scrollback replays before a single terminal was on screen. The UI states
    // what it shows through `setShownTerminals`, and that is the only trigger.
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
      // PRD #745 M7: a snapshot reports what the daemon owns, which says nothing
      // about what is on screen — so re-declare the set the UI last declared,
      // NEVER the fleet the snapshot carries. This re-establishes "everything
      // shown is attached" after a daemon `end`/`error` state event dropped one
      // of them, which no re-render can do because a dead session does not
      // change the derived shown set. It is a no-op whenever the invariant
      // already holds: `attachAgents` filters on `attached`.
      //
      // The invariant is "everything SHOWN is attached", not "everything
      // attached is alive" — a warm terminal whose session dies stays dead
      // until it is shown again. Deliberate: nobody is looking at it, and
      // healing it off screen would spend a socket and a scrollback replay for
      // nothing.
      emit(event.payload);
      void this.attachAgents(Array.from(this.shown));
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
    if (action.type === "stop_agent" || action.type === "rename_agent" || action.type === "submit_text" || action.type === "start_workflow" || action.type === "stop_daemon" || action.type === "restart_daemon" || action.type === "allow_build_mismatch") {
      // `desktop_run_action` resolves with `ok: false` for a non-delivered
      // send rather than raising, so the result must be returned, not dropped.
      const result = await invoke<DesktopActionResultDto>("desktop_run_action", { action: action satisfies DesktopRunActionDto });
      if (action.type === "stop_daemon" || action.type === "restart_daemon") {
        this.sessions.clear();
        this.attached.clear();
        this.terminalChannels.clear();
        // Nothing is attached any more, so nothing is warm. `shown` is left
        // alone on purpose: it mirrors what the UI is displaying, which a
        // daemon stop does not change, and re-declaring it re-attaches.
        this.warm.clear();
        this.lifecycle += 1;
      }
      return { ok: result?.ok !== false, sendResult: result?.sendResult, message: result?.message };
    }
    if (action.type === "start_daemon") {
      const dto = await invoke<DesktopSnapshotDto>("desktop_bootstrap", { options: { startIfMissing: true } });
      if (dto.connection.status !== "connected") {
        throw new Error(dto.connection.error ?? "The local daemon did not become connected.");
      }
      // PRD #745 M7: starting the daemon no longer attaches its whole fleet
      // either — this was the third eager call site, and the one reachable
      // without a snapshot event at all.
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
    this.shown.clear();
    this.warm.clear();
    this.clearAttachSuppression();
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
