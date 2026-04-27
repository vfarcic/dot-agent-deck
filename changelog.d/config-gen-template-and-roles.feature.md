## Customizable Config-Gen Prompt and Orchestration Role Library

The Ctrl+G config-generation prompt and orchestration role definitions now live in editable asset files instead of being hardcoded in the binary. The prompt template is at `assets/config_gen_prompt.md` and the role library (coder, reviewer, auditor, tester, documenter, release, researcher) is at `assets/roles.toml`. Both are bundled at compile time, so behavior is unchanged for users who don't customize, but contributors can iterate on the prompt without touching Rust source.

The default prompt has also been improved: it now teaches the AI to discover project-defined agent launchers (devbox/npm/task scripts, `.claude/`/`opencode.json` configs, etc.), match them to roles by semantic intent, record the full invocation form (e.g. `devbox run agent-big`, never the bare script name), and propose a dedicated `release` role by default whenever the project has release-flow signals — with explicit context-handoff guidance for the orchestrator so workers cold-starting with no shared scratchpad still receive the file paths and prior findings they need.

The bundled `.dot-agent-deck.toml` reflects these defaults: a `release` role with `clear = false` so it can resume after CI flakes, and a context-handoff section in the orchestrator's `prompt_template`.
