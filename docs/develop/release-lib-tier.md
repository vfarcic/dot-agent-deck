# The release-profile lib tier

Issue #2134. Until this tier, CI built no test in the release profile: its test jobs build the dev profile, where `debug_assertions` is on. A shipped deck is a release build, with `debug_assertions` off, overflow checks off and the optimizer on, so a code path that behaves differently there was tested by nothing. This tier runs the deck's own unit tests (the root package's `--lib` target, the same tests `cargo test-fast` runs in-crate) in the release profile.

## Running it

```bash
scripts/test-release-lib.sh            # any extra arguments go to nextest
```

The script runs `cargo build --release --bin dot-agent-deck`, then `cargo nextest run --release --lib` with `OTEL_SDK_DISABLED=true` as the `test-fast` alias sets it. With `DAD_REMOTE_EXEC=rwx` set, run it as `rwx sandbox exec -- scripts/test-release-lib.sh` (CLAUDE.md rule 5): a release build is the heaviest compile in the repository.

**The bin build is not optional.** `--lib` builds no bin target, and the lib tests that spawn a Wrapper-strategy agent (Codex) run `dot-agent-deck wrap` from the build beside the test binary, `target/release/dot-agent-deck`. Without it they fall back to whatever `dot-agent-deck` `$PATH` names. On a runner or in an RWX sandbox that is nothing, so the wrapper never starts, the `/bin/cat` stand-in agent counts as exited at once, and five tests fail with "no live owner" (`quota_block_cleared_before_the_notice_is_not_reported`, `quota_publish_does_not_wait_on_a_busy_orchestrator_writer`, `raw_oversized_codex_rollout_frames_are_bounded_at_admission`, `a_codex_failure_superseded_by_a_newer_turn_does_not_repaint_the_card`, `dispatch_one_owned_reports_an_already_published_block_to_the_new_delegation`). On a developer's machine it would be the installed deck. `cargo test-fast` never meets this, because the integration tests make it build the bin. The same holds for a debug `cargo nextest run --lib` (measured in the RWX sandbox, 2026-11-08: the same five fail in a debug build without the bin, and pass in a release one with it).

## Where CI runs it

| | pull request, `.github/ci-backend` = `rwx` | pull request, `gha` | push to `main` |
| --- | --- | --- | --- |
| `release-lib` job in `.github/workflows/ci.yml` | skipped | runs | runs |
| `.rwx/release-lib.yml`, status `RWX: release-lib` | runs | passes in seconds, nothing built | not triggered |

Both skip a Renovate change that touches only the devbox or flake files, as the `test` job does (`scripts/ci-changes.sh`), and both are bounded at 45 minutes.

**The RWX side is its own run definition, not a task in `.rwx/ci.yml`.** RWX reports one status per run, and any task that fails fails the run. `.rwx/ci.yml`'s status, `RWX: linux`, is a required check, so a task added there would block every merge whatever this page called it. `.rwx/release-lib.yml` copies the setup tasks it needs from `.rwx/ci.yml` and reports `RWX: release-lib`. `scripts/check-pin-lockstep.sh` and Renovate's RWX managers read every `.rwx/*.yml`, so its toolchain, nextest and Node pins move and are checked with the others. It has no tool cache, since `.rwx/ci.yml`'s daily cron is what writes the caches, so each run builds cold.

## Why it is advisory

It is in neither the required `build` aggregate's `needs:` nor the ruleset's required contexts. The tier is new, has no run history on a GitHub runner or in an RWX task, and was red when it was written (issue #2134), so requiring it now would let a release-only finding block every merge until someone fixed it. Making it required is a `REQUIRED_CHECKS` change applied with `scripts/apply-branch-protection.sh`, the same step `cross-version`, `desktop-browser` and `desktop-driver` are waiting on.

Advisory is about pull requests. On `main` the job is part of `ci.yml`'s run, so a red there turns `main`'s run red, which `tag-release.yml` refuses to release. That is why `notify-main-red` lists it.

## What it leaves to the debug tiers

Tests that hold only with `debug_assertions` on are gated `#[cfg(debug_assertions)]` and do not exist in this tier's build. Each still runs in `cargo test-fast` locally, in CI's `test` job (or the `test` task of `.rwx/ci.yml`) and in `build-macos` on `main`, which all build the dev profile.

| test | why it holds only in debug |
| --- | --- |
| `lock_rank::tests::work_done_032_an_out_of_order_lock_panics_before_it_blocks` | rank checking, and the panic it asserts, exist only with debug assertions |
| `lock_rank::tests::work_done_032_the_non_panicking_forms_answer_a_rank_violation` | same: a release build reports no `RankViolation` |
| `lock_rank::tests::work_done_032_lock_in_drop_during_an_unwind_counts_and_does_not_log` | asserts a rank-violation skip inside a destructor during an unwind; in release the assertion fails there and aborts the test process |
| `lock_rank::tests::work_done_032_a_guard_drop_with_the_stack_borrowed_does_not_panic` | borrows the rank stack (gated before issue #2134) |

The other four `orchestration/work-done/032` tests in `src/lock_rank.rs` run in both. Two of them, `work_done_032_try_lock_now_does_not_wait_for_another_thread` and `work_done_032_try_lock_now_answers_poison_and_rank_as_asked`, check `try_lock_now` refusing without waiting and its poison handling in both profiles, and wrap only their rank-stack and `RankViolation` assertions in `if cfg!(debug_assertions)`, so the release tier skips those assertions and nothing else. `tests/CATALOG.md`'s entry for that ID records the split.

## The two release-only failures

Issue #2134 listed two tests that failed in a release build only: `retained_store::tests::recovery_019_the_inventory_writer_and_reader_agree_at_the_boundary` and `agent_pty::report_tests::work_done_053_a_a_failed_attempt_leaves_no_report_behind`. Each ended on an assertion that expected a `debug_assert!` in production code to panic, and a release build compiles that check out. recovery_019 expected `write_inventory_text` (`src/retained_store.rs`) to refuse an entry count over its `max_files`. work_done_053_a expected `RetainedStore::write` to unwind on the report id `"not a report id"`. PR #2142 (`caca26bf`) changed the tests, not the product, and found no product defect behind either. `write_inventory_text` is documented to log and write the full list in a release build, so recovery_019's release arm now asserts that, and that `read_inventory` refuses the result. The id check is the only unwind source that block of work_done_053_a drives, and both production paths that reach `write` (`src/task_completion.rs` for a report, `src/dispatch_return.rs` for a dispatch result, through `save_held`) pass an id from `mint_report_id()`, so that block now runs in debug builds only. Both tests pass in this tier.

## Its cost

Measured on 2026-11-08 on the branch that added it, each from a cold `target/`. Most of each wall time is the release build in front of the 5328 tests, whose run took 175 seconds on GitHub's 4 CPUs (48 seconds in a 16-CPU RWX sandbox).

| where | wall | measured |
| --- | --- | --- |
| RWX task, 4 CPU / 16 GB (`rwx run .rwx/release-lib.yml`) | 7m55s, 8m01s for the whole run | RWX run `01a1b9ccd5c57a53b4ffb401e471828f` |
| GitHub Actions `release-lib`, `ubuntu-latest`, no cache | 13m58s | run [`42454635091`](https://github.com/vfarcic/dot-agent-deck/actions/runs/42454635091) |

The GitHub job never saves a Rust cache, on `main` included (`save-if: false`), so it has none of its own to restore and builds cold every time. The repository's caches were already over GitHub's 10 GB limit when it was added (10.56 GB, 2026-11-08), and one more release-profile cache saved from `main` would evict `main`'s other entries (the cache note above `jobs:` in `ci.yml`). Sharing `build-release`'s cache was not taken: that job builds neither the lib's test target nor the dev-dependencies, so this job would still build those cold.
