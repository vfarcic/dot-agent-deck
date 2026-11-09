import { sanitizeText } from "../lib/displayText";
import type { KeptWorktree } from "../lib/worktreeClose";

/** Keep the path's identifying tail visible, even in a narrow dialog or toast. */
export function KeptWorktreeText({ kept }: { kept: KeptWorktree }) {
  return <span className="kept-worktree-text">{kept.confirmed_dirty ? "Uncommitted work here is KEPT, not deleted:" : "Uncommitted work here, if any, is KEPT:"} <code>{sanitizeText(kept.path)}</code></span>;
}
