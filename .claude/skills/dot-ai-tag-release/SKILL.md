---
name: dot-ai-tag-release
description: Create a release tag based on accumulated changelog fragments, then prune merged worktrees and branches. Run when ready to cut a release.
user-invocable: true
---

# Create Release Tag

Create a semantic version tag based on accumulated changelog fragments.

## When to Use

Run this skill when:
- Multiple PRs have been merged with changelog fragments
- You're ready to cut a release
- After the /prd-done workflow completes (not during it)

## Workflow

### Step 1: Analyze

Run the analysis script bundled with this skill:
```bash
bash .claude/skills/dot-ai-tag-release/analyze.sh
```

If the script fails (non-zero exit) or the output contains `ERROR=true`, show the `MESSAGE` to the user and stop.

If the output contains `NO_FRAGMENTS=true`, inform the user there's nothing to release and stop.

### Step 2: Propose Version

Present the script output to the user:
1. Current version (`CURRENT_VERSION`)
2. Fragments found (the `FRAGMENTS` list with their types)
3. Proposed next version (`PROPOSED_VERSION`) based on bump type (`BUMP_TYPE`)
4. Ask for confirmation or allow override

### Step 3: Handle [skip ci]

If `SKIP_CI=true`, inform the user that tagging HEAD would prevent the release workflow from running. Create a preparation commit:
```bash
git commit --allow-empty -m "chore: prepare release [version]"
git push origin HEAD
```

### Step 4: Sync the Flake Version Pin

Do this **before** creating the tag, so the tagged tree carries the right pin.

`flake.nix` pins the released version in a `version = "..."` let-binding and hands it to the build as `DAD_VERSION`. It has to: a Nix source build gets a tarball with no `.git` for `build.rs` to describe, so with no pin the binary reports the `0.1.0` placeholder from `Cargo.toml`. `release.yml` hard-fails the release when the pin and the tag disagree, and it does so in `prepare`, which runs *after* the tag has been pushed. A missed bump therefore costs you a deleted remote tag, not just a re-run.

The pin carries **no leading `v`**. The tag is `v0.35.7`; the pin is `0.35.7`. Strip it.

```bash
# [version] is the tag as everywhere else in this skill (e.g. v0.35.8).
# [version-no-v] is that same string with the leading `v` removed (0.35.8).
sed -i.bak -E 's/^([[:space:]]*)version = "[^"]+";$/\1version = "[version-no-v]";/' flake.nix && rm flake.nix.bak
git diff flake.nix
```

The diff must show exactly one changed line: `release.yml` also refuses to release unless `flake.nix` contains exactly one anchored `version = "...";` line. Then commit and push:

```bash
git commit -am "chore: pin flake version to [version]"
git push origin HEAD
```

If the repository has no `flake.nix`, skip this step.

### Step 5: Create and Push Tag

After confirmation:
```bash
git tag -a [version] -m "[Brief description summarizing the fragments]"
git push origin [version]
```

### Step 6: Confirm Success

Show the user:
1. The tag created
2. The tag URL on GitHub (if applicable)
3. Note that CI/CD will generate release notes from the fragments

### Step 7: Clean Up Merged Worktrees and Branches

Once the release is tagged, the branches and worktrees whose work it contains are
done. Run the read-only detection script bundled with this skill:
```bash
bash .claude/skills/dot-ai-tag-release/cleanup.sh
```

Interpret the output:
- If `NOTHING_TO_CLEAN=true`, tell the user there is nothing to clean and finish.
- Otherwise present the `WORKTREES`, `LOCAL_BRANCHES`, and `REMOTE_BRANCHES` lists
  and ask the user to confirm before deleting anything.

**This step is destructive — always show the full list and get explicit
confirmation first.** Never touch the default branch (`DEFAULT_BRANCH`) or the
branch/worktree you are currently on; `.claude/skills/dot-ai-tag-release/cleanup.sh` already excludes them.

After confirmation, process the items **in this order**:

1. Remove each worktree (must come before deleting its branch — a branch checked
   out in a worktree cannot be deleted):
   ```bash
   git worktree remove [worktree_path]
   ```
   If a worktree has uncommitted changes git refuses; report it and skip rather
   than using `--force`, unless the user explicitly asks.

2. Delete each local branch:
   ```bash
   git branch -d [branch]
   ```
   Use `-d` (not `-D`) as a safety net — it refuses unmerged branches. If it
   refuses, surface the warning and ask before forcing with `-D`.

3. Delete each remote branch:
   ```bash
   git push origin --delete [branch]
   ```

Finally, prune stale worktree metadata:
```bash
git worktree prune
```

## Guidelines

- **Don't run during PR workflow**: This is a separate release activity
- **Review fragments first**: Make sure all fragments are accurate before tagging
- **Use semantic versioning**: Follow semver strictly based on fragment types. While the project is pre-1.0 (`v0.x`), the minor digit is the compatibility boundary, so `breaking` fragments bump the minor and `feature`/`bugfix` fragments are patch releases; from `1.0` onward, standard semver applies (`breaking`→major, `feature`→minor, `bugfix`→patch). The `.claude/skills/dot-ai-tag-release/analyze.sh` output already reflects this.
- **Brief tag message**: Summarize the release in 1-2 sentences
- **Never tag [skip ci] commits**: Always create a preparation commit first
- **Clean up only after tagging**: Run the cleanup step (Step 7) once the release
  is cut, never before — and always confirm the detected list with the user, since
  removing worktrees and deleting branches is destructive

