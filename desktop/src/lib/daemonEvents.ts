import type { EvidenceItem, HandoffEdge, Verdict } from "../types";

/**
 * Cap on the in-memory live evidence ring. Hook events arrive for every tool
 * call, so an uncapped list grows without bound over a long run.
 */
export const MAX_LIVE_EVIDENCE = 500;

/**
 * The `desktop://daemon-event` payload is the daemon's `BroadcastMsg`, an
 * internally tagged enum (`kind`). Its `event` variant flattens an `AgentEvent`,
 * whose fields are serialized in **snake_case** — the Rust struct carries no
 * camelCase rename, unlike the desktop DTOs.
 */
export interface DaemonHookEvent {
  kind?: string;
  session_id?: unknown;
  agent_type?: unknown;
  event_type?: unknown;
  tool_name?: unknown;
  tool_detail?: unknown;
  cwd?: unknown;
  timestamp?: unknown;
  user_prompt?: unknown;
  pane_id?: unknown;
  agent_id?: unknown;
  /** Handoff-lifecycle payload (roles, task preview, failure reason). */
  metadata?: unknown;
}

/** Resolves the deck-side agent behind a hook event, when one is known. */
export type AgentResolver = (agentId?: string, paneId?: string) => { id: string; role: string } | undefined;

const EVENT_TITLES: Record<string, string> = {
  session_start: "Session started",
  session_end: "Session ended",
  tool_start: "Tool started",
  tool_end: "Tool finished",
  thinking: "Thinking",
  compacting: "Compacting context",
  subagent_start: "Subagent started",
  subagent_stop: "Subagent finished",
  waiting_for_input: "Waiting for input",
  permission_request: "Permission requested",
  idle: "Idle",
  error: "Agent reported an error",
  delegation_dispatched: "Delegation dispatched",
  delegation_delivered: "Task delivered to worker",
  delegation_failed: "Delegation FAILED",
  worker_respawned: "Worker respawned for delegation",
  work_done_received: "Work-done received",
};

/** The daemon-emitted handoff lifecycle (handoff-visibility PRD D1). */
export const HANDOFF_EVENTS = new Set([
  "delegation_dispatched",
  "delegation_delivered",
  "delegation_failed",
  "worker_respawned",
  "work_done_received",
]);

const HUMAN_EVENTS = new Set(["waiting_for_input", "permission_request"]);

function text(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function verdictFor(eventType: string): Verdict {
  if (eventType === "error" || eventType === "delegation_failed") return "ERROR";
  if (HUMAN_EVENTS.has(eventType)) return "HUMAN";
  if (eventType === "work_done_received") return "PASS";
  return "INFO";
}

/** Extracts the string-map metadata a handoff event carries, if any. */
function metadataOf(event: DaemonHookEvent): Record<string, string> {
  if (!event.metadata || typeof event.metadata !== "object") return {};
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(event.metadata as Record<string, unknown>)) {
    if (typeof value === "string") out[key] = value;
  }
  return out;
}

function clockFor(timestamp: unknown): string {
  const raw = text(timestamp);
  if (!raw) return "—";
  const parsed = new Date(raw);
  if (Number.isNaN(parsed.getTime())) return "—";
  return parsed.toLocaleTimeString([], { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

function summaryFor(event: DaemonHookEvent, eventType: string): string {
  const tool = text(event.tool_name);
  const detail = text(event.tool_detail);
  if (tool) return detail ? `${tool} · ${detail}` : tool;
  const prompt = text(event.user_prompt);
  if (prompt) return prompt.length > 240 ? `${prompt.slice(0, 240)}…` : prompt;
  const agentType = text(event.agent_type)?.replaceAll("_", " ");
  return agentType ? `${agentType} reported ${eventType.replaceAll("_", " ")}.` : `Agent reported ${eventType.replaceAll("_", " ")}.`;
}

/**
 * Maps one `desktop://daemon-event` payload onto the evidence model the drawer
 * already renders, or returns `undefined` for anything this build should ignore
 * — the `orchestration_surface` variant, an unrecognised `kind`, or an
 * `event_type` a future daemon adds. Ignoring beats guessing: a mis-mapped
 * event would show the operator an edge that never happened.
 *
 * Hook events are point-in-time signals, not handoff edges, so `to` stays empty
 * rather than inventing a receiver the daemon never reported.
 */
export function mapDaemonEvent(payload: unknown, sequence: number, resolveAgent?: AgentResolver): EvidenceItem | undefined {
  if (!payload || typeof payload !== "object") return undefined;
  const event = payload as DaemonHookEvent;
  if (event.kind !== undefined && event.kind !== "event") return undefined;
  const eventType = text(event.event_type);
  if (!eventType || !(eventType in EVENT_TITLES)) return undefined;

  const agentId = text(event.agent_id);
  const paneId = text(event.pane_id);
  const agent = resolveAgent?.(agentId, paneId);
  const sessionId = text(event.session_id);

  if (HANDOFF_EVENTS.has(eventType)) {
    const metadata = metadataOf(event);
    const failed = eventType === "delegation_failed";
    const workDone = eventType === "work_done_received";
    const summaryParts = [
      metadata.task_preview,
      failed ? metadata.reason : undefined,
      workDone && metadata.done === "true" ? "Run reported complete." : undefined,
    ].filter(Boolean);
    return {
      id: `hook-${sequence}`,
      verdict: verdictFor(eventType),
      title: EVENT_TITLES[eventType],
      summary: summaryParts.join(" · ") || EVENT_TITLES[eventType],
      from: workDone ? (metadata.from_role || agent?.role || "worker") : "orchestrator",
      to: workDone ? "orchestrator" : (metadata.to_role || ""),
      at: clockFor(event.timestamp),
      reason: sessionId ? `Delegation ${sessionId}` : "Daemon handoff event",
      acknowledged: false,
      agentId: agent?.id ?? agentId,
    };
  }

  return {
    id: `hook-${sequence}`,
    verdict: verdictFor(eventType),
    title: EVENT_TITLES[eventType],
    summary: summaryFor(event, eventType),
    from: agent?.role ?? agentId ?? paneId ?? sessionId ?? "Unattributed agent",
    to: "",
    at: clockFor(event.timestamp),
    reason: "Live hook event from the daemon event stream.",
    acknowledged: false,
    agentId: agent?.id ?? agentId,
  };
}

/**
 * Pure reducer for the live handoff-edge model (handoff-visibility PRD D2).
 * Applies one `desktop://daemon-event` payload to the edge list keyed by the
 * daemon's delegation id, returning a NEW list (newest first, capped) when the
 * event changed anything and the same reference when it didn't.
 *
 * Lifecycle: `delegation_dispatched` creates the edge; `worker_respawned`
 * flags it; `delegation_delivered` / `delegation_failed` settle transport;
 * `work_done_received` marks the newest settled edge for that role as done —
 * work-done is a role-level signal (the CLI carries no delegation id), so it
 * correlates by `from_role` rather than by edge id.
 */
export function applyHandoffEvent(edges: HandoffEdge[], payload: unknown): HandoffEdge[] {
  if (!payload || typeof payload !== "object") return edges;
  const event = payload as DaemonHookEvent;
  if (event.kind !== undefined && event.kind !== "event") return edges;
  const eventType = text(event.event_type);
  if (!eventType || !HANDOFF_EVENTS.has(eventType)) return edges;
  const id = text(event.session_id) ?? `dlg-unknown-${edges.length}`;
  const metadata = metadataOf(event);
  const at = clockFor(event.timestamp);

  if (eventType === "delegation_dispatched") {
    const edge: HandoffEdge = {
      id,
      toRole: metadata.to_role || "unknown role",
      orchestration: metadata.orchestration || undefined,
      taskPreview: metadata.task_preview || undefined,
      status: "dispatched",
      respawned: false,
      at,
    };
    return [edge, ...edges].slice(0, MAX_LIVE_HANDOFFS);
  }

  if (eventType === "work_done_received") {
    const role = metadata.from_role;
    if (!role) return edges;
    const index = edges.findIndex((edge) => edge.toRole === role && edge.status === "delivered");
    if (index === -1) return edges;
    const next = edges.slice();
    next[index] = { ...next[index], status: "done", at };
    return next;
  }

  const index = edges.findIndex((edge) => edge.id === id);
  if (index === -1) {
    // A failure with no prior dispatch (e.g. "no live pane for role") still
    // deserves an edge — it is the loudest case of all.
    if (eventType !== "delegation_failed") return edges;
    const edge: HandoffEdge = {
      id,
      toRole: metadata.to_role || "unknown role",
      orchestration: metadata.orchestration || undefined,
      status: "failed",
      respawned: false,
      reason: metadata.reason || "delegation failed",
      at,
    };
    return [edge, ...edges].slice(0, MAX_LIVE_HANDOFFS);
  }

  const next = edges.slice();
  const current = next[index];
  if (eventType === "worker_respawned") {
    next[index] = { ...current, respawned: true, at };
  } else if (eventType === "delegation_delivered") {
    next[index] = { ...current, status: "delivered", at };
  } else {
    next[index] = { ...current, status: "failed", reason: metadata.reason || "delegation failed", at };
  }
  return next;
}

/** Cap on retained handoff edges — same rationale as `MAX_LIVE_EVIDENCE`. */
export const MAX_LIVE_HANDOFFS = 100;
