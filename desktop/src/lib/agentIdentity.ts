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
