# PRD #53: AI-Generated ASCII Art for Idle Dashboard Cards

**Status**: Draft
**Priority**: Low
**Created**: 2026-04-08

## Problem

When agent sessions go idle on the dashboard, cards show static status information — prompts, tool counts, elapsed time. Users stare at unchanging cards while waiting for their next task. This is a missed opportunity to add personality and delight to the tool, and to give users a fun, contextual reminder of what the agent just accomplished.

## Solution

After a configurable idle timeout (default 5 minutes), generate funny, context-aware ASCII art by calling a lightweight LLM (e.g., Haiku) with the session's user prompts and the agent's final response. The art is rendered directly in the idle dashboard card. The LLM prompt instructions live in a standalone `.md` file embedded into the binary at compile time via `include_str!()` for easy maintenance.

The feature is delivered in three phases, each building on the previous and delivering standalone value.

## Phases

### Phase 1: Validate the Prompt

Before writing any integration code, validate that the LLM can consistently produce good ASCII art.

- Create `assets/idle-art-prompt.md` with the system prompt instructions (dimensions, style, humor tone, frame count, delimiters)
- Test it directly against Haiku using sample prompts and agent responses from real sessions
- Iterate on the prompt until output is consistently:
  - Funny and contextually relevant to the prompts/response
  - Correctly sized within card constraints (approx. 40 chars wide, 6-10 rows)
  - Well-formed across multiple frames (if animated) with consistent dimensions
- Document sample inputs/outputs in the PR for review
- **Exit criteria**: At least 10 sample generations reviewed and deemed good enough to ship

### Phase 2: CLI Command

Expose the ASCII art generation as a standalone CLI subcommand:

```
dot-agent-deck ascii --input "user prompts here" --output "agent final response here" [--provider anthropic] [--model claude-haiku-4-5]
```

- Reads the embedded prompt from `assets/idle-art-prompt.md` via `include_str!()`
- Calls the configured LLM with the assembled prompt + provided context
- Prints the ASCII art frames to stdout (delimited for multi-frame output)
- Respects configuration for provider and model; CLI flags `--provider` and `--model` override config
- Useful standalone: users can script it, pipe it, or have agents call it post-completion
- The `idle_art.enabled` config flag is ignored by the CLI — it only gates automatic dashboard art in Phase 3

**Configuration** (in `.dot-agent-deck.toml` or global config):

```toml
[idle_art]
enabled = false              # only affects dashboard (Phase 3); CLI always works
provider = "anthropic"       # or "openai", "ollama" — requires ANTHROPIC_API_KEY / OPENAI_API_KEY env var
model = "claude-haiku-4-5"
```

This phase also establishes the core logic (prompt assembly, LLM call, response parsing) that Phase 3 reuses.

### Phase 3: Dashboard Integration

Wire the proven CLI logic into the dashboard's idle card rendering:

- **Spacious density only**: Art generation only triggers when the dashboard is using `CardDensity::Spacious` (11-12 total rows, ~8 usable content rows). In Normal and Compact modes, cards continue showing the existing flashing-dot idle indicator. This avoids truncating art to fit smaller cards, which would destroy the visual.
- **Capture first prompts**: Add `first_prompts: Vec<String>` to `SessionState`, capped at 2-3 entries, populated on the earliest `UserPrompt` events and never overwritten. Combined with the last few prompts from `recent_events`, this gives the LLM the full narrative arc — what the user set out to do and where they ended up — producing more contextual and funnier art.
- Detect idle state: trigger when `SessionStatus::Idle` persists beyond the configured timeout (default 300 seconds)
- Call the same LLM logic from Phase 2 with the session's first prompts, last prompts, and last agent response
- **Generate-validate-retry**: After each LLM call, validate the response dimensions (line count per frame and line width against current card constraints). If validation fails, retry up to 3 times total. If all attempts fail, discard and fall back to the flashing-dot indicator.
- Parse multi-frame response and cycle frames on the ratatui tick loop
- Render ASCII art as an overlay or replacement of the card's status content while idle
- Cache the generated art per idle stretch — do not regenerate on every tick
- One generation per session per idle period (reset when session becomes active again)
- Fallback: if LLM call fails, times out, or all 3 validation retries fail, show the existing flashing-dot idle indicator (no broken art ever reaches the screen)

**Additional configuration**:

```toml
[idle_art]
enabled = true               # must be explicitly enabled for dashboard art
provider = "anthropic"       # requires ANTHROPIC_API_KEY env var
model = "claude-haiku-4-5"
timeout_secs = 300           # idle time before triggering
```

## Design Decisions

- **Prompt in `.md`, not Rust code**: Easier to iterate on tone, style, and dimensions without touching Rust. Embedded at compile time so the binary remains self-contained.
- **CLI command before dashboard**: Validates the full pipeline (prompt → LLM → output) in isolation. Easier to debug, test, and demo. Also provides standalone value.
- **First + last prompts**: First prompts capture user intent, last prompts capture where the session ended up. Together they give the LLM enough narrative to produce contextually funny art rather than generic filler.
- **One LLM call per idle stretch**: Avoids runaway costs. Art is cached and replayed until the session becomes active.
- **Agent panes untouched**: ASCII art only appears in dashboard cards. Agent output stays clean and "serious" — users need that information.
- **Configurable provider/model**: Users control cost and can use local models (Ollama) for zero-cost art generation.
- **Density-aware rendering** (decided 2026-04-09): ASCII art is only attempted in Spacious card density mode (~8 usable content rows). In Normal and Compact modes, cards fall back to the existing flashing-dot idle indicator. Rationale: truncating ASCII art to fit smaller cards destroys the visual — a stick figure missing its legs is worse than no art at all.
- **Generate-validate-retry** (decided 2026-04-09): LLMs cannot reliably count output lines. Phase 1 validation showed Haiku exceeds the 8-line constraint in ~60% of generations. Rather than relying on the prompt alone, the pipeline validates dimensions after each generation and retries up to 3 times. If all attempts fail, it falls back to the flashing dot. This ensures broken art never reaches the screen while keeping costs low (Haiku calls are cheap and fast).
- **Opt-in by default** (decided 2026-04-09): `idle_art.enabled` defaults to `false`. Most Claude Code users have `ANTHROPIC_API_KEY` set, so defaulting to enabled would silently make LLM calls and incur costs on every idle session without the user opting in. Users enable it explicitly via `dot-agent-deck config set idle_art.enabled true`. The `ascii` CLI subcommand ignores this flag — explicit invocation always works.
- **CLI flags override config** (decided 2026-04-09): The `ascii` subcommand accepts `--provider` and `--model` flags that override config file values. This lets users try the feature without creating a config file first — just `ANTHROPIC_API_KEY=... dot-agent-deck ascii --input "..." --output "..."` works out of the box.
- **API keys via environment only** (decided 2026-04-09): Provider API keys are read from environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) and never stored in config files. This follows standard security practice and avoids secrets on disk. Ollama needs no key. The `config set --help` output documents which env var each provider requires.

## Out of Scope

- Agent-side skill that instructs agents to generate art themselves
- Art in mode panes (could be a follow-up)
- Custom user-provided prompt overrides (could be a follow-up)

## Milestones

- [x] `assets/idle-art-prompt.md` created and validated against Haiku with 10+ sample generations
- [x] `dot-agent-deck ascii` CLI subcommand working end-to-end
- [x] Configuration schema for `[idle_art]` implemented and documented
- [ ] First-prompt capture added to `SessionState` (first 2-3 prompts preserved separately)
- [ ] Dashboard idle detection triggers art generation after configured timeout
- [ ] ASCII art frames render correctly in dashboard cards with proper cycling
- [ ] Fallback animation works when LLM call fails
- [ ] Dimension validation and retry loop (up to 3 attempts) with flashing-dot fallback
- [ ] Art rendering gated on `CardDensity::Spacious` — Normal/Compact show flashing dot only
- [ ] Tests covering CLI command, idle detection trigger, frame rendering, and validation/retry logic
- [ ] Getting-started or user guide updated with feature description and config examples
