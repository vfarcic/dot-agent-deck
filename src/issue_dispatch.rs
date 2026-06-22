//! Pure GitHub-layer helpers for the `issue_dispatch` scheduled-task type
//! (PRD #120). This module is the **foundation / pure-data layer only**: prompt
//! templating, per-issue path & branch derivation, `gh` argv construction, and
//! the idempotency decision. None of it spawns processes, touches the
//! filesystem, or wires the fire-time dispatch callback — those land in a later
//! task that composes #127's `spawn` primitive over the values these functions
//! produce.
//!
//! The config type that carries an issue-dispatch task's GitHub-specific knobs
//! lives next to the rest of the schedules schema as
//! [`crate::config::IssueDispatchConfig`]; the shared scheduler fields (`name`,
//! `cron`, `working_dir`, `prompt`, `enabled`) come from the enclosing
//! [`crate::config::ScheduledTask`]. The functions here take primitives rather
//! than the config struct so they stay decoupled and trivially unit-testable.

use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// U2 — prompt templating + default name
// ---------------------------------------------------------------------------

/// The single placeholder substituted in an issue-dispatch prompt template at
/// fire time. Documented in the PRD as user-owned: the user may change the
/// surrounding prompt freely, but this token is what each issue's number lands
/// in.
pub const ISSUE_NUMBER_PLACEHOLDER: &str = "{{issue_number}}";

/// The default-seed prompt template for a newly-created issue-dispatch task.
/// The user can replace it with anything (e.g. `/prd-full {{issue_number}}`);
/// the agent deduces the repo/URL from the worktree it runs in, so the issue
/// number alone is enough.
pub const DEFAULT_ISSUE_PROMPT_TEMPLATE: &str = "Work on issue {{issue_number}}";

/// Substitute every [`ISSUE_NUMBER_PLACEHOLDER`] occurrence in `template` with
/// `issue_number`. A template with no placeholder is returned unchanged (the
/// user opted out of interpolation) — the prompt is user-owned, so this never
/// errors or appends a context block.
pub fn substitute_issue_number(template: &str, issue_number: u64) -> String {
    template.replace(ISSUE_NUMBER_PLACEHOLDER, &issue_number.to_string())
}

/// The default-seed task name for an issue-dispatch task targeting `repo`:
/// `Issues <repo>`. The name is the reuse key (renames forbidden), so it is
/// resolved once at creation time when the repo is known.
pub fn default_issue_dispatch_name(repo: &str) -> String {
    format!("Issues {repo}")
}

// ---------------------------------------------------------------------------
// U3 — per-issue path & branch derivation
// ---------------------------------------------------------------------------

/// The deterministic filesystem layout + branch for one dispatched issue
/// (PRD #120 locked decisions). Pure data so the fire-time flow can derive it
/// without touching disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePaths {
    /// The repo clone directory: `<working_dir>/<name>`.
    pub clone_dir: PathBuf,
    /// The per-issue worktree: `<clone_dir>/.worktrees/issue-<n>`.
    pub worktree_dir: PathBuf,
    /// The per-issue branch: `agent/issue-<n>`.
    pub branch: String,
}

/// The deterministic per-issue branch name: `agent/issue-<n>`. This is the
/// idempotency key the secondary PR check (U4 `pr_list_for_issue_argv`) matches
/// on, so it is exposed on its own.
pub fn issue_branch(issue_number: u64) -> String {
    format!("agent/issue-{issue_number}")
}

/// Derive the clone dir, per-issue worktree dir, and branch for `issue_number`,
/// given the task's `working_dir` (the workspace root) and `name` (the reuse
/// key, also the clone's directory name). See [`IssuePaths`].
pub fn derive_issue_paths(working_dir: &Path, name: &str, issue_number: u64) -> IssuePaths {
    let clone_dir = working_dir.join(name);
    let worktree_dir = clone_dir
        .join(".worktrees")
        .join(format!("issue-{issue_number}"));
    IssuePaths {
        clone_dir,
        worktree_dir,
        branch: issue_branch(issue_number),
    }
}

// ---------------------------------------------------------------------------
// U4 — `gh` argv construction
// ---------------------------------------------------------------------------

/// Build the `gh issue list` argv — the arguments AFTER the `gh` program, i.e.
/// what the fire-time flow passes to `Command::new("gh").args(..)`.
///
/// Always lists OPEN issues as JSON carrying at least the issue `number`, capped
/// at `max_per_run`. Appends `--label <label>` when a label filter is set and
/// `--search <query>` when a raw query override is set; both are independent and
/// omitted when `None` (the default = all open issues up to the cap).
pub fn issue_list_argv(
    repo: &str,
    max_per_run: usize,
    label: Option<&str>,
    query: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "issue".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--state".to_string(),
        "open".to_string(),
        "--json".to_string(),
        "number".to_string(),
        "--limit".to_string(),
        max_per_run.to_string(),
    ];
    if let Some(label) = label {
        argv.push("--label".to_string());
        argv.push(label.to_string());
    }
    if let Some(query) = query {
        argv.push("--search".to_string());
        argv.push(query.to_string());
    }
    argv
}

/// Build the `gh pr list` argv (arguments after `gh`) for the secondary
/// idempotency check: an OPEN PR whose HEAD branch is `agent/issue-<n>` means
/// the issue is already in flight. Keying on the deterministic head branch is
/// more reliable than parsing `Closes #n` from PR bodies (PRD #120).
pub fn pr_list_for_issue_argv(repo: &str, issue_number: u64) -> Vec<String> {
    vec![
        "pr".to_string(),
        "list".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--state".to_string(),
        "open".to_string(),
        "--head".to_string(),
        issue_branch(issue_number),
        "--json".to_string(),
        "number".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// U5 — idempotency decision
// ---------------------------------------------------------------------------

/// Whether a candidate issue should be dispatched or skipped (PRD #120). The
/// worktree is the ledger — no separate state file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchDecision {
    /// Provision the worktree and spawn an agent for this issue.
    Dispatch,
    /// Skip — the issue is already claimed.
    Skip,
}

/// Decide dispatch-vs-skip from the two idempotency signals: the per-issue
/// worktree already exists (primary), or an open PR's HEAD branch is
/// `agent/issue-<n>` (secondary). Either being true means the issue is already
/// claimed → [`DispatchDecision::Skip`]; only when both are false do we
/// dispatch.
pub fn dispatch_decision(
    worktree_exists: bool,
    open_pr_with_matching_head: bool,
) -> DispatchDecision {
    if worktree_exists || open_pr_with_matching_head {
        DispatchDecision::Skip
    } else {
        DispatchDecision::Dispatch
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- U2: prompt templating + default name ---

    #[test]
    fn substitute_issue_number_replaces_placeholder() {
        assert_eq!(
            substitute_issue_number("Work on issue {{issue_number}}", 42),
            "Work on issue 42"
        );
        // Multiple occurrences are all substituted.
        assert_eq!(
            substitute_issue_number("#{{issue_number}} -> {{issue_number}}", 7),
            "#7 -> 7"
        );
        // The default seed substitutes as documented.
        assert_eq!(
            substitute_issue_number(DEFAULT_ISSUE_PROMPT_TEMPLATE, 120),
            "Work on issue 120"
        );
    }

    #[test]
    fn substitute_issue_number_leaves_placeholderless_template_unchanged() {
        assert_eq!(
            substitute_issue_number("/prd-full please", 9),
            "/prd-full please"
        );
    }

    #[test]
    fn default_issue_dispatch_name_is_issues_repo() {
        assert_eq!(
            default_issue_dispatch_name("vfarcic/dot-ai"),
            "Issues vfarcic/dot-ai"
        );
    }

    // --- U3: path & branch derivation ---

    #[test]
    fn derive_issue_paths_exact_layout() {
        let paths = derive_issue_paths(Path::new("/work/space"), "Issues vfarcic/dot-ai", 17);
        assert_eq!(
            paths.clone_dir,
            PathBuf::from("/work/space/Issues vfarcic/dot-ai")
        );
        assert_eq!(
            paths.worktree_dir,
            PathBuf::from("/work/space/Issues vfarcic/dot-ai/.worktrees/issue-17")
        );
        assert_eq!(paths.branch, "agent/issue-17");
    }

    #[test]
    fn issue_branch_is_deterministic() {
        assert_eq!(issue_branch(1), "agent/issue-1");
        assert_eq!(issue_branch(999), "agent/issue-999");
    }

    // --- U4: gh argv construction ---

    #[test]
    fn issue_list_argv_no_filters() {
        assert_eq!(
            issue_list_argv("vfarcic/dot-ai", 5, None, None),
            vec![
                "issue",
                "list",
                "--repo",
                "vfarcic/dot-ai",
                "--state",
                "open",
                "--json",
                "number",
                "--limit",
                "5",
            ]
        );
    }

    #[test]
    fn issue_list_argv_label_only() {
        assert_eq!(
            issue_list_argv("vfarcic/dot-ai", 3, Some("agent-eligible"), None),
            vec![
                "issue",
                "list",
                "--repo",
                "vfarcic/dot-ai",
                "--state",
                "open",
                "--json",
                "number",
                "--limit",
                "3",
                "--label",
                "agent-eligible",
            ]
        );
    }

    #[test]
    fn issue_list_argv_query_override() {
        assert_eq!(
            issue_list_argv("vfarcic/dot-ai", 10, None, Some("is:open sort:created-asc")),
            vec![
                "issue",
                "list",
                "--repo",
                "vfarcic/dot-ai",
                "--state",
                "open",
                "--json",
                "number",
                "--limit",
                "10",
                "--search",
                "is:open sort:created-asc",
            ]
        );
    }

    #[test]
    fn issue_list_argv_label_and_query_both_present() {
        assert_eq!(
            issue_list_argv("o/r", 2, Some("bug"), Some("milestone:v1")),
            vec![
                "issue",
                "list",
                "--repo",
                "o/r",
                "--state",
                "open",
                "--json",
                "number",
                "--limit",
                "2",
                "--label",
                "bug",
                "--search",
                "milestone:v1",
            ]
        );
    }

    #[test]
    fn pr_list_for_issue_argv_keys_on_head_branch() {
        assert_eq!(
            pr_list_for_issue_argv("vfarcic/dot-ai", 17),
            vec![
                "pr",
                "list",
                "--repo",
                "vfarcic/dot-ai",
                "--state",
                "open",
                "--head",
                "agent/issue-17",
                "--json",
                "number",
            ]
        );
    }

    // --- U5: idempotency decision (truth table) ---

    #[test]
    fn dispatch_decision_truth_table() {
        // Neither signal → dispatch.
        assert_eq!(dispatch_decision(false, false), DispatchDecision::Dispatch);
        // Worktree present (primary) → skip.
        assert_eq!(dispatch_decision(true, false), DispatchDecision::Skip);
        // Open PR on the head branch (secondary) → skip.
        assert_eq!(dispatch_decision(false, true), DispatchDecision::Skip);
        // Both → skip.
        assert_eq!(dispatch_decision(true, true), DispatchDecision::Skip);
    }
}
