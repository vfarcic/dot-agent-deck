# Structured config diff (PRD #116, M1.3)

- **Baseline** (regenerated): `analysis/prd-116/dot-agent-deck/baseline.toml`
- **Improved** (user): `/home/vfarcic/code/dot-agent-deck/.dot-agent-deck.toml`

Regions are compared per decision #2. "B" = regenerated baseline, "U" = user-improved. Modes/orchestrations/roles are matched by name (case-insensitive); panes by command; rules by pattern.

## `[[modes]]`

Mode count — B: **1**, U: **1**.

### Mode `dev-flow` — **B-only (user removed)**

### Mode `dev` — **U-only (user added)**: 1 pane(s), 2 rule(s), reactive_panes=2

## `[[orchestrations]]`

Orchestration count — B: **1**, U: **1**.

### Orchestration `dev-flow` — **B-only (user removed the whole orchestration)**: roles = orchestrator, coder, reviewer, release

### Orchestration `dot-agent-deck` — **U-only (user added)**: roles = orchestrator, coder, reviewer, auditor, tester, release

