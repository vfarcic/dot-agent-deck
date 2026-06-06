# M3 validation recordings

Captured artifacts for the chain-smoke L2 tests delivered by PRD #77 milestone M3.

## L2 — `chain-smoke/claude/001`

```sh
asciinema play .dot-agent-deck/m3-recordings/claude_001_thinking_working_idle.cast
```

Recorded via `DOT_AGENT_DECK_RECORD=1 cargo test-e2e claude_001` against
`claude-haiku-4-5-20251001`, ≲500 input + 200 output tokens per Decision 23.

## L2 — `chain-smoke/opencode/001`

**Not shipping in M3** — see `## M3: Implementation Notes` in
`prds/77-tui-testing-harness.md` for the deck-bug detail. Briefly: OpenCode
1.x's plugin loader does not auto-discover `<config>/plugin/<name>/index.js`,
so the deck's `src/opencode_manage.rs` install path never produces hook
events end-to-end. The catalog entry is parked back on the
`xtask/linkage-check/m2.allowlist` until the deck fix lands.
