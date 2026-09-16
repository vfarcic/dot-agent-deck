/**
 * The composite identity every per-agent map in the runtime is keyed by.
 *
 * # Why a bare agent id is not an identity
 *
 * Agent ids are **per-daemon monotonic integers starting at 1**, so two decks
 * both mint `1`, `2`, `planner`. `AgentSession.daemonId` says so in as many
 * words and PRD #742 M5 fixed the fleet's own key for exactly this reason. What
 * it left bare were the runtime's per-agent maps — the terminal feed's buffers,
 * the recorded send verdicts and the applied geometry — because until PRD #1105
 * nothing could put a second deck's agent in front of one of them: a tile's
 * terminal is always the *selected* deck's, and the overview mounts none.
 *
 * The agent pane is what made them reachable. It can be opened for an agent on
 * a deck that is **not** the selected one, so a map keyed by `"planner"` alone
 * answers a question about deck B with deck A's state — and does it invisibly,
 * because the role and the display text match. The security audit on this PRD
 * found two blocker-class instances of exactly that (a retained terminal buffer
 * replayed under the new agent's heading, and a cached grid submitted to
 * another machine's PTY), which is why the decision to leave these three maps
 * bare was reopened and reversed.
 *
 * # The key
 *
 * `NUL` separates, because it is the one byte neither component can contain:
 * both are daemon identities that travel through JSON and through TOML, and the
 * crate rejects a NUL anywhere it validates one. Nothing may ever `split` this
 * back apart — it is a map key and a dependency key, never a pair of values.
 *
 * An absent `deckId` is its own key rather than a wildcard. A runtime that does
 * not yet know which deck it is on has no business matching one that does; the
 * value is `undefined` only before the first snapshot has landed, which is
 * before any agent exists to key on.
 */
export function agentKey(deckId: string | undefined, agentId: string): string {
  return `${deckId ?? ""}\0${agentId}`;
}
