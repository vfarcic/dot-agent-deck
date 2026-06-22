This GitOps infrastructure project uses Kubernetes, Argo CD, Helm, and Nushell to deploy the dot-ai platform on GKE. The reproducible environment is `devbox.json`, with the project-defined agent launcher `devbox run agent` (aliases `claude --continue`). Persistent panes monitor git status (track manifest changes) and Argo CD applications (verify deployments), while reactive rules capture read-only kubectl, helm, and git inspection commands triggered during agent work. The orchestration pairs coder, reviewer, and release agents with the project's PRD coordination skills (`/dot-ai-prd-done`, `/dot-ai-tag-release`, `/dot-ai-changelog-fragment`) for end-to-end infrastructure management.

```toml
[[modes]]
name = "GitOps"
init_command = "devbox shell"
reactive_panes = 3

[[modes.panes]]
command = "git status -s"
name = "Git Changes"
watch = true

[[modes.panes]]
command = "kubectl get applications -A"
name = "Argo CD Apps"
watch = true

[[modes.rules]]
pattern = "kubectl\\s+(get|describe|logs|tree)"
watch = false

[[modes.rules]]
pattern = "helm\\s+(list|status|diff)"
watch = false

[[modes.rules]]
pattern = "git\\s+(log|diff|show)"
watch = false

[[orchestrations]]
name = "infrastructure-flow"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent"
start = true
prompt_template = """
You coordinate the infrastructure team. You NEVER do work yourself — only delegate.

This is a GitOps repository for the dot-ai platform on GKE. Work typically flows:
1. Check active PRDs with /dot-ai-prds-get and/or /dot-ai-prd-next
2. Delegate implementation to coder
3. After coder finishes, delegate review to reviewer
4. After reviewer clears blocking issues, delegate release/tagging to release
5. Track progress with /dot-ai-prd-update-progress

The prds/ directory contains active and completed specifications — reference it when delegating work.

CONTEXT HANDOFF (CRITICAL): Every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is their entire context. Therefore:
- Always include relevant file paths and spec references (e.g., "Implement the change described in prds/107-...")
- When chaining workers (coder → reviewer), summarize the files changed and key decisions made
- When retrying after a failure, paste the exact error message into --task
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path in --task
"""

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent"
description = "Implements features, fixes bugs, refactors code"
prompt_template = """
Implement the requested changes to the infrastructure (Kubernetes manifests, Helm charts, Nushell scripts, etc.).

Before starting, read any referenced spec file (typically in prds/). Follow Kubernetes and Helm best practices. Use kubectl, helm, and git inspection commands to understand the current state. Validate your changes make sense for a GitOps-deployed system. Report what you changed and any validation steps you ran.
"""

[[orchestrations.roles]]
name = "reviewer"
command = "devbox run agent"
description = "Reviews code changes for correctness, style, and edge cases"
prompt_template = """
Review the infrastructure changes (Kubernetes manifests, Helm charts, Nushell scripts, configs).

Focus on: correctness, Kubernetes best practices, security, consistency with Argo CD patterns, and whether the changes match the requested spec. Use kubectl, helm, and git inspection to understand context. Report findings only — do not modify code. If critical context is missing (e.g., no diff provided), surface that in your work-done summary.
"""

[[orchestrations.roles]]
name = "release"
command = "devbox run agent"
description = "Runs the project's release/PR/merge workflow; never modifies code"
clear = false
prompt_template = """
Your job is to finalize the release: update PRD status, tag the release, and optionally generate a changelog.

Steps:
1. Use /dot-ai-prd-done to mark the associated PRD complete
2. Use /dot-ai-tag-release to create a release tag
3. Optionally use /dot-ai-changelog-fragment to document changes

Do NOT modify source code or manifests. If any step fails, report the exact error and stop — do not attempt to diagnose or fix.
"""
```
