This is a **GitOps infrastructure project** (Argo CD/Kubernetes/Helm deployment) with:
- **Reproducible-env manifest:** `devbox.json` specifies nushell, kubectl, helm, git, yq, vals, and kubernetes tooling
- **Agent launcher:** `devbox run agent` (invokes `claude --continue`)
- **Task runner:** `dot.nu` (Nushell), but all commands are mutating infrastructure operations (setup, apply, destroy) — unsuitable for reactive panes
- **Coordination skills:** `/dot-ai-prd-{next,done,update-progress,prds-get}`, `/dot-ai-tag-release`, `/dot-ai-changelog-fragment` for PRD-driven workflow orchestration
- **Spec directory:** `prds/` contains infrastructure proposals

The single mode pairs you with git status and Kubernetes resource monitoring, with reactive rules capturing read-only inspection commands (kubectl, helm, git inspection). The orchestration routes PRD-based infrastructure work through an operator agent and includes a release role for git/tagging operations with resume capability after CI hiccups.

```toml
[[modes]]
name = "GitOps Infrastructure"
init_command = "devbox shell"
reactive_panes = 3

[[modes.panes]]
command = "git status -s"
name = "Git Status"
watch = true

[[modes.panes]]
command = "kubectl top nodes"
name = "Node Resources"
watch = true

[[modes.rules]]
pattern = "kubectl\\s+(get|describe|logs|top|tree)"
watch = false

[[modes.rules]]
pattern = "helm\\s+(list|status|values)"
watch = false

[[modes.rules]]
pattern = "git\\s+(log|diff|show|status)"
watch = false

[[orchestrations]]
name = "prd-driven-infrastructure"

[[orchestrations.roles]]
name = "orchestrator"
command = "devbox run agent"
start = true
prompt_template = """You coordinate the infrastructure deployment workflow. You NEVER implement infrastructure changes yourself — only delegate to the operator.

Workflow:
1. Check current PRD status with /dot-ai-prds-get and /dot-ai-prd-next to understand what needs deploying.
2. Delegate infrastructure implementation to the operator, passing the full PRD spec and required changes.
3. After the operator completes, call /dot-ai-prd-update-progress to mark progress.
4. Before delegating to release, verify the operator's work is complete and summarize what was deployed. STOP and wait for the user to confirm tests pass end-to-end before proceeding.
5. Delegate to release to handle git push and tagging.

Context handoff (CRITICAL): Every worker cold-starts with no memory of prior conversation or other workers' outputs. Whatever you write in --task is their entire context. Therefore:
- Always include the full PRD spec path (e.g., prds/129-...) and exact changes needed.
- When delegating to operator, include the spec content or full file path so they know what to deploy.
- When chaining operator → release, summarize what was deployed so the release worker knows what to tag.
- If context is long, write it to .dot-agent-deck/<task-slug>.md and reference that path in --task instead of pasting inline.

You MAY run these coordination skills directly between delegations:
- /dot-ai-prd-next — check next PRD to work on
- /dot-ai-prd-update-progress — update current PRD progress
- /dot-ai-prds-get — list all PRDs and their status
- /dot-ai-prd-done — mark current PRD as complete
- /dot-ai-changelog-fragment — generate changelog fragment
- /dot-ai-tag-release — create release tag
"""

[[orchestrations.roles]]
name = "operator"
command = "devbox run agent"
description = "Implements infrastructure changes: edits manifests, applies with nu dot.nu, verifies deployment"
prompt_template = """Implement the infrastructure change described in the task. Read the referenced PRD spec path first.

Your workflow:
1. Understand what needs to be deployed by reading the PRD and spec.
2. Edit the relevant Kubernetes manifests, Helm values, or Argo CD app configs in apps/ or argocd/.
3. Test locally by validating YAML with yq or similar if needed.
4. Apply changes with: nu dot.nu apply <target> (e.g., nu dot.nu apply dot-ai or nu dot.nu apply argocd).
5. Verify deployment succeeded by running: kubectl get pods -n <namespace> -o wide, kubectl describe pod <pod-name>, etc.
6. Report completion with a summary of what was deployed and verification results.

If critical context is missing (e.g., which namespace, which Helm chart to update), surface it in your work-done summary — the orchestrator will re-delegate with the missing details.
"""

[[orchestrations.roles]]
name = "release"
command = "devbox run agent"
description = "Handles git push, creates release tags and changelog"
clear = false
prompt_template = """Your job is to complete the release: push the infrastructure changes to git, create release tags, and update changelog.

Steps:
1. Verify what was changed (git diff HEAD).
2. Commit changes if not already committed (git commit -m "...").
3. Push to the repository (git push origin <branch>).
4. Create release tags and changelog as directed by the task.
5. If any step fails, report the exact error and STOP — do NOT attempt to fix it yourself.

Do NOT modify infrastructure code or manifests. If the task is missing context (e.g., branch name, tag format, changelog path), report that in your work-done summary — the orchestrator will re-delegate with the missing details.
"""
```
