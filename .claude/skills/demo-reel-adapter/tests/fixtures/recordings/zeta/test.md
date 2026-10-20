# mouse/button/006 — Zeta renders its label.

**Source:** `tests/e2e_mouse_zeta.rs::zeta`
**Catalog:** tests/CATALOG.md
**Cast:** `full-stream.cast`

## Scenario

Zeta scenario: an UNMARKED PTY-attached L2 test whose recording dir holds only this test.md, so recording it would not make it a clip and it must be reported as unmarked, not as unrecorded.

## Steps

1. start the app
2. assert the zeta label is visible
