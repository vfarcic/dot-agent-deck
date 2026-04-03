# CLAUDE.md — dot-agent-deck

## PERMANENT INSTRUCTIONS

1. **Ask Before Creating Branches or Worktrees**: When starting feature work (including `/prd-start`), ask the user whether they want a worktree (`/worktree-prd`) or a regular branch. Never assume one or the other.
2. **Run `cargo fmt --check` and `cargo clippy -- -D warnings` Before Committing**: Always run both `cargo fmt --check` and `cargo clippy -- -D warnings` before creating any git commit. If fmt reports issues, run `cargo fmt` to fix them. If clippy reports warnings, fix the code. Stage any fixes before committing.
