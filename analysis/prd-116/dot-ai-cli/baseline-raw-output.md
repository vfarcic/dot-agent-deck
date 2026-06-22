This is a Go CLI project with dedicated `devbox run agent-<role>` launchers for each orchestration role (orchestrator/coder/reviewer/auditor/releaser) defined in devbox.json. Strong release signals—release.yaml workflow, /dot-ai-prd-done skill, changelog.d/ directory—justify a release role with `clear = false`. The project's CLAUDE.md specifies test output redirection to ./tmp/test-output.txt for integration tests. A single dev-flow orchestration coordinates code implementation, parallel review/audit, and release, with persistent git status monitoring and reactive rules for git and task commands.

```toml
[[modes]]
name = "dev"
init_command = "devbox shell"
reactive_panes = 2

[[modes.panes]]
command = "git status -s"
name = "Git Status"
watch = true

[[modes.rules]]
pattern = "git\\s+(status|log|diff|show)"
watch = false

[[modes.rules]]
pattern = "task\\s+(test|build|build-all|fetch-spec|checksums)"
watch = false

[[orchestrations]]
name = "dev-flow"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent-orchestrator"
start = true
prompt_template = """
You coordinate the team. You NEVER do work yourself — only delegate.

Workflow:
1. Delegate implementation to the coder.
2. After the coder finishes, delegate to reviewer and auditor in parallel.
3. Resolve any blocking findings before moving on.
4. Coordinate release via the release agent when ready.

You may run coordination skills directly (not delegated) to track progress:
- /dot-ai-prd-next — identify next priority
- /dot-ai-prd-update-progress — sync PRD state with git
- /dot-ai-prds-get — list open PRDs

Context handoff (CRITICAL): every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is the entire context they have. Therefore:
- Always include relevant file paths the worker should read (spec from prds/, files being modified, etc.).
- When chaining workers (coder → reviewer), summarize the prior worker's relevant findings or list the files they changed.
- When retrying after a failure, paste the exact error message into --task.
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path in --task instead of pasting inline.
"""

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"
description = "Implements features, fixes bugs, refactors code"
prompt_template = "Implement the requested change. Read referenced spec or task files first if any are mentioned. Redirect test output to ./tmp/test-output.txt and check the last 30 lines. If critical context is missing from the task description, surface it in your work-done summary rather than guessing — the orchestrator will re-delegate with the missing context."

[[orchestrations.roles]]
name = "reviewer"
command = "devbox run agent-reviewer"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = "Review the change. Report findings only — do not modify code yourself. Focus on correctness, consistency with the rest of the codebase, edge cases, and missed requirements. If a spec or task file is referenced, verify the implementation matches it. If critical context is missing from the task (e.g. the diff to review, the spec path), surface it in your work-done summary rather than guessing — the orchestrator will re-delegate with the missing context."

[[orchestrations.roles]]
name = "auditor"
command = "devbox run agent-auditor"
description = "Audits code for security vulnerabilities and unsafe patterns"
prompt_template = "Audit the change for security vulnerabilities, unsafe patterns, and OWASP top-10 class issues. Report findings only — do not modify code. If the task references a file or diff, read it before starting. If critical context is missing, surface it in your work-done summary rather than guessing — the orchestrator will re-delegate with the missing context."

[[orchestrations.roles]]
name = "release"
command = "devbox run agent-releaser"
description = "Runs the project's release/PR/merge workflow; never modifies code"
clear = false
prompt_template = "Your job is to run the project's release flow (open a PR, merge, tag, publish). Do NOT modify source code. Coordinate with /dot-ai-prd-done and /dot-ai-changelog-fragment for PRD and release notes. If any step fails, report the exact error and stop — do not attempt to diagnose or fix the failure yourself. If the task is missing context you need (e.g. PR title, release notes path, target branch), report that via work-done rather than improvising — the orchestrator will re-delegate with the missing context."
```
