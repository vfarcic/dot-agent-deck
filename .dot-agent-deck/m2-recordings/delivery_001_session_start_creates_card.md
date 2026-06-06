# hooks/delivery/001 — A Claude Code `SessionStart` hook arriving at the daemon's hook socket creates a session entry on the dashboard.

**Source:** `tests/e2e_hook_delivery.rs::delivery_001_session_start_creates_card`
**Catalog:** PRD #77 `## Test Case Catalog`
**Cast:** `delivery_001_session_start_creates_card.cast`

## Scenario

Launch the deck against the `minimal` fixture, wait for the empty dashboard to render, then write a synthetic Claude Code `SessionStart` hook payload (with `pane_id = pane-m2-001`, `session_id = m2demo`, `agent_type = claude_code`) directly to the per-test hook socket. The deck's daemon auto- registers the unknown pane on its first `SessionStart` event, so a card titled `m2demo` should appear on the dashboard within the test budget. No real LLM tokens are spent — the harness injects the event in-process.

## Steps

1. Launch the deck with fixture `minimal`
2. Wait for `No active sessions` to appear on screen
3. Write `to_string(…)` to the hook socket
4. Wait for `m2demo` to appear on screen

## Catalog spec

- **Layer:** L2.
- **Agent:** none (write JSON directly to the per-test hook socket).
- **Asserts:** a card appears for the new `session_id`; status is the post-`SessionStart` resting state per the `state` module.
- **Does not assert:** card position in the grid (covered by `dashboard/pane/001`).
- **Platform coverage:** mac+linux.

## Replay

```sh
asciinema play .dot-agent-deck/m2-recordings/delivery_001_session_start_creates_card.cast
```

## Rerun

```sh
cargo test-e2e delivery_001
```
