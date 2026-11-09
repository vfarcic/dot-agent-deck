import { sanitizeText } from "./displayText";
import type { AgentSession, ConnectionView, DeckRuntimeState, KeptWorktree } from "../types";

export type WorktreeAgent = Pick<AgentSession, "daemonId" | "paneId" | "incarnation">;

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

/**
 * Build a close confirmation's best-effort preview from the daemon's current
 * agent registry. The daemon suppresses it while another live agent uses the
 * worktree, so a returned report must not be filtered against a stale listing.
 */
export function worktreeClosePreview(
  runtime: Pick<DeckRuntimeState, "previewWorktreeClose">,
  connection: ConnectionView,
  closing: readonly WorktreeAgent[],
): (() => Promise<string | undefined>) | undefined {
  const preview = runtime.previewWorktreeClose;
  const deckId = connection.deckId ?? closing[0]?.daemonId;
  if (!preview || !deckId || !closing.length || closing.some((agent) => !agent.paneId)) return undefined;
  const paneIds = closing.map((agent) => agent.paneId!);
  const incarnation = connection.incarnation ?? closing[0]?.incarnation;
  return async () => {
    const kept = await preview(deckId, paneIds, incarnation);
    return kept ? keptWorktreeText(kept) : undefined;
  };
}
