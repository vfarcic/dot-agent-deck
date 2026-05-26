# M2 validation recordings

Captured artifacts for the two seed tests delivered by PRD #77 milestone M2 (see `prds/77-tui-testing-harness.md`). Both let the reviewer replay or inspect what the tests assert against without re-running the suite.

## L2 — `hooks/delivery/001`

```sh
asciinema play .dot-agent-deck/m2-recordings/delivery_001_session_start_creates_card.cast
```

Recorded with `DOT_AGENT_DECK_RECORD=1` (Decision 28). Plays back the deck's full PTY stream from launch through the SessionStart hook injection.

## L1 — `dashboard/pane/004`

The L1 test is a single in-process render against a `ratatui::TestBackend`; there is no PTY stream and no `full-stream.cast` shape that would apply. `pane_004_card_title_row.snap` is the committed `insta` snapshot the test compares against — the same file lives at `tests/snapshots/render_dashboard__pane_004_card_title_row.snap`.
