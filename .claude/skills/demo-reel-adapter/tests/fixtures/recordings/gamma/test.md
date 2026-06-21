# mouse/button/003 — Gamma renders its label.

**Source:** `tests/render_mouse_button.rs::gamma`
**Catalog:** tests/CATALOG.md

## Scenario

Gamma scenario: an L1 render-only test with a test.md but NO full-stream.cast, so it must be excluded from the reel.

## Steps

1. render the gamma widget to a TestBackend
2. assert against the committed snapshot
