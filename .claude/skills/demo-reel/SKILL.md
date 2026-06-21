---
name: demo-reel
description: Stitch a manifest of terminal recordings into one narrated MP4 (title/description card, then clip, repeated) and optionally upload it unlisted to YouTube. Repo-agnostic engine driven only by a manifest.json; runnable by an agent or directly as reel.sh. Use when asked to build a demo reel / narrated video from a set of asciinema casts, gifs, or mp4 clips.
---

# Demo Reel engine

A reusable, repo-agnostic engine that turns an ordered **manifest** of `{title, description, clip}` entries into a single narrated MP4: for each entry it renders a title/description **card**, plays that entry's **clip**, then moves to the next — concatenated in manifest order. With `--publish` it uploads the result **unlisted to YouTube** and prints the URL.

The engine knows nothing about Rust, tests, PRDs, or any specific repo. Its only input is a `manifest.json`. It is invocable by an agent (via this skill) and directly by a human or CI (`reel.sh manifest.json --out reel.mp4`).

> **Status:** the full engine pipeline is wired. A run validates the manifest and prerequisites, renders a card per entry, stitches `[card, clip, …]` into one uniform MP4 (`reel.sh` → `ffmpeg`), and — with `--publish` and credentials present — uploads it unlisted to YouTube (`upload.sh`) and prints the URL. The stitch path is covered by a re-runnable local smoke (`task reel-smoke`); the live upload is verified by code review plus a documented one-line manual step (see **Verifying the upload path**).

## Usage

```sh
reel.sh MANIFEST [--out OUT.mp4] [--title TITLE] [--publish]
```

| Argument / option | Meaning |
| --- | --- |
| `MANIFEST` | Path to a `manifest.json` (see **Manifest contract** below). Required, positional. |
| `--out OUT.mp4` | Where to write the stitched MP4. Default: `reel.mp4`. |
| `--title TITLE` | Title for the uploaded video (used only with `--publish`). Default: the basename of `--out` without its extension (e.g. `reel` for `reel.mp4`). The engine is repo-agnostic and has no notion of a PRD, so a descriptive title is the caller's job — the dot-agent-deck adapter composes one and passes it through here. |
| `--publish` | After stitching, upload the MP4 unlisted to YouTube and print the URL. Requires the YouTube OAuth credentials (see **Prerequisites**). |
| `-h`, `--help` | Print usage and exit. |

Examples:

```sh
reel.sh manifest.json --out reel.mp4                                   # stitch only, no upload
reel.sh manifest.json --out reel.mp4 --publish                         # stitch + upload unlisted (title = "reel")
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

1. **Card.** A synthetic asciinema cast paints the **bold title** and the
   **dim, word-wrapped, block-centered description** as terminal text, declared
   at the entry's terminal geometry (a `.cast` clip's own `cols`/`rows`; a
   sensible default for a `gif`/`mp4`). It is rendered through the **same `agg`
   invocation** (theme, font, size, fps) as the clips, so the card is
   pixel-identical to a `.cast` clip by construction. The card's **hold**
   duration is a **flat `CARD_HOLD` seconds** (default **4s**,
   env-overridable), independent of how much text the card carries: a fixed,
   deliberately short hold keeps the reel moving, and a viewer who wants to read
   a long description pauses the video rather than the reel parking on every
   long card. (The description's rendered line count still drives the card
   **grid height** so long text doesn't clip — only the hold stopped depending
   on it.) The hold is enforced at the
   **ffmpeg** level — a single painted still
   is frozen from the rendered card and looped to *exactly* the hold duration —
   so it is decoupled from `agg`'s idle handling (which would otherwise collapse
   the static tail to a couple of seconds).
2. **Clip.** A `.cast` is first **re-timed** (`retime.sh` rewrites its event
   timestamps for a watchable cadence — see below), then rendered with that same
   `agg` invocation; a pre-rendered `gif`/`mp4` is used as-is (no re-timing).

## Clip re-timing

e2e casts are recorded at machine speed, so their raw timeline is unwatchable: a keypress and the full repaint it triggers land within a millisecond of each other, while short real waits (daemon startup, polling, debounce) sit between them. A single global `agg --speed` cannot fix this — slowing everything stretches the waits into dead air and still cannot spread coincident events apart. So before rendering, every `.cast` clip is passed through `retime.sh`, which rebuilds the timeline from the event payloads (rendering then runs at `CLIP_SPEED` 1.0):

- **Classification is by output-payload size.** A terminal UI like ratatui emits a *minimal diff* per keypress (a typed char is a small payload) but a *full-region repaint* for an operation (opening a deck/form/pane is a large payload). Events at or below `SIZE_THRESHOLD` bytes are typed chars; larger ones are operations. In this repo's real casts char diffs top out at ~48 bytes and the smallest operation repaint is ~106 bytes — a clean, wide gap straddled by the default threshold (80).
- **Typing** chars are each given their own step `TYPE_GAP` apart, so typing replays at a natural, readable speed.
- **Operations** are held `OP_HOLD` after the repaint so the new state is actually visible; consecutive large chunks within `COALESCE_GAP` of each other are one logical repaint and are coalesced into a single step.
- **Idle** waits (an original gap at/above `IDLE_THRESHOLD`) are clamped to `IDLE_CAP`, killing dead air while still reading as a pause.
- `agg`'s static last-frame hold is left intact, so the final state lingers.

`retime.sh` is repo-agnostic (it operates on any `.cast`) and standalone (`retime.sh [INPUT.cast] [--out OUT.cast]`, reading stdin / writing stdout by default). Its tunables are env-overridable, like the engine's `CLIP_SPEED` (sizes in bytes, everything else in seconds): `SIZE_THRESHOLD` (80), `TYPE_GAP` (0.1), `OP_HOLD` (1.4), `IDLE_CAP` (0.4), `IDLE_THRESHOLD` (0.3), `COALESCE_GAP` (0.05). `CLIP_SPEED` (default 1.0) remains as a global multiplier layered on top of the re-timer for the rare clip that wants a uniform nudge.

Every segment is then **normalized** (`ffmpeg scale` + `pad`) to one common resolution — the max across all segments — at a constant fps and `yuv420p`, so the segments share resolution/fps/pixfmt. This is the safety net for any clip recorded at a different terminal size. The normalized segments are concatenated into a single uniform video stream (`reel.mp4` by default).

## Local smoke test

A re-runnable smoke builds a reel from a tiny self-contained fixture (two hand-written `.cast` clips + a manifest under `.claude/skills/demo-reel/tests/fixtures/`) in **stitch-only** mode (no network, no credentials) and asserts the result with `ffprobe`: non-empty file, exactly one video stream at the expected resolution (a single stream proves there is no resolution/fps/pixfmt seam between segments), `yuv420p`, constant `30/1` fps, and a duration at least the sum of the per-card holds. It is **local-only** (never CI):

```sh
task reel-smoke
# or directly:
.claude/skills/demo-reel/tests/smoke.sh
```

## Verifying the upload path

The live YouTube upload cannot be a routine automated test, so it is verified by code review of `upload.sh` plus a **one-time manual** check: with the three `YOUTUBE_*` credentials exported, run

```sh
.claude/skills/demo-reel/reel.sh some-manifest.json --out reel.mp4 --publish
```

and confirm it prints an `https://youtu.be/<id>` URL that opens an **unlisted** video. All hosting lives in `upload.sh` alone, so swapping hosts later does not touch the rest of the engine.

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
