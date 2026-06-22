# Structured config diff (PRD #116, M1.3)

- **Baseline** (regenerated): `analysis/prd-116/dot-ai/baseline.toml`
- **Improved** (user): `/home/vfarcic/code/dot-ai/.dot-agent-deck.toml`

Regions are compared per decision #2. "B" = regenerated baseline, "U" = user-improved. Modes/orchestrations/roles are matched by name (case-insensitive); panes by command; rules by pattern.

## `[[modes]]`

Mode count — B: **1**, U: **1**.

### Mode `dev` — **B-only (user removed)**

### Mode `develop` — **U-only (user added)**: 1 pane(s), 2 rule(s), reactive_panes=2

## `[[orchestrations]]`

Orchestration count — B: **1**, U: **1**.

### Orchestration match: B `dev-flow` ↔ U `dev-flow`

#### `[[orchestrations.roles]]`

##### Role `orchestrator`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-new` | `devbox run agent-new` | ✓ |
| `start` | yes | yes | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | _(none)_ | _(none)_ | ✓ |
| `prompt_template` (lines) | 15 | 24 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
You coordinate the dev team. You NEVER do implementation, review, or release work yourself — only delegate.

Workflow:
1. Use /dot-ai-prd-next to identify the next task from prds/.
2. Delegate implementation to coder. Coder must run both npm run test:unit and npm run test:integration before finishing.
3. Delegate review to reviewer for code quality.
4. Summarize what to test end-to-end and STOP — wait for user approval before proceeding to release.
5. Delegate to release to open a PR and merge via /dot-ai-prd-done.

Context-handoff rule (CRITICAL): Every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is their entire context. Therefore:
- Always include the spec file path from prds/ if referenced in the task.
- Include file paths the worker should read.
- When chaining workers (e.g., coder → reviewer), paste the changed files or a summary of what was implemented.
- When retrying after failure, include the exact error message in --task.
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path instead of pasting inline.

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
| `prompt_template` (lines) | 5 | 1 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Implement the requested change. If a spec is referenced, read it from prds/ first.
Run npm run test:unit (fast unit tests) first to validate.
Then run npm run test:integration with a scoped pattern to validate the full integration suite.
If tests fail, investigate and fix the code until all tests pass.
Report completion only when tests are green.

```

</details>

<details><summary>User `prompt_template`</summary>

```
Implement the requested change. Read referenced PRD or task files first if any are mentioned (typically under prds/). In a TDD chain, the tester has written a failing integration test that pins the required behavior — read it as your spec and make it pass by changing PRODUCTION code only. You MUST NOT create or modify any integration test under tests/integration/ — those belong exclusively to the tester. If you cannot satisfy the test without changing it (or you believe the test is wrong), do NOT work around or edit it — report back to the orchestrator, who decides what to do. Unit tests and lint are yours: run `npm run test:unit` and `npm run lint` before reporting completion. If critical context is missing from the task description, surface it in your work-done summary rather than guessing — the orchestrator will re-delegate.

```

</details>

##### Role `reviewer`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-medium` | `devbox run agent-new` | ✗ |
| `start` | no | no | ✓ |
| `clear` | yes | yes | ✓ |
| `description` | `Reviews code changes for correctness, style, and edge cases` | `Reviews code changes for correctness, style, and edge cases` | ✓ |
| `prompt_template` (lines) | 4 | 1 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Review the code changes. Report findings only — do not modify code.
Focus on correctness, consistency with the rest of the codebase, edge cases, and spec compliance.
If a spec file is referenced, verify the implementation matches it.
If critical context is missing from the task, surface it in your work-done summary rather than guessing.

```

</details>

<details><summary>User `prompt_template`</summary>

```
Review the change. Report findings only — do not modify code. Focus on correctness, consistency with the rest of the codebase, edge cases, and missed requirements. If a PRD path under prds/ is referenced, verify the implementation matches it. If critical context is missing (e.g. the diff to review, the spec path), surface it in your work-done summary rather than guessing.

```

</details>

##### Role `release`

| Field | Baseline | User-improved | Same? |
|---|---|---|---|
| `command` | `devbox run agent-medium` | `devbox run agent-medium` | ✓ |
| `start` | no | no | ✓ |
| `clear` | no | no | ✓ |
| `description` | `Runs the project's release/PR/merge workflow` | `Runs the project's release/PR/merge workflow; never modifies code` | ✗ |
| `prompt_template` (lines) | 4 | 7 | ✗ |

<details><summary>Baseline `prompt_template`</summary>

```
Your job is to open a PR and merge it via /dot-ai-prd-done.
Do NOT modify source code.
If any step fails, report the exact error and stop — do not attempt to fix code yourself.
If critical context is missing (e.g., PR title, release notes path), report that in your work-done summary.

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

- **U-only role** `auditor` (command=`devbox run agent-new`, clear=yes, start=no)

- **U-only role** `documenter` (command=`devbox run agent-new`, clear=yes, start=no)

- **U-only role** `tester` (command=`devbox run agent-tester`, clear=yes, start=no)

