export type RuntimeMode = "fixture" | "live";
export type ConnectionStatus = "loading" | "connected" | "disconnected" | "error";
export type RunHealth = "healthy" | "attention" | "failed" | "idle";
export type AgentStatus = "queued" | "running" | "waiting" | "passed" | "failed" | "stopped";
export type StageStatus = "queued" | "active" | "passed" | "failed" | "waiting";
export type PanelTab = "terminal" | "diff" | "checks" | "handoffs" | "artifacts";
export type Verdict = "PASS" | "FIX" | "HUMAN" | "ERROR" | "INFO";

export interface ConnectionView {
  status: ConnectionStatus;
  socketPath?: string;
  message?: string;
  /** True when a daemon answered Hello but failed protocol/build compatibility. */
  daemonDetected?: boolean;
  /** Honest count reported by Hello; undefined when the daemon could not report it. */
  runningAgentCount?: number;
}

export interface DeckProject {
  id: string;
  name: string;
  cwd: string;
  workflowName: string;
  notes: string;
}

export interface DeckPrompt {
  id: string;
  name: string;
  body: string;
  note?: string;
}

export interface WorkflowStage {
  id: string;
  label: string;
  agentId?: string;
  status: StageStatus;
  attempt: number;
  enabled: boolean;
}

export interface CheckResult {
  id: string;
  name: string;
  status: "passed" | "failed" | "running" | "queued";
  duration?: string;
  command?: string;
}

export interface Artifact {
  id: string;
  name: string;
  kind: "file" | "report" | "recording";
  path: string;
}

export interface AgentSession {
  id: string;
  paneId?: string;
  role: string;
  displayName: string;
  cli: string;
  model: string;
  status: AgentStatus;
  task: string;
  cwd: string;
  attempt: number;
  duration: string;
  tokens: number;
  cost: number;
  contextPercent: number;
  worktree: string;
  writeLease: "read" | "write" | "none" | "unknown";
  rows: number;
  cols: number;
  activeTool?: string;
  toolCount: number;
  transcript: string;
  diff: string[];
  checks: CheckResult[];
  handoffIds: string[];
  artifacts: Artifact[];
  /** True when the daemon reports this pane as an orchestration role pane. */
  inOrchestration?: boolean;
  /** True for the orchestration's start role — the coordinator an operator should message. */
  isStartRole?: boolean;
}

export interface EvidenceItem {
  id: string;
  verdict: Verdict;
  title: string;
  summary: string;
  from: string;
  to: string;
  at: string;
  command?: string;
  exitCode?: number;
  reason: string;
  acknowledged: boolean;
  /** Deck-side agent this item is attributed to, when the daemon reported one. */
  agentId?: string;
}

export type Provider = "OpenAI" | "Anthropic" | "OpenCode" | "Custom";
export type PermissionMode = "default" | "read-only" | "workspace-write" | "full-access";
export type ProfileCommandMode = "generated" | "custom";

export interface AgentProfile {
  id: string;
  roleId: string;
  role: string;
  provider: Provider;
  cli: string;
  model: string;
  effort: "low" | "medium" | "high" | "xhigh";
  commandMode: ProfileCommandMode;
  command: string;
  customCommand?: string;
  permissionMode: PermissionMode;
  enabled: boolean;
  savedToProject: boolean;
}

export interface DeckSnapshot {
  runId: string;
  repo: string;
  branch: string;
  worktree: string;
  connection: ConnectionView;
  health: RunHealth;
  elapsed: string;
  spend: number;
  currentNode: number;
  totalNodes: number;
  currentAttempt: number;
  paused: boolean;
  stages: WorkflowStage[];
  agents: AgentSession[];
  evidence: EvidenceItem[];
  /** Live delegation edges (handoff-visibility PRD D2), newest first. */
  handoffs: HandoffEdge[];
  profiles: AgentProfile[];
}

/** One delegation's lifecycle, driven by the daemon's handoff events. */
export interface HandoffEdge {
  /** The daemon's delegation id (`dlg-<millis>-<seq>`). */
  id: string;
  toRole: string;
  orchestration?: string;
  taskPreview?: string;
  /**
   * dispatched → delivered → done is the healthy path; failed is terminal and
   * carries `reason`. `respawned` marks that the worker was restarted for this
   * delegation (expected for clear=true roles).
   */
  status: "dispatched" | "delivered" | "failed" | "done";
  respawned: boolean;
  reason?: string;
  /** Wall-clock of the newest event applied to this edge (HH:MM:SS). */
  at: string;
}

export type DeckAction =
  | { type: "pause_run" }
  | { type: "resume_run" }
  | { type: "approve_run" }
  | { type: "advance_fixture" }
  | { type: "start_daemon" }
  | { type: "stop_daemon"; force?: boolean }
  | { type: "restart_daemon" }
  | { type: "start_workflow"; name: string; cwd: string; taskPrompt: string; roles: WorkflowLaunchRole[]; rows: number; cols: number }
  | { type: "retry_stage"; stageId: string }
  | { type: "stop_agent"; agentId: string }
  | { type: "rename_agent"; agentId: string; displayName: string }
  | { type: "submit_text"; agentId: string; text: string };

/**
 * The daemon's honest delivery outcome for submitted input, mirroring the Rust
 * `SendResult` (kebab-case on the wire). Only `applied` and `queued` mean the
 * text reached the agent; every other value — including an unrecognised future
 * one decoded as `unknown` — is a non-delivery the UI must not dress up as
 * success.
 */
export type SendResult =
  | "applied"
  | "queued"
  | "stale"
  | "wrong-session"
  | "history-only"
  | "no-live-target"
  | "ambiguous"
  | "unknown";

/**
 * What `desktop_run_action` reports back. The Rust command returns `ok: false`
 * with a non-delivered `sendResult` instead of raising, so a caller that only
 * awaits the promise cannot tell delivery from silent loss — every consumer of
 * `submit_text` must read this.
 */
export interface DeckActionResult {
  ok: boolean;
  sendResult?: SendResult;
  message?: string;
}

/** True only for the two outcomes that actually reached the agent. */
export function isDelivered(result: DeckActionResult): boolean {
  return result.ok && (result.sendResult === undefined || result.sendResult === "applied" || result.sendResult === "queued");
}

/** Operator-facing explanation of a non-delivered outcome. */
export function sendResultReason(result: SendResult | undefined): string {
  switch (result) {
    case "stale": return "the daemon's view of that pane had already moved on";
    case "wrong-session": return "the pane handle no longer maps to that agent's session";
    case "history-only": return "the agent has no live pane — only its history remains";
    case "no-live-target": return "there is nothing live to write to";
    case "ambiguous": return "the write started but did not complete; some of it may already have landed, so it was not retried";
    case "unknown": return "the daemon reported an outcome this build does not recognise";
    default: return "the daemon did not confirm delivery";
  }
}

export interface WorkflowLaunchRole {
  role: string;
  command: string;
  start: boolean;
}

export interface WorkflowLaunchConfig {
  name: string;
  cwd: string;
  taskPrompt: string;
  roles: WorkflowLaunchRole[];
  rows: number;
  cols: number;
  customCommandCount: number;
  generatedFullAccessCount: number;
}

export interface TerminalChunk {
  agentId: string;
  data: Uint8Array;
  stream: "output" | "end" | "error";
  operation: "append" | "replace";
  generation?: number;
  message?: string;
}

export interface TerminalBuffer {
  data: Uint8Array;
  baseOffset: number;
  generation?: number;
}

export interface TerminalFeed {
  get(agentId: string): TerminalBuffer | undefined;
  subscribe(agentId: string, listener: (buffer: TerminalBuffer) => void): () => void;
}

export interface DeckRuntimeState {
  mode: RuntimeMode;
  snapshot: DeckSnapshot;
  terminalData: Record<string, TerminalBuffer>;
  /** Direct PTY-byte path that bypasses React state; absent in tests/fixture. */
  terminalFeed?: TerminalFeed;
  error?: string;
  runAction: (action: DeckAction) => Promise<DeckActionResult>;
  sendTerminalInput: (agentId: string, data: string) => Promise<void>;
  resizeTerminal: (agentId: string, cols: number, rows: number) => Promise<void>;
  reconnect: () => Promise<void>;
}
