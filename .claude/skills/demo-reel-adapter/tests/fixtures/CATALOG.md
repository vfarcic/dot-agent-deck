<!-- Tiny CATALOG.md fixture for the demo-reel-adapter test. Mirrors the real
     tests/CATALOG.md entry shape (`##### <id> — <headline>`) but with three
     synthetic ids whose catalog order (001, 002, 003) deliberately differs
     from the order the test feeds them to `assemble`, so ordering is proven. -->

# Test-Case Catalog (fixture)

## Test Case Catalog

##### mouse/button/001 — Beta renders its label.
- **Layer:** L2 (PTY end-to-end).

##### mouse/button/002 — Alpha renders its label.
- **Layer:** L2 (PTY end-to-end).

##### mouse/button/003 — Gamma renders its label.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).
