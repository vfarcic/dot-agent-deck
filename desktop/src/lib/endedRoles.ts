import type { EndedRole } from "../types";

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
