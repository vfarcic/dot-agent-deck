import type { EndedRole } from "../types";
import { DISPLAY_LIMITS, displayText } from "./displayText";

/**
 * Issue #1965 — the words both clients use for an orchestration role whose
 * agent exited (CLAUDE.md rule 22). The TUI prints the same strings from
 * `src/ended_role.rs`, and `tests/fixtures/ended-role.json` pins the two to each
 * other: change a word here and there together. The reason itself arrives
 * already worded from the daemon crate (`EndedRole.reason`).
 */
export const ENDED_ROLE_WORDS = {
  /** The status an exited role shows where a live role shows its agent's. */
  status: "Exited",
  /** The label in front of the last lines its pane showed. */
  lastOutput: "Last output",
} as const;

/**
 * The exited roles of one orchestration that its live agents do not cover, in
 * role order: a role whose index a live agent already holds was restarted, and
 * shows as that agent instead.
 */
export function endedRolesOf(endedRoles: readonly EndedRole[] | undefined, orchestrationId: string | undefined, liveRoleIndexes: ReadonlySet<number>): EndedRole[] {
  if (!endedRoles?.length || orchestrationId === undefined) return [];
  const byRole = new Map<number, EndedRole>();
  for (const role of endedRoles) {
    if (role.orchestrationId !== orchestrationId || liveRoleIndexes.has(role.roleIndex)) continue;
    // The daemon lists newest generations last; the newest says how it ends now.
    byRole.set(role.roleIndex, role);
  }
  return [...byRole.values()].sort((left, right) => left.roleIndex - right.roleIndex);
}

/**
 * Issue #1965 — the exited role a card is for: the same pane and the same
 * agent generation. A role restarted since then is another agent, so its
 * card matches nothing.
 */
export function endedRoleOfCard(endedRoles: readonly EndedRole[] | undefined, entry: { pane_id: string; agent_id: string }): EndedRole | undefined {
  return endedRoles?.find((role) => role.paneId === entry.pane_id && role.agentId === entry.agent_id);
}

/**
 * Issue #1965 on the compact card (issue #2418): an exited role's reason and
 * last output as one line of text, to sit on the card's status line, which
 * truncates rather than wraps; the Card tab shows them in full.
 */
export function endedRoleLine(role: EndedRole): string {
  const reason = `${ENDED_ROLE_WORDS.status}: ${displayText(role.reason, DISPLAY_LIMITS.title)}`;
  if (role.lastOutput.length === 0) return reason;
  return `${reason} · ${ENDED_ROLE_WORDS.lastOutput}: ${role.lastOutput.map((line) => displayText(line, DISPLAY_LIMITS.title)).join(" ⏎ ")}`;
}
