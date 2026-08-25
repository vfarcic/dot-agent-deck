# Worktree ownership — how the deck proves it created a worktree

`dot-agent-deck worktree list|reclaim` (issue #422) removes a git worktree *without asking* only when three gates all hold: the branch's PR is `MERGED`, the tree is clean, and the deck can **prove it created the worktree**. This page is about the third gate — a marker file — and about the one rule that keeps it honest.

## The marker

Every worktree the deck creates gets a file named `dot-agent-deck-owner` written into the worktree's **own git metadata dir** — `<repo>/.git/worktrees/<name>/`, resolved by running `git rev-parse --git-dir` from inside the worktree. Writer and reader both live in [`src/worktree_owner.rs`](../../src/worktree_owner.rs), deliberately in one file: the reclaim path deletes directories, so a drift between "where it is written" and "where it is looked for" is a silent regression in which every deck worktree quietly reads as foreign again.

The location is not incidental. Three properties follow from it, and each is a way the obvious alternative (a dotfile in the working tree) gets this wrong:

- **It cannot make the worktree dirty.** The admin dir is outside the working tree, so `git status --porcelain` never sees the marker. That matters more than it first looks: the reclaim gate *keeps* every dirty worktree, so an in-tree marker would make marked worktrees permanently **un**reclaimable — the gate defeating itself. Verified directly rather than assumed.
- **It cannot outlive what it describes.** `git worktree remove` deletes the admin dir along with the worktree, marker included, so there is no stale claim left pointing at a path something else may later occupy.
- **It cannot be committed.** Nothing in the admin dir is part of the repository's content, so the claim never travels to another clone, where it would be a claim about a machine it was not made on.

## When it is written

At **creation time**, in the success arm of `issue_dispatch_run::create_worktree` — the only `git worktree add` anywhere in `src/`. Both creation paths funnel through it:

| Path | Entry point | Creator recorded |
| --- | --- | --- |
| `dot-agent-deck dispatch <name>`, including the orchestration spawn (one worktree, shared by every role) | `dispatch::handle_dispatch` | `dispatch` / the dispatch name |
| The issue-dispatch fire flow (one worktree per issue) | `issue_dispatch_run::run_one_issue` | `issue-dispatch` / `<task>#<issue>` |

Writing it at creation rather than at first use is what closes the window in which a worktree the deck genuinely created is not yet recognisable as its own.

The write is **best-effort and never fatal**: a failure warns and is dropped. The cost of a missing marker is one extra confirmation at reclaim time; the cost of propagating the error would be a failed dispatch. It is also **idempotent** — a single whole-file write, so a re-created or re-attached worktree replaces the document instead of accumulating one per creation.

The marker's content records **who** created the worktree (which creation path, and what for), plus the deck version, a timestamp, the pid and the branch. That content is informational. The gate itself is an **existence check** and never parses it, so a future change to the document's shape cannot silently reclassify every existing deck-created worktree as foreign.

## What is deliberately *not* marked

**A worktree that already exists is never adopted** — not by `create_worktree`, not by a retro-marking sweep, not by any code path. This is the whole point of the marker, so it is worth stating as a rule rather than as an omission:

- The marker is an ownership **claim**, and it is consumed by a path that **deletes directories**. The dangerous direction is therefore the false positive, not the false negative.
- The deck cannot tell "a worktree I created before this marker existed" from "a worktree a human, an orchestrator, or another tool created". On disk they are identical. Anything that marked the first would mark the second.
- The cost of *not* adopting is bounded and visible: an unmarked worktree reaches the `ask` verdict and needs an explicit `--yes`, which is the fail-safe direction. The cost of adopting wrongly is an unattended `git worktree remove` of somebody else's directory.

Concretely, `create_worktree` writes the marker only on `WorktreeCreation::Created`. `WorktreeCreation::AlreadyClaimed` means the directory was already on disk when our `git worktree add` ran — so *this* process did not create it — and that arm leaves the marker alone. Nothing is lost by that: a concurrent dispatch that genuinely created the directory writes its own marker from its own `Created` arm.

This is the same reasoning `cargo xtask clean-e2e-tmp` follows for temp roots (see [`e2e-temp-dirs.md`](e2e-temp-dirs.md), and CLAUDE.md rule 14): ownership is **proven, or asserted by an operator — never inferred**. There, an operator can assert across a reboot with `--ignore-liveness`; here, the operator's assertion is `--yes` on a named path.

## Consequence: worktrees created outside the deck stay foreign

An orchestrator that provisions a worktree with a plain `git worktree add` and then delegates into it produces an **unowned** worktree, and always will. That is not a gap in the marker; it is the marker working. If those worktrees should reclaim unattended, the fix is for the deck to be the thing that creates them — not for the deck to start claiming directories whose origin it cannot establish.

## Coverage

- `worktree/reclaim/010` (`tests/worktree_reclaim.rs`) — a worktree created through the production creation path reads `owned: true` / `verdict: remove` from `worktree list --json`, while an otherwise-identical hand-made sibling reads `owned: false` / `verdict: ask`.
- `create_worktree_marks_the_worktree_as_deck_owned_without_dirtying_it` (`src/issue_dispatch_run.rs`) — the marker lands in the metadata dir, not the working tree; `git status --porcelain` stays empty; re-marking replaces rather than appends.
- `create_worktree_never_marks_a_worktree_it_did_not_create` (`src/issue_dispatch_run.rs`) — the already-claimed arm leaves a foreign worktree unmarked.
