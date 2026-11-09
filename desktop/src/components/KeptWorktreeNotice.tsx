import type { KeptWorktree } from "../types";
import { sanitizeText } from "../lib/displayText";

/** The TUI's words, including the distinction between dirty and unverified. */
export function keptWorktreeHeadline(kept: KeptWorktree): string {
  return kept.confirmed_dirty ? "Uncommitted work here is KEPT, not deleted:" : "Uncommitted work here, if any, is KEPT:";
}

export function KeptWorktreeNotices({ worktrees }: { worktrees?: readonly KeptWorktree[] }) {
  return <>{worktrees?.map((kept, index) => <p className="kept-worktree-notice" role="status" key={index}>{keptWorktreeHeadline(kept)} <code>{sanitizeText(kept.path)}</code></p>)}</>;
}
