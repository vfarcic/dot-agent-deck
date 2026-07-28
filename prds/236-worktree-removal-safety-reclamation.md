# PRD #236: Worktree removal safety + reclamation — never force-remove a dirty tree; reclaim the ones we keep

**GitHub Issue**: [#236](https://github.com/vfarcic/dot-agent-deck/issues/236)

**Priority**: High

**Status**: Not started

## Problem Statement

Two halves of one lifecycle gap.

**Half 1 — removal is unconditional.** `remove_worktree` (`src/issue_dispatch_run.rs:133`) runs `git worktree remove <wt> --force`. The `--force` flag is precisely what overrides git's refusal to remove a worktree holding uncommitted changes or untracked files, and there is no dirty check, no open-PR check, and no merged check anywhere on the path to it. The trigger is a normal pane close: `AttachRequest::StopAgent` (`src/daemon_protocol.rs:1339`) captures the closing agent's record, confirms via `worktree_still_in_use` (`:123`) that it was the last agent rooted in the tree, then removes the tree. So closing the last pane of a dispatched tab destroys any uncommitted work in it, silently and with no confirmation. Committed work survives (the branch is never deleted); uncommitted and untracked work does not. Ctrl+W reads to a user as "close this view", not "destroy this".

**Half 2 — nothing reclaims a tree we keep.** The `WorktreeRegistry` (`:75`) is an in-memory `Arc<Mutex<HashMap<PathBuf, PathBuf>>>`, wiped on daemon restart — a post-restart close finds no entry and leaves the tree in place (documented at `:63-68`). Quitting the TUI is a *detach*, not a stop, so it never triggers cleanup either. Issue-dispatch (#120) tolerates this because the next scheduled fire sees the tree present and treats it as already-claimed, which reclaims it implicitly. A **user-driven** dispatch (#220) has no next fire, so a leaked tree is permanent until someone prunes it by hand. The only existing reclamation is `dot-ai-tag-release`'s step 6, which prunes *merged* worktrees and branches at release time — the wrong granularity and the wrong cadence for this.

These halves are coupled, which is why they belong in one PRD: the fix for half 1 is to **keep** trees we would previously have force-removed, which deliberately makes leaks more common. Shipping half 1 alone trades silent data loss for unbounded disk growth. That is the right trade, but it must not be the end state.

## Solution Overview

**Fail toward leaking.** A leaked worktree costs disk; a force-removed one costs work. That asymmetry decides the policy: never remove a tree that might hold unsaved work, and make the kept trees visible and prunable instead.

1. Gate removal on a clean tree — drop `--force`, or check `git status --porcelain` first — and when the tree is dirty, keep it and surface `worktree kept at <path>: uncommitted changes` rather than failing silently.
2. Add the reclamation half: a way to see and prune kept/orphaned worktrees, so "we kept it" does not mean "it is here forever".

One policy governs every worktree-creating feature — #120 today, #220 next, #174 after — rather than each re-deciding.

## Scope

### In Scope

- **Removal gating** in `remove_worktree` (`src/issue_dispatch_run.rs:133`) and its call sites: the `StopAgent` cleanup path (`src/daemon_protocol.rs:1339`, cleanup at ~`:1400-1408`) and any spawn-failure rollback. A rollback immediately after a failed spawn is the one case where the tree is known-empty and force removal stays correct — the gate must not break it.
- **Visible outcome when a tree is kept.** The user closed a tab and something was retained; that must be reported, not just logged. Surface it where the user is looking (daemon log alone is insufficient).
- **Reclamation path** for kept and orphaned trees. Shape to decide in M2 (see Open Questions): a `prune` CLI verb, a startup/periodic sweep of registered-but-dead trees, a deck surface listing them, or a combination.
- **Restart durability of the registry**, at least enough that a post-restart close can still identify a tree as ours. Today a restart orphans every tree permanently. Persisting the registry is one option; deriving ownership from the tree's path/branch naming convention is another and needs no new state.
- **Tests** (CLAUDE.md rule 4): the dirty-tree-is-kept path and the clean-tree-is-removed path both asserted; the spawn-failure rollback still removes.
- **Docs**: document the removal policy and how to prune, and repoint any copy that currently implies tab close cleans up unconditionally.

### Out of Scope

- **Changing what creates worktrees** or where they live — that is #220's decision (siblings for user-driven dispatch; #120 keeps `.worktrees/` inside its dedicated clone, kept out of `git status` via the clone-local `.git/info/exclude`, `:446-457`).
- **Branch cleanup.** Branches are deliberately never deleted here; committed work must stay recoverable. A branch-pruning policy is separate work.
- **Cross-project dispatch** (#174) — it inherits this policy but adds nothing to it.
- **Replacing `dot-ai-tag-release`'s merged-worktree prune.** That stays; this adds a mechanism for the not-yet-merged case it cannot reach.

## Success Criteria

- Closing a dispatched tab whose worktree has uncommitted or untracked changes **never** destroys them; the tree is kept and the user is told where it is.
- Closing a dispatched tab whose worktree is clean still removes it, and the owning clone is preserved.
- A failed spawn still rolls its just-created (empty) worktree back immediately.
- A user can list kept/orphaned worktrees and prune them in one step, without hand-running `git worktree`.
- A daemon restart no longer permanently orphans a worktree.
- One policy covers #120 and #220; neither feature carries its own divergent removal logic.
- `cargo test-fast` green per task; `cargo test-e2e` green pre-PR.

## Milestones

### Phase 1: Stop the data loss

- [ ] **M1.0** — Gate removal on a clean tree (drop `--force` or pre-check `git status --porcelain`), preserving the known-empty spawn-failure rollback as an explicit exception.
- [ ] **M1.1** — Surface the kept-tree outcome to the user (`worktree kept at <path>: uncommitted changes`), not to the log only.
- [ ] **M1.2** — Tests for all three paths: dirty → kept, clean → removed, spawn failure → removed.

### Phase 2: Reclamation

- [ ] **M2.0** — Decide the reclamation shape (see Open Questions) and make kept/orphaned worktrees *discoverable* — the user can find out what exists without running git by hand.
- [ ] **M2.1** — Make them *prunable* in one step, with the same clean/dirty gate applied (a prune must not become a new force-remove).
- [ ] **M2.2** — Address restart orphaning, either by persisting the registry or by deriving ownership from the path/branch convention.

### Phase 3: Ship

- [ ] **M3.0** — Docs: the removal policy, how to prune, and a correction of any copy implying tab close always cleans up.
- [ ] **M3.1** — Changelog fragment; cross-version check per CLAUDE.md rule 12 (this touches the daemon and the attach `StopAgent` handler); PR, review, merge, close #236.

## Key Files

- `src/issue_dispatch_run.rs` — `remove_worktree` and its `--force` (`:133`); `WorktreeRegistry` type + in-memory caveat (`:75`, `:63-68`); `new_worktree_registry` (`:78`); `record_worktree` (`:85`); `worktree_of_record` (`:95`); `take_worktree` (`:110`); `worktree_still_in_use` (`:123`); clone-local exclude precedent (`:446-457`).
- `src/daemon_protocol.rs` — `AttachRequest::StopAgent` arm (`:1339`) and the tab-close cleanup that calls `take_worktree` → `remove_worktree` (~`:1400-1408`).
- `src/daemon.rs` — the `Daemon::worktree_registry` field threaded through the daemon (`:250`).
- `.claude/skills/dot-ai-tag-release/SKILL.md` — step 6, the existing merged-only prune this complements.
- `.claude/skills/dot-ai-worktree-prd/create.sh` — the human worktree convention: sibling path (`:54`) and refuse-on-existing-branch (`:60-61`).

## Risks and Mitigations

- **Unbounded disk growth.** Keeping dirty trees means more trees. Mitigation: Phase 2 is not optional — it is the other half of the decision, and M2.0 makes the accumulation visible before it becomes a surprise.
- **Users stop trusting tab close to clean up.** The behavior becomes conditional, which is harder to predict. Mitigation: always report the outcome (M1.1) so it is never silent in either direction, and document the rule as one sentence — a clean tree is removed, a dirty one is kept.
- **The `git status` check is another subprocess on the close path.** Close latency matters. Mitigation: it runs once per *last* close of a tree, not per pane; if it proves slow, dropping `--force` alone achieves the same gate with no extra process.
- **A prune verb becomes a new data-loss path.** An eager prune could destroy exactly what M1.0 protected. Mitigation: M2.1 applies the same clean/dirty gate; the prune surfaces dirty trees rather than removing them.
- **Divergence from #120's assumptions.** Issue-dispatch's idempotency relies on a present tree meaning "already claimed". Mitigation: keeping *more* trees strengthens that signal rather than weakening it; verify the dispatch-decision path still skips correctly.

## Open Questions

- **Reclamation shape.** A `prune` CLI verb, a periodic/startup sweep, a deck surface listing orphans, or a combination? Decide in M2.0. A sweep is the most automatic and the most dangerous; a listing is the safest and the least automatic.
- **Does the user ever get to override?** Should a force-prune exist for the "yes, I really mean it" case, and if so does it live on the CLI only (never on Ctrl+W)?
- **Registry durability vs. convention.** Persist the `WorktreeRegistry` across restarts, or derive ownership from the `agent/dispatch-*` branch and `.worktrees/`-or-sibling path convention and keep the registry ephemeral? The latter adds no state but couples cleanup to naming.
- **Should this land before #220?** #220 widens the blast radius from scheduler-created trees in a throwaway clone to interactive trees branched off the user's HEAD. Sequencing #236's Phase 1 first would mean #220 never ships the hazard.
- **Reporting channel for M1.1.** The kept-tree message needs a user-visible home — the closing tab is gone by then. Notification surface, next-launch notice, or the prune listing from M2.0?
