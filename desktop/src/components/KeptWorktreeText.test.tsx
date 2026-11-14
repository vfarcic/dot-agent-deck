import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { nestedWorktreesShown, worktreeKeptNotice } from "../lib/worktreeClose";
import { KeptWorktreeText } from "./KeptWorktreeText";

/* Issue #2285: a dispatched worktree that other worktrees lie inside is kept,
   and both clients say so in the same words as the TUI. */
describe("a kept worktree that holds other worktrees", () => {
  /** Scenario: the daemon reports a worktree kept because another worktree lies inside it. The dialog and notice text lead with "Holds other worktrees, so it is KEPT:", show the path, and name the nested worktree relative to it, as the TUI does; a dirty keep keeps its own wording. */
  it("says why it is kept and names the nested worktrees", () => {
    const kept = { path: "/w/unit", confirmed_dirty: false, nested_worktrees: ["/w/unit/.worktrees/feat-1", "/elsewhere/odd"] };
    const { container, rerender } = render(<KeptWorktreeText kept={kept} />);
    expect(container.textContent).toBe("Holds other worktrees, so it is KEPT: /w/unit (holds: .worktrees/feat-1, /elsewhere/odd)");

    rerender(<KeptWorktreeText kept={{ path: "/w/unit", confirmed_dirty: true }} />);
    expect(container.textContent).toBe("Uncommitted work here is KEPT, not deleted: /w/unit");
  });

  /** Scenario: a `worktree_kept` event carrying `nested_worktrees` becomes a notice that keeps them, dropping anything that is not a path, and one without them stays as it was. */
  it("reads the nested worktrees from the daemon event", () => {
    const notice = worktreeKeptNotice({ kind: "worktree_kept", deck: "d", path: "/w/unit", confirmed_dirty: false, nested_worktrees: ["/w/unit/.worktrees/a", 7] });
    expect(notice).toEqual({ deckId: "d", path: "/w/unit", confirmed_dirty: false, nested_worktrees: ["/w/unit/.worktrees/a"] });
    expect(nestedWorktreesShown(notice!)).toEqual([".worktrees/a"]);
    expect(worktreeKeptNotice({ kind: "worktree_kept", deck: "d", path: "/w", confirmed_dirty: true })).toEqual({ deckId: "d", path: "/w", confirmed_dirty: true });
  });
});
