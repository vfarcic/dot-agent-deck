# PRD #381: Resolve the hook binary path to a durable location

**Status**: Not started
**Priority**: High
**Created**: 2026-08-04

## Problem Statement

Hook installation writes `std::env::current_exe()` into **persistent, user-level, all-projects** configuration. When the binary that performs the install is a local build, the path written is a `target/debug` or `target/release` artifact — the least durable path on the machine. It is gitignored, `cargo clean` removes it, and when it lives in a worktree it disappears the moment that worktree is pruned. Every hook then fails.

This is not hypothetical. It was observed live on 2026-08-04, and the blast radius covered all three agent integrations at once:

| config | state when found |
|---|---|
| `~/.claude/settings.json` | all **10** hooks → `/home/vfarcic/code/dot-agent-deck-pr-356/target/release/dot-agent-deck` — worktree **deleted** |
| `~/.codex/hooks.json` | all **10** hooks → `…/dot-agent-deck-prd-376-…/target/debug/dot-agent-deck` — a *live* worktree, so working but primed to break |
| `~/.config/opencode/plugin/dot-agent-deck.js` | `BINARY_PATH` → the same **deleted** pr-356 path |

The user-visible symptom is a Stop hook failing with `/bin/sh: 1: /home/vfarcic/code/dot-agent-deck-pr-356/target/release/dot-agent-deck: not found`. Nothing else indicates a problem, and the error names a path the user never typed.

Three properties turn this from a footgun into a defect.

**It is silent and automatic.** `hooks_manage.rs:187` documents `auto_install()` as *"Silently install hooks if Claude Code is detected. Intended for dashboard startup — never prints to stdout."* Merely launching the dashboard from a locally-built binary rewrites global config with no output and no consent. The two workflows most likely to do this are the project's own: the `verify-pr` skill checks a PR into a dedicated worktree, builds it, and runs it; `run-dot-agent-deck` builds and runs the binary for local smoke testing. Both are routine, and both poison global config as a side effect.

**It re-arms itself.** The Codex entry found on 2026-08-04 pointed at the *current* worktree's `target/debug` build. It worked at the moment of inspection and would break as soon as that worktree was cleaned up after its PR merged. Every dev-build run replaces one time bomb with another.

**The repository already knows the correct behaviour.** `remote.rs:1034` invokes `~/.local/bin/dot-agent-deck hooks install` and carries the comment *"Use the absolute path consistently."* The remote install path resolves to a durable installed location; the local path does not.

Finally, the reason this shipped: `hooks_manage.rs:213-227`'s `auto_install_to()` — the seam the tests drive — hardcodes `let binary_path = "dot-agent-deck".to_string();` rather than deriving it. **The tests never execute the broken derivation.** This is a coverage gap in the exact line that fails, not bad luck.

## Solution Overview

Introduce one shared resolver that answers "what path should be written into another program's config?", make every call site use it, and make the answer never be a build artifact. Then repair configs that are already broken, because they exist in the field today and no user has a way to know.

The resolver's contract: **a `target/{debug,release}` path is never written into global configuration.** A durable path is used instead, and if none can be found the install fails loudly rather than writing something that will break later.

A bare `dot-agent-deck` is also not an acceptable answer. Hooks execute under `/bin/sh` with an environment the deck does not control, and the observed failure was precisely a `sh` PATH miss. The written value must be an absolute path to a file that exists.

## Scope

### In Scope

- A shared durable-path resolver, with explicit precedence and an explicit refusal case.
- Every call site that writes a deck binary path into external configuration: `hooks_manage.rs:193` (`auto_install`) and `:230` (`install`), `codex_hooks_manage.rs:423`, `opencode_manage.rs:515` and `:523`, `agent_registry.rs:148`.
- Self-healing for the three affected configs: Claude `settings.json`, Codex `hooks.json`, and the OpenCode plugin's `BINARY_PATH`.
- Regression tests that exercise the real derivation, including the case where the running binary *is* a build artifact.
- A loud, actionable failure when no durable path exists.
- A troubleshooting note so a user who sees `not found` in a hook can self-diagnose.

### Out of Scope

- **Changing where or how the binary is installed.** `~/.local/bin` remains the installed location; this PRD changes what gets *written into config*, not packaging.
- **The `hooks install` CLI's surface.** No new flags, no changed output contract beyond the new failure case.
- **Rewriting the hook payload protocol.** Only the command's binary path is in question.
- **Retroactive migration beyond self-heal.** Self-heal fixes what the deck can observe at startup; there is no sweep of arbitrary config locations.
- **Hardening `verify-pr` or `run-dot-agent-deck` against side effects.** They are how the bug was triggered, but the defect is that a dev build can silently write global config at all — fix that, not the callers.

## Technical Approach

### Resolution order

1. If `current_exe()` is **not** inside a `target/debug` or `target/release` directory, use it. An installed binary performing its own install is the normal, correct case and must keep working.
2. If it **is** a build artifact, resolve a durable path instead:
   - `~/.local/bin/dot-agent-deck`, if it exists and is executable — the same choice `remote.rs:1034` already makes;
   - otherwise the absolute path of `dot-agent-deck` as resolved on `PATH`;
   - otherwise **refuse to write** and say so.
3. Never write a bare command name, and never write a path that does not currently exist.

Step 2's refusal is the load-bearing part. Today the failure mode is a silent bad write discovered days later by a broken hook; the replacement is an immediate, legible complaint at the moment of installation.

### Self-heal

On the `auto_install` path, a deck-managed entry whose command points at a **nonexistent** path is rewritten using the resolver above. Deck-managed entries are already identifiable — `hooks_manage.rs:177-183` matches on the command containing `dot-agent-deck` — and that detection must continue to leave user-authored hooks untouched. Repair must be idempotent, and must not fire when the existing path is merely *different* from the resolved one but still valid; the trigger is "the target is missing", not "the target is not what I would have written".

"Left alone" means the valid-but-different rule is **left in place alongside** the newly added durable rule, not adopted as the single rule — so both fire, and that agent produces two deck events per action until the stale binary disappears. That is pre-existing `install_impl` behaviour (`command_matches_binary` normalizes only rules whose executable canonicalizes to the *installing* binary) and it is the safe side of the Open Question 3 tradeoff: a duplicate event is recoverable, silently repointing a path the user chose deliberately is not.

### Closing the test gap

`auto_install_to()` accepts a settings path but hardcodes the binary string, so no test reaches the derivation. The seam needs to accept the resolver (or a resolved path) so tests can drive it with a `target/release/...` value and assert that value is **not** what lands in the config. That single test is the regression guard for this entire PRD; without it the same bug can return unnoticed.

### The three integrations differ in shape

Claude and Codex hooks are JSON documents with a command string per event, so repair is a value substitution. The OpenCode integration is a **generated JavaScript file** with `const BINARY_PATH = "…"`, which is a different write path and needs its own handling — a JSON-shaped fix will silently miss it, which is exactly how it came to be the last of the three to be noticed.

## Success Criteria

- No code path can write a `target/debug` or `target/release` path into global configuration.
- Running the dashboard from a freshly-built local binary leaves existing valid hook paths unchanged.
- A config whose deck hooks point at a deleted binary is repaired automatically on the next startup, for all three integrations.
- An install with no durable path available fails with a message naming what to do, and writes nothing.
- A test drives the real derivation with a build-artifact input and fails if that input is written.
- A user-authored hook that happens to mention `dot-agent-deck` is never rewritten.

## Milestones

- [ ] **M1 — The resolver exists and refuses build artifacts.** One function, the precedence above, with the refusal case, unit-tested directly.
- [ ] **M2 — Every call site uses it.** All six sites across `hooks_manage`, `codex_hooks_manage`, `opencode_manage` and `agent_registry`. No remaining `current_exe()` feeding an external config write.
- [ ] **M3 — Test gap closed.** The derivation is reachable from tests, with a regression test asserting a `target/` path is never written. Landable on its own and the highest-value milestone here, because it prevents recurrence.
- [ ] **M4 — Self-heal for Claude and Codex hooks.** Stale entries repaired on startup, idempotent, user hooks untouched.
- [ ] **M5 — Self-heal for the OpenCode plugin's `BINARY_PATH`.** Separate milestone because it is a generated JS file, not JSON.
- [ ] **M6 — Loud failure path.** No durable path means a clear error and no write, for both `install` and `auto_install` (the latter logging rather than printing, per its contract).
- [ ] **M7 — Troubleshooting docs.** A user seeing `not found` from a hook can find the cause and the fix.

## Risks

- **Self-heal rewriting something it should not.** The detection is a substring match on `dot-agent-deck`, so a user's own wrapper script mentioning the deck could be caught. Mitigation: only repair when the path is missing, never merely because it differs, and keep the existing deck-managed detection rather than widening it.
- **Breaking the legitimate dev-install workflow.** Someone may genuinely want hooks pointing at a local build while developing. Refusing that outright could be worse than the bug for that person. This is the sharpest design question in the PRD — see Open Questions 1.
- **`~/.local/bin` is not universal.** It is this machine's install location and the one `remote.rs` assumes, but it is not guaranteed. The PATH fallback exists for that reason, and the refusal case exists for when neither works.
- **Self-heal masking a real problem.** Silently repairing config is the same class of silent global mutation that caused this bug. Repair should log what it changed.
- **Three integrations, three code paths.** A fix that covers JSON and forgets the generated JS leaves a third of the bug in place — which is precisely what happened during discovery.

## Open Questions

1. **Should a dev build be allowed to write global config at all, even with a durable path?** The strongest reading of this bug is not "the wrong path was written" but "a transient build silently mutated global state". An alternative fix is for `auto_install()` to decline entirely when `current_exe()` is a build artifact, leaving explicit `hooks install` as the only way. That is stricter, simpler to reason about, and would have prevented every instance found — but it removes a convenience that developers of this project may rely on daily. **Decide this before M1**, because it determines whether the resolver has a fallback path or a hard stop.
2. **Should `install` support an explicit opt-in** (e.g. a flag) for pointing hooks at a local build, so the strict default in Q1 has a documented escape hatch?
3. **How should self-heal treat a path that exists but belongs to a different worktree?** It is valid today and stale tomorrow. Repairing it is tempting but violates the "only repair what is missing" rule that keeps self-heal safe.
4. **Is `agent_registry.rs:148` in the same class?** It uses `current_exe()` but may be spawning a child process rather than persisting a path. If it is transient, it is out of scope and the list in M2 shrinks — confirm rather than assume.
5. **Should the repaired path be verified to actually run**, e.g. by invoking `dot-agent-deck hook` with empty input, rather than only checking that the file exists? An existing-but-incompatible binary is a plausible failure after a version change.

## Work Log

### 2026-08-04 — Discovered while diagnosing a Stop hook failure

Found while working PRD #376, from a user-visible error: `Stop hook error: /bin/sh: 1: /home/vfarcic/code/dot-agent-deck-pr-356/target/release/dot-agent-deck: not found`. The pr-356 worktree had been removed after its PR merged, taking the binary with it.

Investigation found the same root cause across three integrations, with 21 broken references in total: 10 Claude hooks, 10 Codex hooks, and the OpenCode plugin's `BINARY_PATH`. All three were repaired by hand against `/home/vfarcic/.local/bin/dot-agent-deck` (v0.35.5), with timestamped backups for the Codex and OpenCode files. That repair is a workaround for one machine and is not a fix.

Two observations worth preserving, because they are the parts most likely to be lost:

The Codex entry pointed at the **then-current** worktree's `target/debug` build rather than a deleted one. It was working at the time of inspection. That is the clearest evidence the bug re-arms on every dev-build run rather than being a historical accident.

The reason no test caught it is structural, not incidental: `auto_install_to()` hardcodes the binary string, so the test suite never runs the line that produces the bad value. Any fix that does not close that seam can regress silently.

The repository already contained the correct pattern at `remote.rs:1034` — `~/.local/bin/dot-agent-deck` with the comment *"Use the absolute path consistently"* — so this is an inconsistency between the remote and local install paths, not a missing insight.
