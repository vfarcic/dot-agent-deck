# PRD #116 — M1.1 Inventory of `.dot-agent-deck.toml` configs

Walk of `~/code/` (depth 3, excluding `node_modules`) on **2026-06-22**. Ten files were
found; collapsing the five `dot-agent-deck-prd-*` git worktrees into the one logical
`dot-agent-deck` project leaves **5 logical projects**, listed below.

## How configs are classified

The deck's config-gen is **interactive**: the agent proposes a config, the user edits it
in-conversation, and only the agreed result is written and committed. So a config can be
heavily hand-shaped yet land in a *single* commit (`dot-ai-cli` is exactly this). Commit
count is therefore a weak signal; the triage below classifies on **content divergence
from the shape the current prompt produces** — does it use project-specific launchers
(`devbox run …`), carry tuned `prompt_template`s naming real commands/paths, or
structurally depart from the prompt's defaults (drop the orchestration, drop all modes,
many narrow rules)? Buckets: *AI-generated, untouched* / *lightly edited* /
*substantially edited*. Precise per-region edit magnitude is quantified by the
baseline diffs (M1.3 pilot here; the rest in Phase 2).

## Inventory

| Project | Path | Classification | Why |
|---|---|---|---|
| **dot-ai-infra** *(pilot)* | `~/code/dot-ai-infra/.dot-agent-deck.toml` | Substantially edited | GitOps/K8s repo. 1 mode, 1 persistent pane (`git status --short`), **6 narrow `kubectl …` rules**, `reactive_panes = 2`, and **no orchestration at all**. Structurally departs from the current prompt (which favors *consolidated* rule alternations, matching `reactive_panes` to rule count, and *always proposing an orchestration*). Edited incrementally Apr 4–22 (a dedicated commit adds the `kubectl rollout status` rule). |
| **dot-ai** | `~/code/dot-ai/.dot-agent-deck.toml` | Substantially edited | Richest config. 1 mode + 2 consolidated rules, plus a **7-role** orchestration (`coder`, `reviewer`, `auditor`, `documenter`, **`tester`**, `release`) with a long, project-specific integration-test TDD chain, `devbox run agent-*` launchers, and a release role that waits on CI + CodeRabbit. Far beyond library defaults. Edited Apr 12–Jun 13 (6 commits incl. "add tester worker with integration-test TDD chain"). |
| **dot-ai-cli** | `~/code/dot-ai-cli/.dot-agent-deck.toml` | Substantially edited | Single commit, but fully hand-shaped: 5-role orchestration with `devbox run agent-*` launchers and `prompt_template`s naming real conventions (`task test` → `tmp/test-output.txt` redirect, integration-vs-unit ownership, Go `//go:build integration`). Closest of the five to the prompt's *intended* shape, but well past an untouched baseline. |
| **youtube-automation** | `~/code/youtube-automation/.dot-agent-deck.toml` | Substantially edited | **Orchestration-only** — every `[[modes]]`/pane/rule was removed; the file is a 5-role orchestration (`coder`, `reviewer`, `auditor`, `release`) with custom prompt_templates and a self-coordinating orchestrator (runs `/prd-update-progress`, `/prd-next` itself). Dropping all modes is a major structural departure from anything the prompt generates. Edited Apr 24–May 9 (8 commits). |
| **dot-agent-deck** | `~/code/dot-agent-deck/.dot-agent-deck.toml` | Substantially edited | The deck dogfooding itself. 1 mode + 2 rules + a 6-role orchestration, refined across **23 commits** over Apr 6–Jun 21 (config-gen, orchestration role-prompt, and workflow refinements). The most-iterated config of the set. |

## Dropped worktree duplicates (same logical project as `dot-agent-deck`)

These are git worktrees of `dot-agent-deck`; their `.dot-agent-deck.toml` is the same
logical file (the project's own dogfooding config), so only the primary checkout is kept:

- `~/code/dot-agent-deck-prd-82-orchestrator-role-reinforcement-against-delegation/`
- `~/code/dot-agent-deck-prd-116-analyze-user-improved-.dot-agent-deck.toml-configs/` *(this worktree)*
- `~/code/dot-agent-deck-prd-120-scheduled-agent-dispatch-on-open-github-issues/`
- `~/code/dot-agent-deck-prd-162-restore-live-session-status-on-daemon-reconnect/`
- `~/code/dot-agent-deck-prd-176-desktop-gui-app-alternative-front-end-to-the-tui/`

## Headline finding

**Every logical project that actually uses the deck has a hand-customized config — none is
AI-generated-untouched.** This directly confirms the PRD's premise that the initial
generated config is rarely the final one, and gives Phase 2 four more substantially-edited
projects (besides the pilot) to diff and aggregate. The configs cluster into recognizable
shapes worth watching as patterns aggregate:

- **Launcher convention:** all four non-deck projects rewrite every role `command` to a
  `devbox run agent-*` script rather than bare `claude` — a likely universal "respect the
  project's devbox agent scripts" pattern.
- **Orchestration reshaping:** projects either drop the orchestration entirely
  (`dot-ai-infra`), drop all modes and keep only the orchestration (`youtube-automation`),
  or grow a much richer one than the library default (`dot-ai`, `dot-ai-cli`).
- **Rule style:** `dot-ai-infra` keeps many narrow `kubectl …` rules while `dot-ai` /
  `dot-ai-cli` use the prompt's preferred consolidated alternations — a candidate
  consistency gap.
