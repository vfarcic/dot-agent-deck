import type { ConfirmState } from "../components/ConfirmDialog";
import { deckName, DISPLAY_LIMITS, displayIdentity, displayText, rendersBlank } from "./displayText";
import type { AgentSession, DeckAction, DeckSnapshot } from "../types";

/** The identity a stop confirmation and its deck-scoped action need. */
export type StopAgent = Pick<AgentSession, "id" | "daemonId" | "displayName" | "tab" | "incarnation">;

/** What an identity cell says when the daemon's own value renders as nothing. */
export function unnamedAgentLabel(agent: Pick<StopAgent, "id">): string {
  const id = displayText(agent.id, DISPLAY_LIMITS.toolName);
  return rendersBlank(id) ? "unnamed agent" : `unnamed agent ${id}`;
}

/** What a stop or close confirmation calls one agent — the sanitised identity its row shows, or, in an orchestration, its role. */
export function stopTargetName(agent: StopAgent): string {
  const raw = agent.tab.kind === "orchestration" && agent.tab.roleName ? agent.tab.roleName : agent.displayName;
  return displayIdentity(raw, DISPLAY_LIMITS.name, unnamedAgentLabel(agent));
}

/** The overview's deck-scoped stop action, kept identical wherever the same agent can be closed. */
export function stopAgentAction(agent: StopAgent): DeckAction {
  return { type: "stop_agent", deckId: agent.daemonId, agentId: agent.id, incarnation: agent.incarnation };
}

/**
 * The overview row's close confirmation, shared by the full-window agent screen.
 * Closing an orchestration role is deliberately only that role: the body says
 * what remains running rather than making it look like the whole run was closed.
 */
export function closeAgentConfirmation(agent: StopAgent, deck: DeckSnapshot, run: (action: DeckAction) => Promise<void>): ConfirmState {
  const name = stopTargetName(agent);
  const roleNote = agent.tab.kind === "orchestration"
    ? ` This closes only ${name}; the other roles of its orchestration keep running without it.`
    : "";
  return {
    title: `Close ${name}?`,
    body: `This sends a stop request to ${name} on ${deckName(deck.connection)}.${roleNote} Unsaved terminal work may be interrupted.`,
    label: "Close agent",
    busyLabel: "Stopping…",
    action: () => run(stopAgentAction(agent)),
  };
}
