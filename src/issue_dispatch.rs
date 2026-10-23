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

/// The deck-owned label a dispatched issue carries (PRD #421 chose it over
/// assigning the issue or a bare `in-progress` label). Namespaced to mark it as
/// the deck's, so that a repository's own workflow vocabulary is unlikely to
/// use the same name; `--force` provisioning converges a label that already
/// has the name onto the deck's colour and description.
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
const CLAIM_RECORD_CLAIMED_AT_FIELD: &str = "- Claimed at: ";

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
/// this deck's own (PRD #421 amendment item 6). `--include` prints the
/// response headers ahead of the body, so the same call also carries GitHub's
/// `Date`, the clock claim records are aged against (PRD #421 M3.1).
pub fn authenticated_user_argv() -> Vec<String> {
    vec![
        "api".to_string(),
        "--include".to_string(),
        "user".to_string(),
    ]
}

/// What [`parse_authenticated_user`] reads from the account lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthenticatedUser {
    pub id: u64,
    /// The response's `Date` header: GitHub's clock when it answered. `None`
    /// when the output carried no headers, or no readable `Date`.
    pub server_date: Option<chrono::DateTime<chrono::Utc>>,
}

/// Read the output of [`authenticated_user_argv`]: an optional HTTP status
/// line and header block (what `--include` prints, ended by a blank line),
/// then the JSON body with the numeric account id. Output that starts with
/// the body is read as a body with no headers. A body without an id is an
/// error: without the id no record can be proven to be this deck's.
pub fn parse_authenticated_user(output: &str) -> Result<AuthenticatedUser, String> {
    #[derive(serde::Deserialize)]
    struct User {
        id: u64,
    }
    let output = output.trim_start();
    let (server_date, body) = if output.starts_with("HTTP/") {
        let mut server_date = None;
        // No blank line means no body, which the parse below rejects.
        let mut body = "";
        let mut consumed = 0;
        for line in output.split_inclusive('\n') {
            consumed += line.len();
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                body = &output[consumed..];
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("date")
            {
                server_date = chrono::DateTime::parse_from_rfc2822(value.trim())
                    .ok()
                    .map(|date| date.with_timezone(&chrono::Utc));
            }
        }
        (server_date, body)
    } else {
        (None, output)
    };
    serde_json::from_str::<User>(body)
        .map(|user| AuthenticatedUser {
            id: user.id,
            server_date,
        })
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
/// endpoint starts with `repos/`, so it cannot be read as a flag. It asks for
/// GitHub's largest page, 100 comments, so a long thread costs as few requests
/// as it can.
pub fn issue_comments_argv(repo: &str, issue_number: u64) -> Vec<String> {
    vec![
        "api".to_string(),
        "--paginate".to_string(),
        format!("repos/{repo}/issues/{issue_number}/comments?per_page=100"),
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
/// and the machine id ([`claim_host_id`]) it ran on, and when it was last
/// claimed. Who actually authored the comment is not part of the body —
/// [`find_own_claim_record`] takes that from the comment's `user.id`, which
/// GitHub sets and a body cannot forge.
///
/// `claimed_at` is `None` for a record written before the field existed; such
/// a record is still a record, and gains the field on its next edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRecord {
    pub task: String,
    pub host: String,
    pub claimed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Render the body of a claimant record for `task` on the machine `host_id`
/// (a [`claim_host_id`]), claimed at `claimed_at`: the marker on the first
/// line, a sentence for a human reading the issue, then the fields. Each field
/// value is a JSON string, so a task name carrying quotes or a newline still
/// renders on one line and parses back exactly. Only the machine id is
/// repeated in the sentence: it is hex by construction, while the task name is
/// the user's free text.
///
/// The claim time is what makes a re-claim's body differ from the record it
/// edits: GitHub leaves a comment's `updated_at` alone when an edit repeats the
/// same body, and `updated_at` is the claim's clock. It carries milliseconds,
/// and a re-claim takes its time from [`next_claim_time`], which is at least a
/// millisecond past the time the edited record carries, so the two bodies
/// differ even when the wall clock repeats or steps back.
pub fn render_claim_record(
    task: &str,
    host_id: &str,
    claimed_at: chrono::DateTime<chrono::Utc>,
) -> String {
    let quote = |value: &str| serde_json::Value::from(value).to_string();
    let claimed_at = claimed_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    format!(
        "{CLAIM_RECORD_MARKER}\nAn agent was dispatched to work on this issue by dot-agent-deck \
         on machine `{host_id}`, last claimed at {claimed_at}.\n\n\
         {CLAIM_RECORD_TASK_FIELD}{}\n{CLAIM_RECORD_HOST_FIELD}{}\n{CLAIM_RECORD_CLAIMED_AT_FIELD}{}\n",
        quote(task),
        quote(host_id),
        quote(&claimed_at)
    )
}

/// The claim time to write when `previous` is the claim time of the record
/// being edited (`None` for a new record, or one with no readable time):
/// `max(now, previous + 1 ms)` when there is a `previous`, and `now` when
/// there is not. So `now` is used only when it is at least one millisecond
/// past `previous` — a `now` that is past `previous` by less than that still
/// yields `previous + 1 ms`. The record renders its time to the millisecond,
/// so the result always renders differently from the time the edited record
/// holds — a wall clock that repeats a millisecond, or is corrected
/// backwards, cannot re-render the same body. A record whose time is in the future therefore
/// carries each re-claim one millisecond further until the clock catches up.
pub fn next_claim_time(
    now: chrono::DateTime<chrono::Utc>,
    previous: Option<chrono::DateTime<chrono::Utc>>,
) -> chrono::DateTime<chrono::Utc> {
    previous
        .and_then(|previous| previous.checked_add_signed(chrono::Duration::milliseconds(1)))
        .map_or(now, |floor| now.max(floor))
}

/// Parse a comment body as a claimant record. Only a body whose first line is
/// [`CLAIM_RECORD_MARKER`] counts, so a reply quoting a record (`> <!-- …`) is
/// not mistaken for one; the task and machine fields must be present. The
/// claim time is optional — records written before it existed have none — and
/// one that does not parse as an RFC 3339 time reads as absent.
pub fn parse_claim_record(body: &str) -> Option<ClaimRecord> {
    let mut lines = body.trim_start().lines();
    if lines.next()?.trim_end() != CLAIM_RECORD_MARKER {
        return None;
    }
    let mut task = None;
    let mut host = None;
    let mut claimed_at = None;
    for line in lines {
        let line = line.trim_end();
        if let Some(value) = line.strip_prefix(CLAIM_RECORD_TASK_FIELD) {
            task = task.or_else(|| serde_json::from_str::<String>(value).ok());
        } else if let Some(value) = line.strip_prefix(CLAIM_RECORD_HOST_FIELD) {
            host = host.or_else(|| serde_json::from_str::<String>(value).ok());
        } else if let Some(value) = line.strip_prefix(CLAIM_RECORD_CLAIMED_AT_FIELD) {
            claimed_at = claimed_at.or_else(|| {
                serde_json::from_str::<String>(value)
                    .ok()
                    .and_then(|time| chrono::DateTime::parse_from_rfc3339(&time).ok())
                    .map(|time| time.with_timezone(&chrono::Utc))
            });
        }
    }
    Some(ClaimRecord {
        task: task?,
        host: host?,
        claimed_at,
    })
}

/// This deck's own claimant record, as [`find_own_claim_record`] found it: the
/// comment's id, its body as GitHub returned it, and the claim time that body
/// carries, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnClaimRecord {
    pub id: u64,
    pub body: String,
    pub claimed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Find this deck's own claimant record in the output of
/// [`issue_comments_argv`], returning its comment id, body and claim time. A
/// record is this deck's only when its body names the same `task` AND the
/// same `host_id` AND the comment was written by the account `user_id` (PRD
/// #421 amendment item 7), whatever claim time it carries, or none. A body is
/// anyone's to copy, so a matching body by someone else is skipped and the
/// search goes on.
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
) -> Result<Option<OwnClaimRecord>, String> {
    let me = ClaimIdentity {
        user_id,
        task,
        host_id,
    };
    Ok(parse_issue_claim_records(comments_json)?
        .into_iter()
        .find(|found| found.is_own(&me))
        .map(|found| OwnClaimRecord {
            id: found.comment_id,
            body: found.body,
            claimed_at: found.record.claimed_at,
        }))
}

// ---------------------------------------------------------------------------
// PRD #421 Phase 3 — claim-aware dispatch: reading every claim record
// ---------------------------------------------------------------------------
//
// The claim is derived from live GitHub at decision time, never from anything
// this deck remembers: every claim record on the issue (found by
// [`CLAIM_RECORD_MARKER`], whoever wrote it), each aged on its OWN clock — the
// comment's `updated_at`, floored by when the claim label was first added —
// and judged on its own (M3.2). Own records are judged on the deck's local
// evidence (its worktree, an open PR), foreign ones on the clock, and only
// when GitHub vouches for their author's standing on the repository. Whenever
// the evidence is missing the record does not gate: a wrongly-stale claim
// costs one duplicate dispatch, a wrongly-live one wedges the issue.

/// The default `claim_ttl_hours`: a week.
pub const DEFAULT_CLAIM_TTL_HOURS: u32 = 168;

/// How far ahead [`check_claim_ttl_against_cron`] walks a task's cron: a year,
/// so every day-of-week and day-of-month pattern, and both DST changes, are in
/// the walk.
const CLAIM_CRON_HORIZON_DAYS: i64 = 366;

/// The most fires [`check_claim_ttl_against_cron`] walks before it gives up
/// on computing the gap — about ten weeks of an every-minute cron.
const CLAIM_CRON_MAX_FIRES: usize = 100_000;

/// What [`check_claim_ttl_against_cron`] adds to twice the longest gap between
/// fires. The owner refreshes from a fire, but its record was written some
/// time AFTER the fire that dispatched it, and the refresh's own edit lands
/// some time after the check that decided it; an hour absorbs both delays and
/// modest clock skew, so a TTL that passes the check is not outlived by the
/// owner's next refresh by a few seconds.
pub const CLAIM_TTL_HEADROOM: chrono::Duration = chrono::Duration::hours(1);

/// How far in the future a foreign record's clock may read before it stops
/// gating. GitHub's own clock is the reference ([`claim_record_age`]), so a
/// record dated later than that is not evidence of a live claim; a few minutes
/// absorb the seconds between reading GitHub's time and deciding.
pub const CLAIM_FUTURE_TOLERANCE: chrono::Duration = chrono::Duration::minutes(5);

/// How many characters of a claim record's schedule name a log line shows.
/// The name is whatever the comment says, so it is bounded like the machine id.
const CLAIM_TASK_LOG_CHARS: usize = 64;

/// The repository standings whose claim records may gate another deck: the
/// `author_association` values GitHub gives the repository's owner, members of
/// the organization that owns it, and its collaborators. Anyone else can
/// comment on a public issue, so their records never gate.
const TRUSTED_AUTHOR_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

/// Who this deck is when it reads claim records (PRD #421 Phase 2's "own"
/// rule): the numeric id of the account `gh` is signed in with, the schedule
/// name, and the machine id ([`claim_host_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimIdentity<'a> {
    pub user_id: u64,
    pub task: &'a str,
    pub host_id: &'a str,
}

/// One claim record on an issue, as any deck reads it: the comment it lives
/// in, that comment's author and body, what the body says, and when GitHub
/// last saw the comment change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueClaimRecord {
    pub comment_id: u64,
    /// The comment author's `user.id`, which GitHub sets and a body cannot
    /// forge. `None` when the listing names no author.
    pub author_id: Option<u64>,
    /// The comment author's `user.login`, which a skip names. `None` when the
    /// listing names no author or no login.
    pub author_login: Option<String>,
    /// The comment's `author_association`: the author's standing on the
    /// repository, as GitHub reports it. `None` when the listing has none.
    pub author_association: Option<String>,
    pub body: String,
    pub record: ClaimRecord,
    /// The comment's `updated_at`: the record's clock. GitHub moves it on an
    /// edit that changes the body, not on one that repeats it (measured for
    /// M3.1), which is why a refresh renders a new claim time. `None` when the
    /// listing carried no readable time.
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl IssueClaimRecord {
    /// Whether this record is `me`'s own: written by the same account, naming
    /// the same schedule and the same machine id. Anything else is foreign.
    pub fn is_own(&self, me: &ClaimIdentity<'_>) -> bool {
        self.author_id == Some(me.user_id)
            && self.record.task == me.task
            && self.record.host == me.host_id
    }

    /// Whether GitHub reports the author as the repository's owner, a member
    /// of its organization or a collaborator ([`TRUSTED_AUTHOR_ASSOCIATIONS`]).
    /// Only such a foreign record can gate: anyone can comment on a public
    /// issue, and a record that anyone can renew would let a stranger hold an
    /// issue back indefinitely. A missing association is untrusted.
    pub fn author_is_trusted(&self) -> bool {
        self.author_association
            .as_deref()
            .is_some_and(|association| TRUSTED_AUTHOR_ASSOCIATIONS.contains(&association))
    }
}

/// Every claim record in the output of [`issue_comments_argv`], in listing
/// order. A comment counts when [`parse_claim_record`] accepts its body,
/// whoever wrote it.
///
/// `gh api --paginate` prints one JSON array per page back to back, and every
/// page is parsed. Malformed output — and output with no page at all, which is
/// what a listing cut short looks like — is an error, never "no records".
pub fn parse_issue_claim_records(comments_json: &str) -> Result<Vec<IssueClaimRecord>, String> {
    #[derive(serde::Deserialize)]
    struct Author {
        id: u64,
        #[serde(default)]
        login: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Comment {
        id: u64,
        #[serde(default)]
        body: Option<String>,
        #[serde(default)]
        user: Option<Author>,
        #[serde(default)]
        author_association: Option<String>,
        #[serde(default)]
        updated_at: Option<String>,
    }
    if comments_json.trim().is_empty() {
        return Err("the issue's comments came back empty".to_string());
    }
    let mut records = Vec::new();
    for page in serde_json::Deserializer::from_str(comments_json).into_iter::<Vec<Comment>>() {
        let page = page.map_err(|e| format!("failed to parse the issue's comments: {e}"))?;
        records.extend(page.into_iter().filter_map(|comment| {
            let body = comment.body?;
            let record = parse_claim_record(&body)?;
            let (author_id, author_login) = comment
                .user
                .map_or((None, None), |user| (Some(user.id), user.login));
            Some(IssueClaimRecord {
                comment_id: comment.id,
                author_id,
                author_login,
                author_association: comment.author_association,
                body,
                record,
                updated_at: comment.updated_at.as_deref().and_then(parse_github_time),
            })
        }));
    }
    Ok(records)
}

/// Build the `gh api` argv (arguments after `gh`) listing `issue_number`'s
/// events, where the time [`CLAIM_LABEL`] was added is found. The endpoint
/// starts with `repos/`, so it cannot be read as a flag; like
/// [`issue_comments_argv`], it asks for 100 events per page.
pub fn issue_events_argv(repo: &str, issue_number: u64) -> Vec<String> {
    vec![
        "api".to_string(),
        "--paginate".to_string(),
        format!("repos/{repo}/issues/{issue_number}/events?per_page=100"),
    ]
}

/// What the issue's events say about [`CLAIM_LABEL`], from the output of
/// [`issue_events_argv`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimLabelState {
    /// Whether the issue carries the label now: its latest `labeled` or
    /// `unlabeled` event for the label is a `labeled` one. The claim gates
    /// only while it does, so removing the label by hand releases the claim.
    pub present: bool,
    /// When the label was first added: the earliest `labeled` event for it,
    /// `None` when there is none.
    ///
    /// The earliest, because it is the floor of every record's clock: the
    /// claim cannot have existed before it. A later add cannot move it, so one
    /// deck's fresh claim cannot hold another deck's dead record open — and
    /// GitHub records no event for a redundant add of a label already present
    /// (measured for M3.1), so re-claims leave it alone anyway.
    pub added_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Read [`ClaimLabelState`] from the output of [`issue_events_argv`]. Of the
/// label's `labeled` and `unlabeled` events the one with the latest time
/// decides whether it is present, a later one in the listing winning a tie; an
/// event with no readable time is left out. Malformed or empty output is an
/// error, like [`parse_issue_claim_records`].
pub fn parse_claim_label_state(events_json: &str) -> Result<ClaimLabelState, String> {
    #[derive(serde::Deserialize)]
    struct Label {
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct Event {
        #[serde(default)]
        event: Option<String>,
        #[serde(default)]
        label: Option<Label>,
        #[serde(default)]
        created_at: Option<String>,
    }
    if events_json.trim().is_empty() {
        return Err("the issue's events came back empty".to_string());
    }
    let mut added_at: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut latest: Option<(chrono::DateTime<chrono::Utc>, bool)> = None;
    for page in serde_json::Deserializer::from_str(events_json).into_iter::<Vec<Event>>() {
        let page = page.map_err(|e| format!("failed to parse the issue's events: {e}"))?;
        for event in page {
            let labeled = match event.event.as_deref() {
                Some("labeled") => true,
                Some("unlabeled") => false,
                _ => continue,
            };
            if event.label.as_ref().map(|label| label.name.as_str()) != Some(CLAIM_LABEL) {
                continue;
            }
            let Some(time) = event.created_at.as_deref().and_then(parse_github_time) else {
                continue;
            };
            if labeled {
                added_at = Some(added_at.map_or(time, |earliest| earliest.min(time)));
            }
            if latest.is_none_or(|(latest, _)| time >= latest) {
                latest = Some((time, labeled));
            }
        }
    }
    Ok(ClaimLabelState {
        present: latest.is_some_and(|(_, labeled)| labeled),
        added_at,
    })
}

/// Parse a GitHub API timestamp (RFC 3339) as UTC; anything else is `None`.
fn parse_github_time(time: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(time)
        .ok()
        .map(|time| time.with_timezone(&chrono::Utc))
}

/// A record's age at `now`: since the later of its own last edit and the time
/// the claim label was added (the floor). `None` when neither time is known —
/// such a record has no clock, so it cannot be fresh. Both times are GitHub's,
/// so `now` should be GitHub's clock too (the run reads it from the account
/// lookup's `Date`); an age below zero means the record is dated after `now`,
/// which [`claim_record_gates`] does not take as a live claim beyond
/// [`CLAIM_FUTURE_TOLERANCE`].
pub fn claim_record_age(
    now: chrono::DateTime<chrono::Utc>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    label_added_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<chrono::Duration> {
    let last_active = updated_at.max(label_added_at)?;
    Some(now.signed_duration_since(last_active))
}

/// Whether one claim record gates dispatch (PRD #421 M3.2), per record:
///
/// | record | evidence | gates |
/// | --- | --- | --- |
/// | own | `own_work_alive` false (no worktree, no open PR) | no |
/// | own | `own_work_alive` true | yes |
/// | foreign | author not `trusted` | no |
/// | foreign | age unknown, at least `ttl`, or dated more than [`CLAIM_FUTURE_TOLERANCE`] ahead | no |
/// | foreign, `trusted` | age below `ttl` | yes |
///
/// The own-and-alive row never decides anything in practice: a worktree or an
/// open PR skips the issue before claims are read. `trusted` is
/// [`IssueClaimRecord::author_is_trusted`].
pub fn claim_record_gates(
    own: bool,
    own_work_alive: bool,
    trusted: bool,
    age: Option<chrono::Duration>,
    ttl: chrono::Duration,
) -> bool {
    if own {
        own_work_alive
    } else {
        trusted && age.is_some_and(|age| age >= -CLAIM_FUTURE_TOLERANCE && age < ttl)
    }
}

/// The claimant a skip names: the schedule and machine id the live record
/// carries, the GitHub account that wrote it, and its age (`None` when it has
/// no clock — only possible for an own record, which gates on evidence rather
/// than age).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimant {
    pub task: String,
    pub host: String,
    pub login: Option<String>,
    pub age: Option<chrono::Duration>,
}

impl std::fmt::Display for Claimant {
    /// The schedule and machine come from a comment body its author chose, so
    /// both are escaped and bounded before they reach the log; the login,
    /// which GitHub sets, is escaped and bounded all the same.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::config_validation::{bound_chars, escape_field_for_log};
        write!(
            f,
            "schedule {:?} on machine {} (GitHub account {}), ",
            bound_chars(&self.task, CLAIM_TASK_LOG_CHARS),
            escape_field_for_log(&self.host, 64),
            self.login.as_deref().map_or_else(
                || "unknown".to_string(),
                |login| escape_field_for_log(login, 64)
            )
        )?;
        match self.age {
            // A record dated slightly ahead of GitHub's clock reads as `0s`.
            Some(age) => write!(f, "last active {} ago", format_claim_duration(age)),
            None => write!(f, "age unknown"),
        }
    }
}

/// The claimant whose record keeps the issue claimed, or `None` when no record
/// on it is live (PRD #421 M3.2: the claim gates iff at least ONE record is
/// individually live — each aged on its own clock, never an aggregate — and,
/// when foreign, written by an author GitHub vouches for). Of several live
/// records the most recently active one is named.
pub fn live_claimant(
    records: &[IssueClaimRecord],
    me: &ClaimIdentity<'_>,
    own_work_alive: bool,
    label_added_at: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
    ttl: chrono::Duration,
) -> Option<Claimant> {
    records
        .iter()
        .filter_map(|found| {
            let age = claim_record_age(now, found.updated_at, label_added_at);
            claim_record_gates(
                found.is_own(me),
                own_work_alive,
                found.author_is_trusted(),
                age,
                ttl,
            )
            .then(|| Claimant {
                task: found.record.task.clone(),
                host: found.record.host.clone(),
                login: found.author_login.clone(),
                age,
            })
        })
        .min_by_key(|claimant| claimant.age.unwrap_or(chrono::Duration::MAX))
}

/// Whether the owner's skip should refresh its own record now (PRD #421
/// M3.1): when the record is at least half `ttl` old, so a live claim gets one
/// refresh inside every TTL while a deck firing every few minutes does not edit
/// it on every fire. `last_edit` is the own record's `updated_at`; `None` — no
/// own record, or one with no readable time — is due, so the liveness the
/// worktree proves gets published.
pub fn claim_refresh_due(
    now: chrono::DateTime<chrono::Utc>,
    last_edit: Option<chrono::DateTime<chrono::Utc>>,
    ttl: chrono::Duration,
) -> bool {
    last_edit.is_none_or(|last_edit| now.signed_duration_since(last_edit) >= ttl / 2)
}

/// Check `claim_ttl_hours` itself: at least 1.
pub fn validate_claim_ttl_hours(ttl_hours: u32) -> Result<(), String> {
    if ttl_hours < 1 {
        return Err(
            "issue_dispatch claim_ttl_hours must be at least 1 (a claim that is never \
             live gates nothing)"
                .to_string(),
        );
    }
    Ok(())
}

/// Check that a claim TTL of `ttl_hours` can be kept current by a task firing
/// on `cron` (PRD #421 M3.1): the TTL must be at least twice the longest gap
/// between consecutive fires, walked from `from` over a year, plus
/// [`CLAIM_TTL_HEADROOM`]. The owner refreshes its record only from a fire,
/// and only once the record is half a TTL old, so a gap wider than half the
/// TTL can let a live claim expire; the headroom covers the record being
/// written after its fire and the refresh landing after its check.
/// `Err` says why gating cannot be trusted for the task — including when the
/// gap cannot be computed (an invalid cron, one that stops firing, or one too
/// dense to walk).
pub fn check_claim_ttl_against_cron<Z: chrono::TimeZone>(
    cron: &str,
    ttl_hours: u32,
    from: &chrono::DateTime<Z>,
) -> Result<(), String> {
    let schedule = crate::scheduler::parse_cron(cron).map_err(|e| e.to_string())?;
    let gap = crate::scheduler::longest_fire_gap(
        &schedule,
        from,
        chrono::Duration::days(CLAIM_CRON_HORIZON_DAYS),
        CLAIM_CRON_MAX_FIRES,
    )
    .ok_or_else(|| {
        format!(
            "the longest gap between fires of cron {cron:?} cannot be computed \
             (it stops firing within a year, or fires more than {CLAIM_CRON_MAX_FIRES} \
             times before the walk ends)"
        )
    })?;
    let ttl = chrono::Duration::hours(i64::from(ttl_hours));
    let needed = gap * 2 + CLAIM_TTL_HEADROOM;
    if ttl < needed {
        let needed_hours = (needed.num_seconds() + 3599) / 3600;
        return Err(format!(
            "claim_ttl_hours = {ttl_hours} is less than twice the longest gap between fires \
             of cron {cron:?} ({}) plus an hour, so a live claim could expire before its \
             owner's next fire refreshes it; set claim_ttl_hours to at least {needed_hours}",
            format_claim_duration(gap)
        ));
    }
    Ok(())
}

/// Render a duration for a log line: whole days, hours, minutes and seconds,
/// leaving out zero parts (`3d 2h`, `45m`, `0s`).
fn format_claim_duration(duration: chrono::Duration) -> String {
    let total = duration.num_seconds().max(0);
    let parts = [
        (total / 86_400, "d"),
        (total % 86_400 / 3600, "h"),
        (total % 3600 / 60, "m"),
        (total % 60, "s"),
    ];
    let rendered: Vec<String> = parts
        .iter()
        .filter(|(value, _)| *value > 0)
        .map(|(value, unit)| format!("{value}{unit}"))
        .collect();
    if rendered.is_empty() {
        "0s".to_string()
    } else {
        rendered.join(" ")
    }
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchDecision {
    /// Provision the worktree and spawn an agent for this issue.
    Dispatch,
    /// Skip — the issue is already being worked on, for the reason given.
    Skip(SkipCause),
}

/// Why an issue was skipped, so each skip can say so (PRD #421 M3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipCause {
    /// The per-issue worktree exists.
    WorktreeExists,
    /// An open PR's head branch is `agent/issue-<n>`.
    OpenPr,
    /// Another deck's claim record on the issue is live.
    Claimed(Claimant),
    /// Another run created the worktree between the checks and this run's
    /// own creation of it.
    ConcurrentCreator,
}

impl std::fmt::Display for SkipCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipCause::WorktreeExists => write!(f, "its worktree exists"),
            SkipCause::OpenPr => write!(f, "an open pull request targets its branch"),
            SkipCause::Claimed(claimant) => write!(f, "claimed by {claimant}"),
            SkipCause::ConcurrentCreator => {
                write!(f, "another run created its worktree first")
            }
        }
    }
}

/// Decide dispatch-vs-skip from the idempotency signals, in the order they
/// are evaluated: the per-issue worktree exists (primary), an open PR's HEAD
/// branch is `agent/issue-<n>` (secondary), and a live claim on GitHub
/// (`claimant`, PRD #421 M3.2), which the caller resolves only when the first
/// two leave the issue open — so a failure to read claims can never turn a
/// clean local skip into a failure — and passes as `None` when gating is off
/// or the claims could not be read. Only when all three are absent do we
/// dispatch.
pub fn dispatch_decision(
    worktree_exists: bool,
    open_pr_with_matching_head: bool,
    claimant: Option<Claimant>,
) -> DispatchDecision {
    if worktree_exists {
        DispatchDecision::Skip(SkipCause::WorktreeExists)
    } else if open_pr_with_matching_head {
        DispatchDecision::Skip(SkipCause::OpenPr)
    } else if let Some(claimant) = claimant {
        DispatchDecision::Skip(SkipCause::Claimed(claimant))
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
            vec![
                "api",
                "--paginate",
                "repos/o/r/issues/7/comments?per_page=100"
            ]
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

    fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn t0() -> chrono::DateTime<chrono::Utc> {
        at("2026-10-22T05:00:13.000Z")
    }

    #[test]
    fn claim_record_round_trips_task_host_and_claim_time() {
        let when = at("2026-10-22T05:00:13.250Z");
        let body = render_claim_record("dispatch-task", "build-box", when);
        assert!(body.starts_with(CLAIM_RECORD_MARKER));
        assert!(body.contains("dispatch-task") && body.contains("build-box"));
        assert!(
            body.contains("- Claimed at: \"2026-10-22T05:00:13.250Z\"\n"),
            "{body}"
        );
        assert!(
            body.lines()
                .nth(1)
                .unwrap()
                .contains("2026-10-22T05:00:13.250Z"),
            "the sentence says when it was last claimed: {body}"
        );
        assert_eq!(
            parse_claim_record(&body),
            Some(ClaimRecord {
                task: "dispatch-task".into(),
                host: "build-box".into(),
                claimed_at: Some(when),
            })
        );
        // Awkward task names survive the round trip on a single field line.
        let tricky = "Issues vfarcic/dot-ai \"quoted\"\nsecond line";
        assert_eq!(
            parse_claim_record(&render_claim_record(tricky, "h", when)),
            Some(ClaimRecord {
                task: tricky.into(),
                host: "h".into(),
                claimed_at: Some(when),
            })
        );
    }

    #[test]
    fn a_re_claim_renders_a_body_that_differs_from_the_record_it_edits() {
        // GitHub moves `updated_at` only when an edit changes the body. A
        // later millisecond renders a different body; `next_claim_time` is
        // what makes the re-claim's time a later one.
        assert_ne!(
            render_claim_record("t", "h", at("2026-10-22T05:00:13.000Z")),
            render_claim_record("t", "h", at("2026-10-22T05:00:13.001Z")),
        );
    }

    #[test]
    fn parse_claim_record_accepts_a_record_without_a_claim_time() {
        // The shape earlier builds wrote, with no `- Claimed at:` line.
        let legacy = format!(
            "{CLAIM_RECORD_MARKER}\nAn agent was dispatched to work on this issue by \
             dot-agent-deck on machine `h1-0123456789abcdef`.\n\n\
             - Task: \"t\"\n- Machine: \"h1-0123456789abcdef\"\n"
        );
        assert_eq!(
            parse_claim_record(&legacy),
            Some(ClaimRecord {
                task: "t".into(),
                host: "h1-0123456789abcdef".into(),
                claimed_at: None,
            })
        );
        // A claim time that is not a time reads as absent, not as no record.
        let garbled = format!("{legacy}- Claimed at: \"yesterday\"\n");
        assert_eq!(parse_claim_record(&garbled).unwrap().claimed_at, None);
    }

    #[test]
    fn parse_claim_record_rejects_non_records() {
        // No marker.
        assert_eq!(parse_claim_record("- Task: \"t\"\n- Host: \"h\""), None);
        // A reply quoting a record is not a record.
        let quoted = render_claim_record("t", "h", t0())
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
        assert_eq!(authenticated_user_argv(), vec!["api", "--include", "user"]);
        // A body with no headers: the id, and no server clock.
        assert_eq!(
            parse_authenticated_user(r#"{"id":12345,"login":"deck-bot"}"#),
            Ok(AuthenticatedUser {
                id: 12345,
                server_date: None,
            })
        );
        assert!(parse_authenticated_user(r#"{"login":"deck-bot"}"#).is_err());
        assert!(parse_authenticated_user("").is_err());
        assert!(parse_authenticated_user("not json").is_err());
    }

    #[test]
    fn the_account_lookup_reads_githubs_date_from_the_included_headers() {
        // What `gh api --include user` printed (gh 2.102): an LF-ended status
        // line, CRLF-ended headers, a blank CRLF line, then the body.
        let output = "HTTP/2.0 200 OK\nContent-Type: application/json\r\n\
                      Date: Fri, 23 Oct 2026 17:55:59 GMT\r\nX-Ratelimit-Limit: 5000\r\n\r\n\
                      {\"login\":\"deck-bot\",\"id\":12345}";
        assert_eq!(
            parse_authenticated_user(output),
            Ok(AuthenticatedUser {
                id: 12345,
                server_date: Some(at("2026-10-23T17:55:59Z")),
            })
        );
        // Header names are case-insensitive; LF-only line ends work too.
        let lf = "HTTP/1.1 200 OK\ndate: Fri, 23 Oct 2026 17:55:59 GMT\n\n{\"id\":7}";
        assert_eq!(
            parse_authenticated_user(lf).unwrap().server_date,
            Some(at("2026-10-23T17:55:59Z"))
        );
        // A missing or unreadable Date leaves the id readable.
        for headers in ["", "Date: yesterday\r\n"] {
            let output = format!("HTTP/2.0 200 OK\n{headers}\r\n{{\"id\":7}}");
            assert_eq!(
                parse_authenticated_user(&output),
                Ok(AuthenticatedUser {
                    id: 7,
                    server_date: None,
                }),
                "{output:?}"
            );
        }
        // Headers with no body after them are an error, not an account.
        assert!(parse_authenticated_user("HTTP/2.0 200 OK\nDate: x\r\n").is_err());
    }

    #[test]
    fn claim_record_names_the_machine_id_not_the_host_name() {
        let host_id = claim_host_id("secret-laptop");
        let body = render_claim_record("dispatch-task", &host_id, t0());
        assert!(!body.contains("secret-laptop"), "{body}");
        let sentence = body.lines().nth(1).unwrap();
        assert!(sentence.contains(&host_id), "{sentence}");
        assert!(!sentence.contains("dispatch-task"), "{sentence}");
    }

    fn comment(id: u64, user: u64, body: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "user": {"id": user, "login": "x"}, "body": body})
    }

    /// The comment id [`find_own_claim_record`] picks, for the tests that are
    /// about which comment matches rather than what it carries.
    fn own_id(json: &str, task: &str, host: &str, user: u64) -> Result<Option<u64>, String> {
        find_own_claim_record(json, task, host, user).map(|own| own.map(|record| record.id))
    }

    #[test]
    fn find_own_claim_record_requires_same_task_host_and_author() {
        let own = render_claim_record("task", "host", t0());
        let other_host = render_claim_record("task", "elsewhere", t0());
        let other_task = render_claim_record("another", "host", t0());
        let json = serde_json::json!([
            comment(1, ME, "plain comment"),
            comment(2, ME, &other_host),
            comment(3, ME, &other_task),
            comment(4, ME, &own),
            {"id": 5, "body": null, "user": null},
        ])
        .to_string();
        assert_eq!(own_id(&json, "task", "host", ME), Ok(Some(4)));
        assert_eq!(own_id(&json, "task", "nowhere", ME), Ok(None));
        assert_eq!(own_id(&json, "task", "host", 999), Ok(None));
        assert_eq!(own_id("[]", "task", "host", ME), Ok(None));
    }

    #[test]
    fn find_own_claim_record_ignores_the_claim_time() {
        // A record claimed at another time, or written before the field
        // existed, is still this deck's own: the time is not part of the match.
        let earlier = render_claim_record("task", "host", at("2025-01-01T00:00:00.000Z"));
        let json = serde_json::json!([comment(4, ME, &earlier)]).to_string();
        assert_eq!(own_id(&json, "task", "host", ME), Ok(Some(4)));
        let legacy = format!(
            "{CLAIM_RECORD_MARKER}\nAn agent was dispatched.\n\n- Task: \"task\"\n- Machine: \"host\"\n"
        );
        let json = serde_json::json!([comment(6, ME, &legacy)]).to_string();
        assert_eq!(own_id(&json, "task", "host", ME), Ok(Some(6)));
    }

    #[test]
    fn find_own_claim_record_returns_the_record_body_and_claim_time() {
        let when = at("2026-10-22T05:00:13.250Z");
        let own = render_claim_record("task", "host", when);
        let json = serde_json::json!([comment(4, ME, &own)]).to_string();
        assert_eq!(
            find_own_claim_record(&json, "task", "host", ME),
            Ok(Some(OwnClaimRecord {
                id: 4,
                body: own,
                claimed_at: Some(when),
            }))
        );
    }

    #[test]
    fn next_claim_time_is_now_when_the_clock_moved_forward() {
        let previous = at("2026-10-22T05:00:13.000Z");
        let now = at("2026-10-22T05:00:14.500Z");
        assert_eq!(next_claim_time(now, Some(previous)), now);
        // Exactly one millisecond on is already strictly later.
        let now = at("2026-10-22T05:00:13.001Z");
        assert_eq!(next_claim_time(now, Some(previous)), now);
    }

    #[test]
    fn next_claim_time_is_strictly_later_when_the_clock_repeats() {
        let previous = at("2026-10-22T05:00:13.250Z");
        let next = next_claim_time(previous, Some(previous));
        assert_eq!(next, at("2026-10-22T05:00:13.251Z"));
        // Within the same millisecond the clock has not moved either.
        let next = next_claim_time(at("2026-10-22T05:00:13.250900Z"), Some(previous));
        assert!(next > previous, "{next}");
        assert_ne!(
            render_claim_record("t", "h", next),
            render_claim_record("t", "h", previous),
            "the same millisecond twice must still change the body"
        );
    }

    #[test]
    fn next_claim_time_is_previous_plus_one_ms_when_the_clock_stepped_back() {
        let previous = at("2026-10-22T05:00:13.250Z");
        let now = at("2026-10-22T04:59:00.000Z");
        assert_eq!(
            next_claim_time(now, Some(previous)),
            at("2026-10-22T05:00:13.251Z")
        );
        assert_ne!(
            render_claim_record("t", "h", next_claim_time(now, Some(previous))),
            render_claim_record("t", "h", previous),
        );
    }

    #[test]
    fn next_claim_time_is_now_for_a_record_without_a_claim_time() {
        let now = t0();
        assert_eq!(next_claim_time(now, None), now);
        // A legacy record, and one whose time does not parse, carry no time
        // to step past: the re-claim's body differs because it gains the field.
        let legacy = format!(
            "{CLAIM_RECORD_MARKER}\nAn agent was dispatched to work on this issue by \
             dot-agent-deck on machine `h`.\n\n- Task: \"t\"\n- Machine: \"h\"\n"
        );
        let garbled = format!("{legacy}- Claimed at: \"yesterday\"\n");
        for old in [legacy, garbled] {
            let json = serde_json::json!([comment(7, ME, &old)]).to_string();
            let own = find_own_claim_record(&json, "t", "h", ME).unwrap().unwrap();
            assert_eq!(own.claimed_at, None);
            let body = render_claim_record("t", "h", next_claim_time(now, own.claimed_at));
            assert_ne!(body, own.body);
        }
    }

    #[test]
    fn a_re_claim_through_the_lookup_never_repeats_the_body() {
        // The path `write_claim_record` takes: find the own record, step past
        // its time, render. A clock frozen on the record's own millisecond, or
        // set behind it, still yields a different body.
        let previous = at("2026-10-22T05:00:13.250Z");
        let old = render_claim_record("t", "h", previous);
        let json = serde_json::json!([comment(7, ME, &old)]).to_string();
        let own = find_own_claim_record(&json, "t", "h", ME).unwrap().unwrap();
        for now in [
            previous,
            at("2026-10-22T05:00:13.250999Z"),
            at("2020-01-01T00:00:00Z"),
        ] {
            let body = render_claim_record("t", "h", next_claim_time(now, own.claimed_at));
            assert_ne!(body, own.body, "clock at {now}");
        }
    }

    #[test]
    fn find_own_claim_record_skips_a_forged_record_and_keeps_searching() {
        let body = render_claim_record("task", "host", t0());
        let forged_first = serde_json::json!([
            comment(1, 99999, &body),
            {"id": 2, "body": body},
            comment(3, ME, &body),
        ])
        .to_string();
        assert_eq!(
            own_id(&forged_first, "task", "host", ME),
            Ok(Some(3)),
            "the forged and the authorless copies are skipped; the genuine one is found"
        );
        let forged_only = serde_json::json!([comment(1, 99999, &body)]).to_string();
        assert_eq!(own_id(&forged_only, "task", "host", ME), Ok(None));
    }

    #[test]
    fn find_own_claim_record_reads_every_paginated_page() {
        let page1 = serde_json::json!([comment(1, ME, "first page")]);
        let page2 = serde_json::json!([comment(9, ME, &render_claim_record("t", "h", t0()))]);
        let stream = format!("{page1}\n{page2}\n");
        assert_eq!(own_id(&stream, "t", "h", ME), Ok(Some(9)));
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
        let page1 = serde_json::json!([comment(9, ME, &render_claim_record("t", "h", t0()))]);
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

    fn claimant() -> Claimant {
        Claimant {
            task: "Issues acme/widgets".to_string(),
            host: "h1-0123456789abcdef".to_string(),
            login: Some("teammate".to_string()),
            age: Some(chrono::Duration::hours(3)),
        }
    }

    #[test]
    fn dispatch_decision_truth_table() {
        use SkipCause::*;
        let skip = DispatchDecision::Skip;
        // No signal → dispatch.
        assert_eq!(
            dispatch_decision(false, false, None),
            DispatchDecision::Dispatch
        );
        // Each signal alone skips, naming itself.
        assert_eq!(dispatch_decision(true, false, None), skip(WorktreeExists));
        assert_eq!(dispatch_decision(false, true, None), skip(OpenPr));
        assert_eq!(
            dispatch_decision(false, false, Some(claimant())),
            skip(Claimed(claimant()))
        );
        // Several: the first in evaluation order (worktree, PR, claim) is named.
        for claim in [None, Some(claimant())] {
            assert_eq!(
                dispatch_decision(true, true, claim.clone()),
                skip(WorktreeExists)
            );
            assert_eq!(
                dispatch_decision(true, false, claim.clone()),
                skip(WorktreeExists)
            );
            assert_eq!(dispatch_decision(false, true, claim), skip(OpenPr));
        }
    }

    #[test]
    fn skip_causes_read_differently_and_a_claim_names_its_claimant() {
        let lines: Vec<String> = [
            SkipCause::WorktreeExists,
            SkipCause::OpenPr,
            SkipCause::Claimed(claimant()),
            SkipCause::ConcurrentCreator,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let distinct: std::collections::HashSet<&String> = lines.iter().collect();
        assert_eq!(distinct.len(), lines.len(), "{lines:?}");
        assert_eq!(
            lines[2],
            "claimed by schedule \"Issues acme/widgets\" on machine h1-0123456789abcdef \
             (GitHub account teammate), last active 3h ago"
        );
        let unknown = Claimant {
            login: None,
            age: None,
            ..claimant()
        };
        assert_eq!(
            unknown.to_string(),
            "schedule \"Issues acme/widgets\" on machine h1-0123456789abcdef \
             (GitHub account unknown), age unknown"
        );
        // A record dated a little ahead of GitHub's clock still renders.
        let ahead = Claimant {
            age: Some(chrono::Duration::seconds(-30)),
            ..claimant()
        };
        assert!(ahead.to_string().ends_with("last active 0s ago"), "{ahead}");
    }

    #[test]
    fn a_claimant_line_bounds_and_escapes_what_the_comment_supplied() {
        // The schedule name is whatever the comment says: a 60 000-character
        // one must not reach the log whole, nor may a control or bidi
        // character in any field act on the terminal.
        let long = Claimant {
            task: format!("{}\u{202e}\n", "x".repeat(60_000)),
            host: format!("h1-\n{}", "y".repeat(500)),
            login: Some("bad\u{1b}[31mlogin".to_string()),
            age: Some(chrono::Duration::hours(1)),
        };
        let line = long.to_string();
        assert!(line.chars().count() < 400, "{} chars", line.chars().count());
        assert!(
            line.contains(&format!("\"{}… (60002 characters total)\"", "x".repeat(64))),
            "{line}"
        );
        assert!(
            !line.contains('\n') && !line.contains('\u{1b}') && !line.contains('\u{202e}'),
            "{line:?}"
        );
        // An ordinary name is shown whole.
        assert!(claimant().to_string().contains("\"Issues acme/widgets\""));
    }

    // --- PRD #421 Phase 3: reading every claim record ---

    fn hours(n: i64) -> chrono::Duration {
        chrono::Duration::hours(n)
    }

    fn record_comment(
        id: u64,
        user: u64,
        task: &str,
        host: &str,
        updated_at: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "user": {"id": user, "login": "x"},
            "author_association": "COLLABORATOR",
            "body": render_claim_record(task, host, t0()),
            "created_at": "2026-10-01T00:00:00Z",
            "updated_at": updated_at,
        })
    }

    #[test]
    fn parse_issue_claim_records_finds_every_record_whoever_wrote_it() {
        let page1 = serde_json::json!([
            comment(1, ME, "plain comment"),
            record_comment(2, ME, "mine", "h1-aaaa", "2026-10-22T07:23:14Z"),
        ]);
        let page2 = serde_json::json!([
            record_comment(3, 4242, "theirs", "h1-bbbb", "2026-10-20T01:02:03Z"),
            {"id": 4, "body": render_claim_record("no author", "h1-cccc", t0()), "user": null},
            {"id": 5, "body": null, "user": null},
        ]);
        let records = parse_issue_claim_records(&format!("{page1}\n{page2}\n")).unwrap();
        let summary: Vec<_> = records
            .iter()
            .map(|r| {
                (
                    r.comment_id,
                    r.author_id,
                    r.author_login.as_deref(),
                    r.author_association.as_deref(),
                    r.record.task.as_str(),
                    r.record.host.as_str(),
                    r.updated_at,
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    2,
                    Some(ME),
                    Some("x"),
                    Some("COLLABORATOR"),
                    "mine",
                    "h1-aaaa",
                    Some(at("2026-10-22T07:23:14Z"))
                ),
                (
                    3,
                    Some(4242),
                    Some("x"),
                    Some("COLLABORATOR"),
                    "theirs",
                    "h1-bbbb",
                    Some(at("2026-10-20T01:02:03Z"))
                ),
                (4, None, None, None, "no author", "h1-cccc", None),
            ]
        );
        assert_eq!(records[0].record.claimed_at, Some(t0()));
        assert_eq!(
            records[0].body,
            render_claim_record("mine", "h1-aaaa", t0())
        );
    }

    #[test]
    fn parse_issue_claim_records_reads_an_unreadable_edit_time_as_absent() {
        let json = serde_json::json!([record_comment(1, ME, "t", "h", "yesterday")]).to_string();
        assert_eq!(
            parse_issue_claim_records(&json).unwrap()[0].updated_at,
            None
        );
    }

    #[test]
    fn parse_issue_claim_records_fails_on_empty_or_malformed_output() {
        assert!(parse_issue_claim_records("").is_err());
        assert!(parse_issue_claim_records("not json").is_err());
        let good = serde_json::json!([record_comment(1, ME, "t", "h", "2026-10-22T07:23:14Z")]);
        assert!(parse_issue_claim_records(&format!("{good}\n{{\"id\": 1}}")).is_err());
        assert_eq!(parse_issue_claim_records("[]"), Ok(Vec::new()));
    }

    #[test]
    fn a_record_is_own_only_with_the_same_author_task_and_machine() {
        let me = ClaimIdentity {
            user_id: ME,
            task: "t",
            host_id: "h",
        };
        let json = serde_json::json!([
            record_comment(1, ME, "t", "h", "2026-10-22T07:23:14Z"),
            record_comment(2, 999, "t", "h", "2026-10-22T07:23:14Z"),
            record_comment(3, ME, "other", "h", "2026-10-22T07:23:14Z"),
            record_comment(4, ME, "t", "elsewhere", "2026-10-22T07:23:14Z"),
            {"id": 5, "body": render_claim_record("t", "h", t0()), "user": null},
        ])
        .to_string();
        let own: Vec<bool> = parse_issue_claim_records(&json)
            .unwrap()
            .iter()
            .map(|r| r.is_own(&me))
            .collect();
        assert_eq!(own, vec![true, false, false, false, false]);
    }

    #[test]
    fn issue_events_argv_addresses_the_issue_events_endpoint() {
        assert_eq!(
            issue_events_argv("acme/widgets", 7),
            vec![
                "api",
                "--paginate",
                "repos/acme/widgets/issues/7/events?per_page=100"
            ]
        );
    }

    fn label_event(event: &str, name: &str, created_at: &str) -> serde_json::Value {
        serde_json::json!({"event": event, "label": {"name": name}, "created_at": created_at})
    }

    #[test]
    fn claim_label_state_floors_at_the_earliest_add_and_follows_the_latest_change() {
        let page1 = serde_json::json!([
            label_event("labeled", "bug", "2026-10-01T00:00:00Z"),
            label_event("labeled", CLAIM_LABEL, "2026-10-22T05:00:11Z"),
            label_event("unlabeled", CLAIM_LABEL, "2026-10-21T00:00:00Z"),
            {"event": "assigned", "created_at": "2026-10-02T00:00:00Z"},
        ]);
        let page2 =
            serde_json::json!([label_event("labeled", CLAIM_LABEL, "2026-10-20T09:00:00Z")]);
        assert_eq!(
            parse_claim_label_state(&format!("{page1}\n{page2}")),
            Ok(ClaimLabelState {
                present: true,
                added_at: Some(at("2026-10-20T09:00:00Z")),
            })
        );
        assert_eq!(
            parse_claim_label_state(
                &serde_json::json!([label_event("labeled", "bug", "2026-10-01T00:00:00Z")])
                    .to_string()
            ),
            Ok(ClaimLabelState {
                present: false,
                added_at: None,
            })
        );
        assert_eq!(
            parse_claim_label_state("[]"),
            Ok(ClaimLabelState {
                present: false,
                added_at: None,
            })
        );
    }

    #[test]
    fn a_claim_label_removed_after_its_last_add_is_not_present() {
        let removed = serde_json::json!([
            label_event("labeled", CLAIM_LABEL, "2026-10-20T09:00:00Z"),
            label_event("unlabeled", CLAIM_LABEL, "2026-10-21T09:00:00Z"),
            label_event("labeled", "bug", "2026-10-22T09:00:00Z"),
        ]);
        assert_eq!(
            parse_claim_label_state(&removed.to_string()),
            Ok(ClaimLabelState {
                present: false,
                added_at: Some(at("2026-10-20T09:00:00Z")),
            })
        );
        // Removed and re-added in the same second: the later event in the
        // listing decides.
        let same_second = serde_json::json!([
            label_event("unlabeled", CLAIM_LABEL, "2026-10-21T09:00:00Z"),
            label_event("labeled", CLAIM_LABEL, "2026-10-21T09:00:00Z"),
        ]);
        assert!(
            parse_claim_label_state(&same_second.to_string())
                .unwrap()
                .present
        );
    }

    #[test]
    fn claim_label_state_fails_on_empty_or_malformed_output() {
        assert!(parse_claim_label_state("").is_err());
        assert!(parse_claim_label_state("{\"event\": \"labeled\"}").is_err());
    }

    #[test]
    fn claim_record_age_runs_from_the_later_of_its_edit_and_the_label_floor() {
        let now = at("2026-10-23T12:00:00Z");
        let edit = at("2026-10-23T02:00:00Z");
        let label = at("2026-10-23T06:00:00Z");
        assert_eq!(claim_record_age(now, Some(edit), None), Some(hours(10)));
        assert_eq!(claim_record_age(now, None, Some(label)), Some(hours(6)));
        // The floor lifts an edit older than the label …
        assert_eq!(
            claim_record_age(now, Some(edit), Some(label)),
            Some(hours(6))
        );
        // … and never ages a later edit.
        assert_eq!(
            claim_record_age(now, Some(label), Some(edit)),
            Some(hours(6))
        );
        assert_eq!(claim_record_age(now, None, None), None);
        // A record dated after `now` has a negative age, which the gate reads.
        assert_eq!(
            claim_record_age(now, Some(now + hours(1)), None),
            Some(-hours(1))
        );
    }

    #[test]
    fn claim_record_liveness_truth_table() {
        let ttl = hours(168);
        let fresh = Some(hours(167));
        let stale = Some(hours(168));
        let slightly_ahead = Some(-chrono::Duration::minutes(4));
        let far_ahead = Some(-chrono::Duration::minutes(6));
        // own + dead → not gating, however fresh its clock; own + live →
        // gating, whatever its clock. Trust plays no part in an own record.
        for age in [fresh, stale, None, far_ahead] {
            for trusted in [false, true] {
                assert!(!claim_record_gates(true, false, trusted, age, ttl));
                assert!(claim_record_gates(true, true, trusted, age, ttl));
            }
        }
        // foreign + trusted + fresh → gating; stale, clockless, or dated more
        // than the tolerance ahead → not. The owner's local evidence plays no
        // part in a foreign record.
        for own_work_alive in [false, true] {
            assert!(claim_record_gates(false, own_work_alive, true, fresh, ttl));
            assert!(claim_record_gates(
                false,
                own_work_alive,
                true,
                slightly_ahead,
                ttl
            ));
            assert!(!claim_record_gates(false, own_work_alive, true, stale, ttl));
            assert!(!claim_record_gates(false, own_work_alive, true, None, ttl));
            assert!(!claim_record_gates(
                false,
                own_work_alive,
                true,
                far_ahead,
                ttl
            ));
            // An author GitHub does not vouch for never gates, however fresh.
            for age in [fresh, slightly_ahead] {
                assert!(!claim_record_gates(false, own_work_alive, false, age, ttl));
            }
        }
    }

    fn found(id: u64, user: u64, task: &str, host: &str, updated_at: &str) -> IssueClaimRecord {
        IssueClaimRecord {
            comment_id: id,
            author_id: Some(user),
            author_login: Some(format!("user-{user}")),
            author_association: Some("COLLABORATOR".to_string()),
            body: render_claim_record(task, host, t0()),
            record: ClaimRecord {
                task: task.to_string(),
                host: host.to_string(),
                claimed_at: Some(t0()),
            },
            updated_at: Some(at(updated_at)),
        }
    }

    const NOW: &str = "2026-10-23T12:00:00Z";

    fn me() -> ClaimIdentity<'static> {
        ClaimIdentity {
            user_id: ME,
            task: "mine",
            host_id: "h1-aaaa",
        }
    }

    #[test]
    fn live_claimant_ages_each_record_on_its_own_clock() {
        let ttl = hours(24);
        let own_recent = found(1, ME, "mine", "h1-aaaa", "2026-10-23T11:00:00Z");
        let foreign_dead = found(2, 4242, "theirs", "h1-bbbb", "2026-10-20T00:00:00Z");
        let foreign_live = found(3, 4242, "theirs", "h1-cccc", "2026-10-23T09:30:00Z");
        let decide =
            |records: &[IssueClaimRecord]| live_claimant(records, &me(), false, None, at(NOW), ttl);
        // Success Criterion 7: the deck's own dead record never gates it.
        assert_eq!(decide(std::slice::from_ref(&own_recent)), None);
        // A dead foreign record does not hold it either, nor does a fresh own
        // record make the dead foreign one read live (no aggregate clock).
        assert_eq!(decide(&[own_recent.clone(), foreign_dead.clone()]), None);
        // A live foreign record gates, and is the one named.
        assert_eq!(
            decide(&[own_recent, foreign_dead, foreign_live]),
            Some(Claimant {
                task: "theirs".to_string(),
                host: "h1-cccc".to_string(),
                login: Some("user-4242".to_string()),
                age: Some(chrono::Duration::minutes(150)),
            })
        );
        assert_eq!(decide(&[]), None);
    }

    #[test]
    fn live_claimant_treats_the_same_body_by_another_account_as_foreign() {
        let forged = found(1, 999, "mine", "h1-aaaa", "2026-10-23T11:00:00Z");
        assert_eq!(
            live_claimant(&[forged], &me(), false, None, at(NOW), hours(24)).map(|c| c.host),
            Some("h1-aaaa".to_string())
        );
    }

    #[test]
    fn only_a_foreign_record_by_an_owner_member_or_collaborator_gates() {
        // The same body, equally fresh: what decides is the standing GitHub
        // reports for its author on the repository.
        let record = |association: Option<&str>| IssueClaimRecord {
            author_association: association.map(str::to_string),
            ..found(1, 4242, "theirs", "h1-bbbb", "2026-10-23T11:00:00Z")
        };
        let decide = |association: Option<&str>| {
            live_claimant(
                &[record(association)],
                &me(),
                false,
                None,
                at(NOW),
                hours(24),
            )
        };
        for trusted in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert_eq!(
                decide(Some(trusted)).and_then(|c| c.login),
                Some("user-4242".to_string()),
                "{trusted}"
            );
        }
        for untrusted in [
            Some("CONTRIBUTOR"),
            Some("FIRST_TIME_CONTRIBUTOR"),
            Some("FIRST_TIMER"),
            Some("MANNEQUIN"),
            Some("NONE"),
            Some("collaborator"),
            Some(""),
            None,
        ] {
            assert_eq!(decide(untrusted), None, "{untrusted:?}");
        }
        // An untrusted record does not hide a trusted live one beside it.
        let trusted = IssueClaimRecord {
            comment_id: 2,
            author_login: Some("teammate".to_string()),
            ..record(Some("MEMBER"))
        };
        assert_eq!(
            live_claimant(
                &[record(Some("NONE")), trusted],
                &me(),
                false,
                None,
                at(NOW),
                hours(24)
            )
            .and_then(|c| c.login),
            Some("teammate".to_string())
        );
    }

    #[test]
    fn the_comments_listing_carries_the_authors_standing() {
        let json = serde_json::json!([
            {"id": 1, "user": {"id": 4242, "login": "teammate"}, "author_association": "MEMBER",
             "body": render_claim_record("theirs", "h1-bbbb", t0()),
             "updated_at": "2026-10-23T11:00:00Z"},
            {"id": 2, "user": {"id": 666, "login": "stranger"}, "author_association": "NONE",
             "body": render_claim_record("theirs", "h1-bbbb", t0()),
             "updated_at": "2026-10-23T11:00:00Z"},
            {"id": 3, "user": {"id": 667, "login": "silent"},
             "body": render_claim_record("theirs", "h1-bbbb", t0()),
             "updated_at": "2026-10-23T11:00:00Z"},
        ])
        .to_string();
        let trusted: Vec<(Option<String>, bool)> = parse_issue_claim_records(&json)
            .unwrap()
            .into_iter()
            .map(|r| (r.author_login.clone(), r.author_is_trusted()))
            .collect();
        assert_eq!(
            trusted,
            vec![
                (Some("teammate".to_string()), true),
                (Some("stranger".to_string()), false),
                (Some("silent".to_string()), false),
            ]
        );
    }

    #[test]
    fn a_foreign_record_dated_ahead_of_githubs_clock_does_not_gate() {
        let ttl = hours(24);
        let decide = |updated_at: &str, label: Option<&str>| {
            live_claimant(
                &[found(1, 4242, "theirs", "h1-bbbb", updated_at)],
                &me(),
                false,
                label.map(at),
                at(NOW),
                ttl,
            )
        };
        // A few seconds ahead: still a live claim, shown as just now.
        assert_eq!(
            decide("2026-10-23T12:00:30Z", None).and_then(|c| c.age),
            Some(-chrono::Duration::seconds(30))
        );
        // More than the tolerance ahead: not evidence of a live claim, rather
        // than read as age zero and held for a whole TTL.
        assert_eq!(decide("2026-10-23T12:06:00Z", None), None);
        assert_eq!(decide("2026-10-24T12:00:00Z", None), None);
        // The label floor counts the same way: a label dated ahead lifts the
        // record's clock ahead too.
        assert_eq!(
            decide("2026-10-23T11:00:00Z", Some("2026-10-23T13:00:00Z")),
            None
        );
    }

    #[test]
    fn live_claimant_floors_every_record_by_the_label_time() {
        let foreign = found(2, 4242, "theirs", "h1-bbbb", "2026-10-01T00:00:00Z");
        let label = Some(at("2026-10-23T10:00:00Z"));
        assert_eq!(
            live_claimant(
                std::slice::from_ref(&foreign),
                &me(),
                false,
                label,
                at(NOW),
                hours(24)
            )
            .and_then(|c| c.age),
            Some(hours(2))
        );
        assert_eq!(
            live_claimant(&[foreign], &me(), false, None, at(NOW), hours(24)),
            None
        );
    }

    #[test]
    fn live_claimant_names_the_most_recently_active_of_several_live_records() {
        let older = found(1, 4242, "a", "h1-bbbb", "2026-10-23T08:00:00Z");
        let newer = found(2, 4343, "b", "h1-cccc", "2026-10-23T11:00:00Z");
        assert_eq!(
            live_claimant(&[older, newer], &me(), false, None, at(NOW), hours(24)).map(|c| c.task),
            Some("b".to_string())
        );
    }

    #[test]
    fn live_claimant_gates_on_an_own_record_only_while_its_work_is_alive() {
        let own_old = found(1, ME, "mine", "h1-aaaa", "2026-01-01T00:00:00Z");
        let own_old = std::slice::from_ref(&own_old);
        assert_eq!(
            live_claimant(own_old, &me(), false, None, at(NOW), hours(24)),
            None
        );
        assert!(live_claimant(own_old, &me(), true, None, at(NOW), hours(24)).is_some());
    }

    #[test]
    fn a_refresh_is_due_once_the_own_record_is_half_a_ttl_old() {
        let now = at(NOW);
        let ttl = hours(168);
        assert!(!claim_refresh_due(now, Some(now), ttl));
        assert!(!claim_refresh_due(now, Some(now - hours(83)), ttl));
        assert!(claim_refresh_due(now, Some(now - hours(84)), ttl));
        assert!(claim_refresh_due(now, Some(now - hours(500)), ttl));
        // No own record (or no readable edit time): write one.
        assert!(claim_refresh_due(now, None, ttl));
    }

    #[test]
    fn claim_ttl_hours_must_be_at_least_one() {
        assert!(validate_claim_ttl_hours(0).is_err());
        assert!(validate_claim_ttl_hours(1).is_ok());
        assert!(validate_claim_ttl_hours(DEFAULT_CLAIM_TTL_HOURS).is_ok());
    }

    fn utc(y: i32, mo: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone;
        chrono::Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
    }

    #[test]
    fn the_documented_weekday_cron_needs_twice_its_72h_weekend_gap_plus_an_hour() {
        // `docs/scheduled-tasks.md`'s example: 24h apart on weekdays, 72h
        // across a weekend. Evaluated in UTC so no DST hour moves the gap.
        let cron = "0 9 * * MON-FRI";
        let from = utc(2026, 10, 21);
        assert_eq!(check_claim_ttl_against_cron(cron, 145, &from), Ok(()));
        assert_eq!(
            check_claim_ttl_against_cron(cron, DEFAULT_CLAIM_TTL_HOURS, &from),
            Ok(())
        );
        let err = check_claim_ttl_against_cron(cron, 100, &from).unwrap_err();
        assert!(
            err.contains("(3d)") && err.contains("at least 145"),
            "the error names the gap and the TTL it needs: {err}"
        );
        // Exactly twice the gap is not enough any more.
        assert!(check_claim_ttl_against_cron(cron, 144, &from).is_err());
    }

    #[test]
    fn a_ttl_check_that_cannot_compute_the_gap_fails() {
        let from = utc(2026, 10, 21);
        // Invalid cron.
        assert!(check_claim_ttl_against_cron("not a cron", 168, &from).is_err());
        // A schedule that stops firing (7-field form with a past year range).
        assert!(check_claim_ttl_against_cron("0 0 9 * * * 2026", 10_000, &from).is_err());
        // Too dense to walk: every second.
        assert!(check_claim_ttl_against_cron("* * * * * *", 168, &from).is_err());
    }

    #[test]
    fn a_sparse_cron_needs_a_ttl_twice_its_gap() {
        // Monthly on the 1st: the walk covers a year, so the longest month
        // gap (31 days) decides, wherever the walk starts.
        let from = utc(2026, 10, 21);
        assert!(check_claim_ttl_against_cron("0 9 1 * *", 168, &from).is_err());
        assert_eq!(
            check_claim_ttl_against_cron("0 9 1 * *", 31 * 24 * 2 + 1, &from),
            Ok(())
        );
        assert!(check_claim_ttl_against_cron("0 9 1 * *", 31 * 24 * 2, &from).is_err());
    }

    /// Walk a healthy owner over `weeks` of `cron` fires, starting at the
    /// first fire after `from`: its record is written `write_delay` after the
    /// fire that dispatched it; every later fire checks the record
    /// `check_delay` after the fire and, when [`claim_refresh_due`] says so,
    /// its edit lands `edit_delay` after that. Returns the first moment a
    /// peer would see the record expired (its age reaching `ttl` before the
    /// next edit lands), or `None` when it never lapses.
    fn first_lapse(
        cron: &str,
        ttl_hours: u32,
        from: chrono::DateTime<chrono::Utc>,
        weeks: i64,
        write_delay: chrono::Duration,
        check_delay: chrono::Duration,
        edit_delay: chrono::Duration,
    ) -> Option<chrono::DateTime<chrono::Utc>> {
        let ttl = hours(i64::from(ttl_hours));
        let end = from + chrono::Duration::weeks(weeks);
        let schedule = crate::scheduler::parse_cron(cron).unwrap();
        let mut fires = schedule.after(&from);
        let mut last_edit = fires.next().unwrap() + write_delay;
        for fire in fires {
            if fire > end {
                return None;
            }
            let check = fire + check_delay;
            if check >= last_edit + ttl {
                return Some(last_edit + ttl);
            }
            if claim_refresh_due(check, Some(last_edit), ttl) {
                let landed = check + edit_delay;
                if landed >= last_edit + ttl {
                    return Some(last_edit + ttl);
                }
                last_edit = landed;
            }
        }
        None
    }

    #[test]
    fn a_ttl_that_passes_the_check_outlives_every_refresh_of_a_healthy_owner() {
        // The reviewer's case: a daily cron, the record written 10 seconds
        // after its fire, each check 1 second after its fire and its edit 30
        // seconds after that. The second check after the write sees an age
        // just under half the TTL and does nothing, so the edit falls due at
        // the third — when exactly twice the gap had already expired.
        let from = utc(2026, 10, 21);
        let reviewer = |ttl_hours| {
            first_lapse(
                "0 9 * * *",
                ttl_hours,
                from,
                8,
                chrono::Duration::seconds(10),
                chrono::Duration::seconds(1),
                chrono::Duration::seconds(30),
            )
        };
        assert!(
            reviewer(48).is_some(),
            "twice the gap alone lets the claim lapse, which the headroom exists for"
        );
        assert_eq!(
            check_claim_ttl_against_cron("0 9 * * *", 48, &from).ok(),
            None
        );
        assert_eq!(check_claim_ttl_against_cron("0 9 * * *", 49, &from), Ok(()));
        assert_eq!(reviewer(49), None);

        // And for every cron the docs use, at the smallest TTL the check
        // accepts, with a slower write and edit: the record written ten
        // minutes after its fire, the edit landing ten minutes after a check
        // that runs a minute in.
        for cron in [
            "*/10 * * * *",
            "0 * * * *",
            "0 9 * * *",
            "0 9 * * MON-FRI",
            "0 9 * * MON",
        ] {
            let minimum = (1..=2_000)
                .find(|ttl| check_claim_ttl_against_cron(cron, *ttl, &from).is_ok())
                .unwrap();
            assert_eq!(
                first_lapse(
                    cron,
                    minimum,
                    from,
                    8,
                    chrono::Duration::minutes(10),
                    chrono::Duration::minutes(1),
                    chrono::Duration::minutes(10),
                ),
                None,
                "{cron} at claim_ttl_hours = {minimum}"
            );
        }
        // The 168-hour default passes for the documented weekday cron.
        assert_eq!(
            check_claim_ttl_against_cron("0 9 * * MON-FRI", DEFAULT_CLAIM_TTL_HOURS, &from),
            Ok(())
        );
    }

    #[test]
    fn format_claim_duration_leaves_out_zero_parts() {
        assert_eq!(format_claim_duration(hours(72)), "3d");
        assert_eq!(
            format_claim_duration(hours(26) + chrono::Duration::seconds(65)),
            "1d 2h 1m 5s"
        );
        assert_eq!(format_claim_duration(chrono::Duration::minutes(45)), "45m");
        assert_eq!(format_claim_duration(chrono::Duration::zero()), "0s");
    }
}
