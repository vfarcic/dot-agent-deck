import { sanitizeText } from "../lib/displayText";
import { nestedWorktreesShown, type KeptWorktree } from "../lib/worktreeClose";

/** Keep the path's identifying tail visible, even in a narrow dialog or toast. A keep the user asked for reads as the TUI's status line does (issue #1958), and so does one that other worktrees inside it caused (issue #2285). */
export function KeptWorktreeText({ kept }: { kept: KeptWorktree }) {
  const nested = kept.on_request ? [] : nestedWorktreesShown(kept);
  const lead = kept.on_request ? "Worktree kept on request:" : nested.length ? "Holds other worktrees, so it is KEPT:" : kept.confirmed_dirty ? "Uncommitted work here is KEPT, not deleted:" : "Uncommitted work here, if any, is KEPT:";
  return <span className="kept-worktree-text">{lead} <code>{sanitizeText(kept.path)}</code>{nested.length ? <> (holds: {nested.map((p, i) => <span key={i}>{i ? ", " : ""}<code>{sanitizeText(p)}</code></span>)})</> : null}</span>;
}
