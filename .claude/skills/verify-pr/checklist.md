# verify-pr review rubric

Read this during Phase 3, with the diff open. Every item is either a repo rule from `CLAUDE.md` / `CONTRIBUTING.md` that automation cannot check, or a failure mode this repo has actually shipped. Skip items whose bucket the PR does not touch — `scan.sh` tells you which buckets are present.

Cite findings as `file_path:line`. For each finding, decide and record: **blocking** (contributor must fix before merge), **follow-up** (real, file an issue, merge anyway), or **nit** (mention, no action).

## A. Does it do what it says

- [ ] The diff matches the PR description and the linked issue/PRD. Anything extra is either called out in the description or is a finding.
- [ ] No unrelated drive-by changes riding along — reformatting, dependency bumps, renamed symbols outside the stated scope.
- [ ] Nothing was silently **removed**. Deleted tests, dropped `#[spec]` annotations, and orphaned `tests/CATALOG.md` entries are the specific shape that has broken `main` here before: a revert removed two `#[spec]` tests, left their catalog entries dangling, and linkage-check went red on `main` because nothing ran it. Diff `git log --diff-filter=D` and check test counts before/after.
- [ ] If it closes a PRD, the PRD's acceptance criteria are actually met, not just its checkboxes ticked.

## B. Tests — rule 4's ladder

Applies when the diff changes user-visible TUI behaviour (panes, statuses, prompts, focus, layout, modes, embedded panes, hook delivery). Pure refactors with no observable behaviour change need no new tests.

- [ ] L1 (`TestBackend` + `insta`) for pure widget/layout changes; L2 (`e2e_*.rs`, `#[cfg(feature = "e2e")]`, PTY + vt100) when the change touches the spawned binary, daemon, hooks, attach protocol, or a real agent.
- [ ] For a **major user-facing feature**: at least one **PTY-attached L2** test, not only a headless daemon-serve (`RunNow`) test. PRD #120 is the cautionary case — `scheduler/dispatch/001-010` were all headless.
- [ ] For a **major user-facing feature**: at least one **real-agent** test on a cheap model (Haiku / a mini model), driving the genuine spawn→agent→work path, with a directive prompt plus a uniquely-named **sentinel file** so the assertion survives LLM phrasing variance. A stand-in (`cat`, scripted echo, synthesized hook events) proves plumbing, never that an agent runs against the cloned/worktree state.
- [ ] The bar: **at least one test validates the feature as a user actually uses and sees it** — a live interactive agent working in the pane with real status. `claude -p` print mode and stand-ins do not clear this bar. `scheduler/dispatch/013` is the reference implementation; "headless is hard" is not an accepted reason where it is achievable.
- [ ] `[reel]` marker on the `tests/CATALOG.md` entry only if the clip genuinely spins up a real agent. A cast alone does not make a clip demo-reel-eligible.
- [ ] Rule 3: no milestone/PRD prefixes in new filenames (`tests/event_forwarding.rs`, never `tests/m2_17_event_forwarding.rs` or `src/prd76_*.rs`).
- [ ] Rule 7: every new/modified `#[spec(...)]` test carries a 1–3 sentence `/// Scenario:` doc comment. `linkage-check` enforces this, but read the comment anyway — a comment that does not describe what the test does passes the gate and fails the reader.
- [ ] Test names follow `<sub-area>_<NNN>_<suffix>` (Decision 17) so rule 6's single-test filter picks exactly one.

## C. Snapshots

- [ ] Read every changed `.snap` under `tests/snapshots/` as a rendered screen — each line is one row of the parsed dashboard grid. Accept only if the new rendering matches the catalog entry's prose. A snapshot updated to match a regression is how a visual bug lands green.

## D. Rule 12 — the cross-version contract

Applies when the diff touches the daemon, the TUI↔daemon protocol, orchestration, or hooks (`scan.sh` reports `RULE_12_TRIGGERED=true`).

- [ ] Answer explicitly: **did this change the TUI↔daemon contract?** "Breaking" here means an older and a newer build can no longer interoperate — including a **semantic** break behind a stable wire (a field whose meaning shifts, a role-map value type that changes). It does not mean generic user-facing breakage.
- [ ] Wire *shape* moved → `PROTOCOL_VERSION` bumped in `src/daemon_protocol.rs`.
- [ ] Same wire, different meaning → a `changelog.d/<issue>.breaking.md` fragment exists so it is versioned as a compatibility break.
- [ ] The cross-version manual test was run, or is recorded as unverified: previous-release daemon with an agent under it, branch TUI against it, confirm a **delegate** still routes and **hooks** (work-done, status) still arrive.
- [ ] Bump policy respected — while `0.x`, breaking → minor, feature/bugfix → patch. See `docs/develop/versioning.md`.

## E. Rule 9 — the experimental flag

- [ ] A new user-visible surface (pane, field, command, tab, footer, keybinding) either ships behind `experimental` or was a deliberate visible-by-default decision.
- [ ] If gated: exactly **one** wrapper in `src/features.rs` (`show_<feature>()`), and it is checked **only at the render / input-binding seam**. Business logic, daemon protocols, and hook handling must not branch on the flag — it is a presentation switch (M3.2).
- [ ] The flag is noted in the PRD, the changelog fragment, and `docs/develop/experimental-flag.md`, and a `graduate-<feature>` follow-up issue exists.

## F. Docs and changelog

- [ ] Rule 10: prose paragraphs are single lines. A PR that hard-wraps at 72/80 columns changes nothing in the rendered output and makes every future diff noisier.
- [ ] Rule 11: contributor-facing docs are under `docs/develop/` and are **not** added to `site/sidebars.js`; user-facing docs are under `docs/` and are listed there. Dev docs omit Docusaurus-only frontmatter (`sidebar_position`).
- [ ] A `changelog.d/<issue>.<type>.md` fragment exists for anything user-visible, with the right type (`breaking` / `feature` / `bugfix`).
- [ ] User-facing behaviour changes are reflected in the published docs, not only in the PRD.

## G. Dependencies

- [ ] Every **exact pin** in `Cargo.toml` carries a comment explaining why. A PR that bumps one must have done the work that comment demands — for `crossterm = "=0.29.0"` that means re-verifying the `parse.rs` line references in `ui::ctrl_c0_byte`, because the encoder must stay that decoder's exact inverse. A silent pin bump is blocking.
- [ ] `portable-pty`'s pin comment documents a Windows ConPTY stall (`ESC[6n`). Same rule.
- [ ] New dependencies: justified, maintained, licence-compatible, and not a second crate doing a job an existing dependency already does.
- [ ] `cargo audit` clean, or the advisory is understood and accepted.

## H. Code quality

- [ ] Errors handled, not `unwrap()`-ed on paths reachable from user input or IPC.
- [ ] New code reads like its neighbours — same comment density, naming, and idiom. This codebase comments the *why* (often at length) where a decision is non-obvious; a subtle change with no explanation is a finding.
- [ ] Concurrency: no blocking call on the async runtime, no lock held across `await`.
- [ ] Rendering changes respect the four invariants in `docs/develop/rendering-contract.md` (single layout pass, layout-driven PTY size, 1:1 widget render, fixed resize sequencing).
- [ ] `#[cfg(unix)]` used where Unix-only APIs appear, at the right granularity (per-item vs file-level — see `docs/develop/windows-cross-check.md`).

## I. Security — for any PR you did not write

- [ ] `.claude/**`: hooks, settings, and skills execute on **your** machine. A modified `.claude/settings.json` hook runs as you, with your credentials, as soon as you work in that worktree. Read every line; a hook addition in an otherwise-innocent PR is blocking until explained.
- [ ] `.github/workflows/**`, `.github/actions/**`: workflow changes run in CI with repository secrets. Check for added `pull_request_target`, new `permissions:`, secret echoes, curl-to-shell, and third-party actions pinned to a mutable tag instead of a SHA.
- [ ] `build.rs`, `.cargo/config.toml`, `xtask/**`, `scripts/**`, `devbox.json`: all execute during an ordinary `cargo build` / `cargo xtask` / devbox shell. Network access or writes outside the target dir are findings.
- [ ] Secrets: nothing added to the diff; nothing new logged. This project's tests import host `claude`/`codex` credentials into a per-test HOME — code that reads those paths deserves a hard look.
- [ ] No new outbound network calls to hosts the project does not already talk to.

## J. What the local run cannot tell you

State these explicitly in the report rather than letting them read as verified:

- **macOS and Windows** — `checks.sh`'s `windows-cross` step is a type-check only (and fails outright without an MSVC cross-toolchain), and there is no local macOS proxy. Both are CI-only signal, which is why Phase 1b exists: `build-macos` and `build-windows` each run a real build + clippy + `cargo nextest run` on the real OS. If those runs were never released, this is unverified, not merely partial. A macOS-only break is not hypothetical: the `libc` `openpty`/`ioctl` pointer-type differences in `src/wrap.rs` broke the v0.34.0 release build.
- **Real-agent coverage that skipped** — see `e2e-skips.txt`. Skipped real-agent tests count as PASSED.
- **The rule 12 manual cross-version test**, unless you actually ran it.
- **Flake vs defect** — a single e2e failure is not a verdict until it has been rerun in isolation (rule 6).
- **No red test is left merely tracked** (#908) — the lane must be green before the PR is done: each failure is either fixed in the PR or quarantined with a named owner and an expiry issue.
