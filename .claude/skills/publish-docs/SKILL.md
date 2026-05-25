---
name: publish-docs
description: Publish the docs site to GHCR with a main-<sha> tag and bump site/helm/values.yaml so Argo CD picks it up — without cutting a SemVer release. Use when docs/site changes need to go live between releases.
user-invocable: true
---

# Publish Docs (no new release)

Publishes a docs-only image to `ghcr.io/vfarcic/dot-agent-deck-docs` with a `main-<short-sha>` tag and updates `site/helm/values.yaml` so Argo CD picks it up. Does **not** create a release, version bump, binary build, Homebrew formula, Scoop manifest, or GitHub release.

## When to Use

- Docs / site changes have been merged to `main` and you want them live now.
- You don't want to cut a SemVer release just for documentation.

## When NOT to Use

- You're cutting a versioned release — use `/dot-ai-tag-release` instead. The release workflow already publishes docs as part of the release via the same underlying `docs-publish.yml` workflow.
- You have un-released non-docs (code) changes that should also ship — cut a release.

## Workflow

### Step 1: Sync main locally

The workflow dispatches against `origin/main`, so make sure you know what's there:

```bash
git fetch origin
git checkout main
git pull --rebase origin main
```

If the user is in a worktree, fetch is enough — they don't need to switch branches; `gh workflow run --ref main` dispatches against the remote ref regardless of local checkout.

### Step 2: Confirm there are docs/site changes since the last release

```bash
LAST_TAG=$(git tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1)
echo "Last release: ${LAST_TAG}"
git log --oneline "${LAST_TAG}..origin/main" -- docs/ site/
git diff --stat "${LAST_TAG}..origin/main" -- docs/ site/
```

If there are no docs/site changes since the last release, inform the user and stop — there is nothing meaningful to publish.

### Step 3: Show the user what will change

Present:
- **Current chart tag**: read from `site/helm/values.yaml` `image.tag` (e.g. `v0.26.0`).
- **New tag**: `main-<short-sha>` where short-sha is `git rev-parse --short=7 origin/main`.
- **Commits included**: the list from Step 2.

Ask the user to confirm before triggering the workflow.

### Step 4: Trigger the workflow

```bash
gh workflow run docs-publish.yml --ref main
```

### Step 5: Watch the run

```bash
sleep 5
RUN_ID=$(gh run list --workflow=docs-publish.yml --branch=main --limit 1 --json databaseId --jq '.[0].databaseId')
gh run watch "$RUN_ID"
```

### Step 6: Report result

On success, tell the user:
- The image tag that was pushed (`main-<sha>`).
- That a `chore: publish docs image main-<sha> [skip ci]` commit was pushed to `main` — they should `git pull` to pick it up.
- Argo CD will detect the `values.yaml` change and sync within a minute or two; the site at https://agent-deck.devopstoolkit.ai will update shortly after.
- The chart now points at a `main-<sha>` tag. The next `/dot-ai-tag-release` will re-pin it to `v<semver>` automatically.

## Notes

- **Same sha → no-op**: re-running on a SHA that's already published is harmless — the workflow pushes the same image bytes and the `values.yaml` diff is empty, so no commit happens.
- **`:latest` is untouched**: manual runs never push or move the `:latest` tag. That tag follows formal releases only.
- **Not for release flows**: do not run this inside `/prd-done`, `/dot-ai-prd-full`, or any release skill — it would interfere with the release path's own docs publish step.
- **No changelog fragment**: a docs-only publish is not a release, so no entry in `changelog.d/` is needed.
