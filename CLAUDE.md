# CLAUDE.md — dot-agent-deck

## PERMANENT INSTRUCTIONS

1. **Ask Before Creating Branches or Worktrees**: When starting feature work (including `/prd-start`), ask the user whether they want a worktree (`/worktree-prd`) or a regular branch. Never assume one or the other.
2. **Run `cargo fmt --check` and `cargo clippy -- -D warnings` Before Committing**: Always run both `cargo fmt --check` and `cargo clippy -- -D warnings` before creating any git commit. If fmt reports issues, run `cargo fmt` to fix them. If clippy reports warnings, fix the code. Stage any fixes before committing.
3. **No Milestone or PRD Prefixes in Source / Test Filenames**: Name files for what they contain (e.g. `tests/event_forwarding.rs`), not for the milestone or PRD that introduced them (e.g. `tests/m2_17_event_forwarding.rs`, `src/prd76_*.rs`). Milestone tags belong in commit messages and the PRD doc — long-lived filenames must outlive the branch. Existing `m*_` test files are scheduled for cleanup; do not create new ones in that pattern.
