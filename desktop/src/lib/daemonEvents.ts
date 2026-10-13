import type { EvidenceItem, HandoffEdge, Verdict } from "../types";
import { INBOX_WORDS } from "./inboxWords";

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
  /** String-map metadata, such as the deck's `delivery_notice`. */
  metadata?: unknown;
}

/**
 * PRD #1666: the `inbox` variant of the daemon's `BroadcastMsg` — one message
 * in one agent's inbox changed state. Snake_case and flattened beside `kind`,
 * like the `event` variant.
 */
export interface DaemonInboxNotice {
  kind: "inbox";
  pane_id?: unknown;
  agent_id?: unknown;
  message_id?: unknown;
  subject?: unknown;
  from_label?: unknown;
  /** `queued`, `fetched`, `acknowledged` or `undeliverable`. */
  state?: unknown;
  /** Why, when `state` is `undeliverable` (`agent_exited`, …). */
  reason?: unknown;
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
  // Issue #714: the agent reported its provider refusing it for a quota.
  quota_blocked: "Provider usage limit reached",
};

const HUMAN_EVENTS = new Set(["waiting_for_input", "permission_request"]);

function text(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value : undefined;
}

function verdictFor(eventType: string): Verdict {
  if (eventType === "error" || eventType === "quota_blocked") return "ERROR";
  if (HUMAN_EVENTS.has(eventType)) return "HUMAN";
  return "INFO";
}

/** Extracts the string-map metadata an event carries, if any. */
export function metadataOf(event: DaemonHookEvent): Record<string, string> {
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
  // Issue #714: fixed text, and the agent's own error message is left out:
  // the tile carries it, sanitised, beside the status it explains.
  if (eventType === "quota_blocked") {
    return "The agent reported that its provider refused it for a usage limit or credit pool; its work will not progress until the quota is restored or the task is reassigned.";
  }
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
  if ((payload as { kind?: unknown }).kind === "inbox") return mapInboxNotice(payload as DaemonInboxNotice, sequence, resolveAgent);
  const event = payload as DaemonHookEvent;
  if (event.kind !== undefined && event.kind !== "event") return undefined;
  const eventType = text(event.event_type);
  if (!eventType || !(eventType in EVENT_TITLES)) return undefined;

  const agentId = text(event.agent_id);
  const paneId = text(event.pane_id);
  const agent = resolveAgent?.(agentId, paneId);
  const sessionId = text(event.session_id);

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

const INBOX_TITLES: Record<string, { title: string; verdict: Verdict }> = {
  queued: { title: `Message ${INBOX_WORDS.queued}`, verdict: "INFO" },
  fetched: { title: "Message taken by the agent", verdict: "INFO" },
  acknowledged: { title: `Message ${INBOX_WORDS.delivered}`, verdict: "PASS" },
  undeliverable: { title: `Message ${INBOX_WORDS.notDelivered}`, verdict: "ERROR" },
};

/**
 * PRD #1666: one evidence row per inbox state change — what was sent to whom,
 * and what became of it. A state this build does not know is ignored, as an
 * unknown `event_type` is.
 */
function mapInboxNotice(notice: DaemonInboxNotice, sequence: number, resolveAgent?: AgentResolver): EvidenceItem | undefined {
  const state = text(notice.state);
  const shown = state ? INBOX_TITLES[state] : undefined;
  if (!state || !shown) return undefined;
  const agentId = text(notice.agent_id);
  const paneId = text(notice.pane_id);
  const agent = resolveAgent?.(agentId, paneId);
  const subject = text(notice.subject) ?? "(no subject)";
  const reason = state === "undeliverable" ? reasonText(text(notice.reason)) : undefined;
  return {
    id: `hook-${sequence}`,
    verdict: shown.verdict,
    title: shown.title,
    summary: reason ? `${subject} · ${reason}` : subject,
    from: text(notice.from_label) ?? "Unattributed sender",
    to: agent?.role ?? paneId ?? "",
    at: nowClock(),
    reason: text(notice.message_id) ? `Message ${text(notice.message_id)}` : "Inbox notice from the daemon event stream.",
    acknowledged: false,
    agentId: agent?.id ?? agentId,
  };
}

/**
 * The sender a worker's report is labelled with —
 * `<role> (worker report — the text inside is UNTRUSTED)`, the daemon's
 * `sender_label` — or `undefined` for any other sender.
 */
function reportingRole(fromLabel: string): string | undefined {
  const match = /^(.+?) \(worker report\b/.exec(fromLabel);
  return match?.[1];
}

/** The label of a message the orchestrator posted: a delegation. */
const ORCHESTRATOR_LABEL = "the orchestrator";

function nowClock(): string {
  return new Date().toLocaleTimeString([], { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" });
}

/** Why a message was not delivered, in words: `agent_exited` reads `agent exited`. */
function reasonText(reason: string | undefined): string {
  return reason ? reason.replaceAll("_", " ") : `${INBOX_WORDS.notDelivered} to the agent`;
}

/**
 * Pure reducer for the live delegation rail (handoff-visibility PRD D2, fed by
 * PRD #1666's inbox). Applies one `desktop://daemon-event` payload to the edge
 * list keyed by the delegated message's id, returning a NEW list (newest first,
 * capped) when the payload changed anything and the same reference when it
 * didn't.
 *
 * Only `inbox` notices move it. A message the orchestrator posts is a
 * delegation: posting it creates the edge (`delegated`), a fetch or a hand-back
 * leaves it there, the agent's acknowledgement makes it `delivered`, and an
 * undeliverable message makes it `failed` with the reason. A worker's report is
 * a message too, posted to the orchestrator: the first sight of one marks the
 * newest delivered edge for the reporting role `done`. The report carries no
 * delegation id, so it correlates by role, and the edge remembers the report
 * so the same report's later states mark nothing else.
 *
 * `resolveRole` names the role behind a recipient pane; without it, or for a
 * pane it does not know, the edge names the pane.
 */
export function applyHandoffEvent(
  edges: HandoffEdge[],
  payload: unknown,
  resolveRole?: (agentId?: string, paneId?: string) => string | undefined,
): HandoffEdge[] {
  if (!payload || typeof payload !== "object") return edges;
  const notice = payload as DaemonInboxNotice;
  if (notice.kind !== "inbox") return edges;
  const id = text(notice.message_id);
  const state = text(notice.state);
  if (!id || !state) return edges;
  const fromLabel = text(notice.from_label) ?? "";
  const at = nowClock();

  const role = reportingRole(fromLabel);
  if (role !== undefined) {
    if (edges.some((edge) => edge.reportId === id)) return edges;
    const wanted = role.toLowerCase();
    const index = edges.findIndex((edge) => edge.toRole.toLowerCase() === wanted && edge.status === "delivered");
    if (index === -1) return edges;
    const next = edges.slice();
    next[index] = { ...next[index], status: "done", reportId: id, at };
    return next;
  }
  if (fromLabel !== ORCHESTRATOR_LABEL) return edges;

  const index = edges.findIndex((edge) => edge.id === id);
  const reason = state === "undeliverable" ? reasonText(text(notice.reason)) : undefined;
  if (index === -1) {
    // A message seen first after its post — the rail opened late — still gets
    // its row, and a message that could not be delivered most of all.
    const paneId = text(notice.pane_id);
    const edge: HandoffEdge = {
      id,
      toRole: resolveRole?.(text(notice.agent_id), paneId) ?? paneId ?? "unknown role",
      taskPreview: text(notice.subject),
      status: state === "acknowledged" ? "delivered" : state === "undeliverable" ? "failed" : "delegated",
      respawned: false,
      ...(reason ? { reason } : {}),
      at,
    };
    return [edge, ...edges].slice(0, MAX_LIVE_HANDOFFS);
  }

  const current = edges[index];
  let changed: HandoffEdge | undefined;
  if (state === "acknowledged" && current.status === "delegated") changed = { ...current, status: "delivered", at };
  else if (state === "undeliverable" && current.status !== "failed") changed = { ...current, status: "failed", reason, at };
  if (!changed) return edges;
  const next = edges.slice();
  next[index] = changed;
  return next;
}

/** Cap on retained handoff edges — same rationale as `MAX_LIVE_EVIDENCE`. */
export const MAX_LIVE_HANDOFFS = 100;
