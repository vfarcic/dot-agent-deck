# devbox and concurrent starts — why a fresh worktree lost a role

Starting a multi-role orchestration in a **fresh** worktree used to kill one role at launch, with one of two errors (issue #791):

```
Error: error running script "agent-orchestrator" in Devbox: remove
  <worktree>/.devbox/virtenv/bin/setup-corepack.mjs: no such file or directory

Error: error running script "agent-orchestrator" in Devbox: symlink
  <worktree>/.devbox/virtenv/nodejs/bin/setup-corepack.mjs
  <worktree>/.devbox/virtenv/bin/setup-corepack.mjs: file exists
```

The two are complementary halves of one non-atomic operation — a process removing that link while another creates it. **It is silent unless you count roles**: the deck reports the orchestration as started, and the unit simply runs with five roles instead of six. Both observed occurrences took `agent-orchestrator`, which leaves workers with nobody to delegate to.

## What is actually unsafe

devbox's **per-project `.devbox` materialization is not safe against concurrent invocations of itself**. The corepack symlink is one manifestation, not the whole problem: with the nodejs plugin out of the picture, concurrent cold runs in the same directory can still fail with `compact json for hashing: unexpected end of JSON input`, which is devbox reading a partially-written state file.

Only the **first** materialization is exposed. Once `.devbox` exists every later invocation succeeds, which is why the main checkout never fails, why a failed orchestration succeeds on restart (the failed attempt materialized the directory as a side effect), and why this only started biting once we began creating a fresh worktree per dispatched unit.

It has no history before 2026-08-29 because it could not: `git show daf94f0^:devbox.json` contains neither `nodejs` nor `pnpm`, and `daf94f0` ("Add visual agent control deck", #416) adds both. `setup-corepack.mjs` is, per its own header, "the nodejs plugin's init_hook", so before that commit there was no Node plugin and nothing to race on.

**A clean run proves nothing here, and that is why neither fix below is justified by one.** Six simultaneous `devbox run -- true` in a cold worktree passed 6/6 on one attempt and failed 1/6 on a later one, against the same code. The evidence for each fix is a mechanism — a thing that can no longer exist, and an ordering that cannot be skipped — not a run count.

## Fix 1 — the nodejs plugin is disabled

`devbox.json` sets `disable_plugin: true` on the **existing** `nodejs` entry. The package and its version pin stay; only the plugin's init hook goes, and with it the symlink the two errors above are about.

Measured on this repository's manifest (devbox 0.18.0, 2026-09-04), cold `.devbox` both times, the two runs differing only in that flag:

| | `.devbox/virtenv` after a cold `devbox run -- true` |
| --- | --- |
| plugin enabled | `runx/`, `rustc/`, **`nodejs/bin/setup-corepack.mjs`**, and **`bin/setup-corepack.mjs`** symlinked to it |
| `disable_plugin: true` | `runx/`, `rustc/` — no `nodejs/` subtree and no `bin/` at all |

The path the two errors name is therefore never created. A thing that cannot exist cannot be raced. What does not change: `devbox list` reports the same 22 entries at the same versions, `devbox.lock` is byte-identical, and the profile still supplies `node` **v24.12.0** and `pnpm` **11.22.0** (read straight out of `.devbox/nix/profile/default/bin/`, because `devbox.json`'s `init_hook` prepends `$HOME/.local/bin` and a host-installed Node would otherwise answer first).

**This costs nothing here because corepack is unused in this repository.** before this change `git grep -i corepack` over tracked files matched nothing at all, and it now matches only prose — this page and the doc comment on `warm_project_environment` that quotes the error; `DEVBOX_COREPACK_ENABLED` is set nowhere; none of the three tracked `package.json` files (`desktop/`, `pi-extension/`, `site/`) carries a `packageManager` field; and CI takes Node and pnpm from `actions/setup-node` and `pnpm/action-setup`, invoking `pnpm install --frozen-lockfile` / `pnpm test` / `pnpm build` directly. The hook script itself opens by exiting 0 unless `DEVBOX_COREPACK_ENABLED` is set, so its only effect here was a symlink to a script that returns immediately.

### The consequence nobody expects: `packages` changes spelling

`devbox add nodejs@24.12.0 --disable-plugin` **rewrites the whole `packages` block from the array form to the object form** — all 22 entries, not just the one that gained an option. That is devbox's choice, not a style preference: a single object inside the array is rejected outright (`Error loading devbox.json. source: Value looks like object, but can't find closing '}' symbol`), so there is no minimal hand-edit that avoids it.

`scripts/check-pin-lockstep.sh` (issue #648) read the array form with a `grep` for `"<name>@<version>"`, so the conversion did not fail it on the one changed entry — it lost every Rust pin at once and reported `devbox.json pins no rustc at all` four times over, plus the same for `cargo-nextest`. To its credit the guard refuses to pass vacuously and its message says what to do (*"or its spelling changed (fix the script)"*). Its `scan_devbox` now parses both spellings, including a `version` nested inside a per-package object, scoped to the `packages` block so a `shell.scripts` entry sharing a package's name is not read as a pin. `xtask/linkage-check/src/pin_lockstep.rs` covers the object form the same way it covers the array form: an agreeing fixture, a drifted one, a nested `version`, a missing component, and the script-name collision.

Reading both forms is also what keeps the lockstep between the right two things. Renovate's devbox manager parses `packages` as an array-or-record union whose record values may be a version string *or* an object carrying one (`lib/modules/manager/devbox/schema.ts`), so both spellings are pins Renovate tracks and bumps — and a guard that reads only one of them is blind to drift in the other, the same shape of hole as reading only block-style YAML (issue #710).

## Fix 2 — dispatch warms the environment once, serially

The plugin change does **not** subsume this. The partially-written-state-file race survives it, and it is not Node-related.

`issue_dispatch_run::create_worktree` — the only `git worktree add` in `src/` — runs one `devbox run -- true` in the new worktree on its `Created` arm, and **awaits it before reporting `Created`**. Every caller spawns only after awaiting that function, so by the time any role starts there is no first materialization left for it to lose. Both creation paths get it: `dispatch::handle_dispatch` (the orchestration case the issue is about) and the issue-dispatch fire flow.

The ordering is the claim worth pinning, and `create_worktree_warms_the_environment_before_it_reports_created` pins it with a stand-in that writes a marker file, asserted the instant `create_worktree` returns. Moving the warm-up into a background task fails that test; deleting it fails that test. Both were checked by making the change and watching it go red.

**It never fails a dispatch.** Every outcome returns normally and the caller goes on to spawn:

| Situation | Outcome | Cost |
| --- | --- | --- |
| No `devbox.json` in the worktree | `NoManifest`, and **no process is started** — the manifest is probed first, so a repository that does not use devbox pays nothing | none |
| `devbox` not on `PATH` | `Unavailable`, logged at debug | the optimisation |
| `devbox` exits non-zero | `Failed`, logged at warn with its stderr | the optimisation |
| `devbox` still running after 120s | `TimedOut`, logged at warn | the optimisation, plus the wait |

In every degraded case the roles then race exactly as they did before the warm-up existed, which is the status quo rather than a new failure mode. Failing closed here would turn a devbox problem into a dispatch problem for the users who have no devbox at all — devbox is optional for this project.

Two details are load-bearing:

- **The bound is 120 seconds, and expiry does not kill the child.** `tokio`'s `kill_on_drop` is deliberately left off, so a `devbox` that outlives the bound keeps running and finishes what it started. Killing it would leave a half-materialized `.devbox`, which is strictly worse than the cold one we began with — the one way this could be worse than doing nothing. The bound exists so a wedged `devbox` costs a dispatch two minutes rather than hanging it forever; 120s is ~23x the 5.2s a cold `.devbox` for this repository's 22-package manifest took against an already-populated nix store, which is the case being fixed (a fresh worktree of a repo the machine has built before).
- **The per-repository worktree lock is released first.** That lock (issue #541) serializes `git worktree add` against itself. Holding it across a subprocess bounded in minutes would push a concurrent dispatch of *another* worktree past `WORKTREE_LOCK_WAIT` into the unserialized path, reintroducing the hazard the lock exists for — to protect two materializations that do not touch each other's `.devbox` in the first place.

## What is still true after both fixes

**The underlying materialization is still not concurrency-safe.** Neither fix makes it so, and neither claims to. What they remove is the exposure: one manifestation is gone because the file it needs is never created, and the window itself is closed for worktrees the deck creates, because the deck materializes them first.

Two ways to still meet it: create a worktree by hand (`git worktree add`) and start several devbox-launched processes in it at once, or have the warm-up degrade for one of the reasons in the table above and then start roles concurrently. In both cases the mitigation is the same single `devbox run -- true` in the new directory before anything else runs.

## What was rejected, and why it stays rejected

**Removing `nodejs` and `pnpm` from `devbox.json`** and relying on OS-installed versions. It was tried in PR #792 and closed unmerged. It does not address the remaining race, it drops the version pins that keep a devbox shell aligned with CI's Node 24 / pnpm 11 — the two-halves drift issue #648 exists to prevent — and, decisively, it contradicts the project's stance: issue #780 keeps the devbox environment **self-sufficient** (a `tauri-deps` flake supplying the GTK/WebKit closure) on the rationale that `devbox shell` should need nothing installed by hand. Both cannot be the stance, and #780's is the stronger one. Do not re-litigate it by removing packages from `devbox.json`.
