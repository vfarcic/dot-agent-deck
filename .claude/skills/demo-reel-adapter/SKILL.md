---
name: demo-reel-adapter
description: dot-agent-deck-specific adapter that builds a demo-reel manifest.json from this repo's per-test recordings — selects the e2e #[spec] tests added/changed on the branch (diff vs main) that are explicitly marked reel-eligible (` [reel]` in tests/CATALOG.md), lifts each test's title/description from its test.md, orders by catalog id, points each entry at its full-stream.cast, and invokes the repo-agnostic demo-reel engine. Clean-skips when no eligible e2e tests changed. Use when asked to build the PRD demo reel for this repo.
---

# Demo Reel adapter (dot-agent-deck)

The **adapter** is the repo-specific half of PRD #180. It discovers the work-list and builds a `manifest.json`, then hands it to the repo-agnostic **engine** ([`demo-reel`](../demo-reel/SKILL.md), `reel.sh`). The engine renders the cards, stitches the MP4, and (with `--publish`) uploads it — it knows nothing about Rust, `#[spec]`, `tests/CATALOG.md`, or `.dot-agent-deck/recordings/`. The **only** contract between the two is the manifest:

```json
[{ "title": "...", "description": "...", "clip": "<path-to-.cast|.gif|.mp4>" }, ...]
```

Everything dot-agent-deck-specific (which tests, where their title/description live, the catalog ordering) lives here; nothing of it leaks into the engine.

## Usage

```sh
# Default: select in-scope e2e tests, build the manifest, invoke the engine.
.claude/skills/demo-reel-adapter/build.sh                         # stitch only
.claude/skills/demo-reel-adapter/build.sh --out reel.mp4 --publish  # stitch + upload
```

| Command | What it does |
| --- | --- |
| `build.sh [reel] [--out OUT.mp4] [--publish] [--manifest PATH] [--title TITLE]` | Full pipeline: **select** → **assemble** → invoke the engine, forwarding `--out`/`--publish` plus a composed `--title`. Clean-skips (no manifest, no engine, exit 0) when no e2e tests changed. `--manifest` sets where `manifest.json` is written (default `manifest.json` in CWD). `--title` overrides the composed title verbatim (see **Title composition**). |
| `build.sh title [--title TITLE]` | Print the title the `reel` pipeline would pass to the engine on the current branch — the composed title, or `--title` verbatim. Dry-run: no selection, no manifest, no engine, no upload. |
| `build.sh select` | Print the in-scope recording-dir IDs, one per line (the git-diff half — concern **a**). |
| `build.sh assemble [ID...] [--manifest PATH]` | Build `manifest.json` from an explicit list of recording-dir IDs (the pure half — concern **b**; no git, no network). Excludes cast-less IDs **and** IDs whose catalog entry lacks the ` [reel]` marker, orders by catalog id, clean-skips an empty/all-ineligible list. |

Run the full `reel` pipeline from the repo root so the default relative paths (`.dot-agent-deck/recordings`, `tests/CATALOG.md`) resolve. The engine resolves `clip` paths relative to its own CWD, so it is invoked from the same directory.

## Title composition

The engine names the uploaded video after its `--out` basename unless given a `--title`; the engine is repo-agnostic and has no notion of a PRD, so the adapter composes a descriptive title and forwards it. The format is:

```text
<repo> · PRD #<prd> · PR #<pr> — <short desc>
```

for example `dot-agent-deck · PRD #180 · PR #182 — PRD demo reel`. Each piece is derived from the repo and the current branch:

| Piece | Source |
| --- | --- |
| `<repo>` | basename of the `origin` remote URL, minus a trailing `.git`. |
| `<prd>` | the digits after the leading `prd-` in the current branch name (e.g. `prd-180-…` → `180`). |
| `<pr>` | the open PR number for the branch (`gh pr view --json number`). **Omitted** (the whole ` · PR #<pr>` segment) when there is no open PR yet — no error. |
| `<short desc>` | the H1 of `prds/<prd>-*.md`, stripped of a leading `PRD #<n>:` prefix (e.g. `# PRD #180: PRD demo reel` → `PRD demo reel`). Falls back to `demo reel` if no PRD heading is found. |

Composition degrades gracefully — a missing repo/PRD/PR drops only its own segment — so it never errors. Pass `--title "…"` to override the whole thing verbatim; this is needed for manual/dogfood runs where the branch/PRD don't match the clips being stitched. Inspect what would be used without publishing via `build.sh title`.

## Selection rule (concern a)

`select` lists the recording dirs under `.dot-agent-deck/recordings/<id>/` that are **in scope** for this branch's reel. File-level granularity; robustness over cleverness. A dir is in scope **iff all three** hold:

1. **It contains a `full-stream.cast`** — the e2e proxy. The `cargo xtask docs` generator writes a `test.md` for *every* `#[spec]` test but emits a cast only for **L2** tests; **L1** render tests have a `test.md` and **no** cast, so they are excluded by construction (which is also exactly the right "user-journey" subset). Casts are local-only (PRD #77) and only written on failure or under `DOT_AGENT_DECK_RECORD=1`, so the reel step runs the relevant e2e tests with that flag first; without casts, every dir fails this check and the step clean-skips. That run is LOCAL and is not discharged by CI — CI's e2e lanes (CLAUDE.md rule 5) set no such flag and upload no casts — and since issue #502 removed the pre-PR full-tier obligation there is no longer an unfiltered run to piggyback on, so record the tests the branch adds or changes by filter.
2. **Its catalog entry carries the ` [reel]` eligibility marker** (see
   [Reel-eligibility marker](#reel-eligibility-marker-real-user-facing-usage-only)
   below). Eligibility is **opt-in**: a cast alone means the test is PTY-attached,
   not that it belongs in the reel, so an unmarked test is excluded even with a
   cast and a changed source.
3. **Its source file changed on this branch vs `origin/main`.** Each `test.md`
   carries a `**Source:** `<dir>/<file>::<fn>`` line. The file is matched **by
   basename** against `git diff --name-only origin/main` restricted to `*.rs`.
   `select` first does a best-effort `git fetch origin main` so the diff is
   against the true remote tip, not a stale local `main`. Basename matching
   sidesteps the `test.md` `<immediate-parent>/<file>` path quirk and is robust
   for the flat `tests/*.rs` (and `src/*.rs`) layout this repo uses.

> The recording dir is named after the test **function** (e.g. `mytest`), while the **catalog id** (e.g. `mouse/button/001`) lives in the test.md H1 — the two are not the same string, which is why ids for ordering are read from the H1, not the dir name.

## Reel-eligibility marker: real user-facing usage only

A cast just means a test is PTY-attached; it does **not** mean it belongs in the reel. A clip exists so a human can *watch and validate real behavior* — so a test that drives the feature under a **test-only artifice** must **not** become a clip, because the viewer would be validating a fiction. Eligibility is therefore **opt-in and explicit**: a test is a reel candidate only when an author has marked it, and only if it exercises the feature **the way a user actually runs it** — a real agent genuinely spinning up (spawn → agent → work). Never mark a synthetic/stand-in test: `cat`, scripted echo, recorder-stub binaries, terminal-probe, or synthesized/fake hook events. Concretely, a marked test must not rely on:

- non-representative CLI flags a user would never pass (e.g. `pi --no-builtin-tools`, or tool allow/deny-lists that force a particular code path);
- stand-in binaries (`cat`, echo scripts) standing in for a real agent;
- delivering a prompt as a command-line argument when production delivers it by **injection** — the pane must be seeded the way the daemon does it (`write_to_pane_and_submit`), not `agent … '<prompt>'`.

If a feature can only be *proven* under such an artifice, split it: a **real-usage** test for the reel plus a separate **headless** (non-recorded) test for the forensic proof. This applies CLAUDE.md rule 4's "validate it AS A USER ACTUALLY USES AND SEES IT" bar at the clip-selection boundary.

### The marker: a trailing ` [reel]` on the catalog line

The marker is a small trailing tag on the test's `##### <id> — <headline>` line in `tests/CATALOG.md` — the same line the adapter already parses for ordering, so no gitignored artifact and no Rust macro change are involved:

```text
##### codex/live/001 — A real interactive cheap-model Codex run … reports live status (PRD #20). [reel]
```

- **Default is NOT eligible.** A line with no ` [reel]` (or an id absent from the catalog) is never selected, so an unmarked/artifice test can never *auto*-select as a clip even with a cast and a changed source.
- **Both concerns enforce it.** `select` (concern a) and `assemble` (concern b) each drop an unmarked id, so an injected id list can no more smuggle an unmarked test in than a cast-less one.
- **The marker never reaches the card.** `cargo xtask docs` copies the catalog headline (marker and all) verbatim into `test.md`'s H1, so the adapter strips a trailing ` [reel]` when lifting the title — the card shows clean text.
- One `##### <id>` catalog line can back **several** recording dirs (two test functions sharing one catalog id); marking the single line makes all of them eligible.

## Recording discipline for a `[reel]`-marked test

Marking a test `[reel]` means its cast will be **published as video**, so the recording itself has to be publishable. Two constraints come from the asciinema format and the reel's frame, and neither is fixable downstream — the engine can only render what the cast contains. Both were learned the hard way on PRD #339, whose first reel ([`HYXKJokZ8JI`](https://youtu.be/HYXKJokZ8JI)) was unwatchable.

### Never resize the terminal mid-recording

An asciinema v2 cast stores **one** terminal size, in its header, and the format has **no resize event**. A recorder that is resized mid-session writes the **final** size — so every earlier, wider frame in the stream is replayed into a narrower grid and **hard-wraps into garbage**. PRD #339's cast declared 60 columns while its earlier frames addressed column 68, and the resulting clip was illegible.

To demonstrate width-dependent behaviour, change the **app's own layout inside a fixed terminal** — open a pane, add cards, toggle the layout, narrow a card by adding a sibling — rather than changing the terminal. The app reflowing at a constant terminal size is both a valid demonstration and a recordable one. (The engine warns when a cast addresses a column beyond its header width, but by then the recording already has to be redone.)

### Record at laptop-ish proportions, roughly 16:9

The reel's canvas is a fixed landscape 16:9 frame and segments are **fit into it, never cropped**, so a portrait cast can only ever occupy a centre strip with black bars either side. Character cells are roughly **1:2.3**, so ~16:9 means about **4x as many columns as rows**:

| Terminal grid | Rendered aspect | Covers (of a 1920x1080 frame) |
| --- | --- | --- |
| 60x50 | 0.52:1 | **29%** — a tall centre strip. PRD #339's first recording. |
| 80x24 | 1.41:1 | 79% |
| **68x16** | **1.77:1** | **99%** |
| **200x50** | **1.70:1** | **95%** |

Pick the grid for what the scenario needs to show, then keep the ratio near 4:1. The engine warns when a clip is more than 35% off the canvas aspect, naming the coverage it will get.

## Assembly rule (concern b)

`assemble` is pure: given a list of recording-dir IDs it reads only `test.md` and `CATALOG.md` (no test-body parsing, no git, no network) and emits the manifest:

- **title** ← the `test.md` **H1** line, minus the leading `# ` and a trailing
  ` [reel]` marker (e.g. `mouse/button/001 — Beta renders its label.`).
- **description** ← the `## Scenario` paragraph(s), blank lines dropped and
  collapsed to a single line.
- **catalog id** (for ordering only) ← the part of the H1 **before the first
  ` — `** (em dash).
- **clip** ← `<recordings>/<id>/full-stream.cast`.
- Any ID lacking a `full-stream.cast` is **excluded** (the same L1 guard as
  selection, applied at assembly so an injected list can't smuggle an L1 test in).
- Any ID whose catalog entry lacks the ` [reel]` marker is **excluded** (the same
  eligibility guard as selection, applied at assembly so an injected list can't
  smuggle an unmarked test in).
- Entries are **ordered by catalog id's line position in `CATALOG.md`** (the
  authoritative order); an id absent from the catalog sorts last.
- **Clean skip:** if no ID resolves to a reel-eligible e2e clip, it prints
  `skipped: no e2e tests changed on this branch`, writes **no** manifest, and
  exits 0.

Splitting selection (a) from assembly (b) is deliberate: (b) is fully deterministic and fixture-testable without git or the network, which is what the acceptance test below exercises.

## Environment overrides

All paths default to this repo's layout and are overridable (the test uses this to point at fixtures):

| Var | Default |
| --- | --- |
| `REEL_ADAPTER_RECORDINGS_DIR` | `.dot-agent-deck/recordings` |
| `REEL_ADAPTER_CATALOG` | `tests/CATALOG.md` |
| `REEL_ADAPTER_MAIN_REF` | `origin/main` |
| `REEL_ADAPTER_ENGINE` | `<skill>/../demo-reel/reel.sh` |

## Acceptance test

A **re-runnable, pure-shell** test (no `agg`/`ffmpeg`, no git, no network — so it **may** run in CI, unlike the engine smoke and the reel step itself) drives the deterministic `assemble` path against a tiny fixture (`tests/fixtures/recordings/` with two `[reel]`-marked e2e dirs that have casts, one L1 dir with no cast, and one cast-bearing dir that is **not** marked, plus a `CATALOG.md` fixture). It asserts:

1. given `alpha beta gamma delta`, the manifest has the right
   titles/descriptions/clip paths **in catalog order** (`beta`=001 before
   `alpha`=002), **excludes** the cast-less L1 `gamma`, and **excludes** the
   cast-bearing but unmarked `delta`;
2. given an empty list, it **clean-skips** — no manifest, exit 0, skip message —
   and likewise for an L1-only list and for an unmarked-cast-only list.

```sh
task reel-adapter-test
# or directly:
.claude/skills/demo-reel-adapter/tests/adapter_test.sh
```
