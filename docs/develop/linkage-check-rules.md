# The linkage-check rule registry

`cargo xtask linkage-check` is the repository's own static gate: a set of numbered rules over files that no compiler, bundler or test runner reads as code — the test-case catalog, the `#[spec]` annotations, workflow YAML, a TOML command table, the docs site's image paths. CI runs it as a step of the required `build` job, so a rule here fails a pull request the same way clippy does.

This page is about **adding one**. What each existing rule enforces is not duplicated here — `cargo xtask linkage-check --list-rules` prints the current set from the same table the tool tags its findings from, which is the only copy that cannot go stale.

## One registration per rule

Every rule is one entry in `RULES`, in `xtask/linkage-check/src/main.rs`:

```rust
Rule {
    number: 16,
    name: "site-image-refs",
    summary: "Every `/img/...` and `./img/...` image reference under `docs/` and \
              `site/src/` resolves to a file in `site/static/img/`. …",
    check: rule_site_image_refs,
},
```

Four things derive from that entry and are written nowhere else: the `[16]` tag on each of its findings, its line in `--list-rules`, the rule count in the success line (`RULES.len()`), and the order findings print in.

**This used to be four hand-maintained places** (issue [#1216](https://github.com/vfarcic/dot-agent-deck/issues/1216)) — a numbered `//! N.` doc list at the top of the file, a `// Check N` comment at the registration site, a `format!("[N] {v}")` tag, and a literal count in the success line. Every rule-adding pull request edited the same lines with sequential numbers, so two of them conflicted *by construction* whatever they were about: measured three times in one day, between #1190/#1163, #1190/#1179 and #1163/#1179, none of which shared a subject. The count was the worse half of it. Two branches each raising the same literal produce **identical text**, which git merges cleanly with nothing to look at — it shipped saying `14` with fifteen rules registered, twice in that same day, past `cargo fmt`, clippy and `cargo test-fast`, and was caught only by someone reading the number in the tool's own output.

## Adding a rule

1. **Write the rule.** Two shapes exist, and the second is the default for anything new.

   - Its **own module**, `xtask/linkage-check/src/<rule>.rs`, exposing `pub fn run(root: &Path) -> Vec<String>` — one string per finding, no `[N]` prefix. Use this whenever the rule reads its own files: `desktop_project_boundary` (12), `git_program_literal` (13), `voice_command_registry` (14), `site_image_refs` (16) and `unit_test_endpoint_pin` (17) all do. Declare the module in `main.rs`'s alphabetical `mod` list with a doc comment saying what it is for.
   - A **bucket on the shared scan**, for a rule that is a line scan over `tests/` + `src/`. `scan_sources` walks and comment-strips those files once, and each rule that reads that walk owns one field of `ScannedFindings` — rules 5, 8, 10, 11 and 15. Add a field, fill it in the loop, and have your rule function return a clone of it. Do not add a second walk of the same tree.

2. **Report what the rule cannot see, at the finding.** Every rule here is a text or AST scan with a blind spot, and the repository's convention is that the rule sentence says so rather than leaving a green result to be over-read — see `VOICE_REGISTRY_RULE`, which states outright that it proves the registry is consistent and not that it is complete.

3. **Refuse to pass vacuously.** A rule whose input has moved or gone missing reports *nothing*, which is indistinguishable from a clean tree. Make the missing input a finding: rule 12 reports an absent `desktop/src-tauri/src/`, and rule 16 reports both an absent `site/static/img/` and a scan that matched no reference at all.

4. **Register it.** One entry in `RULES`, with the **next unused** `number`, a kebab-case `name`, a `summary` that reads as a sentence (it is the whole rule list `--list-rules` prints), and `check`.

5. **Test it against synthetic input, not only against the checkout.** A rule whose only coverage is the live tree tests nothing whenever that tree is clean — which is its normal state, and would be its state on the day it silently stopped matching. Every rule module here plants bad input in a `tempfile::tempdir()` or feeds contents in directly, *and* asserts the real tree is clean, so "no findings" means something. These tests run in `cargo test-fast` via `--workspace` (CLAUDE.md rule 5), so they are in the required `build` job too.

6. **Give it an opt-out only if an exception is a local judgement call.** Rules 8, 10 and 16 take a marker on the offending line (`linkage-check:allow-bare-tempdir`, `linkage-check:allow-unarmed-agent-spawn`, `linkage-check:allow-missing-image`), and rule 17 one on or directly above the offending `fn` (`linkage-check:allow-unpinned-emitter`), so the exception is declared where it is taken and review meets it in the diff that needs it. Rule 11 deliberately has none: it guards a pair of numbers that must move together, so the only correct response is to raise the constant.

Nothing else needs editing — in particular there is no doc list, no comment and no count to update.

## Rule numbers never change

The numbers are **stable identifiers**, cited by number in `CLAUDE.md`, under `docs/`, and in comments throughout `tests/` ("linkage-check rule 8", "rule 10 requires arming", "rule 11 fails the build if a pin exceeds it"). Renumbering a rule silently falsifies all of those, and a failure tag someone pasted into an issue stops meaning anything.

So: a new rule takes the next unused number, and no existing rule is ever renumbered — including when one is retired. Retire a rule by making its `check` a no-op with a `summary` that says it is retired, not by deleting the entry and shifting its successors. `number` is written in each entry rather than taken from the entry's position for exactly this reason: moving an entry cannot renumber a cited rule.

`name` is the same identity without the renumbering hazard, which is what makes it the better handle in new prose.

### The invariants that hold this honest

Four tests in `main.rs` (`rule_numbers_are_unique_and_run_from_one_without_a_gap`, `rule_names_are_unique_and_kebab_case`, `every_rule_carries_a_summary_that_says_something`, `no_rule_summary_cites_another_rules_number`):

- the numbers run `1..=RULES.len()` in declaration order, with no gap and no duplicate. This is what turns the one collision the registry cannot prevent — two branches both claiming the next number, appended at different offsets so git merges them — into a **red test** instead of a quiet merge with two rules numbered the same and one number never used;
- names are unique and kebab-case, so a `grep` for one finds every mention;
- every entry carries a usable summary, because `--list-rules` is the only rule list there is;
- no entry's summary cites another rule's number. That drift is not hypothetical: `check 13` named three different rules across this one crate — the git-literal rule (13, correctly), the bare-`git` rule (15) and the voice-registry rule (14) — because each was written while its rule was expected to land as 13, and nothing tied the prose to the registration.

## Reading a failure

```text
linkage-check: 1 failure(s):
  [16] docs/workspace-modes.md:238: `/img/<missing>.png` resolves to `site/static/img/<missing>.png`, which does not exist — …
each `[N]` names a rule — `cargo xtask linkage-check --list-rules` prints all 16
```

The `[N]` is the rule's registered number. `--list-rules` maps it back to a name and a summary; the module named in that summary carries the reasoning.

## What is not a rule

The **repository-state preflight** (`repo_state`, issue #557) runs before every rule and short-circuits on its own. It answers "is this repository sane to reason about" — is the object store unexpectedly shallow, has the worktree registry drifted from what is on disk — which is a different question from anything the rules ask, and it runs first so a repository in a state that would misdiagnose them is caught before any of them run.

Most of this crate is also **not** rules at all. Seventeen modules — `build_gate`, `contract_breaks`, `desktop_palette`, `desktop_settings_secrets`, `devbox_gtk_origin`, `gh_aw_lock_consistency`, `issue_labeler_memory`, `issue_labeler_policy`, `junit_strip`, `pin_lockstep`, `pr_review_verdict`, `reap_orphans`, `release_workflow_wiring`, `sample_attribution`, `sidecar_staging`, `skill_frontmatter` and `verify_pr_stream` — are `#[cfg(test)]` modules whose whole content is tests — usually of a shell or Python script whose safety properties are runtime behaviour, so a compile-time gate would prove nothing about them. They live here because `cargo test-fast --workspace` reaches them and therefore so does the required `build` job. If what you are guarding is a script's behaviour rather than a property of the tree, that is the shape you want, and it needs no `RULES` entry. The remaining four are neither rules nor tests: `clean_tmp` and `list_tests` are the `clean-e2e-tmp` and `list-tests` subcommands, `paths` is a shared helper, and `repo_state` is the preflight above.
