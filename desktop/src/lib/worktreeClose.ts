import type { AgentSession, DeckRuntimeState } from "../types";

/** The existing daemon preview and post-cleanup report share this wire shape. */
export interface KeptWorktree {
  path: string;
  confirmed_dirty: boolean;
}

export interface WorktreeKeptNotice extends KeptWorktree {
  deckId: string;
}

export interface WorktreeKeptEntry extends WorktreeKeptNotice {
  id: number;
  deck: string;
}

export type WorktreeClosePreview = (deckId: string, paneIds: string[], incarnation?: string) => Promise<KeptWorktree | null>;

/** Capture the close's panes and process, rather than re-reading a refreshed card. */
export function worktreePreview(runtime: Pick<DeckRuntimeState, "previewWorktreeClose">, deckId: string, agents: Pick<AgentSession, "paneId" | "incarnation">[]): (() => Promise<KeptWorktree | null>) | undefined {
  const ask = runtime.previewWorktreeClose;
  const paneIds = agents.flatMap((agent) => agent.paneId ? [agent.paneId] : []);
  const incarnation = agents[0]?.incarnation;
  return ask && paneIds.length ? () => ask(deckId, paneIds, incarnation) : undefined;
}

/** Worktree reports belong to their source deck, including one not selected. */
export function worktreeKeptNotice(payload: unknown): WorktreeKeptNotice | undefined {
  if (!payload || typeof payload !== "object") return undefined;
  const event = payload as Record<string, unknown>;
  if (event.kind !== "worktree_kept" || typeof event.deck !== "string" || typeof event.path !== "string" || typeof event.confirmed_dirty !== "boolean") return undefined;
  return { deckId: event.deck, path: event.path, confirmed_dirty: event.confirmed_dirty };
}
