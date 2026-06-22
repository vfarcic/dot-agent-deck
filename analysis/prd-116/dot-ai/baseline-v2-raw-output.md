This is a **TypeScript/Node MCP server** with strong infrastructure and mandatory integration testing. Key signals:

- **Reproducible environment:** `devbox.json` with Node + K8s toolchain (helm, kubectl, kind, yq, jq, gh, git) → `init_command = "devbox shell"`
- **Agent launchers:** devbox scripts `agent` (full), `agent-new`, `agent-tester`, `agent-medium` (sonnet) → use these for role commands
- **Test discipline (from CLAUDE.md):** Two-tier split—fast unit (`npm run test:unit`, vitest) and mandatory heavy integration (`npm run test:integration`, Kind cluster). Integration is the PR gate. → **include tester role for RED/GREEN cycle**
- **Release flow signals:** `/dot-ai-prd-done` skill + CI workflows + `changelog.d/` fragments → **include release role**
- **Spec directory:** `prds/` (35 PRDs)
- **Dev commands:** `npm run test:unit|test:integration|lint|audit|format`, `git status|log|diff`, `kubectl get|describe|logs`

**Mode:** Single `typescript-dev` mode with git status pane (persistent) + reactive rules for npm test/lint and git inspection.

**Orchestration:** Six-role TDD-enabled workflow (orchestrator → RED/GREEN/verify → review in parallel → integration gate → release with user confirmation before merge).

```toml
[[modes]]
name = "typescript-dev"
init_command = "devbox shell"
reactive_panes = 3

[[modes.panes]]
command = "git status -s"
name = "Changes"
watch = true

[[modes.rules]]
pattern = "npm run (test:unit|lint|audit)"
watch = false

[[modes.rules]]
pattern = "npm run test:integration"
watch = false

[[modes.rules]]
pattern = "git (log|status|diff|show)"
watch = false

[[orchestrations]]
name = "typescript-workflow"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent"
start = true
prompt_template = """
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
"""

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-new"
description = "Implements features, fixes bugs, refactors code"
prompt_template = """
Implement the requested change. Read referenced spec files in `prds/` or task files first. Run `npm run test:unit` before reporting completion. If critical context is missing from the task, surface it in your work-done summary — the orchestrator will re-delegate with missing context.
"""

[[orchestrations.roles]]
name = "tester"
command = "devbox run agent-tester"
description = "Writes and runs tests; drives RED/GREEN TDD cycles"
prompt_template = """
Own the test suite. In RED phase, write or extend a failing test capturing the requirement and confirm it fails. In GREEN phase, after coder finishes, re-run the scoped test and confirm it passes. Use `npm run test:unit` for fast per-task validation and `npm run test:integration` for the full suite before release. For integration tests, monitor `./tmp/test-output.log` and use `./tests/integration/infrastructure/teardown-cluster.sh` if needed. If critical context is missing, surface it in your work-done summary — the orchestrator will re-delegate with missing context.
"""

[[orchestrations.roles]]
name = "reviewer"
command = "devbox run agent-medium"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = """
Review the code changes. Report findings only — do not modify code. Focus on correctness, consistency, edge cases, and missed requirements. If a spec file in `prds/` is referenced, verify the implementation matches it. If critical context is missing, surface it in your work-done summary — the orchestrator will re-delegate with missing context.
"""

[[orchestrations.roles]]
name = "auditor"
command = "devbox run agent-medium"
description = "Audits code for security vulnerabilities and unsafe patterns"
prompt_template = """
Audit the change for security vulnerabilities, unsafe patterns, and OWASP top-10 issues. Report findings only — do not modify code. Focus on TypeScript/Node-specific risks (prototype pollution, unsafe eval, XXE, etc.). If critical context is missing, surface it in your work-done summary — the orchestrator will re-delegate with missing context.
"""

[[orchestrations.roles]]
name = "release"
command = "devbox run agent-medium"
description = "Runs the release/PR/merge workflow; never modifies code"
clear = false
prompt_template = """
Run the release flow in two phases — NEVER modify code.

**Phase 1**: Open the PR via the project's release flow (use `/dot-ai-prd-done` or equivalent). WAIT for CI and review to settle. Report findings summary: PR URL, per-check CI conclusions, review findings. Then STOP — do NOT merge.

**Phase 2**: Merge the PR and close the issue ONLY when the orchestrator re-delegates with explicit go-ahead. If any step fails, report the exact error and stop — do not attempt to diagnose or fix.

If context is missing (PR title, notes path, target branch), report via work-done — the orchestrator will re-delegate with missing context.
"""
```
