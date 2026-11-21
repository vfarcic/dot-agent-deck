import type { AgentSession, DeckRuntimeState } from "../types";

/** The existing daemon preview and post-cleanup report share this wire shape. */
export interface KeptWorktree {
  path: string;
  confirmed_dirty: boolean;
  /** Issue #1958: kept because the close asked to keep it. Absent means false. */
  on_request?: boolean;
  /** Issue #2285: kept because these other worktrees lie inside it. Absent means none. */
  nested_worktrees?: string[];
}

/**
 * What the close dialog learns before it asks (`desktop_worktree_close_preview`).
 * `keepWorktree` is the dispatched worktree to offer "Keep the worktree" for
 * (issue #1958, a checkbox since issue #2436): set only when the deck can keep and the closed panes own a
 * recorded worktree, `null` otherwise — so an older deck never offers it.
 */
export interface WorktreeCloseReport {
  kept: KeptWorktree | null;
  keepWorktree: string | null;
}

export const NO_WORKTREE_CLOSE_REPORT: WorktreeCloseReport = { kept: null, keepWorktree: null };

export interface WorktreeKeptNotice extends KeptWorktree {
  deckId: string;
}

export interface WorktreeKeptEntry extends WorktreeKeptNotice {
  id: number;
  deck: string;
}

export type WorktreeClosePreview = (deckId: string, paneIds: string[], incarnation?: string) => Promise<WorktreeCloseReport>;

/** Capture the close's panes and process, rather than re-reading a refreshed card. */
export function worktreePreview(runtime: Pick<DeckRuntimeState, "previewWorktreeClose">, deckId: string, agents: Pick<AgentSession, "paneId" | "incarnation">[]): (() => Promise<WorktreeCloseReport>) | undefined {
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
  const nested = Array.isArray(event.nested_worktrees) ? event.nested_worktrees.filter((p): p is string => typeof p === "string") : [];
  return { deckId: event.deck, path: event.path, confirmed_dirty: event.confirmed_dirty, ...(event.on_request === true ? { on_request: true } : {}), ...(nested.length ? { nested_worktrees: nested } : {}) };
}

/** Issue #2285: the most nested worktrees the daemon names (`MAX_NESTED_WORKTREES_REPORTED` in `src/issue_dispatch_run.rs`); at it there may be more. */
export const MAX_NESTED_WORKTREES_REPORTED = 16;

/** Issue #2285: the worktrees that keep `kept`, each relative to it where it can be, as the TUI shows them. */
export function nestedWorktreesShown(kept: KeptWorktree): string[] {
  const parent = kept.path.replace(/\/+$/, "");
  return (kept.nested_worktrees ?? []).map((nested) => nested.startsWith(`${parent}/`) && nested.length > parent.length + 1 ? nested.slice(parent.length + 1) : nested);
}
