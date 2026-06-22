# Structured config diff (PRD #116, M1.3)

- **Baseline** (regenerated): `analysis/prd-116/youtube-automation/baseline.toml`
- **Improved** (user): `/home/vfarcic/code/youtube-automation/.dot-agent-deck.toml`

Regions are compared per decision #2. "B" = regenerated baseline, "U" = user-improved. Modes/orchestrations/roles are matched by name (case-insensitive); panes by command; rules by pattern.

## `[[modes]]`

Mode count — B: **1**, U: **0**.

### Mode `develop` — **B-only (user removed)**

## `[[orchestrations]]`

Orchestration count — B: **1**, U: **1**.

### Orchestration `dev-flow` — **B-only (user removed the whole orchestration)**: roles = orchestrator, coder, reviewer, auditor, release

### Orchestration `youtube-automation` — **U-only (user added)**: roles = orchestrator, coder, reviewer, auditor, release

