import { sanitizeText } from "./displayText";
import type { AgentSession, ConnectionView, DeckRuntimeState, KeptWorktree } from "../types";

export type WorktreeAgent = Pick<AgentSession, "id" | "daemonId" | "paneId" | "cwd" | "tab" | "incarnation">;

/** The same two headlines the TUI uses for a kept dispatched worktree. */
export function keptWorktreeHeadline(kept: Pick<KeptWorktree, "confirmedDirty">): string {
  return kept.confirmedDirty
    ? "Uncommitted work here is KEPT, not deleted:"
    : "Uncommitted work here, if any, is KEPT:";
}

/** A kept-worktree report with its full, sanitised recovery path. */
export function keptWorktreeText(kept: KeptWorktree): string {
  return `${keptWorktreeHeadline(kept)} ${sanitizeText(kept.path)}`;
}

/** The directory the daemon's worktree accounting derives from a listed agent. */
function worktreeOf(agent: WorktreeAgent): string | undefined {
  return agent.tab.kind === "orchestration" ? agent.tab.cwd ?? agent.cwd : agent.cwd;
}

/**
 * Build a close confirmation's best-effort preview. A surviving agent rooted in
 * the returned directory suppresses it: the daemon will not clean up that tree
 * until its last agent closes, so an earlier role's close is not a kept-tree event.
 */
export function worktreeClosePreview(
  runtime: Pick<DeckRuntimeState, "previewWorktreeClose">,
  connection: ConnectionView,
  closing: readonly WorktreeAgent[],
  listed: readonly WorktreeAgent[],
): (() => Promise<string | undefined>) | undefined {
  const preview = runtime.previewWorktreeClose;
  const deckId = connection.deckId ?? closing[0]?.daemonId;
  if (!preview || !deckId || !closing.length || closing.some((agent) => !agent.paneId)) return undefined;
  const paneIds = closing.map((agent) => agent.paneId!);
  const incarnation = connection.incarnation ?? closing[0]?.incarnation;
  const survivors = listed.filter((agent) => !closing.some((closed) => closed.daemonId === agent.daemonId && closed.id === agent.id));
  return async () => {
    const kept = await preview(deckId, paneIds, incarnation);
    if (!kept || survivors.some((agent) => worktreeOf(agent) === kept.path)) return undefined;
    return keptWorktreeText(kept);
  };
}
