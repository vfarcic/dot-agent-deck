# chain-smoke/claude/001 — A real Claude Code agent run end-to-end emits hook events that drive the card through Thinking → Working → Idle.

**Source:** `tests/e2e_chain_smoke_claude.rs::claude_001_thinking_working_idle`
**Catalog:** PRD #77 `## Test Case Catalog`
**Cast:** `claude_001_thinking_working_idle.cast`

## Scenario

Import the host's Claude Code credentials into a per-test HOME, stage a saved session whose pane runs `claude -p "…use the Bash tool to run pwd…" --model claude-haiku-4-5-20251001 --allowedTools Bash`, then launch the deck with `--continue` so the agent process auto-starts. As the real Claude run unfolds, the deck's hook plugin posts events that drive the card through Thinking → Working → Idle, with the `Bash` tool name visible on the card during Working. Runs against the real Anthropic API; cost is bounded at one Haiku invocation.

## Steps

1. Skip unless Claude Code CLI is available
2. Import Claude credentials into the test HOME
3. Stage a saved session `claude-smoke` running `agent_command`
4. Launch the deck with fixture `chain-smoke-claude`
5. Wait for `claude-smoke` to appear on screen
6. Wait for `Thinking` to appear on screen
7. Wait for `Working` to appear on screen
8. Wait for `Bash` to appear on screen
9. Wait for `Idle` to appear on screen

## Catalog spec

- **Layer:** L2.
- **Agent:** Claude Code (`claude-haiku-4-5-20251001` per Decision 8).
- **Asserts:** card status traverses Thinking → Working → Idle within the test budget; tool name appears on the card during Working.
- **Does not assert:** any specific text the agent prints.
- **Platform coverage:** mac+linux (chain-smoke is local-only per Decision 8).
- **Cost note:** one Haiku invocation, ≲500 input + 200 output tokens — well under Decision 23's bound.

## Replay

```sh
asciinema play .dot-agent-deck/m3-recordings/claude_001_thinking_working_idle.cast
```

## Rerun

```sh
cargo test-e2e claude_001
```
