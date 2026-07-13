---
name: demo-reel-adapter
description: dot-agent-deck-specific adapter that builds a demo-reel manifest.json from this repo's per-test recordings — selects the e2e #[spec] tests added/changed on the branch (diff vs main), lifts each test's title/description from its test.md, orders by catalog id, points each entry at its full-stream.cast, and invokes the repo-agnostic demo-reel engine. Clean-skips when no e2e tests changed. Use when asked to build the PRD demo reel for this repo.
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
| `build.sh assemble [ID...] [--manifest PATH]` | Build `manifest.json` from an explicit list of recording-dir IDs (the pure half — concern **b**; no git, no network). Excludes cast-less IDs, orders by catalog id, clean-skips an empty/all-L1 list. |

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

`select` lists the recording dirs under `.dot-agent-deck/recordings/<id>/` that are **in scope** for this branch's reel. File-level granularity; robustness over cleverness. A dir is in scope **iff both** hold:

1. **It contains a `full-stream.cast`** — the e2e proxy. The `cargo xtask docs`
   generator writes a `test.md` for *every* `#[spec]` test but emits a cast only
   for **L2** tests; **L1** render tests have a `test.md` and **no** cast, so they
   are excluded by construction (which is also exactly the right "user-journey"
   subset). Casts are local-only (PRD #77) and only written on failure or under
   `DOT_AGENT_DECK_RECORD=1`, so the reel step runs the e2e suite with that flag
   first; without casts, every dir fails this check and the step clean-skips.
2. **Its source file changed on this branch vs `main`.** Each `test.md` carries a
   `**Source:** `<dir>/<file>::<fn>`` line. The file is matched **by basename**
   against `git diff --name-only main` restricted to `*.rs`. Basename matching
   sidesteps the `test.md` `<immediate-parent>/<file>` path quirk and is robust
   for the flat `tests/*.rs` (and `src/*.rs`) layout this repo uses.

> The recording dir is named after the test **function** (e.g. `mytest`), while the **catalog id** (e.g. `mouse/button/001`) lives in the test.md H1 — the two are not the same string, which is why ids for ordering are read from the H1, not the dir name.

## Reel-eligibility contract: real user-facing usage only

The mechanical selection above (has a cast + source changed) decides *which* tests are candidates. On top of it is a **hard authoring rule**: a test is a legitimate reel clip only if it exercises the feature **the way a user actually runs it**. A clip exists so a human can *watch and validate real behavior* — so a test that drives the feature under a **test-only artifice** must **not** become a clip, because the viewer would be validating a fiction. A reel-eligible PTY-attached test must not rely on:

- non-representative CLI flags a user would never pass (e.g. `pi --no-builtin-tools`, or tool allow/deny-lists that force a particular code path);
- stand-in binaries (`cat`, echo scripts) standing in for a real agent;
- delivering a prompt as a command-line argument when production delivers it by **injection** — the pane must be seeded the way the daemon does it (`write_to_pane_and_submit`), not `agent … '<prompt>'`.

If a feature can only be *proven* under such an artifice, split it: a **real-usage** test for the reel plus a separate **headless** (non-recorded) test for the forensic proof. This applies CLAUDE.md rule 4's "validate it AS A USER ACTUALLY USES AND SEES IT" bar at the clip-selection boundary.

> **Today this is an authoring convention, not enforced.** Two robustness follow-ups are tracked: (1) an explicit opt-in marker so an artifice test can never be *auto*-selected as a clip; (2) the selection diff (concern **a**, item 2) currently compares against the **local** `main` ref, which can be stale and over-select unrelated tests — it should fetch/diff `origin/main`. Until (1) ships, authors are responsible for not letting an artifice test become a clip.

## Assembly rule (concern b)

`assemble` is pure: given a list of recording-dir IDs it reads only `test.md` and `CATALOG.md` (no test-body parsing, no git, no network) and emits the manifest:

- **title** ← the `test.md` **H1** line, minus the leading `# ` (e.g.
  `mouse/button/001 — Beta renders its label.`).
- **description** ← the `## Scenario` paragraph(s), blank lines dropped and
  collapsed to a single line.
- **catalog id** (for ordering only) ← the part of the H1 **before the first
  ` — `** (em dash).
- **clip** ← `<recordings>/<id>/full-stream.cast`.
- Any ID lacking a `full-stream.cast` is **excluded** (the same L1 guard as
  selection, applied at assembly so an injected list can't smuggle an L1 test in).
- Entries are **ordered by catalog id's line position in `CATALOG.md`** (the
  authoritative order); an id absent from the catalog sorts last.
- **Clean skip:** if no ID resolves to an e2e clip, it prints
  `skipped: no e2e tests changed on this branch`, writes **no** manifest, and
  exits 0.

Splitting selection (a) from assembly (b) is deliberate: (b) is fully deterministic and fixture-testable without git or the network, which is what the acceptance test below exercises.

## Environment overrides

All paths default to this repo's layout and are overridable (the test uses this to point at fixtures):

| Var | Default |
| --- | --- |
| `REEL_ADAPTER_RECORDINGS_DIR` | `.dot-agent-deck/recordings` |
| `REEL_ADAPTER_CATALOG` | `tests/CATALOG.md` |
| `REEL_ADAPTER_MAIN_REF` | `main` |
| `REEL_ADAPTER_ENGINE` | `<skill>/../demo-reel/reel.sh` |

## Acceptance test

A **re-runnable, pure-shell** test (no `agg`/`ffmpeg`, no git, no network — so it **may** run in CI, unlike the engine smoke and the reel step itself) drives the deterministic `assemble` path against a tiny fixture (`tests/fixtures/recordings/` with two e2e dirs that have casts and one L1 dir that does not, plus a `CATALOG.md` fixture). It asserts:

1. given `alpha beta gamma`, the manifest has the right titles/descriptions/clip
   paths **in catalog order** (`beta`=001 before `alpha`=002) and **excludes**
   the cast-less L1 `gamma`;
2. given an empty list, it **clean-skips** — no manifest, exit 0, skip message
   (and likewise for an L1-only list).

```sh
task reel-adapter-test
# or directly:
.claude/skills/demo-reel-adapter/tests/adapter_test.sh
```
