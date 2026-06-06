# dashboard/pane/004 — Card title row carries card number, display name, and a status badge.

**Source:** `tests/render_dashboard.rs::pane_004_card_title_row`
**Catalog:** PRD #77 `## Test Case Catalog`

## Scenario

Render a single dashboard card for a Working agent session (with a Read tool active and a recent user prompt) into a `ratatui::TestBackend` buffer at 80 columns × Normal-density height, then snapshot the buffer with `insta`. The card title row should carry the card number (1), the display name (`example-coder`), and the `● Working` status badge — and the stats line should show the wide layout's inline `Last: … Tools: …` because 80 cells crosses the wide-layout width threshold.

## Steps

1. Call: working_session_fixture()
2. Call: resolve_palette(`Dark`)
3. Call: rendered_height(`wide`)
4. Render the session card into a `ratatui::TestBackend` buffer
5. Snapshot the rendered buffer (insta)

## Catalog spec

- **Layer:** L1 (ratatui `TestBackend` + `insta`).
- **Agent:** none.
- **Asserts:** rendered card buffer matches the committed snapshot for a single Working session in the Normal density.
- **Does not assert:** pane content; this is a card layout snapshot only.
- **Platform coverage:** mac+linux+windows.

## Rerun

```sh
cargo test-fast pane_004
```
