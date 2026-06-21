# PRD #180: PRD demo reel

**Status**: In Progress
**Priority**: Low
**Created**: 2026-06-21
**GitHub Issue**: [#180](https://github.com/vfarcic/dot-agent-deck/issues/180)
**Related**: PRD #77 (per-test asciinema `.cast` + `final-grid.svg` recording infrastructure under `.dot-agent-deck/recordings/<test>/`, this PRD's raw input), the `#[spec]` / `tests/CATALOG.md` / `cargo xtask docs --tests` pipeline that emits each `test.md` with its `## Scenario` block (the source of the on-screen description), PRD #139 (`experimental` feature flag — explicitly **does not apply**, see Design Decisions)

## Problem Statement

When a PRD's e2e tests pass, the only way to confirm that the implemented behavior actually matches what was intended is to read the test code or replay the per-test asciinema casts one at a time (`asciinema play .dot-agent-deck/recordings/<test>/full-stream.cast`). There is no single, watchable artifact that shows — end to end — what the PRD does. So the most valuable pre-merge check ("watch the new behavior and judge whether it's what I expected") is tedious, manual, and easy to skip, even though the raw material to produce it already exists for every L2 e2e test.

The repo already generates, per L2 e2e test: a plain-English description (`test.md`, with a `## Scenario` paragraph) and a terminal recording (`full-stream.cast`, asciinema v2). What is missing is the last mile: turning those paired artifacts into one continuous "description, then the test running, repeat" reel that a human can watch in a few minutes to accept or reject a PRD, and a place to host/link it so it rides along with the PR.

## Solution Overview

At PRD completion, produce **one narrated MP4** for the PRD: for each e2e test the PRD **adds or changes**, show a title card carrying that test's description (its `## Scenario` text), then play that test's recording, then move to the next test — concatenated in catalog order. The casts are recorded during the pre-PR e2e gate (with `DOT_AGENT_DECK_RECORD=1`); the reel itself is built, uploaded **unlisted to YouTube**, and linked only **after** the PR is open and its CI + Greptile review are green, in the **pre-merge window**. The orchestrator surfaces the link to the human pre-merge and posts it in three places — a PR comment, the PR description, and the changelog fragment (so it flows into the release notes).

The work splits cleanly into a **reusable engine** and a **project-specific adapter**, so the genuinely generic part can later serve other projects (including a separate VHS-based project) unchanged:

1. **Engine — a reusable skill whose payload is standalone scripts.** Input is a *manifest*: an ordered list of `{title, description, clip}` entries where `clip` is an asciinema `.cast` **or** an already-rendered `gif`/`mp4` (format-agnostic on purpose). The engine renders a title/description card per entry, stitches the cards and clips into one MP4, uploads it unlisted to YouTube, and returns the URL. It has no knowledge of Rust, tests, PRDs, or this repo. Because the payload is plain scripts, it is invocable by an agent (via the skill) *and* directly by a human or CI (`reel.sh manifest.json`).

2. **Adapter — stays in dot-agent-deck.** It discovers the work-list and builds the manifest: select the e2e `#[spec]` tests added/changed on the branch, pull each test's title (the `test.md` H1 headline) and description (its `## Scenario` paragraph), order by catalog ID, point each entry at its `full-stream.cast`. It invokes the engine, then wires the returned URL into the orchestrator's pre-merge step and a PR comment. If the branch changed no e2e tests, it skips cleanly (no reel, no upload, no comment).

### Where the on-screen text comes from

Both strings already exist per test in `.dot-agent-deck/recordings/<test>/test.md` — no test-body parsing is needed:

- **Title** ← the `test.md` H1 headline (e.g. `mouse/button/001 — The Button widget renders its inline-shortcut label…`).
- **Description** ← the `## Scenario` paragraph (the 1–3 sentence plain-English summary that CLAUDE.md rule 7 already requires on every `#[spec]` test).

### How the title/description card becomes video

The card is still rendered as a **terminal frame through `agg`** (no separately-designed image), but deliberately **not** at the clip's dimensions. Each card is painted on a **small fixed grid (`CARD_COLS`×`CARD_ROWS`) at a larger `CARD_FONT_SIZE`** than the clips' recorded grid, so the title and description fill the frame and **read bigger** than they would on a clip's wide terminal. The card is therefore **not** pixel-identical to the clips; uniform geometry for `ffmpeg` concat is instead provided by an **`ffmpeg` scale+pad NORMALIZE pass that fits every segment — cards *and* clips — to one common resolution/fps/pixel-format**, and *that* normalize pass is what keeps the concat seamless even though cards and clips render at different sizes. The adapter (or engine, given the text) hand-builds a tiny synthetic asciinema cast: a **bold, bright** title (centered) above a **bright** (not dim) description rendered **one line per sentence / bullet / source line**, left-aligned and vertically centered, so it reads as prose rather than one wrapped wall of text. The card grid is a *minimum* — a long description grows it taller (the generator reports the effective height) so text never clips, and the normalize pass scales/letterboxes the result. Each card's **on-screen hold is a flat, tunable `CARD_HOLD` (default 4s)** enforced at the **`ffmpeg` level** (freeze the single painted still, then loop it to an exact duration), **not** scaled to text length — a short, uniform hold keeps the reel moving and a viewer who wants to dwell on a long card pauses the video.

### How the clips are paced

An e2e cast is recorded at machine speed, so its raw event stream is unwatchable: instantaneous bursts (a keypress and the full repaint it triggers land in the same millisecond) separated by short real waits (daemon startup, polling, debounce). A single global `agg --speed` can't fix that — slowing everything stretches the waits into dead air and still can't *spread* coincident events apart. So each `.cast` clip is run through a **cast re-timer (`retime.sh`) before `agg` renders it**: it rewrites every event's timestamp so typing replays at a readable cadence (`TYPE_GAP`), each operation repaint is held long enough to register (`OP_HOLD`), and dead idle waits are clamped (`IDLE_CAP`). Classification is by output-payload size — a small diff is a typed character, a large repaint is an operation. Clips then render at real time of the *retimed* cast (`CLIP_SPEED` defaults to `1.0`, kept only as an escape-hatch global multiplier layered on top), replacing the old global-speed slowdown idea. Pre-rendered `gif`/`mp4` clips bypass `agg` and the re-timer entirely.

### Recording the passing tests

Casts are only written on test **failure** or when `DOT_AGENT_DECK_RECORD=1` is set (PRD #77). A passing run records nothing by default, so the reel step runs the e2e suite with `DOT_AGENT_DECK_RECORD=1` to populate casts for the *passing* tests it wants to show. This folds into the existing pre-PR e2e gate (CLAUDE.md rule 5) rather than adding a separate run.

## User-facing behavior (documentation-first)

This is developer/release-workflow tooling, so its "users" are maintainers; its documentation lives under `docs/develop/` (CLAUDE.md rule 11), not the published site.

### One-time setup (per environment that will publish)

The reel step needs three CLIs available — `agg` (cast → frames), `ffmpeg` (stitch/encode), and a YouTube uploader (`youtube-upload` or equivalent) — plus a YouTube OAuth **refresh token**. The token requires a one-time Google OAuth client + human consent and is stored via the repo's existing secrets path (`vals` / `.env.vals.yaml`); the agent cannot self-provision it. `agg` and `ffmpeg` are added to `devbox.json`. The engine skill declares and checks these prerequisites and fails with an actionable message if any is missing (it does not self-install them).

### Building a reel locally (engine, direct invocation)

```sh
# manifest.json: [{ "title": "...", "description": "...", "clip": ".dot-agent-deck/recordings/<test>/full-stream.cast" }, ...]
reel.sh manifest.json --out reel.mp4               # stitch only, no upload
reel.sh manifest.json --out reel.mp4 --publish     # stitch + upload unlisted, prints the YouTube URL
```

### At PRD completion (adapter, via the orchestrator)

The reel's casts are recorded during the pre-PR e2e gate (the suite runs with `DOT_AGENT_DECK_RECORD=1`), but the reel is built, uploaded, and linked only **after** the PR is open and its CI + Greptile review are green, and **before** merge. In that pre-merge window the adapter builds the manifest from the branch's new/changed e2e tests and invokes the engine (passing a descriptive composed title) to produce and upload the reel, then:

- **Surfaces the URL to the human pre-merge** — "Demo reel for PRD #NNN: <unlisted YouTube link>. Watch before merging."
- **Posts the link in three places** so it rides along with the PR *and* the release notes: a PR comment, the PR description/body, and an append to the changelog fragment `changelog.d/<prd>.feature.md`. Committing the changelog/PR-body update triggers one final quick CI + review pass before merge. (An unlisted link placed in the public release notes is reachable by anyone who reads the notes — intended.)
- **Skips cleanly** when the branch changed no e2e tests (no reel, no upload, no comment, no changelog link) — and reports that it skipped and why.

## Scope

### In Scope

- A **reusable engine skill** under `.claude/skills/` (project-local for now) whose payload is standalone scripts: manifest (ordered `{title, description, clip}`, `clip` = `.cast` *or* `gif`/`mp4`) → per-entry title/description card → stitched MP4 → unlisted YouTube upload → returned URL. Runnable by the agent and directly by a human/CI.
- **Card rendering** as a terminal frame via `agg` on a small fixed grid at a larger font (synthetic cast: bold/bright centered title + bright, one-line-per-sentence, vertically-centered description; flat tunable `CARD_HOLD` enforced at the `ffmpeg` level, with the grid growing taller for long text so it never clips), with the `ffmpeg scale`+`pad` normalize pass fitting cards and clips to one common resolution/fps/pixel-format.
- **Clip re-timing** (`retime.sh`): rewrite each `.cast`'s event timestamps before `agg` renders it so typing replays at a readable cadence, operation repaints are held, and idle waits are clamped — replacing a blunt global `agg --speed` slowdown. Pre-rendered `gif`/`mp4` clips bypass it.
- **Stitch + encode** via `ffmpeg` into a single MP4 with uniform resolution/fps/pixel-format.
- **Unlisted YouTube upload** returning the video URL; prerequisite/credential checks with actionable failure messages.
- A **dot-agent-deck adapter** that: selects the e2e `#[spec]` tests added/changed on the branch (diff vs `main`), lifts each test's title (`test.md` H1) and description (`## Scenario`), orders by catalog ID, points each entry at its `full-stream.cast`, and builds the manifest. Skips cleanly when there are no in-scope e2e changes.
- **Orchestrator integration**: record casts during the pre-PR e2e gate (`DOT_AGENT_DECK_RECORD=1`), then — after the PR is open and its CI + Greptile review are green, in the pre-merge window — invoke the engine (with a descriptive composed title), surface the URL to the human pre-merge, and post it in three places (PR comment, PR description, changelog fragment).
- **Toolchain**: add `agg` and `ffmpeg` (and the YouTube uploader) to `devbox.json`; document the one-time YouTube OAuth refresh-token provisioning via `vals`.
- **Developer docs** under `docs/develop/` describing the manifest contract, prerequisites/credential setup, how to build a reel locally, and how the orchestrator step behaves — linked from `CONTRIBUTING.md`, not the published site.

### Out of Scope / Non-Goals

- **Promoting the engine skill to user-level (`~/.claude/skills/`) or a plugin/marketplace.** Deliberately deferred: build it project-local here first to prove it end-to-end, promote for cross-project reuse in a follow-up.
- **Covering L1 render tests.** L1 tests have a `test.md` but no PTY recording (no `.cast`), so they cannot be shown as clips; the reel is the L2 e2e subset by construction — which is also the right "user-journey" subset.
- **The whole e2e suite in every reel.** Each PRD's reel shows only the tests that PRD added/changed, so reels stay focused and bounded as the suite grows.
- **A designed/branded card** (custom fonts, logos, marketing layout via Pillow/ImageMagick/`ffmpeg drawtext`). The terminal-style card is the chosen aesthetic; a branded card would add a font/image dependency and manual dimension-matching, and is not pursued.
- **Hosting other than unlisted YouTube** (e.g. GitHub Release asset). Considered; YouTube chosen.
- **Running the reel step in CI.** Recordings are local-only (PRD #77); the reel is produced wherever the pre-PR e2e run happens, not in GitHub Actions.
- **The `experimental` feature flag** (PRD #139). This is dev/release-workflow tooling, not a user-facing TUI surface, so no `features.rs` wrapper and no `graduate-` follow-up apply.

## Design Decisions

1. **Engine/adapter split.** The stitch-and-publish core is generic (its input is a format-standard manifest); the test-selection and orchestrator wiring are inherently coupled to this repo's `#[spec]`/`CATALOG.md`/recordings conventions. Splitting at that seam is what makes the core reusable later without dragging dot-agent-deck specifics along.

2. **Package the engine as a skill with scripts, not a separate repo/CLI.** The consumer is already an agent (the orchestrator generating the reel pre-merge), so a skill *is* the idiomatic integration rather than a CLI the agent shells out to; the work is shell-glue around `agg`/`ffmpeg`/uploader; and a skill is portable across projects without a versioning/CI/distribution pipeline. Keeping the payload as standalone scripts preserves direct human/CI invocability too.

3. **Project-local now, promote later.** Build under `.claude/skills/` in this repo to prove the pipeline end-to-end before committing to a user-level or plugin distribution. Lowest commitment; the engine/adapter seam keeps later promotion cheap. The one current consumer (this repo) does not justify standing up a shared interface up front, and the second known candidate (a VHS-based project) uses a different recording format — which is exactly why the engine's `clip` input is format-agnostic.

4. **Format-agnostic `clip` input.** Accepting an already-rendered `gif`/`mp4` in addition to a `.cast` lets a VHS-based project feed the same engine directly later. Cast→clip rendering (`agg`) is then just one optional front-end, not a hard-wired assumption.

5. **Render the card as a terminal frame via `agg`, on its own small grid, and normalize to concat.** The card is still painted through `agg` (not a separately-designed image), which keeps word-wrap/centering/styling trivial (we place every cell), adds no font/image dependency, and looks cohesive with the recordings. But it is rendered on a **small fixed grid at a larger font**, *not* at the clip's cols/rows, so the text reads bigger than the wide terminal would allow — it is therefore **not** pixel-identical to the clips. Uniform resolution/fps/pixfmt for `ffmpeg` concat is instead guaranteed by the **`scale`+`pad` normalize pass** that fits every segment (cards *and* clips) to one common target, which also backstops any clip recorded at a different terminal size. A separately-designed image would reintroduce a font/image dependency for no benefit to a faithful "here's the terminal doing the thing" record. (The original plan rendered the card at the clip's exact cols/rows for pixel-identity; the bigger-card-plus-normalize approach shipped instead because it makes the on-screen text far more readable.)

6. **Scope each reel to the branch's new/changed e2e tests.** A per-PRD reel should show what *that PRD* does; including the whole suite would grow unboundedly and re-show unchanged behavior every time. Selection diffs the branch's `#[spec]` e2e tests against `main`.

7. **Record passing tests via `DOT_AGENT_DECK_RECORD=1` inside the existing pre-PR e2e gate.** Casts only dump on failure by default; the reel needs the *passing* runs. Folding the recording flag into the pre-PR e2e run (CLAUDE.md rule 5) avoids a second suite run.

8. **One-time human prerequisite for YouTube, stored in `vals`.** The Data API needs OAuth user consent (a service account can't cleanly upload to a personal channel), so a refresh token must be minted once by a human and stored via the repo's `vals`/`.env.vals.yaml` path. The engine checks for it and fails actionably rather than attempting to self-provision.

9. **Not behind the `experimental` flag.** Per CLAUDE.md rule 9 the flag gates user-facing TUI surfaces; this feature adds none. It ships as normal maintainer tooling.

10. **Re-time each cast instead of a global `agg --speed`.** Machine-speed e2e casts have a pathological cadence — coincident bursts (keypress + repaint in the same millisecond) separated by short real waits — that no single playback-speed multiplier can fix: slowing everything stretches the waits into dead air and still can't spread coincident events apart. So a dedicated re-timer (`retime.sh`) rebuilds each cast's timeline from the event payloads (classifying by payload size: small diff = typed char, large repaint = operation), spacing typing to a readable cadence, holding each operation repaint (`OP_HOLD`), and clamping idle waits (`IDLE_CAP`). The engine then renders the retimed cast at speed `1.0`. `CLIP_SPEED` survives only as an escape-hatch global multiplier; gif/mp4 clips bypass the re-timer.

11. **Flat card hold at the `ffmpeg` level, not scaled to text.** The on-screen card hold is a flat, env-tunable `CARD_HOLD` (default 4s) applied by freezing the painted still and looping it to an exact duration in `ffmpeg`, rather than encoded in the synthetic cast's timeline (where `agg` would collapse a static tail) or scaled to the description length (the earlier plan). A short, uniform hold keeps the reel moving; a viewer who wants to read a long description pauses the video. The rendered content-line count still drives the card *grid height* so long text doesn't clip — only the hold stopped depending on it.

## Success Criteria

- Running the reel step on a branch that added/changed e2e tests produces a single MP4 in which, for each in-scope test, a readable title/description card (its `## Scenario`) is shown, immediately followed by that test's recording, in catalog order — with no resolution/format seams between segments.
- The reel is uploaded **unlisted to YouTube** and the URL is returned; the orchestrator surfaces it to the human pre-merge and posts it as a PR comment.
- On a branch that changed **no** e2e tests, the step skips cleanly — no reel, no upload, no comment — and reports that it skipped and why.
- The engine runs both as a skill (agent-invoked) and as a direct script (`reel.sh manifest.json`) with the same result, and accepts a `.cast` or a pre-rendered `gif`/`mp4` as a clip.
- Missing prerequisites (`agg`/`ffmpeg`/uploader/OAuth token) produce an actionable failure message rather than a partial or silent result.
- The card text is sourced from `test.md` (H1 title + `## Scenario`) with no test-body parsing; each card holds a flat, tunable `CARD_HOLD` that keeps the reel moving (a viewer pauses to dwell on a long one) and its grid grows taller so longer descriptions never clip.
- Developer docs under `docs/develop/` (linked from `CONTRIBUTING.md`, absent from the published site and `site/sidebars.js`) describe the manifest contract, prerequisite/credential setup, local usage, and the orchestrator step.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, and `cargo test-fast` pass; `cargo test-e2e` passes before the PR (CLAUDE.md rules 2 & 5).

## Milestones

### Phase 1 — Engine (reusable stitch + publish skill)

- [x] **M1.1** — Engine skill scaffold under `.claude/skills/` with standalone scripts and the manifest contract (`{title, description, clip}`, `clip` = `.cast` | `gif` | `mp4`); prerequisite checks (`agg`, `ffmpeg`, uploader, OAuth token) that fail with actionable messages.
- [x] **M1.2** — Card rendering: synthetic-cast generator (bold/bright centered title + bright, one-line-per-sentence, vertically-centered description) rendered via `agg` on a small fixed grid at a larger font; flat tunable `CARD_HOLD` enforced at the `ffmpeg` level (freeze still + loop to exact duration); `ffmpeg scale`+`pad` normalize pass fits cards and clips to one common resolution. Clip pacing via the cast re-timer (`retime.sh`). Verified cards concat seamlessly with real clips.
- [x] **M1.3** — Stitch + encode all cards and clips into one uniform MP4 (`ffmpeg`); unlisted YouTube upload returning the URL. End-to-end on a hand-written manifest.

### Phase 2 — Adapter (dot-agent-deck manifest builder)

- [x] **M2.1** — Select the e2e `#[spec]` tests added/changed on the branch (diff vs `main`); lift each test's title (`test.md` H1) and description (`## Scenario`); order by catalog ID; emit a manifest pointing at each `full-stream.cast`. Clean skip when there are no in-scope e2e changes.
- [x] **M2.2** — Toolchain + secrets: add `agg` and `ffmpeg` (and the uploader) to `devbox.json`; document and validate the one-time YouTube OAuth refresh-token provisioning via `vals`/`.env.vals.yaml`.

### Phase 3 — Orchestrator integration

- [x] **M3.1** — Orchestrator flow: record casts during the pre-PR e2e gate (`DOT_AGENT_DECK_RECORD=1`), then build the reel in the **pre-merge window** — after the PR is open and its CI + Greptile review are green — via the adapter (M2.1) + engine (M1.3), with a descriptive composed title (`<repo> · PRD #<prd> · PR #<pr> — <desc>`). Surface the URL to the human pre-merge and post it in three places: a PR comment, the PR description/body, and the changelog fragment `changelog.d/<prd>.feature.md` (so it flows into the release notes); the changelog/PR-body commit triggers one final quick CI + review pass before merge. Report a clean skip (no reel, no URL, no comment, no changelog link) when no e2e tests changed.

### Phase 4 — Docs & release gate

- [ ] **M4.1** — Developer docs under `docs/develop/` (manifest contract, prerequisite/credential setup, local usage, orchestrator behavior), linked from `CONTRIBUTING.md`, excluded from the published site; changelog fragment via `dot-ai-changelog-fragment`.
- [ ] **M4.2** — Pre-PR gate: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test-fast`, and `cargo test-e2e` green; review (Greptile) settled per CLAUDE.md rule 8.

## Risks & Mitigations

- **Segments won't concat (resolution/fps/pixfmt mismatch).** Cards render on their own small grid and clips at their recorded grid, so they are *not* the same size by construction; the `ffmpeg scale`+`pad` normalize pass fits every segment (cards and clips) to one common resolution/fps/pixfmt — which is what makes the concat seamless, and also backstops any clip recorded at a different terminal size.
- **Reels are slow/boring because e2e tests contain waits.** Each `.cast` clip is run through the re-timer (`retime.sh`) before rendering — typing is spaced to a readable cadence, each operation repaint is held (`OP_HOLD`), and dead idle waits are clamped (`IDLE_CAP`); `agg --idle-time-limit` caps any remaining gaps and the card hold is a flat, bounded `CARD_HOLD`. `CLIP_SPEED` remains as an escape-hatch global multiplier.
- **YouTube OAuth is the brittle part.** It needs one-time human consent and a stored refresh token; the engine checks for the token and fails actionably. Hosting is isolated behind one upload script, so swapping hosts later (or running stitch-only) does not touch the rest.
- **Casts aren't recorded on a passing run.** The reel step explicitly runs e2e with `DOT_AGENT_DECK_RECORD=1`; without it the manifest builder finds no casts and the step skips with a clear message rather than producing a broken reel.
- **Per-environment toolchain drift (`agg`/`ffmpeg` absent).** Added to `devbox.json`; the engine's prerequisite check names exactly what's missing.
- **Channel clutter / privacy.** Uploads are unlisted; promotion to a shared distribution and any retention policy are deferred follow-ups, not v1 concerns.
- **Premature generalization.** The engine is built project-local with a deliberately small, format-standard manifest contract; promotion to user-level/plugin waits for a real second consumer.
