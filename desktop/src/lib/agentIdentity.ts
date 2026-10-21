/**
 * PRD #1666 part 2 (G-fix5): what both clients say when an action on an agent
 * is refused because the deck restarted since the agent was listed — the
 * agent is gone. The TUI's `AGENT_GONE_AFTER_RESTART`, word for word
 * (CLAUDE.md rule 22).
 */
export const AGENT_GONE_AFTER_RESTART = "that agent is gone (the deck restarted)";

/**
 * PRD #1666 part 2 (G-fix6): what both clients say when an action on an agent
 * was not sent because the deck named no daemon process, so there is no
 * telling its agent from a successor's. The TUI's `AGENT_IDENTITY_UNKNOWN`.
 */
export const AGENT_IDENTITY_UNKNOWN = "the deck did not identify itself, so the action on that agent was not sent";

/**
 * PRD #1666 part 2 (G-fix7c): how both clients open the one notice they show
 * when a deck they were connected to comes back as another daemon process —
 * it restarted, and the agents it ran before are dropped from the screen. The
 * TUI's `AGENTS_STOPPED_BY_RESTART`, word for word (CLAUDE.md rule 22).
 */
export const AGENTS_STOPPED_BY_RESTART = "The deck restarted; these agents were stopped";

/** One agent a deck restart stopped, as {@link agentsStoppedByRestartNotice} names it. */
export interface StoppedAgent {
  label: string;
  role?: string;
  orchestration?: string;
}

/**
 * The notice naming the agents a deck restart stopped: each by its label, with
 * its role and orchestration when it had them, so the user can tell which
 * agent was doing what. The TUI's `agents_stopped_by_restart_notice`.
 */
export function agentsStoppedByRestartNotice(agents: readonly StoppedAgent[]): string {
  const items = agents.map(({ label, role, orchestration }) => {
    if (role && orchestration) return `${label} (${role} in ${orchestration})`;
    if (role) return `${label} (${role})`;
    if (orchestration) return `${label} (in ${orchestration})`;
    return label;
  });
  return `${AGENTS_STOPPED_BY_RESTART}: ${items.join(", ")}.`;
}
