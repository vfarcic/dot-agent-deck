/**
 * PRD #1541 — what the voice surface knows about the agent in the open pane
 * when it presses keys at it: which agent it is, whether it is mid-turn, and
 * the keys the deck served for it.
 *
 * Nothing here holds a key. The keys are the DECK's (`AgentRecord.prompt_keys`,
 * served from the daemon's own agent registry), because they depend on the
 * agent version on the deck's host, not on this app's build — so this module
 * only names agents and reads statuses.
 */
import type { AgentSession, AgentTurn, AgentTypeId, PromptKeys } from "../types";

/**
 * The name a sentence uses for an agent type ("Pi has no key to …"). Absent
 * for `none`, which is not an agent anybody can be told about by name.
 *
 * A local table on purpose, unlike the keys: it is wording this app owns, and
 * the deck serves no label. A type a newer deck invents arrives as `none`.
 */
const AGENT_TYPE_LABELS: Record<AgentTypeId, string | undefined> = {
  claude_code: "Claude Code",
  open_code: "OpenCode",
  codex: "Codex",
  pi: "Pi",
  devin: "Devin",
  none: undefined,
};

export function agentTypeLabel(agentType: AgentTypeId | undefined): string | undefined {
  return agentType === undefined ? undefined : AGENT_TYPE_LABELS[agentType];
}

/**
 * Mid-turn or idle, from the daemon's status vocabulary (`DesktopAgent.status`).
 *
 * Only the statuses that SAY so answer: `running` is what the desktop crate
 * reports for an agent with no hook state at all, so it is not evidence of a
 * turn, and `error`, `blocked` and `unknown` say nothing about one either.
 * Voice's interrupt is sent only on `working`, so every one of those refuses.
 */
export function agentTurn(status: string): AgentTurn | undefined {
  switch (status) {
    case "thinking":
    case "working":
    case "compacting":
      return "working";
    case "idle":
    case "waiting_for_input":
      return "idle";
    default:
      return undefined;
  }
}

/** The agent half of the voice surface's pane, read off the open agent. */
export interface VoicePaneAgent {
  /** The daemon's agent type; absent when the deck did not say. */
  agentType?: AgentTypeId;
  /** The type's name for sentences; absent for `none` and when the type is unknown. */
  agentLabel?: string;
  /** Mid-turn or idle; absent when the status says neither. */
  turn?: AgentTurn;
  /** The deck's keys for this agent; absent ⇒ the deck is too old or the agent is unsupported. */
  promptKeys?: PromptKeys;
}

export function voicePaneAgent(agent: Pick<AgentSession, "agentType" | "turn" | "promptKeys">): VoicePaneAgent {
  const agentLabel = agentTypeLabel(agent.agentType);
  return {
    ...(agent.agentType ? { agentType: agent.agentType } : {}),
    ...(agentLabel ? { agentLabel } : {}),
    ...(agent.turn ? { turn: agent.turn } : {}),
    ...(agent.promptKeys ? { promptKeys: agent.promptKeys } : {}),
  };
}
