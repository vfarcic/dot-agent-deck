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
/// key). The clone-dir path component is a SANITIZED single segment of `name`
/// (see [`sanitize_clone_segment`]), so the human-friendly reuse key — including
/// the default seed `Issues <owner>/<repo>`, which carries a `/` — can never nest
/// or escape `<working_dir>/<segment>`. See [`IssuePaths`].
pub fn derive_issue_paths(working_dir: &Path, name: &str, issue_number: u64) -> IssuePaths {
    let clone_dir = working_dir.join(sanitize_clone_segment(name));
    let worktree_dir = clone_dir
        .join(".worktrees")
        .join(format!("issue-{issue_number}"));
    IssuePaths {
        clone_dir,
        worktree_dir,
        branch: issue_branch(issue_number),
    }
}

/// Reduce `name` to a SINGLE filesystem segment safe to join under a workspace
/// root: path separators (`/`, `\`) collapse to `-` and `..`/NUL are stripped, so
/// the result can never contain a separator or a parent reference and therefore
/// can never escape or nest outside `<working_dir>/<segment>` (L2 + S4).
///
/// `name` itself stays the human-friendly reuse key — only the *path component*
/// derived from it is sanitized. A name with no surviving alphanumeric character
/// (empty, or only separators/`..`/punctuation) falls back to a fixed `issues`
/// segment so a path is always derivable. An already-safe single segment (e.g.
/// `dispatch-task`) is returned unchanged.
pub fn sanitize_clone_segment(name: &str) -> String {
    let collapsed = name
        .replace(['/', '\\'], "-")
        .replace('\0', "")
        .replace("..", "");
    let trimmed = collapsed.trim();
    if trimmed.chars().any(char::is_alphanumeric) {
        trimmed.to_string()
    } else {
        "issues".to_string()
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
    // M1: end-of-options marker. `gh issue list` takes no positionals, so this is
    // a belt-and-suspenders second layer behind the leading-`-` rejection in
    // [`validate_issue_dispatch_config`] — it guarantees no later argv element can
    // be reinterpreted as a flag.
    argv.push("--".to_string());
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
        // M1: end-of-options marker (see `issue_list_argv`).
        "--".to_string(),
    ]
}

// ---------------------------------------------------------------------------
// PRD #421 Phase 2 — the visible dispatch claim
// ---------------------------------------------------------------------------
//
// A dispatched issue is labelled [`CLAIM_LABEL`] and carries one claimant
// record: a comment naming the task and the machine id that dispatched it,
// written by the account `gh` is signed in with, and edited rather than
// appended on a re-claim. There is deliberately no builder that removes a label
// or deletes a comment (PRD #421 M2.1/M2.2): the deck never releases a claim.
// `tests/dispatch_claim_never_released.rs` scans `src/` for the spellings of a
// label removal and of a REST `DELETE`.

/// The deck-owned label a dispatched issue carries (PRD #421 M2.0). Namespaced
/// so that no repository's own workflow vocabulary can collide with it, which
/// is what makes provisioning it with `--force` safe.
pub const CLAIM_LABEL: &str = "agent-deck/claimed";

/// The colour [`claim_label_provision_argv`] converges the label onto.
pub const CLAIM_LABEL_COLOR: &str = "1D76DB";

/// The description [`claim_label_provision_argv`] converges the label onto.
pub const CLAIM_LABEL_DESCRIPTION: &str = "An agent was dispatched to this issue by dot-agent-deck";

/// The fixed marker opening every claimant record, which any deck can find.
/// Task and machine id are fields inside the body, not part of the marker.
pub const CLAIM_RECORD_MARKER: &str = "<!-- dot-agent-deck:claim -->";

const CLAIM_RECORD_TASK_FIELD: &str = "- Task: ";
const CLAIM_RECORD_HOST_FIELD: &str = "- Machine: ";

/// The domain separator [`claim_host_id`] hashes in front of the host name, so
/// the digest differs from a plain SHA-256 of the bare name computed for some
/// other purpose. The `v1` is the scheme's version: a different derivation gets
/// a new separator and a new id prefix rather than silently changing ids.
const CLAIM_HOST_DOMAIN: &[u8] = b"dot-agent-deck/claim-host/v1\0";

/// The machine id a claimant record names instead of the raw host name
/// (PRD #421 amendment item 8): `h1-` and the first 16 hex digits of SHA-256
/// over [`CLAIM_HOST_DOMAIN`] followed by `hostname`. Stable for a machine
/// whose host name does not change, which is what lets a re-claim find its own
/// record.
///
/// It keeps the raw name out of the public comment, but it is NOT
/// anonymisation: the digest is unsalted and host names are low-entropy, so
/// anyone who can guess a name can confirm it by hashing it the same way.
pub fn claim_host_id(hostname: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(CLAIM_HOST_DOMAIN);
    hasher.update(hostname.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("h1-{hex}")
}

/// Build the `gh api` argv (arguments after `gh`) that reads the account `gh`
/// is signed in with — whose numeric `id` decides which claimant records are
/// this deck's own (PRD #421 amendment item 6).
pub fn authenticated_user_argv() -> Vec<String> {
    vec!["api".to_string(), "user".to_string()]
}

/// Read the numeric account id out of the output of
/// [`authenticated_user_argv`]. Anything without one is an error: without the
/// id no record can be proven to be this deck's.
pub fn parse_authenticated_user_id(json: &str) -> Result<u64, String> {
    #[derive(serde::Deserialize)]
    struct User {
        id: u64,
    }
    serde_json::from_str::<User>(json)
        .map(|user| user.id)
        .map_err(|e| format!("failed to read the signed-in GitHub account: {e}"))
}

/// Build the `gh label create` argv (arguments after `gh`) that provisions
/// [`CLAIM_LABEL`] in `repo`, creating it or, with `--force`, converging an
/// existing one onto the deck's colour and description. Every value after
/// `create` is a constant or the validated `repo` slug, so no element can be
/// read as an unexpected flag.
pub fn claim_label_provision_argv(repo: &str) -> Vec<String> {
    vec![
        "label".to_string(),
        "create".to_string(),
        CLAIM_LABEL.to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--force".to_string(),
        "--color".to_string(),
        CLAIM_LABEL_COLOR.to_string(),
        "--description".to_string(),
        CLAIM_LABEL_DESCRIPTION.to_string(),
    ]
}

/// Build the `gh issue edit` argv (arguments after `gh`) that adds
/// [`CLAIM_LABEL`] to `issue_number`, with the issue number after the `--`
/// end-of-options marker (see `issue_list_argv`).
pub fn claim_issue_argv(repo: &str, issue_number: u64) -> Vec<String> {
    vec![
        "issue".to_string(),
        "edit".to_string(),
        "--repo".to_string(),
        repo.to_string(),
        "--add-label".to_string(),
        CLAIM_LABEL.to_string(),
        "--".to_string(),
        issue_number.to_string(),
    ]
}

/// Build the `gh api` argv (arguments after `gh`) listing every comment on
/// `issue_number`, which is where an existing claimant record is found. The
/// endpoint starts with `repos/`, so it cannot be read as a flag.
pub fn issue_comments_argv(repo: &str, issue_number: u64) -> Vec<String> {
    vec![
        "api".to_string(),
        "--paginate".to_string(),
        format!("repos/{repo}/issues/{issue_number}/comments"),
    ]
}

/// Build the `gh api` argv (arguments after `gh`) that posts `body` as a new
/// comment on `issue_number` — used when the issue carries no claimant record
/// of this deck's yet. `-f` sends the body as a raw string, never a file read.
pub fn create_claim_record_argv(repo: &str, issue_number: u64, body: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        "--method".to_string(),
        "POST".to_string(),
        format!("repos/{repo}/issues/{issue_number}/comments"),
        "-f".to_string(),
        format!("body={body}"),
    ]
}

/// Build the `gh api` argv (arguments after `gh`) that replaces the body of
/// the existing claimant record `comment_id` — a re-claim edits the record
/// rather than appending another one.
pub fn edit_claim_record_argv(repo: &str, comment_id: u64, body: &str) -> Vec<String> {
    vec![
        "api".to_string(),
        "--method".to_string(),
        "PATCH".to_string(),
        format!("repos/{repo}/issues/comments/{comment_id}"),
        "-f".to_string(),
        format!("body={body}"),
    ]
}

/// What a claimant record's body says about who wrote it: the scheduled task
/// and the machine id ([`claim_host_id`]) it ran on. Who actually authored the
/// comment is not part of the body — [`find_own_claim_record`] takes that from
/// the comment's `user.id`, which GitHub sets and a body cannot forge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRecord {
    pub task: String,
    pub host: String,
}

/// Render the body of a claimant record for `task` on the machine `host_id`
/// (a [`claim_host_id`]): the marker on the first line, a sentence for a human
/// reading the issue, then the two fields. Each field value is a JSON string,
/// so a task name carrying quotes or a newline still renders on one line and
/// parses back exactly. Only the machine id is repeated in the sentence: it is
/// hex by construction, while the task name is the user's free text.
pub fn render_claim_record(task: &str, host_id: &str) -> String {
    let quote = |value: &str| serde_json::Value::from(value).to_string();
    format!(
        "{CLAIM_RECORD_MARKER}\nAn agent was dispatched to work on this issue by dot-agent-deck \
         on machine `{host_id}`.\n\n\
         {CLAIM_RECORD_TASK_FIELD}{}\n{CLAIM_RECORD_HOST_FIELD}{}\n",
        quote(task),
        quote(host_id)
    )
}

/// Parse a comment body as a claimant record. Only a body whose first line is
/// [`CLAIM_RECORD_MARKER`] counts, so a reply quoting a record (`> <!-- …`) is
/// not mistaken for one; both fields must be present.
pub fn parse_claim_record(body: &str) -> Option<ClaimRecord> {
    let mut lines = body.trim_start().lines();
    if lines.next()?.trim_end() != CLAIM_RECORD_MARKER {
        return None;
    }
    let mut task = None;
    let mut host = None;
    for line in lines {
        let line = line.trim_end();
        if let Some(value) = line.strip_prefix(CLAIM_RECORD_TASK_FIELD) {
            task = task.or_else(|| serde_json::from_str::<String>(value).ok());
        } else if let Some(value) = line.strip_prefix(CLAIM_RECORD_HOST_FIELD) {
            host = host.or_else(|| serde_json::from_str::<String>(value).ok());
        }
    }
    Some(ClaimRecord {
        task: task?,
        host: host?,
    })
}

/// Find this deck's own claimant record in the output of
/// [`issue_comments_argv`], returning its comment id. A record is this deck's
/// only when its body names the same `task` AND the same `host_id` AND the
/// comment was written by the account `user_id` (PRD #421 amendment item 7):
/// a body is anyone's to copy, so a matching body by someone else is skipped
/// and the search goes on.
///
/// `gh api --paginate` prints one JSON array per page back to back, and every
/// page is parsed even after an own record turns up, so a stream that goes bad
/// on a later page is still an error. Malformed output — and output with no
/// page at all, which is what a listing cut short looks like — is an error,
/// never "no record", which would append a duplicate.
pub fn find_own_claim_record(
    comments_json: &str,
    task: &str,
    host_id: &str,
    user_id: u64,
) -> Result<Option<u64>, String> {
    #[derive(serde::Deserialize)]
    struct Author {
        id: u64,
    }
    #[derive(serde::Deserialize)]
    struct Comment {
        id: u64,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        user: Option<Author>,
    }
    if comments_json.trim().is_empty() {
        return Err("the issue's comments came back empty".to_string());
    }
    let mut own = None;
    for page in serde_json::Deserializer::from_str(comments_json).into_iter::<Vec<Comment>>() {
        let page = page.map_err(|e| format!("failed to parse the issue's comments: {e}"))?;
        if own.is_some() {
            continue;
        }
        own = page
            .into_iter()
            .find(|comment| {
                comment.user.as_ref().is_some_and(|user| user.id == user_id)
                    && comment
                        .body
                        .as_deref()
                        .and_then(parse_claim_record)
                        .is_some_and(|record| record.task == task && record.host == host_id)
            })
            .map(|comment| comment.id);
    }
    Ok(own)
}

/// The character budget of a claim diagnostic BEFORE escaping: longer text is
/// cut to this many characters, with a note of the full length. Escaping comes
/// after the cut, and each control or bidi character it escapes grows into a
/// sequence of up to eight characters, so the text that reaches a notification
/// or the log can be longer than this — still bounded, and never split inside
/// an escape sequence.
const MAX_CLAIM_ERROR_CHARS: usize = 500;

/// Make a claim diagnostic safe to put in a notification or the log (PRD #421
/// amendment item 11). Its text comes partly from `gh`'s stderr, which echoes
/// server responses, so before it goes anywhere:
///
/// - token-shaped strings (`ghp_…`, `gho_…`, `ghu_…`, `ghs_…`, `ghr_…`,
///   `github_pat_…`) are replaced with `<redacted>` — first, so that cutting
///   the text short cannot leave half a token behind. A prefix is matched
///   wherever it appears, including glued to the word before it
///   (`token_ghp_…`), so an ordinary word that happens to contain one is
///   redacted too: over-redacting a diagnostic costs less than leaking a token;
/// - the text is cut to [`MAX_CLAIM_ERROR_CHARS`] characters;
/// - control characters (C0 including ESC, BEL and CR, DEL, C1) and bidi
///   overrides are escaped, so the text cannot forge or rewrite a log line.
pub fn sanitize_claim_error(text: &str) -> String {
    static TOKEN: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?:gh[pousr]_|github_pat_)[A-Za-z0-9_]+")
            .expect("the token pattern is valid")
    });
    let redacted = TOKEN.replace_all(text, "<redacted>");
    crate::config_validation::escape_field_for_log(&redacted, MAX_CLAIM_ERROR_CHARS)
}

// ---------------------------------------------------------------------------
// M1 — validate the user-config GitHub knobs that flow into `gh`/`git` argv
// ---------------------------------------------------------------------------

/// Validate the GitHub-specific knobs of an `issue_dispatch` task before they
/// reach `gh`/`git`. `repo`/`label`/`query` come from hand-edited TOML and flow
/// into `gh repo clone <repo>` and `gh issue list --repo <repo> [--label …]
/// [--search …]`; even via `Command::args` (no shell) a value beginning with `-`
/// is parsed as a FLAG, and `repo` is an argument-injection vector (e.g. `ext::`,
/// `file://`, a local repo carrying hooks) run unattended by the daemon.
///
/// `repo` must be a strict GitHub `owner/name` slug — letters, digits, `.`, `_`,
/// `-` in each segment (`^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$`) — AND must not start
/// with `-` (the regex's character class permits a leading `-`, which `gh` would
/// still read as a flag). `max_per_run` must be at least 1 — a `0` cap makes
/// `gh issue list --limit 0` enumerate (and dispatch) nothing on every fire.
/// `label`/`query` are rejected when they are empty (a meaningless filter that
/// `gh` would still pass through) or start with `-` (parsed as a `gh` flag).
pub fn validate_issue_dispatch_config(
    repo: &str,
    max_per_run: usize,
    label: Option<&str>,
    query: Option<&str>,
) -> Result<(), String> {
    if repo.starts_with('-') || !is_owner_name(repo) {
        return Err(format!(
            "issue_dispatch repo {repo:?} must be a GitHub `owner/name` slug \
             (letters, digits, '.', '_', '-' in each segment; no leading '-')"
        ));
    }
    if max_per_run < 1 {
        return Err(format!(
            "issue_dispatch max_per_run must be at least 1 (a cap of {max_per_run} \
             makes `gh issue list --limit 0` dispatch nothing every fire)"
        ));
    }
    if let Some(label) = label {
        if label.is_empty() {
            return Err("issue_dispatch label must not be empty".to_string());
        }
        if label.starts_with('-') {
            return Err(format!(
                "issue_dispatch label {label:?} must not start with '-' (it would be parsed as a `gh` flag)"
            ));
        }
    }
    if let Some(query) = query {
        if query.is_empty() {
            return Err("issue_dispatch query must not be empty".to_string());
        }
        if query.starts_with('-') {
            return Err(format!(
                "issue_dispatch query {query:?} must not start with '-' (it would be parsed as a `gh` flag)"
            ));
        }
    }
    Ok(())
}

/// Whether `repo` matches `^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$` — exactly one `/`
/// with a non-empty allowed-char segment on each side.
fn is_owner_name(repo: &str) -> bool {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) => {
            !owner.is_empty()
                && !name.is_empty()
                && owner.chars().all(is_repo_char)
                && name.chars().all(is_repo_char)
        }
        _ => false,
    }
}

fn is_repo_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
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
        // A plain single-segment name is used verbatim as the clone dir.
        let paths = derive_issue_paths(Path::new("/work/space"), "dispatch-task", 17);
        assert_eq!(paths.clone_dir, PathBuf::from("/work/space/dispatch-task"));
        assert_eq!(
            paths.worktree_dir,
            PathBuf::from("/work/space/dispatch-task/.worktrees/issue-17")
        );
        assert_eq!(paths.branch, "agent/issue-17");
    }

    #[test]
    fn derive_issue_paths_sanitizes_default_seed_name_with_slash() {
        // The default-seeded name `Issues <owner>/<repo>` carries a `/`; it must
        // collapse to a single clone-dir segment, never nesting `owner/repo`.
        let paths = derive_issue_paths(Path::new("/work/space"), "Issues vfarcic/dot-ai", 17);
        assert_eq!(
            paths.clone_dir,
            PathBuf::from("/work/space/Issues vfarcic-dot-ai")
        );
        assert_eq!(
            paths.worktree_dir,
            PathBuf::from("/work/space/Issues vfarcic-dot-ai/.worktrees/issue-17")
        );
    }

    #[test]
    fn derive_issue_paths_never_escapes_working_dir() {
        // L2 + S4: absolute, `..`, and slash-laden names all map to a single safe
        // segment strictly inside the working dir.
        let wd = Path::new("/work/space");
        for name in [
            "/etc/passwd",
            "../../escape",
            "a/b/c",
            "Issues vfarcic/dot-ai",
            r"..\..\windows",
        ] {
            let clone = derive_issue_paths(wd, name, 1).clone_dir;
            assert!(
                clone.starts_with(wd),
                "clone dir {clone:?} escaped working dir for name {name:?}"
            );
            let rel = clone.strip_prefix(wd).expect("clone dir under working dir");
            assert_eq!(
                rel.components().count(),
                1,
                "clone dir {clone:?} must be ONE segment under the working dir (name {name:?})"
            );
            assert!(
                !clone.to_string_lossy().contains(".."),
                "clone dir {clone:?} must not contain `..` (name {name:?})"
            );
        }
    }

    #[test]
    fn sanitize_clone_segment_passthrough_and_fallback() {
        assert_eq!(sanitize_clone_segment("dispatch-task"), "dispatch-task");
        assert_eq!(
            sanitize_clone_segment("Issues vfarcic/dot-ai"),
            "Issues vfarcic-dot-ai"
        );
        // Reduces-to-nothing inputs fall back to a fixed segment.
        assert_eq!(sanitize_clone_segment(".."), "issues");
        assert_eq!(sanitize_clone_segment("/"), "issues");
        assert_eq!(sanitize_clone_segment(""), "issues");
        assert_eq!(sanitize_clone_segment("."), "issues");
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
                "--",
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
                "--",
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
                "--",
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
                "--",
            ]
        );
    }

    #[test]
    fn argv_builders_carry_end_of_options_separator() {
        // M1: both builders terminate with the `--` end-of-options marker.
        assert!(issue_list_argv("o/r", 1, None, None).contains(&"--".to_string()));
        assert!(pr_list_for_issue_argv("o/r", 1).contains(&"--".to_string()));
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
                "--",
            ]
        );
    }

    // --- PRD #421 Phase 2: the dispatch claim ---

    #[test]
    fn claim_label_provision_argv_forces_the_deck_owned_label() {
        assert_eq!(
            claim_label_provision_argv("vfarcic/dot-ai"),
            vec![
                "label",
                "create",
                "agent-deck/claimed",
                "--repo",
                "vfarcic/dot-ai",
                "--force",
                "--color",
                CLAIM_LABEL_COLOR,
                "--description",
                CLAIM_LABEL_DESCRIPTION,
            ]
        );
        assert_eq!(CLAIM_LABEL_COLOR.len(), 6);
        assert!(CLAIM_LABEL_COLOR.chars().all(|c| c.is_ascii_hexdigit()));
        // GitHub caps a label description at 100 characters.
        assert!(CLAIM_LABEL_DESCRIPTION.chars().count() <= 100);
    }

    #[test]
    fn claim_issue_argv_puts_the_issue_after_end_of_options() {
        assert_eq!(
            claim_issue_argv("vfarcic/dot-ai", 17),
            vec![
                "issue",
                "edit",
                "--repo",
                "vfarcic/dot-ai",
                "--add-label",
                "agent-deck/claimed",
                "--",
                "17",
            ]
        );
    }

    #[test]
    fn claim_record_argvs_address_the_issue_and_comment_endpoints() {
        assert_eq!(
            issue_comments_argv("o/r", 7),
            vec!["api", "--paginate", "repos/o/r/issues/7/comments"]
        );
        assert_eq!(
            create_claim_record_argv("o/r", 7, "hello\nworld"),
            vec![
                "api",
                "--method",
                "POST",
                "repos/o/r/issues/7/comments",
                "-f",
                "body=hello\nworld",
            ]
        );
        assert_eq!(
            edit_claim_record_argv("o/r", 4242, "hello"),
            vec![
                "api",
                "--method",
                "PATCH",
                "repos/o/r/issues/comments/4242",
                "-f",
                "body=hello",
            ]
        );
    }

    #[test]
    fn claim_record_round_trips_task_and_host() {
        let body = render_claim_record("dispatch-task", "build-box");
        assert!(body.starts_with(CLAIM_RECORD_MARKER));
        assert!(body.contains("dispatch-task") && body.contains("build-box"));
        assert_eq!(
            parse_claim_record(&body),
            Some(ClaimRecord {
                task: "dispatch-task".into(),
                host: "build-box".into(),
            })
        );
        // Awkward task names survive the round trip on a single field line.
        let tricky = "Issues vfarcic/dot-ai \"quoted\"\nsecond line";
        assert_eq!(
            parse_claim_record(&render_claim_record(tricky, "h")),
            Some(ClaimRecord {
                task: tricky.into(),
                host: "h".into(),
            })
        );
    }

    #[test]
    fn parse_claim_record_rejects_non_records() {
        // No marker.
        assert_eq!(parse_claim_record("- Task: \"t\"\n- Host: \"h\""), None);
        // A reply quoting a record is not a record.
        let quoted = render_claim_record("t", "h")
            .lines()
            .map(|l| format!("> {l}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(parse_claim_record(&quoted), None);
        // The marker alone, or a missing field, is not a record.
        assert_eq!(parse_claim_record(CLAIM_RECORD_MARKER), None);
        assert_eq!(
            parse_claim_record(&format!("{CLAIM_RECORD_MARKER}\n- Task: \"t\"")),
            None
        );
    }

    const ME: u64 = 12345;

    #[test]
    fn claim_host_id_is_a_stable_versioned_digest_of_the_host_name() {
        let id = claim_host_id("build-box");
        assert_eq!(id, claim_host_id("build-box"), "stable across calls");
        assert!(id.starts_with("h1-"), "{id}");
        let hex = &id["h1-".len()..];
        assert_eq!(hex.len(), 16, "{id}");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert!(
            !id.contains("build-box"),
            "the raw name stays out of the id"
        );
        assert_ne!(id, claim_host_id("build-box2"));
        // Domain-separated: not a prefix of the plain SHA-256 of the name.
        use sha2::{Digest, Sha256};
        let plain: String = Sha256::digest(b"build-box")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(!plain.starts_with(hex));
    }

    #[test]
    fn authenticated_user_id_is_read_from_gh_api_user() {
        assert_eq!(authenticated_user_argv(), vec!["api", "user"]);
        assert_eq!(
            parse_authenticated_user_id(r#"{"id":12345,"login":"deck-bot"}"#),
            Ok(12345)
        );
        assert!(parse_authenticated_user_id(r#"{"login":"deck-bot"}"#).is_err());
        assert!(parse_authenticated_user_id("").is_err());
        assert!(parse_authenticated_user_id("not json").is_err());
    }

    #[test]
    fn claim_record_names_the_machine_id_not_the_host_name() {
        let host_id = claim_host_id("secret-laptop");
        let body = render_claim_record("dispatch-task", &host_id);
        assert!(!body.contains("secret-laptop"), "{body}");
        let sentence = body.lines().nth(1).unwrap();
        assert!(sentence.contains(&host_id), "{sentence}");
        assert!(!sentence.contains("dispatch-task"), "{sentence}");
    }

    fn comment(id: u64, user: u64, body: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "user": {"id": user, "login": "x"}, "body": body})
    }

    #[test]
    fn find_own_claim_record_requires_same_task_host_and_author() {
        let own = render_claim_record("task", "host");
        let other_host = render_claim_record("task", "elsewhere");
        let other_task = render_claim_record("another", "host");
        let json = serde_json::json!([
            comment(1, ME, "plain comment"),
            comment(2, ME, &other_host),
            comment(3, ME, &other_task),
            comment(4, ME, &own),
            {"id": 5, "body": null, "user": null},
        ])
        .to_string();
        assert_eq!(
            find_own_claim_record(&json, "task", "host", ME),
            Ok(Some(4))
        );
        assert_eq!(
            find_own_claim_record(&json, "task", "nowhere", ME),
            Ok(None)
        );
        assert_eq!(find_own_claim_record(&json, "task", "host", 999), Ok(None));
        assert_eq!(find_own_claim_record("[]", "task", "host", ME), Ok(None));
    }

    #[test]
    fn find_own_claim_record_skips_a_forged_record_and_keeps_searching() {
        let body = render_claim_record("task", "host");
        let forged_first = serde_json::json!([
            comment(1, 99999, &body),
            {"id": 2, "body": body},
            comment(3, ME, &body),
        ])
        .to_string();
        assert_eq!(
            find_own_claim_record(&forged_first, "task", "host", ME),
            Ok(Some(3)),
            "the forged and the authorless copies are skipped; the genuine one is found"
        );
        let forged_only = serde_json::json!([comment(1, 99999, &body)]).to_string();
        assert_eq!(
            find_own_claim_record(&forged_only, "task", "host", ME),
            Ok(None)
        );
    }

    #[test]
    fn find_own_claim_record_reads_every_paginated_page() {
        let page1 = serde_json::json!([comment(1, ME, "first page")]);
        let page2 = serde_json::json!([comment(9, ME, &render_claim_record("t", "h"))]);
        let stream = format!("{page1}\n{page2}\n");
        assert_eq!(find_own_claim_record(&stream, "t", "h", ME), Ok(Some(9)));
    }

    #[test]
    fn find_own_claim_record_fails_on_malformed_output() {
        assert!(find_own_claim_record("not json", "t", "h", ME).is_err());
        assert!(find_own_claim_record("{\"id\": 1}", "t", "h", ME).is_err());
    }

    #[test]
    fn find_own_claim_record_fails_on_empty_output() {
        assert!(find_own_claim_record("", "t", "h", ME).is_err());
        assert!(find_own_claim_record(" \n\t\r\n", "t", "h", ME).is_err());
    }

    #[test]
    fn find_own_claim_record_validates_pages_after_the_own_record() {
        let page1 = serde_json::json!([comment(9, ME, &render_claim_record("t", "h"))]);
        let stream = format!("{page1}\n[{{\"id\": 10, \"body\": ");
        assert!(
            find_own_claim_record(&stream, "t", "h", ME).is_err(),
            "a later page that does not parse fails the lookup even after a match"
        );
        let stream = format!("{page1}\n{{\"message\": \"Server Error\"}}");
        assert!(find_own_claim_record(&stream, "t", "h", ME).is_err());
    }

    #[test]
    fn sanitize_claim_error_escapes_caps_and_redacts() {
        let token = format!("ghp_{}", "A1b2C3d4".repeat(5));
        let pat = format!("github_pat_{}", "x9".repeat(30));
        let raw = format!(
            "HTTP 401\u{1b}[2J\u{7}\rforged line token={token} and {pat}; {}",
            "y".repeat(2000)
        );
        let clean = sanitize_claim_error(&raw);
        for raw_char in ['\u{1b}', '\u{7}', '\r', '\n'] {
            assert!(
                !clean.contains(raw_char),
                "{raw_char:?} survived: {clean:?}"
            );
        }
        assert!(
            clean.contains("\\u{1b}") && clean.contains("\\r"),
            "{clean}"
        );
        assert!(
            !clean.contains(&token) && !clean.contains("ghp_A1b2"),
            "{clean}"
        );
        assert!(!clean.contains("github_pat_x9"), "{clean}");
        assert_eq!(clean.matches("<redacted>").count(), 2, "{clean}");
        assert!(
            clean.chars().count() < 600,
            "capped: {} chars",
            clean.chars().count()
        );
        // Every gh token prefix is redacted.
        for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
            let text = format!("x {prefix}abc123 y");
            assert_eq!(sanitize_claim_error(&text), "x <redacted> y");
        }
        // A token glued to the word before it — by an underscore, a letter, a
        // digit or a non-ASCII letter — is redacted all the same; only the
        // prefix word survives.
        for (text, expected) in [
            ("xghp_abc", "x<redacted>"),
            ("token_ghp_abc123 y", "token_<redacted> y"),
            ("prefixgithub_pat_11AB_cd y", "prefix<redacted> y"),
            ("id7gho_abc y", "id7<redacted> y"),
            ("cléghs_abc y", "clé<redacted> y"),
            ("令牌ghr_abc y", "令牌<redacted> y"),
        ] {
            assert_eq!(sanitize_claim_error(text), expected, "{text}");
        }
        assert_eq!(sanitize_claim_error("plain error"), "plain error");
    }

    // --- M1: user-config validation ---

    #[test]
    fn validate_issue_dispatch_config_accepts_valid_slug_and_filters() {
        assert!(validate_issue_dispatch_config("vfarcic/dot-ai", 3, None, None).is_ok());
        assert!(
            validate_issue_dispatch_config(
                "acme/widgets.v2",
                5,
                Some("agent-eligible"),
                Some("is:open sort:created-asc")
            )
            .is_ok()
        );
        // The smallest valid cap (1) is accepted.
        assert!(validate_issue_dispatch_config("o/r", 1, None, None).is_ok());
    }

    #[test]
    fn validate_issue_dispatch_config_rejects_bad_repo() {
        // Not an owner/name slug.
        assert!(validate_issue_dispatch_config("not-a-slug", 3, None, None).is_err());
        assert!(validate_issue_dispatch_config("a/b/c", 3, None, None).is_err());
        assert!(validate_issue_dispatch_config("owner/", 3, None, None).is_err());
        assert!(validate_issue_dispatch_config("/name", 3, None, None).is_err());
        // Injection-shaped values.
        assert!(validate_issue_dispatch_config("ext::sh -c id", 3, None, None).is_err());
        assert!(validate_issue_dispatch_config("file:///etc", 3, None, None).is_err());
        // Leading `-` would be read as a `gh` flag even though the char is in the
        // slug character class.
        assert!(validate_issue_dispatch_config("-x/y", 3, None, None).is_err());
    }

    #[test]
    fn validate_issue_dispatch_config_rejects_max_per_run_below_one() {
        // A 0 cap makes `gh issue list --limit 0` dispatch nothing every fire.
        let err = validate_issue_dispatch_config("o/r", 0, None, None).unwrap_err();
        assert!(
            err.contains("max_per_run"),
            "error should name the offending field, got {err:?}"
        );
        // A valid slug with a 0 cap is still rejected (the cap, not the slug).
        assert!(validate_issue_dispatch_config("vfarcic/dot-ai", 0, None, None).is_err());
    }

    #[test]
    fn validate_issue_dispatch_config_rejects_empty_label_or_query() {
        // An empty filter is meaningless and would still be passed through to `gh`.
        let label_err = validate_issue_dispatch_config("o/r", 3, Some(""), None).unwrap_err();
        assert!(
            label_err.contains("label"),
            "error should name the label, got {label_err:?}"
        );
        let query_err = validate_issue_dispatch_config("o/r", 3, None, Some("")).unwrap_err();
        assert!(
            query_err.contains("query"),
            "error should name the query, got {query_err:?}"
        );
    }

    #[test]
    fn validate_issue_dispatch_config_rejects_leading_dash_label_or_query() {
        assert!(validate_issue_dispatch_config("o/r", 3, Some("-rf"), None).is_err());
        assert!(validate_issue_dispatch_config("o/r", 3, None, Some("--owner")).is_err());
        // Non-leading dashes are fine.
        assert!(validate_issue_dispatch_config("o/r", 3, Some("agent-eligible"), None).is_ok());
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
