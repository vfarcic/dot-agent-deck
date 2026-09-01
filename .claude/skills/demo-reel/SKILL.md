---
name: demo-reel
description: Stitch a manifest of terminal recordings into one narrated MP4 (title/description card, then clip, repeated) and optionally upload it privately to YouTube. Repo-agnostic engine driven only by a manifest.json; runnable by an agent or directly as reel.sh. Use when asked to build a demo reel / narrated video from a set of asciinema casts, gifs, or mp4 clips.
---

# Demo Reel engine

A reusable, repo-agnostic engine that turns an ordered **manifest** of `{title, description, clip}` entries into a single narrated MP4: for each entry it renders a title/description **card**, plays that entry's **clip**, then moves to the next — concatenated in manifest order. With `--publish` it uploads the result **private to YouTube** and prints the URL.

The engine knows nothing about Rust, tests, PRDs, or any specific repo. Its only input is a `manifest.json`. It is invocable by an agent (via this skill) and directly by a human or CI (`reel.sh manifest.json --out reel.mp4`).

> **Status:** the full engine pipeline is wired. A run validates the manifest and prerequisites, renders a card per entry, stitches `[card, clip, …]` into one uniform MP4 (`reel.sh` → `ffmpeg`), and — with `--publish` and credentials present — uploads it **private** to YouTube (`upload.sh`) and prints the URL. The stitch path is covered by a re-runnable local smoke (`task reel-smoke`); the live upload is verified by code review plus a documented one-line manual step (see **Verifying the upload path**).

## Usage

```sh
reel.sh MANIFEST [--out OUT.mp4] [--title TITLE] [--publish]
```

| Argument / option | Meaning |
| --- | --- |
| `MANIFEST` | Path to a `manifest.json` (see **Manifest contract** below). Required, positional. |
| `--out OUT.mp4` | Where to write the stitched MP4. Default: `reel.mp4`. |
| `--title TITLE` | Title for the uploaded video (used only with `--publish`). Default: the basename of `--out` without its extension (e.g. `reel` for `reel.mp4`). The engine is repo-agnostic and has no notion of a PRD, so a descriptive title is the caller's job — the dot-agent-deck adapter composes one and passes it through here. |
| `--publish` | After stitching, upload the MP4 **private** to YouTube and print the URL. Requires the YouTube OAuth credentials (see **Prerequisites**). |
| `-h`, `--help` | Print usage and exit. |

Examples:

```sh
reel.sh manifest.json --out reel.mp4                                   # stitch only, no upload
reel.sh manifest.json --out reel.mp4 --publish                         # stitch + upload private (title = "reel")
reel.sh manifest.json --out reel.mp4 --title "My demo reel" --publish  # stitch + upload with an explicit title
```

## Manifest contract

`manifest.json` is the **only** contract between a caller and the engine. It is a JSON **array** of one or more objects, in the order the segments should appear:

```json
[
  {
    "title": "mouse/button/001 — inline-shortcut label",
    "description": "Start the app, focus the dashboard, and confirm the Button widget renders its inline-shortcut label.",
    "clip": "recordings/mouse-button-001/full-stream.cast"
  },
  {
    "title": "Second segment",
    "description": "What this clip shows, in 1–3 plain-English sentences.",
    "clip": "clips/second.mp4"
  }
]
```

The engine rejects a manifest that breaks any of these rules, with a specific message and a non-zero exit:

- The top level is a **non-empty JSON array**.
- Every entry is a JSON **object** with non-empty string `title`,
  `description`, and `clip`.
- `clip` is a path to an existing `.cast` (asciinema v2), `.gif`, or `.mp4`
  file. Paths are resolved relative to the current working directory. The
  format is intentionally open: a cast renderer is just one optional
  front-end, so an already-rendered `gif`/`mp4` can be fed directly (this is
  what lets a different recording tool reuse the same engine).

## Prerequisites

The engine checks these **before doing any work** and fails fast with an actionable message naming exactly what is missing; it never self-installs anything.

**Always required (CLIs on PATH):**

| CLI | Used for | Package |
| --- | --- | --- |
| `agg` | render an asciinema cast to frames | nix `asciinema-agg` |
| `ffmpeg` | stitch and encode the final MP4 | nix `ffmpeg` |
| `jq` | parse and validate the manifest | nix `jq` |
| `curl` | upload to YouTube (only with `--publish`) | nix `curl` |

**Required only with `--publish`** — YouTube Data API v3 OAuth credentials, read from the environment (never hardcoded). In this repo they are sourced from `vals` / `.env.vals.yaml`:

| Env var | Meaning |
| --- | --- |
| `YOUTUBE_CLIENT_ID` | OAuth client id |
| `YOUTUBE_CLIENT_SECRET` | OAuth client secret |
| `YOUTUBE_REFRESH_TOKEN` | OAuth refresh token (minted once via a human consent flow) |

Stitch-only runs (no `--publish`) do **not** require any credentials. The one-time OAuth provisioning is documented in `docs/develop/demo-reel.md`.

## How a reel is built

For each manifest entry, in order:

1. **Card.** A synthetic asciinema cast paints the **bold bright-cyan title** and a
   **bright-white, left-aligned, vertically-centered description** as terminal
   text, on the engine's own fixed `CARD_COLS`x`CARD_ROWS` grid (84x20 — shaped
   ~16:9 in character cells so a card **fills** the output canvas rather than
   letterboxing inside it). `CARD_ROWS` is a *minimum*: a long description grows
   the grid taller and the font shrinks to match, so text scales instead of
   clipping. The card's **hold** duration is a **flat `CARD_HOLD` seconds**
   (default **4s**, env-overridable), independent of how much text the card
   carries: a fixed, deliberately short hold keeps the reel moving, and a viewer
   who wants to read a long description pauses the video rather than the reel
   parking on every long card. The hold is enforced at the **ffmpeg** level — a
   single painted still is frozen from the rendered card and looped to *exactly*
   the hold duration — so it is decoupled from `agg`'s idle handling (which would
   otherwise collapse the static tail to a couple of seconds).
2. **Clip.** A `.cast` is first **re-timed** (`retime.sh` rewrites its event
   timestamps for a watchable cadence — see below), then rendered through `agg` at
   its **recorded terminal grid**; a pre-rendered `gif`/`mp4` is used as-is (no
   re-timing).

Both are rendered at an `agg` **font size fitted to the output canvas**, not a
fixed one. A terminal grid carries no pixel size of its own, so choosing the font
is how the engine gets glyphs rasterized *at* output resolution: a 68x16 cast at
font 16 is only 674x381, which on a 1080p canvas is a ~2.8x upscale of already-
rasterized text (mush); at the fitted font (45) it renders 1896x1071 and stays
sharp. Set `CLIP_FONT_SIZE` / `CARD_FONT_SIZE` to pin a font and skip the fit.

## Clip re-timing

e2e casts are recorded at machine speed, so their raw timeline is unwatchable: a keypress and the full repaint it triggers land within a millisecond of each other, while short real waits (daemon startup, polling, debounce) sit between them. A single global `agg --speed` cannot fix this — slowing everything stretches the waits into dead air and still cannot spread coincident events apart. So before rendering, every `.cast` clip is passed through `retime.sh`, which rebuilds the timeline from the event payloads (rendering then runs at `CLIP_SPEED` 1.0):

**The contract:** the re-timer *re-distributes* time, it does not manufacture it. Time reclaimed from dead air is re-spent holding operations, so the output totals about `max(MIN_BUDGET, the input duration)` — and never more than `max(MIN_BUDGET, MAX_STRETCH × input)`, which is a hard ceiling that compresses everything proportionally if the base gaps alone would exceed it. When there is no dead air to reclaim, nothing is held and the clip plays at roughly real time. No cast can come out as a slideshow.

Two measured worked examples. A 28.7s cast of a real agent working, already smoothly paced (no gap over 0.03s), comes out at 32.3s — **1.12×**, essentially unchanged. A synthetic cast of 6 repaints separated by 3s waits — 15s that is almost entirely dead air — comes out at **7s**, with each repaint held the full `OP_HOLD` 1.4s: the waits are gone, every operation is now visible, and the whole thing is *shorter* than the original.

Each event is classified into one of **three** kinds, by payload **size** *and* **content**:

- **op** — a payload **larger** than `SIZE_THRESHOLD` bytes is a full-region repaint (opening a deck/form/pane). Consecutive large chunks within `COALESCE_GAP` of each other are one logical repaint and are coalesced into a single step. An op is **held** after the repaint (up to `OP_HOLD`) so the new state is actually visible — *budget permitting*.
- **type** — a small payload that actually **prints** something is a typed character (ratatui emits a minimal diff per keypress). Each gets its own step, at least `TYPE_GAP` apart, so typing replays at a readable speed.
- **tick** — a small payload that prints **nothing**: pure control sequences (SGR reset, show-cursor, cursor-position). That is the render loop's per-frame tail, not a keystroke, so it keeps its **original** gap (clamped to `IDLE_CAP`) and is never spread.

Any single gap is clamped to `IDLE_CAP`, which is what kills dead air while still reading as a pause. `agg`'s static last-frame hold is left intact, so the final state lingers.

> **Why content and not size alone.** PRD #339's published reel turned a 15.5s cast into a **161s** video (10.4×), and the classifier is why: it called *every* small payload a keystroke. A ratatui render loop emits a per-frame tail of `SGR-reset + show-cursor + cursor-position` that prints nothing at all, and those tails are the overwhelming majority of a cast's events — so each was given its own 100ms "typed char" step, and each coalesced repaint was then *unconditionally* held 1.4s on top. Re-measured on that test's replacement recording (28.7s, 1621 events: **1565 ticks**, 44 real typed chars, 12 repaints), the old re-timer produces **172.4s** — 1609 × 0.1s of fabricated typing cadence, which is ~93% of the total — while the new one produces **32.3s**. Both failure modes are closed: `tick` events are no longer mistaken for typing, and `OP_HOLD` is granted only out of reclaimed slack under the duration budget.

`retime.sh` is repo-agnostic (it operates on any `.cast`) and standalone (`retime.sh [INPUT.cast] [--out OUT.cast]`, reading stdin / writing stdout by default). Its tunables are env-overridable, like the engine's `CLIP_SPEED` — `SIZE_THRESHOLD` (80) is in bytes, `MAX_STRETCH` (1.6) is a ratio, everything else is in seconds: `TYPE_GAP` (0.1), `OP_HOLD` (1.4, a *maximum* now), `IDLE_CAP` (0.4), `MIN_BUDGET` (8), `COALESCE_GAP` (0.05). (`IDLE_THRESHOLD` is gone — every gap is simply clamped to `IDLE_CAP`, so there is no separate "is this a real wait" threshold.) `CLIP_SPEED` (default 1.0) remains as a global multiplier layered on top of the re-timer for the rare clip that wants a uniform nudge; note that it multiplies duration *outside* the re-timer's budget.

## Output canvas

Every segment is then **normalized** (`ffmpeg scale` + `pad`) to one common resolution at a constant fps and `yuv420p`, so all segments share resolution/fps/pixfmt and concat into a single uniform stream (`reel.mp4` by default).

That resolution is a **fixed, landscape 16:9 canvas** — `REEL_W`x`REEL_H`, default **1920x1080**, a normal laptop screen — and is deliberately *not* derived from the segments. It used to be the per-axis max across every native segment, which silently produced a canvas whose aspect ratio belonged to **neither** a card nor a clip: PRD #339's reel took its **width** from the card (1140) and its **height** from a portrait 60x50 clip (1142), came out 1140x1142, and showed the terminal in a ~585px centre strip with black bars either side.

Segments are **fit** into the canvas (scale preserving aspect, then pad) and are **never cropped** — cropping a terminal would cut off content. So the canvas fixes the *frame*, but how much of it a clip *fills* remains a property of the recording:

- A **~16:9 cast fills it nearly edge to edge.** In character cells that is roughly **4x as many columns as rows** (a cell is about 1:2.3), e.g. 68x16 or 200x50.
- A **portrait cast can only ever occupy a centre strip.** A 60x50 terminal is 0.52:1 against a 1.78:1 canvas and covers ~29% of the frame — no engine setting can fix that; it has to be re-recorded.

The engine **warns** (non-fatally, per segment) when a clip's aspect is more than `ASPECT_TOLERANCE` (1.35) off the canvas, naming the percentage of the frame it will actually cover.

## Checks the engine runs for you

Every run reports these to stderr, so a bad artifact is visible *before* it is published rather than after someone watches it. All are non-fatal — the engine's job is to render what it was given — but each one names a specific thing to fix:

| Check | What it catches |
| --- | --- |
| **Cast integrity** — a cursor-position escape (`ESC[row;colH`, `ESC[colG`) addressing a column beyond the cast header's `width` | The terminal was **resized mid-recording**. asciinema v2 stores **one** fixed size in its header and has **no resize event**, so the recorder writes the *final* size and every earlier, wider frame hard-wraps into garbage on replay. This is exactly what made PRD #339's clip unreadable (column 68 in a 60-column header). Re-record at one fixed size. |
| **Re-timing ratio** — `clip N: re-timed 15.5s -> 24.5s (1.58x)` | A clip being stretched. The ratio is bounded by `MAX_STRETCH`, so anything wildly larger means a tunable (or `CLIP_SPEED`) was overridden into producing a slideshow. |
| **Aspect** — a segment more than `ASPECT_TOLERANCE` off the canvas | A recording that will letterbox badly, with the percentage of the frame it will actually cover. |

Still worth doing by eye before publishing: **extract a frame and look at it** (`ffmpeg -ss <t> -i reel.mp4 -frames:v 1 -update 1 frame.png`). No automated check substitutes for seeing the thing.

## Local smoke test

A re-runnable smoke builds a reel from a tiny self-contained fixture (two hand-written `.cast` clips + a manifest under `.claude/skills/demo-reel/tests/fixtures/`) in **stitch-only** mode (no network, no credentials) and asserts the result with `ffprobe`: non-empty file, exactly one video stream at the expected resolution (a single stream proves there is no resolution/fps/pixfmt seam between segments), `yuv420p`, constant `30/1` fps, and a duration **between** the sum of the per-card holds and the engine's own upper bound on it (card holds + each clip's re-timing budget + a small per-clip allowance for `agg`'s trailing hold). That upper bound is the regression guard for the 10.4x stretch above. It is **local-only** (never CI):

```sh
task reel-smoke
# or directly:
.claude/skills/demo-reel/tests/smoke.sh
```

## Privacy — uploads are PRIVATE by default, and that is a security boundary

`upload.sh` creates the video with `privacyStatus: private`, and `reel.sh --publish` passes no privacy flag at all, so **the automated path can only ever produce a private video**. `upload.sh --privacy unlisted|public` is the explicit escape hatch for a hand-run.

Why the default moved off `unlisted`: unlisted means *anyone with the link can watch*, and in this project's flow the link is written into the PR body **and** the changelog fragment, which flows into the public release notes. A reel clip is only eligible if the recorded run spun up a **real agent**, so every cast the reel stitches was written by a process holding live credentials. Uploading unlisted therefore put a credential-bearing recording on a publicly-reachable URL with no human between the two. Private keeps the same automation — the channel owner can always watch their own private videos, so an agent still uploads unattended and the link still goes in the PR — while making the human review a **publication** gate instead of a merge gate: flipping the video to unlisted before a release is a deliberate step somebody takes.

The video id survives a private → unlisted flip, so the link already in the PR and the changelog starts working with no re-upload and no link edit.

**What private does not do.** The upload has already happened; the bytes sit on Google's servers either way. Private *contains* a leak to the owner's own account. It does not undo one. And redaction upstream of this is a blocklist that can only remove values the harness registered — see `docs/develop/e2e-lanes.md` for its known gaps.

## Verifying the upload path

The live YouTube upload cannot be a routine automated test, so it is verified by code review of `upload.sh` plus a **one-time manual** check: with the three `YOUTUBE_*` credentials exported, run

```sh
.claude/skills/demo-reel/reel.sh some-manifest.json --out reel.mp4 --publish
```

and confirm it prints an `https://youtu.be/<id>` URL that opens a **private** video — signed in as the channel owner it plays normally and shows a *Private* badge; signed out, or as any account the owner has not deliberately shared it with, it is unavailable. That asymmetry is the point (see **Privacy** below). All hosting lives in `upload.sh` alone, so swapping hosts later does not touch the rest of the engine.

## Failure behavior

- **Bad invocation** (no manifest, unknown flag, `--out` without a value)
  prints usage to stderr and exits non-zero.
- A **missing manifest file**, **malformed JSON**, or a manifest that breaks
  the contract above fails with a specific message and a non-zero exit.
- A **missing CLI** (`agg`/`ffmpeg`/`jq`/`curl`) is a **hard** failure: it is
  reported by name in the pre-flight check before any work starts; the message
  points at `docs/develop/demo-reel.md` or to asking the agent, and does not
  embed setup steps.
- **Missing `--publish` credentials degrade gracefully:** the reel is still
  stitched and the local MP4 is kept; only the upload is skipped, with a
  "reel is at `<path>`; could not publish (missing …)" note. Stitch-only runs
  never need credentials.
- **Runtime upload errors** (expired/revoked token, exhausted quota, API
  disabled) are only knowable at upload time; `upload.sh` passes the API's raw
  error through rather than swallowing it.
