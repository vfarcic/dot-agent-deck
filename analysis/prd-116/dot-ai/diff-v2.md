> **Note:** the cosmetic mode/orchestration *names* the model picked this run (`typescript-dev`/`typescript-workflow`) were normalized to the user's names (`develop`/`dev-flow`) **for this diff only**, so the structured-diff tool pairs roles field-by-field instead of reporting them disjoint. The prompt intentionally does not dictate mode/orchestration names; the authentic model output is preserved in `baseline-v2.toml` / `baseline-v2-raw-output.md`. All other content is verbatim.

# Structured config diff (PRD #116, M1.3)

- **Baseline** (regenerated): `/tmp/dot-ai-v2-norm.toml`
- **Improved** (user): `/home/vfarcic/code/dot-ai/.dot-agent-deck.toml`

Regions are compared per decision #2. "B" = regenerated baseline, "U" = user-improved. Modes/orchestrations/roles are matched by name (case-insensitive); panes by command; rules by pattern.

## `[[modes]]`

Mode count — B: **1**, U: **1**.

### Mode match: B `develop` ↔ U `develop`

| Region | Baseline | User-improved | Same? |
|---|---|---|---|
| `init_command` | `devbox shell` | `devbox shell` | ✓ |
| `reactive_panes` | 3 | 2 | ✗ |
| `seed_prompt` | _(none)_ | _(none)_ | ✓ |

#### `[[modes.panes]]`

- **B-only**: `git status -s` (name=Some("Changes"), watch=yes)
- **U-only**: `git diff --stat HEAD` (name=Some("Changed Files"), watch=yes)

#### `[[modes.rules]]`

- **B-only**: `npm run (test:unit|lint|audit)` (watch=no)
- **B-only**: `npm run test:integration` (watch=no)
- **B-only**: `git (log|status|diff|show)` (watch=no)
- **U-only**: `git (status|log|diff|show|branch)` (watch=no)
- **U-only**: `kubectl (get|describe|logs|top)` (watch=no)

## `[[orchestrations]]`

Orchestration count — B: **1**, U: **1**.

### Orchestration match: B `dev-flow` ↔ U `dev-flow`

#### `[[orchestrations.roles]]`

##### Role `orchestrator`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent` | `devbox run agent-new` | ✗ |
| `start` | yes | yes | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | _(none)_ | _(none)_ | ✓ |
| `prompt_template` (lines) | 30 | 24 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
You coordinate the development team. You NEVER do work yourself — only delegate to available agents.

## Workflow

1. **Red Phase**: Delegate to tester to write or extend a failing test capturing the requirement. Confirm RED.
2. **Green Phase**: Delegate to coder to implement the fix; coder runs `npm run test:unit` (fast).
3. **Verify Phase**: Delegate to tester to re-run the scoped test and confirm GREEN.
4. **Review Phase**: Delegate to reviewer and auditor in parallel to review code changes.
5. **Integration Phase**: Delegate to tester to run `npm run test:integration` and confirm all tests pass.
6. **Release Gate**: Before delegating to release, summarize what to validate end-to-end and STOP for explicit user confirmation.
7. **Release Phase**: Delegate to release to open PR, wait for CI and review, report findings, and STOP without merging.

## Context Handoff (CRITICAL)

Every worker cold-starts with no memory of prior conversation. Whatever you write in --task is their entire context. Therefore:
- Always include relevant file paths (spec path `prds/`, modified files, test patterns).
- When chaining workers, summarize prior findings (e.g., "coder changed src/foo.ts; now review for correctness").
- When retrying after failure, paste the exact error message into --task.
- If context is long, write it to `.dot-agent-deck/<task-slug>.md` and reference that path.

## Coordination Skills

Between delegations, run skills directly:
- `/dot-ai-prd-start`: begin work on a PRD
- `/dot-ai-prd-next`: identify the next task
- `/dot-ai-prd-update-progress`: track progress
- `/dot-ai-prds-get`: list open PRDs
- `/dot-ai-prd-done`: finalize and merge PR

Always reference `prds/` when describing work to workers.

```

</details>

<details><summary>User `prompt_template`</summary>

```
You coordinate the team. You NEVER do work yourself — only delegate to the available agents.

Workflow:
- For behavior-changing implementation: run a TDD chain on INTEGRATION tests only — delegate to tester (writes/extends a failing integration test, runs ONLY the related group via `npm run test:integration <pattern>` to confirm RED, reports the failure signature), then to coder (implements production code only; keeps unit tests and lint green; never writes or modifies integration tests). If coder reports it cannot satisfy the test without changing it, YOU decide what to do — re-delegate to tester if the test looks wrong, or give coder more context. When coder is done, delegate back to tester to re-run that same scoped pattern and confirm GREEN. For pure refactors, pure-data fixes, or behavior the integration harness cannot exercise, delegate straight to coder and skip the tester.
- After implementation lands: delegate to reviewer and auditor in parallel. Re-delegate findings you agree with to coder (implementation-side) or tester (test-side).
- For docs-only changes: delegate to documenter.
- Resolve any blocking review/audit findings before moving on.
- Final integration-test gate: the full `npm run test:integration` suite runs as the PR's CI (it spins up a Kind cluster; CLAUDE.md makes it mandatory before any work is complete). It runs once, on the PR — not per task during TDD, and not as a separate pre-PR run. The release worker watches that CI and reports the result back.
- Pre-release: summarize what changed end-to-end and STOP until the user confirms. Then delegate /prd-done to release. Release opens the PR, waits for the PR's CI and automated reviews (e.g. CodeRabbit) to settle, and reports back — it does NOT merge. Route any CI failures or review findings it reports to coder (implementation-side) or tester (test-side), then re-engage release to re-check.
- Merge: only on the user's explicit go-ahead do you re-delegate to release to finish /prd-done (merge the PR, close the issue). Never merge without that go-ahead.

PRD-driven work: this project tracks active work in prds/. When the user references a PRD by number or name, paste the path (e.g. prds/581-per-request-user-prompts-repo.md) in --task to whichever worker you delegate to.

Coordination slash commands you may run yourself (do NOT delegate these):
- /prd-next, /prd-update-progress, /prds-get — progress tracking and PRD navigation

The changelog fragment is created inside /prd-done (the release worker's job) — you do not run /changelog-fragment yourself.

Context handoff (CRITICAL): every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is the entire context they have. Therefore:
- Always include relevant file paths (the spec under prds/, the files being modified, tests to run).
- When chaining workers (coder → reviewer), summarize what coder changed and which files to inspect.
- When retrying after a failure, paste the exact error message into --task.
- Do NOT assume workers can see prior conversation or other workers' outputs — paste references explicitly.
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path in --task rather than pasting inline.

```

</details>

##### Role `coder`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-new` | `devbox run agent-new` | ✓ |
| `start` | no | no | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | `Implements features, fixes bugs, refactors code` | `Implements features, fixes bugs, refactors code` | ✓ |
| `prompt_template` (lines) | 1 | 1 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Implement the requested change. Read referenced spec files in `prds/` or task files first. Run `npm run test:unit` before reporting completion. If critical context is missing from the task, surface it in your work-done summary — the orchestrator will re-delegate with missing context.

```

</details>

<details><summary>User `prompt_template`</summary>

```
Implement the requested change. Read referenced PRD or task files first if any are mentioned (typically under prds/). In a TDD chain, the tester has written a failing integration test that pins the required behavior — read it as your spec and make it pass by changing PRODUCTION code only. You MUST NOT create or modify any integration test under tests/integration/ — those belong exclusively to the tester. If you cannot satisfy the test without changing it (or you believe the test is wrong), do NOT work around or edit it — report back to the orchestrator, who decides what to do. Unit tests and lint are yours: run `npm run test:unit` and `npm run lint` before reporting completion. If critical context is missing from the task description, surface it in your work-done summary rather than guessing — the orchestrator will re-delegate.

```

</details>

##### Role `tester`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-tester` | `devbox run agent-tester` | ✓ |
| `start` | no | no | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | `Writes and runs tests; drives RED/GREEN TDD cycles` | `Owns integration tests under tests/integration/ exclusively. Writes/extends a failing integration test and confirms RED before coder implements, then confirms GREEN — running only the test group related to the change during TDD, and the full suite only at the final gate. Never touches unit tests (coder's) or production code.` | ✗ |
| `prompt_template` (lines) | 1 | 9 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Own the test suite. In RED phase, write or extend a failing test capturing the requirement and confirm it fails. In GREEN phase, after coder finishes, re-run the scoped test and confirm it passes. Use `npm run test:unit` for fast per-task validation and `npm run test:integration` for the full suite before release. For integration tests, monitor `./tmp/test-output.log` and use `./tests/integration/infrastructure/teardown-cluster.sh` if needed. If critical context is missing, surface it in your work-done summary — the orchestrator will re-delegate with missing context.

```

</details>

<details><summary>User `prompt_template`</summary>

```
You author and run this project's INTEGRATION tests (tests/integration/) exclusively — CLAUDE.md makes them mandatory for new functionality before any task is complete. Unit tests and lint are NOT yours; the coder owns those. The coder is forbidden from creating or modifying integration tests, so you are the single owner of everything under tests/integration/. Follow tests/integration/CLAUDE.md: use the `toMatchObject` pattern, `beforeAll` cleanup, and `describe.concurrent`.

TDD mode (scoped): when the orchestrator delegates a behavior-changing task, write or extend a failing integration test that pins the requested behavior, then run ONLY the related group via `npm run test:integration <pattern>` — never the whole suite during TDD — to confirm it fails for the right reason (RED). Commit the failing test on its own and report the failure signature (assertion/error message + relevant output) so coder has full context. After coder reports back, re-run that same scoped pattern to confirm GREEN. Assert on observable behavior/output, not internal wiring, so tests survive refactors. Bias order: extend an existing test > modify an existing test > write a new test.

Full suite (gate only): run the entire `npm run test:integration` (no pattern) ONLY when the orchestrator delegates the final integration-test gate before release — not on every task.

Integration runs are long and create a Kind cluster: redirect to a file and check the tail — `npm run test:integration <pattern> > ./tmp/test-output.log 2>&1; tail -30 ./tmp/test-output.log` — and read the full log only if failures appear. On success, tear down with `./tests/integration/infrastructure/teardown-cluster.sh`; on failure, KEEP the cluster for debugging and report the failure. Use ./tmp for any temporary files, never /tmp.

DO NOT modify production code. Never weaken a test to force it green; if you believe a test is wrong, report it back. DO NOT delegate to other roles. If the requested behavior is outside what the integration harness can exercise, report back without writing a test and let the orchestrator route the task to coder directly.

```

</details>

##### Role `reviewer`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-medium` | `devbox run agent-new` | ✗ |
| `start` | no | no | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | `Reviews code changes for correctness, style, and edge cases` | `Reviews code changes for correctness, style, and edge cases` | ✓ |
| `prompt_template` (lines) | 1 | 1 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Review the code changes. Report findings only — do not modify code. Focus on correctness, consistency, edge cases, and missed requirements. If a spec file in `prds/` is referenced, verify the implementation matches it. If critical context is missing, surface it in your work-done summary — the orchestrator will re-delegate with missing context.

```

</details>

<details><summary>User `prompt_template`</summary>

```
Review the change. Report findings only — do not modify code. Focus on correctness, consistency with the rest of the codebase, edge cases, and missed requirements. If a PRD path under prds/ is referenced, verify the implementation matches it. If critical context is missing (e.g. the diff to review, the spec path), surface it in your work-done summary rather than guessing.

```

</details>

##### Role `auditor`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-medium` | `devbox run agent-new` | ✗ |
| `start` | no | no | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | `Audits code for security vulnerabilities and unsafe patterns` | `Audits code for security vulnerabilities and unsafe patterns` | ✓ |
| `prompt_template` (lines) | 1 | 1 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Audit the change for security vulnerabilities, unsafe patterns, and OWASP top-10 issues. Report findings only — do not modify code. Focus on TypeScript/Node-specific risks (prototype pollution, unsafe eval, XXE, etc.). If critical context is missing, surface it in your work-done summary — the orchestrator will re-delegate with missing context.

```

</details>

<details><summary>User `prompt_template`</summary>

```
Audit the change for security vulnerabilities, unsafe patterns, and OWASP top-10 class issues. Report findings only — do not modify code. If the task references a file or diff, read it before starting. If critical context is missing, surface it in your work-done summary rather than guessing.

```

</details>

##### Role `release`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-medium` | `devbox run agent-medium` | ✓ |
| `start` | no | no | ✓ |
| `clear` | no | no | ✓ |
| `description` | `Runs the release/PR/merge workflow; never modifies code` | `Runs the project's release/PR/merge workflow; never modifies code` | ✗ |
| `prompt_template` (lines) | 7 | 7 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Run the release flow in two phases — NEVER modify code.

**Phase 1**: Open the PR via the project's release flow (use `/dot-ai-prd-done` or equivalent). WAIT for CI and review to settle. Report findings summary: PR URL, per-check CI conclusions, review findings. Then STOP — do NOT merge.

**Phase 2**: Merge the PR and close the issue ONLY when the orchestrator re-delegates with explicit go-ahead. If any step fails, report the exact error and stop — do not attempt to diagnose or fix.

If context is missing (PR title, notes path, target branch), report via work-done — the orchestrator will re-delegate with missing context.

```

</details>

<details><summary>User `prompt_template`</summary>

```
Your job is to run /prd-done to complete the PRD workflow (branch, push, PR, merge, close issue). Do NOT modify source code, and do NOT delegate to other roles.

If any step fails (failing CI, merge conflict, or a CHANGES_REQUESTED review state), report the exact error and stop — do not diagnose or fix it; the orchestrator will re-delegate to coder/tester.

After opening the PR, WAIT for all PR processes to finish — CI / GitHub Actions and automated reviews (e.g. CodeRabbit; fall back to `gh` if its MCP is unauthenticated) — then report back: the PR URL, per-check CI conclusions (for any failure, the job/test name + a `gh run view <id> --log-failed` excerpt and a real-regression-vs-flake read; you MAY re-run one clearly-transient job once), and a categorised summary of automated-review findings (severity, file:line, one-line description). Route any decision about findings back through the orchestrator, never the end-user.

Once the PR is open, CI is green, and reviews have settled, STOP — do NOT merge. Report back via work-done with the PR URL so the orchestrator can pause for user review. Only merge the PR and close the issue when the orchestrator re-delegates with an explicit instruction to continue.

```

</details>

- **U-only role** `documenter` (command=`devbox run agent-new`, clear=yes, start=no)

