---
name: worktree-prd
description: Create a git worktree for PRD work with a descriptive branch name. Infers PRD from context or asks user.
user-invocable: true
---

# Create Git Worktree for PRD

Create a git worktree with a descriptive branch name based on the PRD title.

This skill is a project-local fork of the `dot-ai` mirror of the same name (CLAUDE.md rule 13), changed only where it assumed the PRD is a `prds/` file. Since issue #1591 a new PRD lives in its GitHub issue ([`../prd-start/issue-prd.md`](../prd-start/issue-prd.md)), so the title comes from the file when one exists and from the issue otherwise.

## Workflow

### Step 1: Identify the PRD

Infer the PRD number from the current conversation. Look for references like "PRD 353", "PRD #353", or "prd-353".

If not found, ask the user: "Which PRD should I create a worktree for? (e.g., 353)"

### Step 2: Create the Worktree

If the PRD title is already known from conversation context, pass both number and title:
```bash
bash .claude/skills/worktree-prd/create.sh [number] "[title]"
```

Otherwise let the script look it up — from `prds/[number]-*.md` when that file exists, and from issue #[number]'s title otherwise:
```bash
bash .claude/skills/worktree-prd/create.sh [number]
```

### Step 3: Copy Local Settings

Copy `.claude/settings.local.json` from the main repo to the new worktree so local settings (which are not tracked in git) are available:
```bash
cp .claude/settings.local.json [worktree_path]/.claude/settings.local.json
```

If the source file doesn't exist, skip this step silently.

### Step 4: Handle Result

- If `SUCCESS=true`: report the branch name, worktree path, and suggest `cd [worktree_path]`. If `PRD_SOURCE=none`, also say the issue does not carry a PRD yet, so `/prd-start` will stop at its readiness check until `/prd-create [number]` writes one
- If `ERROR=true`: show the errors to the user and ask how to proceed

## Guidelines

- **Descriptive names**: Branch names describe the feature, not just the PRD number
- **Base on main**: Always branches from `main` for new feature work
- **Clean names**: The script keeps branch names concise and URL-safe

