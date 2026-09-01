# mouse/button/004 — Delta renders its label.

**Source:** `tests/e2e_mouse_delta.rs::delta`
**Catalog:** tests/CATALOG.md
**Cast:** `full-stream.cast`

## Scenario

Delta scenario: a PTY-attached L2 test WITH a full-stream.cast, but whose catalog entry is NOT marked `[reel]`, so it must be excluded from the reel despite having a cast.

## Steps

1. start the app
2. assert the delta label is visible
