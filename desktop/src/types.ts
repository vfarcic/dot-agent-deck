export type RuntimeMode = "fixture" | "live";

/**
 * The control deck's legacy stand-in for a value the daemon did not report.
 * The deck renders it as text AND matches on it (`mapDesktopSnapshot` picks a
 * repo directory by skipping agents whose `cwd` is this), so it cannot simply
 * be dropped; M8 is where live mode stops presenting these as facts.
 *
 * It is exported so the two sides of that substitution — the one that writes
 * it in `agentFromDto` and the one that reverses it in `toOverviewAgent` —
 * cannot drift apart into a placeholder leaking onto a screen that promises
 * none. An agent's `cwd` is an absolute path, so this can never be one.
 */
export const UNREPORTED = "Unavailable";

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
  /**
   * True when the ONLY thing that failed the handshake is the git-describe build
   * stamp: the wire protocol agreed on both sides, so proceeding is a judgement
   * the user may legitimately make and **Connect anyway** is offered (issue
   * #801). A protocol mismatch never sets it — that check runs first in the
   * desktop crate and is not overridable from anywhere, so a screen must never
   * offer a button for it.
   */
  buildStampMismatchOnly?: boolean;
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
  /**
   * FIXTURE-ONLY, and optional for the same reason as `AgentSession.attempt`:
   * live mode derived it from that hardcoded `1`, so every node claimed an
   * attempt count no daemon tracks (PRD #745 M8).
   */
  attempt?: number;
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

/**
 * Which top-level surface is mounted. A discriminated union from the start even
 * though it carries only two variants today, so PRD #745 iteration 3's group
 * and single-agent views arrive as added variants rather than as a refactor of
 * a boolean. No router library is warranted for this.
 */
export type DeckView =
  | { kind: "deck" }
  | { kind: "overview" };

/**
 * An agent's tab membership exactly as the daemon reports it, mirroring
 * `DesktopAgentDto.tab`. It is the grouping key for the agent overview, which
 * would otherwise have to reconstruct membership from the role string —
 * everything below already reaches the webview and used to be discarded in
 * `agentFromDto`.
 */
export type AgentTab =
  | { kind: "dashboard" }
  | { kind: "mode"; name: string }
  /**
   * `cwd` is the ORCHESTRATION TAB's own directory, shared by every role pane
   * in the tab and distinct from each pane's `AgentSession.cwd` — an
   * orchestrator and its workers may sit in different per-pane directories
   * while belonging to one orchestration. Optional because the daemon reports
   * it only when the tab declared one (PRD #745 M8).
   */
  | { kind: "orchestration"; name: string; roleIndex: number; roleName: string; isStartRole: boolean; cwd?: string; displayTitle?: string; orchestrationId?: string };

/**
 * A pane the deck knows about.
 *
 * PRD #745 splits this interface in two, and the split is load-bearing rather
 * than cosmetic. The fields marked HONEST are ones the daemon genuinely
 * reports, so a screen may present them as fact. The fields marked
 * FIXTURE-ONLY have no source in daemon state at all — live mode hardcodes
 * them in `agentFromDto` to `"Unavailable"` / `0` / `1` / `"—"` — and they
 * exist solely because the existing control deck already renders them for the
 * deterministic fixture. **No new surface may read a FIXTURE-ONLY field**: a
 * design settled against one goes half-empty the moment it meets a real
 * daemon. See `OverviewAgent` in `components/AgentOverview.tsx`, which is the
 * compiler-enforced honest projection.
 */
export interface AgentSession {
  /** HONEST. Per-daemon monotonic integer, so it is unique only within a daemon. */
  id: string;
  /** HONEST. */
  paneId?: string;
  /** HONEST. Orchestration role name, else the agent type. */
  role: string;
  /** HONEST. */
  displayName: string;
  /** HONEST. The daemon's agent type / CLI. */
  cli: string;
  /** FIXTURE-ONLY — the daemon tracks no model per agent (PRD #745, #633). */
  model: string;
  /** HONEST. */
  status: AgentStatus;
  /** HONEST only in so far as it restates `activeTool`; otherwise a placeholder. */
  task: string;
  /**
   * HONEST when the daemon reported one — but the deck's model has no way to
   * say "absent", so `agentFromDto` substitutes {@link UNREPORTED} and the deck
   * both prints and pattern-matches that sentinel. Any surface that must not
   * show a placeholder has to reverse the substitution at its own boundary;
   * `toOverviewAgent` is the one that does. NOT a worktree.
   */
  cwd: string;
  /**
   * FIXTURE-ONLY — no retry counter exists anywhere in the daemon, so live mode
   * reports NOTHING here rather than the `1` it used to hardcode: every tile
   * read `ATT 01` as if it were a fact (PRD #745 M8). Optional, so a surface
   * that renders it has to decide what absence looks like.
   */
  attempt?: number;
  /** FIXTURE-ONLY — `started_at` is invented on hydration, so a duration lies across a daemon restart. */
  duration: string;
  /** FIXTURE-ONLY — no token accounting in daemon state. */
  tokens: number;
  /** FIXTURE-ONLY — no cost accounting in daemon state. */
  cost: number;
  /** FIXTURE-ONLY — no context-window accounting in daemon state. */
  contextPercent: number;
  /** FIXTURE-ONLY — the daemon has no per-agent worktree or branch field. */
  worktree: string;
  /**
   * HONEST as of M8: live mode projects it from `SessionSnapshot.live_target`'s
   * `writable` half, which the desktop's own DTO used to drop.
   *
   * `"unknown"` is this field's sentinel for "the daemon declared no live
   * target", exactly as {@link UNREPORTED} is `cwd`'s — the deck's model has no
   * way to say "absent" — and it is reversed to `undefined` at the honest
   * projection (`toOverviewAgent`) so no screen that promises no placeholders
   * can print it. Absence must NOT be read as read-only: the TUI treats a
   * missing `live_target` as the legacy live default.
   */
  writeLease: "read" | "write" | "none" | "unknown";
  /**
   * HONEST. The most recent prompt the operator sent this agent
   * (`SessionSnapshot.last_user_prompt`), surfaced by M8 — the honest
   * replacement for live mode's hardcoded "Task metadata unavailable from
   * daemon". Optional rather than sentinel-bearing: a NEW field can represent
   * absence directly, so there is nothing here for a screen to leak.
   *
   * Free-form, agent-influenced text and the most attacker-shaped string the
   * overview renders, so every display copy goes through `displayText` with
   * `DISPLAY_LIMITS.prompt`.
   */
  lastUserPrompt?: string;
  /** HONEST. */
  rows: number;
  /** HONEST. */
  cols: number;
  /** HONEST. Name of the tool the daemon last reported as active. */
  activeTool?: string;
  /** HONEST. The active tool's detail, when the daemon reported one. */
  activeToolDetail?: string;
  /** HONEST. */
  toolCount: number;
  /**
   * FIXTURE-ONLY, all five. These are deck-internal collections the fixture
   * populates to make the control deck's panels demonstrable; live mode leaves
   * every one of them empty (`agentFromDto`), so a surface that renders one
   * shows nothing at all against a real daemon.
   */
  transcript: string;
  diff: string[];
  checks: CheckResult[];
  handoffIds: string[];
  artifacts: Artifact[];
  /**
   * HONEST. Which daemon owns this agent. Agent ids are per-daemon monotonic
   * integers starting at 1, so two daemons both mint `"1"` — anything keyed by
   * a bare `id` is wrong the moment #742 connects a second daemon. New surfaces
   * key by the composite `(daemonId, id)`; the pre-existing bare-id maps in the
   * bridge, the deck and the terminal registry are #742's to fix.
   */
  daemonId: string;
  /** HONEST. Tab membership as the daemon reported it. Drives grouping. */
  tab: AgentTab;
  /** HONEST. True when the daemon reports this pane as an orchestration role pane. */
  inOrchestration?: boolean;
  /** HONEST. True for the orchestration's start role — the coordinator an operator should message. */
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
  /**
   * FIXTURE-ONLY. Nothing tracks a per-agent or per-run git branch daemon-side
   * — the only `git branch` calls in `src/` are deletions in the dispatch flows
   * — so live mode reports nothing here instead of the literal `"Unavailable"`
   * it used to put in the topbar (PRD #745 M8). Reconstructing it would mean a
   * subprocess per agent cwd on the daemon or a desktop-side git call that
   * breaks the local-daemons-only boundary; both are out of scope.
   */
  branch?: string;
  worktree: string;
  connection: ConnectionView;
  health: RunHealth;
  elapsed: string;
  spend: number;
  currentNode: number;
  totalNodes: number;
  /** FIXTURE-ONLY — see `AgentSession.attempt`. Absent in live mode. */
  currentAttempt?: number;
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
  | { type: "allow_build_mismatch" }
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
  /**
   * States the whole set of agents whose terminal is on screen (PRD #745 M7).
   * A screen that mounts terminals calls this once per render commit with every
   * shown id; a screen that mounts none calls it with `[]`. Attach follows this
   * and nothing else, so a screen that renders no output opens no PTYs either.
   */
  setShownTerminals: (agentIds: string[]) => Promise<void>;
  reconnect: () => Promise<void>;
}
