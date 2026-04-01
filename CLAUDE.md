# CLAUDE.md — dot-agent-deck

## PERMANENT INSTRUCTIONS

1. **Ask Before Creating Branches or Worktrees**: When starting feature work (including `/prd-start`), ask the user whether they want a worktree (`/worktree-prd`) or a regular branch. Never assume one or the other.
2. **Run `cargo fmt --check` Before Committing**: Always run `cargo fmt --check` before creating any git commit. If it reports formatting issues, run `cargo fmt` to fix them, then stage the changes before committing.
