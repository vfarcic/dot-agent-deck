> **Note:** the cosmetic mode/orchestration *names* the model picked this run (`gitops-dev`/``) were normalized to the user's names (`gitops`/``) **for this diff only**, so the structured-diff tool pairs roles field-by-field instead of reporting them disjoint. The prompt intentionally does not dictate mode/orchestration names; the authentic model output is preserved in `baseline-v2.toml` / `baseline-v2-raw-output.md`. All other content is verbatim.

# Structured config diff (PRD #116, M1.3)

- **Baseline** (regenerated): `/tmp/dot-ai-infra-v2-norm.toml`
- **Improved** (user): `/home/vfarcic/code/dot-ai-infra/.dot-agent-deck.toml`

Regions are compared per decision #2. "B" = regenerated baseline, "U" = user-improved. Modes/orchestrations/roles are matched by name (case-insensitive); panes by command; rules by pattern.

## `[[modes]]`

Mode count — B: **1**, U: **1**.

### Mode match: B `gitops` ↔ U `gitops`

| Region | Baseline | User-improved | Same? |
|---|---|---|---|
| `init_command` | `devbox shell` | `devbox shell` | ✓ |
| `reactive_panes` | 3 | 2 | ✗ |
| `seed_prompt` | _(none)_ | _(none)_ | ✓ |

#### `[[modes.panes]]`

- **B-only**: `git status -s` (name=Some("Git Status"), watch=yes)
- **B-only**: `kubectl get nodes` (name=Some("Cluster Nodes"), watch=yes)
- **U-only**: `git status --short` (name=Some("Git Status"), watch=yes)

#### `[[modes.rules]]`

- **B-only**: `git\s+(log|diff|show|status)` (watch=no)
- **B-only**: `kubectl\s+(get|describe|logs|tree)` (watch=no)
- **B-only**: `helm\s+(list|status|values|get)` (watch=no)
- **U-only**: `kubectl\s+get\s+applications` (watch=no)
- **U-only**: `kubectl\s+get` (watch=no)
- **U-only**: `kubectl\s+describe` (watch=no)
- **U-only**: `kubectl\s+logs` (watch=no)
- **U-only**: `kubectl\s+rollout\s+status` (watch=no)
- **U-only**: `helm\s+list` (watch=no)

## `[[orchestrations]]`

Orchestration count — B: **1**, U: **0**.

### Orchestration `gitops-flow` — **B-only (user removed the whole orchestration)**: roles = orchestrator, coder, reviewer, auditor, release

