---
name: demo-reel-adapter
description: dot-agent-deck-specific adapter that builds a demo-reel manifest.json from this repo's per-test recordings — selects the e2e #[spec] tests added/changed on the branch (diff vs main) that are explicitly marked reel-eligible (` [reel]` in tests/CATALOG.md), lifts each test's title/description from its test.md, orders by catalog id, points each entry at its full-stream.cast, refuses any cast whose provenance sidecar does not record a passing run at the current commit, and invokes the repo-agnostic demo-reel engine. Clean-skips when no eligible e2e tests changed. Use when asked to build the PRD demo reel for this repo.
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
| `build.sh [reel] [--out OUT.mp4] [--publish] [--manifest PATH] [--title TITLE]` | Full pipeline: **select** → **assemble** → invoke the engine, forwarding `--out`/`--publish` plus a composed `--title`. Clean-skips (no manifest, no engine, exit 0) when no e2e tests changed — and just as cleanly, but with a **different message naming the tests**, when e2e tests *did* change and none is `[reel]`-marked (see **Which clean skip you got**). `--manifest` sets where `manifest.json` is written (default `manifest.json` in CWD). `--title` overrides the composed title verbatim (see **Title composition**). |
| `build.sh title [--title TITLE]` | Print the title the `reel` pipeline would pass to the engine on the current branch — the composed title, or `--title` verbatim. Dry-run: no selection, no manifest, no engine, no upload. |
| `build.sh select` | Print the in-scope recording-dir IDs, one per line (the git-diff half — concern **a**). |
| `build.sh assemble [ID...] [--manifest PATH]` | Build `manifest.json` from an explicit list of recording-dir IDs (concern **b**; no network, and one `git rev-parse HEAD` unless `REEL_ADAPTER_EXPECT_COMMIT` is set). Excludes cast-less IDs, IDs whose catalog entry lacks the ` [reel]` marker, **and** IDs whose recording fails the [cast-provenance gate](#cast-provenance-issue-808), orders by catalog id, clean-skips a list where nothing survives — naming what each gate dropped, separately. |

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

1. **It contains a `full-stream.cast`** — the e2e proxy. The `cargo xtask docs` generator writes a `test.md` for *every* `#[spec]` test but emits a cast only for **L2** tests; **L1** render tests have a `test.md` and **no** cast, so they are excluded by construction (which is also exactly the right "user-journey" subset). Casts are local-only (PRD #77) and only written on failure or under `DOT_AGENT_DECK_RECORD=1`, so the reel step runs the relevant e2e tests with that flag first; without casts, every dir fails this check and the step clean-skips. That run is LOCAL and is not discharged by CI — CI's lane 1 (CLAUDE.md rule 5) sets no such flag and uploads no casts, and every reel-eligible test is a **lane-2** test that CI does not run at all — and since issue #502 removed the pre-PR full-tier obligation there is no longer an unfiltered run to piggyback on, so record the tests the branch adds or changes by filter with `DOT_AGENT_DECK_RECORD=1 cargo test-e2e-live <filter>`.

**A reel-eligible cast comes from a run that held a real agent credential, and this skill publishes it.** Eligibility requires a real agent spinning up, which is exactly the `e2e-live` boundary, so every cast that reaches `--publish` was recorded by a process holding your credentials — and the URL goes into the PR body *and* the changelog fragment, which flows into the public release notes. Two things stand between a key and a public URL, and it is worth being precise about which does what.

**The upload is PRIVATE by default**, and that is the boundary that actually holds: `upload.sh` creates the video with `privacyStatus: private` and `reel.sh --publish` passes no privacy flag, so this automated path cannot produce a third-party-visible video at all. Only the channel owner, and any account they deliberately share it with, can watch it; flipping it to unlisted before a release is a deliberate human step, and the video id survives the flip so the link already in the PR keeps working. See `docs/develop/demo-reel.md`.

**The harness's redaction of `full-stream.cast` is best-effort, not a guarantee.** It removes registered credential values, including ones the terminal wrapped across rows, and it is not optional housekeeping — but it is a **blocklist**: it can only remove values something registered, and its matcher is known incomplete in two reproduced ways (a registered value starting inside an earlier match, and a decoy continuation that suppresses a real one). `docs/develop/e2e-lanes.md` has the mechanism and names both gaps. So do not treat a redacted cast as a scanned cast: watch the reel before you flip it public, per `docs/develop/demo-reel.md`, and treat private-by-default plus that review as the thing standing between a credential and a public URL.
2. **Its catalog entry carries the ` [reel]` eligibility marker** (see
   [Reel-eligibility marker](#reel-eligibility-marker-real-user-facing-usage-only)
   below). Eligibility is **opt-in**: a cast alone means the test is PTY-attached,
   not that it belongs in the reel, so an unmarked test is excluded even with a
   cast and a changed source.
   This gate is evaluated **last** even though it is listed second: the three conditions are ANDed, so order cannot change *which* dirs are selected, but checking the marker last means its `excluding '<id>' … has no [reel] marker` diagnostic fires only for a dir that would otherwise have been selected — a genuine near-miss — rather than for every unmarked recording on disk.
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
- **Clean skip:** if no ID resolves to a reel-eligible e2e clip it writes **no** manifest and exits 0, printing either the plain `skipped: no e2e tests changed on this branch` or, when an ID was dropped for a missing marker, the eligibility message that names it (see **Which clean skip you got**).

Splitting selection (a) from assembly (b) is deliberate: (b) is deterministic and fixture-testable, and it has exactly one impure edge — resolving the commit the provenance gate checks against, which `REEL_ADAPTER_EXPECT_COMMIT` overrides and which is resolved *lazily*, so a list that reaches no provenance check touches no git at all. That is what most of the acceptance test below exercises.

## Cast provenance (issue #808)

The three selection gates are a `test -f` plus two **static facts about the test** — is it branch-scoped, is its catalog line marked — so none of them says anything about the **artifact**. A `full-stream.cast` an older revision left on disk satisfies every one of them, which means the adapter could stitch a clip from an unknown revision into a video whose URL goes into the PR body *and* the changelog fragment, and from there into the public release notes.

PR #805 fixed the cheap half: the harness now discards the previous run's artifacts at launch (before it spawns anything) and on `skip_unless!`'s runtime-skip path, and a deletion that fails for any reason other than `NotFound` panics instead of warning. `tests/harness_isolation.rs` carries all three guards. That closes every route that **reaches** one of those call sites — and **two routes do not**:

1. A **filtered** run — the normal way anyone works, and what CLAUDE.md rule 5 asks for — never selects the test at all, so nothing on the discard path executes and an unselected test's older cast is untouched.
2. `skip_unless!` evaluates its **preflight expression before** `_skip_if_err` is entered: `skip_unless!(check_claude_available())` calls the check first and only then hands the `Result` over, so a kill or an abort inside a `check_*_available` — or inside an importer it calls, which is where the credential work happens — lands before the skip-path discard.

Provenance covers both at once, which is why the answer was not a third discard. The harness writes a `provenance.json` sidecar beside each dump, and `assemble` checks it.

### What is refused, and what is only reported

| Field | Adapter behaviour |
| --- | --- |
| sidecar **absent**, unparseable, unknown `schema`, or missing a required field | **REFUSED.** Covers a cast written before the sidecar existed and a dump that died before writing it — the sidecar is written **last** for exactly that reason. |
| `outcome` other than `passed` | **REFUSED.** `Drop` dumps on panic, so a failure diagnostic is the commonest artifact on disk; it is not a clip. |
| `commit` not the revision the reel is being built at | **REFUSED.** This is the field that closes both residual routes: a cast an older revision left behind names that older revision. |
| `run_id` | Reported; **differing ids across clips WARN**, never refuse. A reel legitimately assembles clips from several filtered runs at one commit. |
| `dirty` | Reported loudly; never refuses. Recording from a dirty tree is the ordinary dogfood case. |
| `build_id`, `recorded_at_unix`, `redaction_version` | Reported. `recorded_at_unix` is never thresholded: a clip recorded at this commit is publishable however old it is, and any age limit would be an arbitrary number that refuses correct clips. |

The gate is **per-clip**: one stale sidecar drops one clip and the rest of the reel is still built, with the omission named where the manifest is announced. Dropping a whole reel over one stale clip would only teach people to bypass the gate.

### What this proves — and what it does not

A selected clip was written by a harness **built from this commit**, by a run that **was not unwinding a panic**. Stated no wider than that, because each of these is outside it:

- It does **not** prove the clip came from the **latest** run. A passing clip recorded at this commit by an earlier run is accepted, and correctly so — the code that produced it is the code under test. Provenance establishes revision and outcome, not recency.
- Under `dirty` even the revision narrows: one commit then covers more than one working state, so `commit` stops identifying the code. That is why the dirty flag is reported rather than swallowed.
- `outcome: passed` means **the harness was not unwinding a panic when the deck was dropped** — not that nextest reported the test as passed. A test that panics after its deck is already dropped is not observed; a test killed outright reaches no `Drop`, writes no sidecar at all, and is covered by the launch-time discard instead.
- It says **nothing about whether the cast's content is redacted.** The harness redaction is a best-effort blocklist with two reproduced gaps ([#810](https://github.com/vfarcic/dot-agent-deck/issues/810)). Provenance and redaction do not cover for each other, and a passing test lowers the probability of a credential-bearing dump without changing the boundary — successful terminal output can carry a credential too.
- It is **not a signature.** The sidecar sits in the same gitignored directory as the cast, so whoever can write one can write the other. It establishes which revision and which outcome produced an artifact the harness itself wrote.
- Watching the finished reel before flipping it public remains defence in depth, and is **not** a reliable secret scanner.

### Where it is enforced

At **assembly only**, and that is sufficient rather than a shortcut: every route to a manifest runs `assemble` — the `reel` pipeline and the standalone `build.sh assemble <id...>` that an injected id list would use. `select` deliberately does not check it: "in scope" is a statement about the test, provenance is a statement about the artifact, so `build.sh select` stays a pure scope query with no git-HEAD dependency of its own and there is exactly one place the publish decision is made. (The ` [reel]` marker is duplicated across both halves for a different reason — so its near-miss diagnostic fires during selection too — not because assembly's copy is insufficient.)

The harness end of the contract is `write_provenance` in `tests/common/mod.rs`, with `RECORDING_PROVENANCE_SCHEMA` and `RECORDING_REDACTION_VERSION` next to it. The two ends are in different languages and neither compiles the other, so `tests/harness_isolation.rs` asserts in the fast tier that every field the adapter reads is one the harness writes and that both agree on the schema number — drift would otherwise refuse every clip silently until somebody next tried to publish.

**If the reel refuses everything, re-record rather than reaching for the override.** `DOT_AGENT_DECK_RECORD=1 cargo test-e2e-live <filter>` at the current commit is the fix for a stale cast; `REEL_ADAPTER_EXPECT_COMMIT` exists for the acceptance test and for a deliberate manual assertion, not as a way past a refusal.

## Which clean skip you got

Both skips are identical in behaviour — no manifest, no engine, **exit 0** — and that is deliberate: a reel is not owed on every branch, so the pre-merge reel step must not fail an ordinary PR. Only the **wording** differs, and it has to, because the two causes call for opposite responses (issue #735):

| Printed | Cause | What to do |
| --- | --- | --- |
| `skipped: no e2e tests changed on this branch` | Nothing was in scope at all — no changed e2e test with a cast. | Nothing. A reel was never possible on this branch. (If you expected one, check that the e2e suite ran with `DOT_AGENT_DECK_RECORD=1` so the casts exist.) |
| `skipped: N e2e test(s) …, but none is reel-eligible — no [reel] marker in tests/CATALOG.md for: <ids>` | e2e tests **did** change and have casts; they were dropped by the opt-in marker gate alone. | Read the named ids. Usually **nothing** — an unmarked test is unmarked on purpose. Add the marker only if that test genuinely spins up a **real agent** and shows the feature as a user runs it; a stand-in stays unmarked. |
| `skipped: N e2e test(s) …, but none has a recording whose provenance checks out` | The tests changed and **are** marked, but every one of their recordings failed the [cast-provenance gate](#cast-provenance-issue-808) — most often a cast an earlier revision left on disk that no filtered run since has overwritten. | **Re-record** at this commit: `DOT_AGENT_DECK_RECORD=1 cargo test-e2e-live <filter>`. Each id's own verdict — absent sidecar, wrong commit, non-passing run — is on stderr above the skip. A ` [reel]` marker will not fix it, and neither will the diff. |

The three wordings are deliberately distinct because the three causes call for opposite responses, and the provenance one is a third unrelated reason rather than a variant of the marker: blaming the marker for a stale cast would send a reader to add a tag that changes nothing, which is the same misattribution issue #735 was about.

A hand-written id list can trip **more than one** gate at once — one id with no cast, another with a cast but no marker — and then the second message carries an extra parenthetical naming the cast-less ids separately, because "none is reel-eligible" is not the reason those were dropped and adding a marker to them would change nothing. Only the standalone `build.sh assemble <id...>` reaches this: the `reel` pipeline's `select` half drops cast-less dirs before `assemble` ever sees them, so its skips always have a single cause.

The second message names the ids because that is what makes it actionable, and because the older generic wording pointed at the wrong gate: a reader who saw "no e2e tests changed" on a branch that *had* changed e2e tests would go debugging the git-diff selection, which was working correctly. The inverse hazard is worth naming too — a message claiming nothing changed invites someone to reach for a ` [reel]` marker to make a reel appear, when the absent marker was the deliberate, correct answer.

## Environment overrides

All paths default to this repo's layout and are overridable (the test uses this to point at fixtures):

| Var | Default |
| --- | --- |
| `REEL_ADAPTER_RECORDINGS_DIR` | `.dot-agent-deck/recordings` |
| `REEL_ADAPTER_CATALOG` | `tests/CATALOG.md` |
| `REEL_ADAPTER_MAIN_REF` | `origin/main` |
| `REEL_ADAPTER_ENGINE` | `<skill>/../demo-reel/reel.sh` |
| `REEL_ADAPTER_EXPECT_COMMIT` | `git rev-parse HEAD`, resolved lazily |

`REEL_ADAPTER_EXPECT_COMMIT` is the commit each recording's provenance must name. Setting it is an **operator assertion**, in the same class as `clean-e2e-tmp --ignore-liveness`: it is what keeps the acceptance test offline and deterministic, and pointing it at the wrong revision defeats the commit gate. It cannot loosen anything by accident, though — a wrong value refuses every clip rather than admitting a stale one. With it unset and `git rev-parse HEAD` unable to answer, `assemble` **dies** rather than publishing unchecked.

## Acceptance test

A **re-runnable, pure-shell** test (no `agg`/`ffmpeg`, no git, no network — so it **may** run in CI, unlike the engine smoke and the reel step itself) drives the deterministic `assemble` path against a tiny fixture (`tests/fixtures/recordings/` with two `[reel]`-marked e2e dirs that have casts, one L1 dir with no cast, and one cast-bearing dir that is **not** marked, plus a `CATALOG.md` fixture). It asserts:

1. given `alpha beta gamma delta`, the manifest has the right
   titles/descriptions/clip paths **in catalog order** (`beta`=001 before
   `alpha`=002), **excludes** the cast-less L1 `gamma`, and **excludes** the
   cast-bearing but unmarked `delta`;
2. given an empty list — and likewise an L1-only list — it **clean-skips** with
   the `no e2e tests changed` message, while an **unmarked-cast-only** list
   clean-skips with the **eligibility** message naming `delta`, so the two
   wordings cannot drift back into one — and a **mixed** `gamma delta` list names
   **both** reasons rather than attributing the whole skip to the marker;
3. on the **`reel`** path, a branch that changed only the *unmarked* test's
   source skips with that eligibility message (never `no e2e tests changed` —
   the issue #735 defect) and still writes no manifest and invokes no engine,
   while a branch that changed a **marked** test's source selects, assembles and
   invokes the engine (a stub) with no near-miss reported;
4. the **cast-provenance** gate refuses a clip whose sidecar is absent, corrupt,
   of an unknown schema, missing a required field, recorded at a different
   commit, or recorded from a non-passing run — while a valid sidecar publishes,
   one stale sidecar drops **one** clip rather than the reel, a dirty-tree or
   multi-run reel publishes **with a warning** rather than a refusal, and an
   adapter that cannot resolve any commit to check against **dies** instead of
   publishing unchecked.

Section 4's cases mutate **copies** of the fixture tree, editing one provenance field at a time, so the difference between "publishes" and "refused" is visible in the test body rather than buried across a fixture dir per refusal. Sections 1, 2 and 4 are pure shell (a `cp` and a `jq`), apart from two provenance cases that deliberately unset `REEL_ADAPTER_EXPECT_COMMIT` to prove the git-HEAD default is live and that it fails closed. Section 3 exercises the selection half, which exists to run `git diff`, so it shells out to **git** — building every repository inside its own `mktemp -d` with the ambient git configuration switched off, so it can neither read nor write the checkout it runs in, and with no network and no sleep (the same discipline CLAUDE.md rule 5 sets for the `xtask` real-git tests). It **skips**, without failing, where `git` is unavailable.

```sh
task reel-adapter-test
# or directly:
.claude/skills/demo-reel-adapter/tests/adapter_test.sh
```
