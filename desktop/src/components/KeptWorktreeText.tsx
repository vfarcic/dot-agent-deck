import { sanitizeText } from "../lib/displayText";
import type { KeptWorktree } from "../lib/worktreeClose";

/** Keep the path's identifying tail visible, even in a narrow dialog or toast. A keep the user asked for reads as the TUI's status line does (issue #1958). */
export function KeptWorktreeText({ kept }: { kept: KeptWorktree }) {
  const lead = kept.on_request ? "Worktree kept on request:" : kept.confirmed_dirty ? "Uncommitted work here is KEPT, not deleted:" : "Uncommitted work here, if any, is KEPT:";
  return <span className="kept-worktree-text">{lead} <code>{sanitizeText(kept.path)}</code></span>;
}
