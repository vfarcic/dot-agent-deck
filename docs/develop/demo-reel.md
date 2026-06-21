# Demo Reel

> **Developer / maintainer reference.** This page documents an internal release-workflow tool and is intentionally excluded from the published documentation site. It renders as plain Markdown here on GitHub.

When a PRD's e2e tests pass, the only way to confirm the implemented behavior is what was intended is to read the test code or replay each per-test asciinema cast one at a time. The **demo reel** turns those casts into one watchable artifact: for each e2e test a PRD adds or changes, it shows a title/description card (the test's `## Scenario`), then plays that test's recording, then moves to the next — concatenated in catalog order — and uploads the result **unlisted to YouTube** so it can ride along with the PR. See PRD #180 for the full rationale.

The tooling splits cleanly into two skills:

- **Engine** — [`.claude/skills/demo-reel/`](../../.claude/skills/demo-reel/SKILL.md) (`reel.sh`). Repo-agnostic: its only input is a `manifest.json`. It renders the cards, stitches the MP4, and optionally uploads it. It knows nothing about Rust, tests, PRDs, `tests/CATALOG.md`, or `.dot-agent-deck/recordings/`.
- **Adapter** — [`.claude/skills/demo-reel-adapter/`](../../.claude/skills/demo-reel-adapter/SKILL.md) (`build.sh`). dot-agent-deck-specific: it discovers which e2e tests this branch changed, lifts their title/description from the generated `test.md`, builds the manifest, and invokes the engine.

The **only contract** between the two is the manifest.

## Manifest contract

`manifest.json` is a non-empty JSON **array** of objects, in the order the segments should appear. Each object has three non-empty string fields:

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

| Field | Meaning |
| --- | --- |
| `title` | The card's bold headline. In the repo flow this is the `test.md` H1 (catalog id + headline). |
| `description` | The card's dim, word-wrapped body. In the repo flow this is the test's `## Scenario` paragraph. |
| `clip` | Path to the recording shown after the card: an existing `.cast` (asciinema v2), `.gif`, or `.mp4`. Resolved relative to the engine's current working directory. |

The `clip` format is intentionally open — a cast renderer (`agg`) is just one optional front-end, so an already-rendered `gif`/`mp4` can be fed straight in. The engine rejects a manifest that breaks any of these rules (not a non-empty array, an entry that is not an object, an empty/missing field, a `clip` whose extension is not `.cast`/`.gif`/`.mp4`, or a `clip` file that does not exist) with a specific message and a non-zero exit.

## Prerequisites

The engine **checks these before doing any work** and fails fast with a message naming exactly what is missing; it never self-installs anything. Inside `devbox shell` all four CLIs are already on `PATH` — they are pinned in [`devbox.json`](../../devbox.json):

| CLI | Used for | devbox package |
| --- | --- | --- |
| `agg` | render an asciinema cast to frames | `asciinema-agg` |
| `ffmpeg` | stitch and encode the final MP4 | `ffmpeg` |
| `jq` | parse and validate the manifest | `jq` |
| `curl` | upload to YouTube (`--publish`) | `curl` |

A missing CLI is a **hard** failure (the reel cannot be built). YouTube credentials are different: they are needed **only with `--publish`**, and a stitch-only run never touches them.

### YouTube OAuth credentials

Publishing reads three values from the environment (never hardcoded). In this repo they are sourced from [`vals`](../../.env.vals.yaml) — `vals env -export -f .env.vals.yaml` resolves the `ref+gcpsecrets://…` references into the process environment, which `devbox shell` does for you when `USE_VALS` is set:

| Env var | Meaning | vals reference |
| --- | --- | --- |
| `YOUTUBE_CLIENT_ID` | OAuth client id | `ref+gcpsecrets://vfarcic/youtube-client-id` |
| `YOUTUBE_CLIENT_SECRET` | OAuth client secret | `ref+gcpsecrets://vfarcic/youtube-client-secret` |
| `YOUTUBE_REFRESH_TOKEN` | OAuth refresh token (minted once via human consent) | `ref+gcpsecrets://vfarcic/youtube-refresh-token` |

> **One-time human provisioning.** The refresh token requires Google OAuth user consent, which a service account cannot grant cleanly for a personal channel, so the agent cannot self-provision it. Mint it once with the recipe below and store the three values in the secrets backend so the `vals` references resolve. After that, every reel run reuses the stored refresh token.

## Minting the YouTube refresh token (one-time)

Do this once per channel that will host reels. All of it happens in the [Google Cloud Console](https://console.cloud.google.com/) plus a couple of `curl` calls.

1. **Pick (or create) a Google Cloud project** for the channel's owner account.
2. **Enable the API.** APIs & Services → Library → enable **YouTube Data API v3** for that project.
3. **Configure the OAuth consent screen.** APIs & Services → OAuth consent screen → User type **External**. Add the channel owner's Google account as a **Test user** (a test app does not need verification for your own use), and add the scope `https://www.googleapis.com/auth/youtube.upload`.
4. **Create the OAuth client.** APIs & Services → Credentials → Create credentials → **OAuth client ID** → Application type **Desktop app** (the "TV and Limited Input devices" type also works). This yields the **client id** and **client secret**.
5. **Run the consent flow with offline access** to mint a refresh token. In a browser, signed in as the channel owner, open (substituting your client id):

   ```text
   https://accounts.google.com/o/oauth2/v2/auth?client_id=YOUR_CLIENT_ID&redirect_uri=http://localhost&response_type=code&scope=https://www.googleapis.com/auth/youtube.upload&access_type=offline&prompt=consent
   ```

   `access_type=offline` plus `prompt=consent` are what force Google to return a **refresh** token (without them you only get a short-lived access token). Approve the consent screen; the browser redirects to a `http://localhost/?code=…` URL that fails to load — that is expected. Copy the `code` value out of the address bar.
6. **Exchange the authorization code for tokens:**

   ```sh
   curl --silent --request POST https://oauth2.googleapis.com/token \
     --data-urlencode "client_id=YOUR_CLIENT_ID" \
     --data-urlencode "client_secret=YOUR_CLIENT_SECRET" \
     --data-urlencode "code=THE_CODE_FROM_STEP_5" \
     --data-urlencode "redirect_uri=http://localhost" \
     --data-urlencode "grant_type=authorization_code" | jq .
   ```

   The response JSON contains `refresh_token`. That value is the `YOUTUBE_REFRESH_TOKEN`.
7. **Store the three secrets** in the backend the repo's `.env.vals.yaml` references (GCP Secret Manager, under the `vfarcic` project), so the `ref+gcpsecrets://…` references resolve:

   ```sh
   printf '%s' 'YOUR_CLIENT_ID'     | gcloud secrets create youtube-client-id     --data-file=- --project vfarcic
   printf '%s' 'YOUR_CLIENT_SECRET' | gcloud secrets create youtube-client-secret --data-file=- --project vfarcic
   printf '%s' 'THE_REFRESH_TOKEN'  | gcloud secrets create youtube-refresh-token --data-file=- --project vfarcic
   ```

   (Use `gcloud secrets versions add <name> --data-file=-` instead of `create` if a secret already exists.)

After this, `vals env -export -f .env.vals.yaml` (or `USE_VALS=1 devbox shell`) puts the three `YOUTUBE_*` vars in the environment, and `reel.sh … --publish` can upload.

> **The refresh token is long-lived but not eternal.** Google may revoke it (password change, scope/consent revocation, a test app left idle for months, or exceeding the per-client refresh-token limit). When that happens the upload fails at runtime with the API's own error passed through; re-run steps 5–7 to mint and store a fresh token.

## Building a reel locally

### Engine, direct invocation

Run from a directory where the manifest's `clip` paths resolve:

```sh
.claude/skills/demo-reel/reel.sh manifest.json --out reel.mp4             # stitch only, no upload
.claude/skills/demo-reel/reel.sh manifest.json --out reel.mp4 --publish   # stitch + upload unlisted, prints the YouTube URL
```

`--out` defaults to `reel.mp4`. With `--publish` the engine uploads the stitched MP4 unlisted and prints the `https://youtu.be/<id>` watch URL on stdout (all other output goes to stderr, so the URL is cleanly capturable).

### Adapter, the repo flow

The adapter builds the manifest from this repo's conventions, then forwards `--out`/`--publish` to the engine. Run it from the repo root so the default relative paths (`.dot-agent-deck/recordings`, `tests/CATALOG.md`) resolve:

```sh
.claude/skills/demo-reel-adapter/build.sh --out reel.mp4              # select → assemble → stitch only
.claude/skills/demo-reel-adapter/build.sh --out reel.mp4 --publish    # select → assemble → stitch + upload
```

It selects the e2e `#[spec]` tests this branch added/changed (diff vs `main`), reads each one's title (`test.md` H1) and `## Scenario` description, orders by catalog id, points each entry at its `full-stream.cast`, and invokes the engine. The `build.sh select` and `build.sh assemble` subcommands expose the two halves (the git-diff selection and the pure manifest assembly) for inspection — see the [adapter SKILL.md](../../.claude/skills/demo-reel-adapter/SKILL.md).

> **Casts only exist for recorded runs.** Per-test casts are written only on test **failure** or when `DOT_AGENT_DECK_RECORD=1` is set (PRD #77), and they are local-only — never produced in CI. To build a reel for *passing* tests, run the e2e suite with `DOT_AGENT_DECK_RECORD=1` first so the casts under `.dot-agent-deck/recordings/<test>/full-stream.cast` are populated. Without casts, the adapter finds nothing in scope and clean-skips.

### Smoke tests

Two re-runnable checks guard the tooling locally:

```sh
task reel-smoke           # engine: stitch a tiny fixture and assert the MP4 with ffprobe (needs agg+ffmpeg; no network)
task reel-adapter-test    # adapter: assert manifest order + L1 exclusion + clean-skip (pure shell; CI-safe)
```

The live YouTube upload cannot be a routine automated test; it is verified by code review of `upload.sh` plus a one-time manual `--publish` run (see the engine SKILL.md).

## How the orchestrator step behaves

At PRD completion the orchestrator runs the demo-reel step as part of the pre-PR gate (between the e2e gate and `/prd-done`):

1. **Record while gating.** The pre-PR e2e suite runs with `DOT_AGENT_DECK_RECORD=1`, so the passing tests' casts are populated — one e2e run, recording turned on, not a second run.
2. **Build + upload.** The adapter selects the branch's new/changed e2e tests, builds the manifest, and the engine stitches the reel and uploads it unlisted, returning the watch URL.
3. **Surface to the human pre-merge.** The orchestrator presents the link at the merge gate: `Demo reel for PRD #NNN: <unlisted YouTube link>. Watch before merging.`
4. **Post as a PR comment.** Once `/prd-done` has opened the PR, the link is posted as a PR comment so it rides along with the PR.
5. **Clean skip.** If the branch changed **no** e2e tests, the adapter writes no manifest, builds no reel, uploads nothing, and prints `skipped: no e2e tests changed on this branch`. The orchestrator records that there is no reel and why — no URL, no comment.

> **Graceful degrade on a publish failure.** A stitch-only run always succeeds without credentials. If `--publish` is requested but the `YOUTUBE_*` credentials are missing, the engine still produces the local `reel.mp4` and skips only the upload, reporting `reel is at <path>; could not publish (missing …)`. Runtime upload errors (expired/revoked token, exhausted quota, API disabled) are only knowable at upload time and are passed through from the API rather than swallowed — re-mint the refresh token (above) if it was revoked.
