<!-- Tiny CATALOG.md fixture for the demo-reel-adapter test. Mirrors the real
     tests/CATALOG.md entry shape (`##### <id> — <headline>`) with four synthetic
     ids. Two dimensions are exercised at once:
       * ORDERING — catalog order (001, 002, …) deliberately differs from the
         order the test feeds ids to `assemble`, so ordering is proven.
       * REEL ELIGIBILITY — the trailing ` [reel]` marker is opt-in. 001/002 are
         MARKED; 003 (L1, cast-less) and 004 (has a cast but UNMARKED) are not, so
         the test proves a marked test is included while an unmarked cast-bearing
         test is excluded and an all-unmarked list clean-skips. -->

# Test-Case Catalog (fixture)

## Test Case Catalog

##### mouse/button/001 — Beta renders its label. [reel]
- **Layer:** L2 (PTY end-to-end).

##### mouse/button/002 — Alpha renders its label. [reel]
- **Layer:** L2 (PTY end-to-end).

##### mouse/button/003 — Gamma renders its label.
- **Layer:** L1 (ratatui `TestBackend` + `insta`).

##### mouse/button/004 — Delta renders its label.
- **Layer:** L2 (PTY end-to-end, but NOT reel-marked).
