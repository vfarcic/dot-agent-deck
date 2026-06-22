This Go project (youtube-automation) uses `devbox.json` as its reproducible-environment manifest with `just` and `make` as task runners. Critically, devbox.json defines rich, role-specific agent launchers (`agent-orchestrator`, `agent-coder`, `agent-reviewer`, `agent-auditor`, `agent-release`), eliminating any need to fall back to bare CLIs. The project has strong release-flow signals: a `.github/workflows/release.yml` workflow, a `/prd-done` skill, and version-bump tooling. The single mode "develop" tracks in-flight changes via `git status -s` (persistent, auto-refreshed) and captures reactive patterns for read-only go/git/just commands (3 rules). The orchestration pairs the project's role launchers directly (orchestrator coordinates, coder implements, reviewer+auditor audit in parallel, release handles the workflow) with clear context-handoff rules and spec-directory references.

```toml
[[modes]]
name = "develop"
init_command = "devbox shell"
reactive_panes = 3

[[modes.panes]]
command = "git status -s"
name = "Git Status"
watch = true

[[modes.rules]]
pattern = "go\\s+(test|build|vet)"
watch = false

[[modes.rules]]
pattern = "git\\s+(status|log|diff|show)"
watch = false

[[modes.rules]]
pattern = "just\\s+(test|build)"
watch = false

[[orchestrations]]
name = "dev-flow"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent-orchestrator"
start = true
prompt_template = """
You coordinate the team. You NEVER do work yourself — only delegate to the available agents.

Workflow:
- Delegate implementation to coder.
- After coder finishes, delegate to reviewer and auditor in parallel.
- Resolve any blocking findings before proceeding.
- Before releasing, summarize what has been completed and validated, and STOP for user confirmation.
- After user confirms, delegate to release.

Context handoff (CRITICAL): every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is the entire context they have. Therefore:
- Always include relevant file paths the worker should read (specs in prds/, files being modified, etc.).
- When chaining workers (coder → reviewer), paste the changed files or summarize coder's output.
- When retrying after a failure, paste the exact error message into --task.
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path in --task instead of pasting inline.

The project has PRD specs in prds/. Reference them when relevant.
"""

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"
description = "Implements features, fixes bugs, refactors code"
prompt_template = "Implement the requested change. Read the spec in prds/ if referenced. Run `just test` or `go test ./...` before reporting completion."

[[orchestrations.roles]]
name = "reviewer"
command = "devbox run agent-reviewer"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = "Review the change. Report findings only — do not modify code. Focus on correctness, consistency, edge cases, and spec adherence."

[[orchestrations.roles]]
name = "auditor"
command = "devbox run agent-auditor"
description = "Audits code for security vulnerabilities and unsafe patterns"
prompt_template = "Audit the change for security vulnerabilities and OWASP top-10 class issues. Report findings only — do not modify code."

[[orchestrations.roles]]
name = "release"
command = "devbox run agent-release"
description = "Runs the project's release workflow"
clear = false
prompt_template = "Run the project's release flow. Do NOT modify source code. If any step fails, report the exact error and stop — do not attempt to diagnose or fix yourself."
```
