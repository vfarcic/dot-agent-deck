# Structured config diff (PRD #116, M1.3)

- **Baseline** (regenerated): `analysis/prd-116/dot-ai-infra/baseline.toml`
- **Improved** (user): `/home/vfarcic/code/dot-ai-infra/.dot-agent-deck.toml`

Regions are compared per decision #2. "B" = regenerated baseline, "U" = user-improved. Modes/orchestrations/roles are matched by name (case-insensitive); panes by command; rules by pattern.

## `[[modes]]`

Mode count — B: **1**, U: **1**.

### Mode match: B `GitOps` ↔ U `gitops`

| Region | Baseline | User-improved | Same? |
|---|---|---|---|
| `init_command` | `devbox shell` | `devbox shell` | ✓ |
| `reactive_panes` | 3 | 2 | ✗ |
| `seed_prompt` | _(none)_ | _(none)_ | ✓ |

#### `[[modes.panes]]`

- **B-only**: `git status -s` (name=Some("Git Changes"), watch=yes)
- **B-only**: `kubectl get applications -A` (name=Some("Argo CD Apps"), watch=yes)
- **U-only**: `git status --short` (name=Some("Git Status"), watch=yes)

#### `[[modes.rules]]`

- **B-only**: `kubectl\s+(get|describe|logs|tree)` (watch=no)
- **B-only**: `helm\s+(list|status|diff)` (watch=no)
- **B-only**: `git\s+(log|diff|show)` (watch=no)
- **U-only**: `kubectl\s+get\s+applications` (watch=no)
- **U-only**: `kubectl\s+get` (watch=no)
- **U-only**: `kubectl\s+describe` (watch=no)
- **U-only**: `kubectl\s+logs` (watch=no)
- **U-only**: `kubectl\s+rollout\s+status` (watch=no)
- **U-only**: `helm\s+list` (watch=no)

## `[[orchestrations]]`

Orchestration count — B: **1**, U: **0**.

### Orchestration `infrastructure-flow` — **B-only (user removed the whole orchestration)**: roles = orchestrator, coder, reviewer, release

