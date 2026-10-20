//! PRD #1401: the pull request an agent's work produced, as the daemon reports
//! it on [`crate::state::SessionSnapshot::pull_request`].
//!
//! The wire is additive and optional (`#[serde(default)]` +
//! `skip_serializing_if`), so it is a do-not-bump case per
//! [`crate::daemon_protocol`]'s policy. Both enums are tolerant of values they
//! do not know: a newer daemon may report a state or review decision an older
//! client has never heard of, and that must read as
//! [`PullRequestState::Unknown`] / [`PullRequestReview::Unknown`] rather than
//! fail the whole agent record it rides on.
//!
//! # Where the value comes from
//!
//! The daemon resolves it ([`run_pull_request_monitor`]): for every live
//! session whose working directory is a git repository on a branch other than
//! `origin`'s default, with a GitHub `origin`, it asks `gh pr list --head
//! <branch> --state all --repo <owner>/<repo>` and keeps the exact
//! `headRefName` match ([`parse_pr_list`]). Results are cached per
//! (repository, branch), so sessions sharing a worktree share one call, and at
//! most one `gh` runs per key at a time. An open or draft PR is re-read on a
//! slow poll ([`DOT_AGENT_DECK_PR_REFRESH_SECS_ENV`]); a merged or closed one is
//! not read again until the branch changes, unless its CI checks were still
//! pending, which keep it on that poll until they finish or
//! [`TERMINAL_PENDING_CHECKS_WINDOW`] has passed since it closed; a turn ending or an agent stopping
//! asks for an early, debounced re-read ([`fetch_due`]).
//!
//! No `gh`, no login, no network, a non-GitHub `origin`, the default branch, a
//! detached `HEAD` or a cwd outside git all mean the same thing: no badge. A
//! failure is logged once per key and retried on a long backoff, never in a
//! loop.
//!
//! # How it reaches attached clients
//!
//! A change is set on the daemon's own sessions and broadcast as an
//! [`EventType::PullRequest`] carrying the value under
//! [`PULL_REQUEST_METADATA_KEY`], both under one `AppState` write lock. That
//! orders the daemon's state against its broadcast, not a client's `ListAgents`
//! reply against its event stream: a reply built before a report can reach a
//! client after the report did. So every change also gets a revision
//! ([`next_revision`]), carried by the event under
//! [`crate::event::PULL_REQUEST_REVISION_METADATA_KEY`] and by the snapshot's
//! [`crate::state::SessionSnapshot::pull_request_revision`]. Clients apply the
//! event in [`crate::state::AppState::apply_event`] and the snapshot in
//! hydration (the TUI and the desktop's fold both run those), and each keeps
//! whichever value has the higher revision; with no revision on either, as
//! from a daemon that sends none, the one applied last wins, as before, and
//! the card's revision drops to `0`.
//!
//! A revision is the daemon's wall clock in microseconds, raised past the
//! previous one when the clock has not moved on, so it strictly increases
//! within one daemon process. A session the daemon has reported no change for
//! carries revision `0`. Revisions from two daemon processes are not
//! comparable — a replacement's clock may be behind, and its unchanged
//! sessions carry `0` — so a client compares them only within one connection.
//! A client that keeps a card across a reconnect, which is how a replacement
//! daemon's first answer reaches it, takes the reply of the resync that
//! follows the stream gap outright, value and revision both
//! ([`crate::state::AppState::resync_after_event_gap`]): that reply is built
//! after everything the client holds, so it is the newer answer whatever its
//! revision, and the revision it brings is the new daemon's, which that
//! daemon's later reports are stamped above.
//!
//! An older client decodes the event type as `Unknown`. That is not a no-op
//! there: the event is still journalled on the card and still runs the
//! client's admission and cwd reconciliation. What keeps it harmless is that
//! the classifiers that matter stay neutral — `Unknown` changes no status and
//! is neither delivery proof nor retry evidence — and that the event names no
//! agent type and no prompt and is stamped at the session's own last activity,
//! so it moves no activity clock. That is why no `PROTOCOL_VERSION` bump is
//! needed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::event::{
    AgentEvent, BroadcastMsg, EventType, PULL_REQUEST_METADATA_KEY,
    PULL_REQUEST_REVISION_METADATA_KEY,
};
use crate::git_env::git_at;
use crate::state::{SessionState, SharedState};
use crate::untrusted_text::escape_control_and_bidi;

// The wire types live in their own I/O-free module so the desktop crate can
// name them without reaching this resolver. Re-exported so code that names
// them through this module, as the e2e tests do, still resolves.
pub use crate::pull_request_info::{
    PullRequestChecks, PullRequestChecksState, PullRequestInfo, PullRequestReview, PullRequestState,
};

/// The `--json` fields the badge needs from `gh pr list`.
/// `isCrossRepository` because `--head` matches a branch NAME, so a fork's
/// pull request from a branch named like ours is listed too.
///
/// `statusCheckRollup` (issue #1781) is the heavy one: every check of the head
/// commit, with its name, timestamps and URL. Measured against this
/// repository's own open PR with 28 checks, it took the reply from 461 bytes
/// to about 20 KB and the call from about 0.55–0.9 s to 0.87–1.4 s. The
/// worktree reclaimer's lookup asks for its own, smaller list.
pub(crate) const GH_PR_FIELDS: &str =
    "number,state,isDraft,reviewDecision,url,headRefName,isCrossRepository,statusCheckRollup";

/// The arguments of `gh pr list` for `branch` of `repo_slug`, asking for
/// `fields`. Shared with [`crate::worktree_reclaim`]'s PR lookup: `--state all`
/// because the default (`open`) hides every merged PR, and `--repo` because
/// letting `gh` infer it queries the upstream from a fork checkout.
pub(crate) fn gh_pr_list_args<'a>(
    branch: &'a str,
    repo_slug: &'a str,
    fields: &'a str,
) -> [&'a str; 10] {
    [
        "pr", "list", "--head", branch, "--state", "all", "--repo", repo_slug, "--json", fields,
    ]
}

/// Environment seam for the poll interval of an open or draft PR, in seconds.
/// Unset, unparsable or zero means [`DEFAULT_REFRESH`].
pub const DOT_AGENT_DECK_PR_REFRESH_SECS_ENV: &str = "DOT_AGENT_DECK_PR_REFRESH_SECS";

/// How often an open or draft PR is re-read when the seam is unset.
pub const DEFAULT_REFRESH: Duration = Duration::from_secs(60);

/// A failed lookup is retried this many intervals later, whatever happens in
/// between.
const FAILURE_BACKOFF_FACTOR: u32 = 5;

/// How long a merged or closed PR is still re-read while its CI checks are
/// pending (issue #1781). A check GitHub marked stale, or a commit status
/// nobody ever reports, stays pending for good, so the re-reads stop after
/// this and the badge keeps the last answer. Two hours outlasts this
/// repository's own longest CI job (one hour) with room for a queue.
const TERMINAL_PENDING_CHECKS_WINDOW: Duration = Duration::from_secs(2 * 60 * 60);

/// The longest gap a turn-end re-read waits for after the previous lookup.
const MAX_KICK_DEBOUNCE: Duration = Duration::from_secs(10);

/// How long one `gh` may run before it counts as a failure.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

/// How long one `git` probe may run before the directory counts as having no
/// key. A repository on a stalled filesystem, or one whose config is a FIFO,
/// costs this much and a killed child, never a stuck one.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// The most `git` probes and `gh` queries the monitor runs at once, across
/// every directory and key. Further jobs wait for a permit.
const MAX_CONCURRENT_JOBS: usize = 4;

/// How much of `gh`'s stderr a failure keeps.
const MAX_LOGGED_STDERR_CHARS: usize = 300;

/// The interval from [`DOT_AGENT_DECK_PR_REFRESH_SECS_ENV`]'s value.
pub fn refresh_interval_from(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_REFRESH)
}

/// What a PR is looked up and cached by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct PrKey {
    /// `owner/name` from `origin`.
    pub slug: String,
    /// The checked-out branch.
    pub branch: String,
}

/// Whether `branch` is the remote's default. `default` is what
/// `refs/remotes/origin/HEAD` names; with none recorded, `main` and `master`
/// count as the default so a clone without it gets no badge for them.
pub(crate) fn is_default_branch(branch: &str, default: Option<&str>) -> bool {
    match default {
        Some(default) => branch == default,
        None => matches!(branch, "main" | "master"),
    }
}

/// One line of `git <args>` run in `cwd`, or `None` when it fails, prints
/// nothing or outlives [`GIT_TIMEOUT`]. The child is `git_at`'s command (the
/// ambient git location switched off), run async and killed when the future is
/// dropped — so a timed-out probe, or one whose job is aborted, leaves no git
/// behind.
async fn git_line(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::from(git_at(cwd));
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// The key `cwd`'s PR is looked up by, or `None` when it gets no badge: not a
/// git repository, a detached `HEAD`, the remote's default branch, or an
/// `origin` that is missing or not on GitHub. Each `git` it runs is bounded by
/// [`GIT_TIMEOUT`] and killed when the future is dropped.
pub(crate) async fn branch_key(cwd: &Path) -> Option<PrKey> {
    // `symbolic-ref` fails on a detached HEAD and outside a repository alike.
    let branch = git_line(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"]).await?;
    let default = git_line(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .await;
    let default = default
        .as_deref()
        .map(|d| d.strip_prefix("origin/").unwrap_or(d));
    if is_default_branch(&branch, default) {
        return None;
    }
    let origin = git_line(cwd, &["remote", "get-url", "origin"]).await?;
    let slug = crate::worktree_reclaim::parse_github_owner_repo(&origin)?;
    Some(PrKey { slug, branch })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPr {
    number: u64,
    state: String,
    #[serde(default)]
    is_draft: bool,
    #[serde(default)]
    review_decision: Option<String>,
    url: String,
    head_ref_name: String,
    /// The PR's branch lives in a fork. Absent (an older `gh`) reads as not.
    #[serde(default)]
    is_cross_repository: bool,
    /// Every check of the head commit. Absent (an older `gh`) or null reads
    /// as no checks.
    #[serde(default)]
    status_check_rollup: Option<Vec<GhCheck>>,
}

/// One entry of `statusCheckRollup`: a check run (`status` and, once it has
/// finished, `conclusion`) or a commit status (`state`). The names and the
/// start time only tell repeated runs of one check apart ([`latest_runs`]).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhCheck {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// A check run's name.
    #[serde(default)]
    name: Option<String>,
    /// A commit status's name.
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    workflow_name: Option<String>,
    #[serde(default)]
    started_at: Option<String>,
}

/// Where one check counts on the badge.
#[derive(Debug, PartialEq, Eq)]
enum CheckBucket {
    Passing,
    Failing,
    Pending,
    Skipped,
}

impl GhCheck {
    /// The bucket GitHub puts this check in, or `None` for a value this build
    /// does not know, which is left out of the counts rather than guessed.
    fn bucket(&self) -> Option<CheckBucket> {
        let field = |value: &Option<String>| value.clone().filter(|v| !v.is_empty());
        if let Some(state) = field(&self.state) {
            return match state.as_str() {
                "SUCCESS" => Some(CheckBucket::Passing),
                "PENDING" | "EXPECTED" => Some(CheckBucket::Pending),
                "FAILURE" | "ERROR" => Some(CheckBucket::Failing),
                _ => None,
            };
        }
        if field(&self.status).is_some_and(|status| status != "COMPLETED") {
            return Some(CheckBucket::Pending);
        }
        match field(&self.conclusion)?.as_str() {
            "SUCCESS" => Some(CheckBucket::Passing),
            "NEUTRAL" | "SKIPPED" => Some(CheckBucket::Skipped),
            "FAILURE" | "TIMED_OUT" | "CANCELLED" | "ACTION_REQUIRED" | "STARTUP_FAILURE" => {
                Some(CheckBucket::Failing)
            }
            // A run GitHub marked stale never finished, so it is not a pass;
            // `gh pr checks` lists it as pending too.
            "STALE" => Some(CheckBucket::Pending),
            _ => None,
        }
    }
}

/// What tells one check from another: its workflow, its name, and a commit
/// status's context.
type CheckKey<'a> = (Option<&'a str>, Option<&'a str>, Option<&'a str>);

/// The newest run of each check in `rollup`. The rollup lists every run of a
/// check, so one that failed and then passed on a re-run appears twice; like
/// `gh pr checks`, only the newest counts, one per workflow and check name (or
/// per commit status context). A run with no start time is queued and so the
/// newest; GitHub's timestamps are all UTC `…Z`, so they order as text.
fn latest_runs(rollup: &[GhCheck]) -> Vec<&GhCheck> {
    let mut latest: HashMap<CheckKey<'_>, &GhCheck> = HashMap::new();
    for check in rollup {
        let key = (
            check.workflow_name.as_deref(),
            check.name.as_deref(),
            check.context.as_deref(),
        );
        // `None` sorts below `Some`, so flip it: no start time is newest.
        let newness = |c: &GhCheck| (c.started_at.is_none(), c.started_at.clone());
        match latest.get(&key) {
            Some(kept) if newness(kept) >= newness(check) => {}
            _ => {
                latest.insert(key, check);
            }
        }
    }
    latest.into_values().collect()
}

/// Issue #1781: the badge's CI summary of `rollup`, or `None` when it holds no
/// check this build can place. Failing wins over pending, and pending over
/// passing, as GitHub's pull request list decides its one icon.
fn summarize_checks(rollup: &[GhCheck]) -> Option<PullRequestChecks> {
    let mut checks = PullRequestChecks {
        state: PullRequestChecksState::Passing,
        passing: 0,
        failing: 0,
        pending: 0,
        skipped: 0,
    };
    for bucket in latest_runs(rollup).into_iter().filter_map(GhCheck::bucket) {
        let count = match bucket {
            CheckBucket::Passing => &mut checks.passing,
            CheckBucket::Failing => &mut checks.failing,
            CheckBucket::Pending => &mut checks.pending,
            CheckBucket::Skipped => &mut checks.skipped,
        };
        *count = count.saturating_add(1);
    }
    if checks.passing + checks.failing + checks.pending + checks.skipped == 0 {
        return None;
    }
    checks.state = if checks.failing > 0 {
        PullRequestChecksState::Failing
    } else if checks.pending > 0 {
        PullRequestChecksState::Pending
    } else {
        PullRequestChecksState::Passing
    };
    Some(checks)
}

impl GhPr {
    fn into_info(self) -> Option<PullRequestInfo> {
        let state = match (self.state.as_str(), self.is_draft) {
            ("OPEN", true) => PullRequestState::Draft,
            ("OPEN", false) => PullRequestState::Open,
            ("MERGED", _) => PullRequestState::Merged,
            ("CLOSED", _) => PullRequestState::Closed,
            _ => return None,
        };
        // The URL is what a badge opens, so only GitHub's own pages qualify.
        if !self.url.starts_with("https://github.com/") {
            return None;
        }
        let review = match self.review_decision.as_deref() {
            Some("APPROVED") => Some(PullRequestReview::Approved),
            Some("CHANGES_REQUESTED") => Some(PullRequestReview::ChangesRequested),
            Some("REVIEW_REQUIRED") => Some(PullRequestReview::ReviewRequired),
            _ => None,
        };
        let checks = summarize_checks(self.status_check_rollup.as_deref().unwrap_or_default());
        Some(PullRequestInfo {
            number: self.number,
            url: self.url,
            state,
            review,
            checks,
        })
    }
}

/// The PR `gh pr list --json` output reports for exactly `branch`, or `None`
/// when it reports none. Only an exact `headRefName` match counts, from a
/// branch of the repository itself: a fork's pull request from a branch with
/// the same name is someone else's. Of several,
/// an open (or draft) one wins, then the highest number. An entry with a state
/// this build does not know, or a URL off `https://github.com/`, is skipped.
pub fn parse_pr_list(json: &str, branch: &str) -> Result<Option<PullRequestInfo>, String> {
    let entries: Vec<GhPr> =
        serde_json::from_str(json).map_err(|e| format!("could not parse gh output: {e}"))?;
    Ok(entries
        .into_iter()
        .filter(|pr| pr.head_ref_name == branch && !pr.is_cross_repository)
        .filter_map(GhPr::into_info)
        .max_by_key(|pr| {
            let open = matches!(pr.state, PullRequestState::Open | PullRequestState::Draft);
            (open, pr.number)
        }))
}

/// Run `gh pr list` for `key`. Never blocks the runtime, and gives up after
/// [`GH_TIMEOUT`].
async fn query_gh(cwd: PathBuf, key: PrKey) -> Result<Option<PullRequestInfo>, String> {
    let mut cmd = tokio::process::Command::new("gh");
    cmd.current_dir(&cwd)
        .args(gh_pr_list_args(&key.branch, &key.slug, GH_PR_FIELDS))
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(GH_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("gh pr list timed out after {}s", GH_TIMEOUT.as_secs()))?
        .map_err(|e| format!("gh unavailable: {e}"))?;
    if !out.status.success() {
        return Err(gh_failure(&out.status, &out.stderr));
    }
    parse_pr_list(&String::from_utf8_lossy(&out.stdout), &key.branch)
}

/// The error a failed `gh` reports. Its stderr is quoted into a log line, so
/// control characters, line breaks and bidi marks are escaped first, and the
/// result is capped at [`MAX_LOGGED_STDERR_CHARS`].
fn gh_failure(status: &impl std::fmt::Display, stderr: &[u8]) -> String {
    let stderr = escape_control_and_bidi(String::from_utf8_lossy(stderr).trim());
    let stderr: String = stderr.chars().take(MAX_LOGGED_STDERR_CHARS).collect();
    format!("gh pr list failed ({status}): {stderr}")
}

/// What the monitor knows about one key.
#[derive(Debug, Clone, Default)]
pub(crate) struct KeyEntry {
    /// The last successful answer: `Some(None)` is "no PR". `None` until one.
    pub value: Option<Option<PullRequestInfo>>,
    /// When the last lookup finished.
    pub fetched_at: Option<Instant>,
    /// The last lookup failed.
    pub failed: bool,
    /// A lookup is running.
    pub in_flight: bool,
    /// A turn ended or an agent stopped on this key since the last lookup.
    pub kicked: bool,
    /// The current run of failures has been logged.
    pub failure_logged: bool,
    /// When a lookup first answered with this key's PR merged or closed;
    /// `None` while it is open, or there is none.
    pub terminal_since: Option<Instant>,
}

impl KeyEntry {
    /// Store a successful lookup's `value`, answered at `now`.
    pub(crate) fn record(&mut self, value: Option<PullRequestInfo>, now: Instant) {
        let terminal = value.as_ref().is_some_and(|pr| {
            matches!(
                pr.state,
                PullRequestState::Merged | PullRequestState::Closed
            )
        });
        self.terminal_since = if terminal {
            Some(self.terminal_since.unwrap_or(now))
        } else {
            None
        };
        self.value = Some(value);
        self.failed = false;
        self.failure_logged = false;
    }
}

/// Whether `entry` needs a lookup at `now`. The refresh policy, pure:
///
/// * never two at once, and a key never looked up is due at once;
/// * after a failure, only once [`FAILURE_BACKOFF_FACTOR`] intervals passed;
/// * a merged or closed PR, never (a new branch is a new key) — unless its
///   CI checks are still pending, which are then re-read as an open PR's
///   are for [`TERMINAL_PENDING_CHECKS_WINDOW`] after it closed, so the badge
///   does not keep a pending mark for good, nor poll for good on a check that
///   never finishes (issue #1781);
/// * an open or draft PR, every `interval`, or after a turn end once the
///   debounce passed;
/// * no PR, every `interval` too — a PR the user opens while the agent is
///   idle ends no turn, and must still show within about a minute — or after
///   a turn end once the debounce passed.
pub(crate) fn fetch_due(entry: &KeyEntry, now: Instant, interval: Duration) -> bool {
    if entry.in_flight {
        return false;
    }
    let Some(at) = entry.fetched_at else {
        return true;
    };
    let age = now.saturating_duration_since(at);
    let kicked = entry.kicked && age >= interval.min(MAX_KICK_DEBOUNCE);
    if entry.failed {
        return age >= interval * FAILURE_BACKOFF_FACTOR;
    }
    match &entry.value {
        None => true,
        Some(Some(pr))
            if matches!(
                pr.state,
                PullRequestState::Merged | PullRequestState::Closed
            ) && !(pr
                .checks
                .is_some_and(|checks| checks.state == PullRequestChecksState::Pending)
                && entry.terminal_since.is_some_and(|since| {
                    now.saturating_duration_since(since) < TERMINAL_PENDING_CHECKS_WINDOW
                })) =>
        {
            false
        }
        Some(Some(_)) | Some(None) => age >= interval || kicked,
    }
}

/// The [`EventType::PullRequest`] that reports `session`'s current
/// `pull_request` to attached clients. Stamped at the session's own
/// `last_activity`, so an older client, which applies it as an `Unknown`
/// event, sees no activity either.
///
/// It is excluded from every piece of delivery evidence: `is_daemon_synthetic`
/// covers its type, `worker_event_proves_delivery` and the delegate retry's
/// classifier read it as nothing, and it names no agent type and no prompt.
pub(crate) fn report_event(session: &SessionState) -> AgentEvent {
    let mut metadata = HashMap::new();
    if let Some(pr) = &session.pull_request
        && let Ok(json) = serde_json::to_string(pr)
    {
        metadata.insert(PULL_REQUEST_METADATA_KEY.to_string(), json);
    }
    metadata.insert(
        PULL_REQUEST_REVISION_METADATA_KEY.to_string(),
        session.pull_request_revision.to_string(),
    );
    AgentEvent {
        session_id: session.session_id.clone(),
        // `None`, like the shell-activity monitor's events: this is the
        // daemon speaking, not the agent, so it must not read as the producer
        // identifying itself to a prompt-delivery watch on the pane. An older
        // client only ever upgrades a card's type FROM `None`.
        agent_type: crate::event::AgentType::None,
        event_type: EventType::PullRequest,
        tool_name: None,
        tool_detail: None,
        cwd: session.cwd.clone(),
        timestamp: session.last_activity,
        user_prompt: None,
        metadata,
        pane_id: session.pane_id.clone(),
        agent_id: session.agent_id.clone(),
        agent_version: None,
        schema_version: None,
        live_target: None,
        // Issue #1678: no aggregate, so a client keeps the card's held one.
        background: None,
        background_revision: None,
    }
}

/// The revision for a pull request change the daemon is about to report —
/// see the module docs. Never `0`, which a session that has had no change
/// reports.
pub(crate) fn next_revision() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        });
    let mut last = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(last.saturating_add(1));
        match LAST.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// Which event types ask for an early re-read: an agent starting, a turn
/// ending, an agent stopping.
fn is_refresh_trigger(event_type: &EventType) -> bool {
    matches!(
        event_type,
        EventType::SessionStart | EventType::Idle | EventType::SessionEnd
    )
}

#[derive(Debug, Default)]
struct CwdEntry {
    key: Option<PrKey>,
    resolved_at: Option<Instant>,
    kicked: bool,
}

/// Whether a directory's branch needs re-reading at `now`: never read, once an
/// interval, or after a turn end once the same debounce as a lookup's passed —
/// so a stream of Idle reports cannot drive `git` at its completion rate.
fn resolve_due(
    resolved_at: Option<Instant>,
    kicked: bool,
    now: Instant,
    interval: Duration,
) -> bool {
    let Some(at) = resolved_at else {
        return true;
    };
    let age = now.saturating_duration_since(at);
    age >= interval || (kicked && age >= interval.min(MAX_KICK_DEBOUNCE))
}

/// The monitor's background jobs of one kind (`git` probes per directory, `gh`
/// queries per key): at most one per id, all owned here and aborted when the
/// pool is dropped — the monitor stopping drops it — or when [`Self::retain`]
/// drops their id. Aborting a job drops its future, which kills its child
/// (`kill_on_drop`). Every job runs only while holding a permit from a
/// semaphore shared by every pool, which bounds how many run at once.
pub(crate) struct JobPool<K, T> {
    jobs: tokio::task::JoinSet<(K, u64, T)>,
    running: HashMap<K, (u64, tokio::task::AbortHandle)>,
    next_id: u64,
    permits: Arc<tokio::sync::Semaphore>,
}

impl<K, T> JobPool<K, T>
where
    K: Clone + Eq + std::hash::Hash + Send + 'static,
    T: Send + 'static,
{
    pub(crate) fn new(permits: Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            jobs: tokio::task::JoinSet::new(),
            running: HashMap::new(),
            next_id: 0,
            permits,
        }
    }

    pub(crate) fn is_running(&self, id: &K) -> bool {
        self.running.contains_key(id)
    }

    /// Jobs started and neither finished nor cancelled, waiting for a permit
    /// or running.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.running.len()
    }

    /// Start `job` for `id`, cancelling one already running for it.
    pub(crate) fn spawn<F>(&mut self, id: K, job: F)
    where
        F: std::future::Future<Output = T> + Send + 'static,
    {
        self.cancel(&id);
        let generation = self.next_id;
        self.next_id += 1;
        let permits = self.permits.clone();
        let task_id = id.clone();
        let handle = self.jobs.spawn(async move {
            // The semaphore is never closed, so this only waits.
            let _permit = permits.acquire_owned().await;
            (task_id, generation, job.await)
        });
        self.running.insert(id, (generation, handle));
    }

    pub(crate) fn cancel(&mut self, id: &K) {
        if let Some((_, handle)) = self.running.remove(id) {
            handle.abort();
        }
    }

    /// Cancel every job whose id `keep` rejects.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) {
        self.running.retain(|id, (_, handle)| {
            let kept = keep(id);
            if !kept {
                handle.abort();
            }
            kept
        });
    }

    /// The next job to finish, skipping cancelled and superseded ones. `None`
    /// once no job is left.
    pub(crate) async fn next(&mut self) -> Option<(K, T)> {
        while let Some(joined) = self.jobs.join_next().await {
            let Ok((id, generation, value)) = joined else {
                continue;
            };
            if self.running.get(&id).is_some_and(|(g, _)| *g == generation) {
                self.running.remove(&id);
                return Some((id, value));
            }
        }
        None
    }
}

/// PRD #1401: the daemon's background task that resolves every live
/// session's pull request and reports changes — see the module docs. Runs
/// until aborted (`run_daemon_with`'s cleanup) or the broadcast closes; either
/// way its jobs, and the children they run, go with it.
pub async fn run_pull_request_monitor(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    interval: Duration,
) {
    let mut events = event_tx.subscribe();
    let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_JOBS));
    let mut probes: JobPool<PathBuf, Option<PrKey>> = JobPool::new(permits.clone());
    let mut lookups: JobPool<PrKey, Result<Option<PullRequestInfo>, String>> =
        JobPool::new(permits);
    // One timer for the monitor's life: a sleep rebuilt on every pass would
    // start over at each broadcast message, so steady unrelated traffic could
    // keep it from ever firing. A tick missed while a pass runs is delayed,
    // not burst.
    let mut ticker = tokio::time::interval(interval.min(Duration::from_secs(5)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut cwds: HashMap<PathBuf, CwdEntry> = HashMap::new();
    let mut keys: HashMap<PrKey, KeyEntry> = HashMap::new();
    let mut kicked_sessions: HashSet<String> = HashSet::new();
    // The key each session's current value was resolved under (`None`: no
    // key), so a session that moved to another branch drops the old branch's
    // PR before the new branch has an answer.
    let mut shown_keys: HashMap<String, Option<PrKey>> = HashMap::new();
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            msg = events.recv() => match msg {
                Ok(BroadcastMsg::Event(event)) => {
                    if is_refresh_trigger(&event.event_type) {
                        kicked_sessions.insert(event.session_id);
                    } else {
                        continue;
                    }
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
            Some((cwd, key)) = probes.next() => {
                // A directory dropped meanwhile had its job cancelled, so a
                // result always has its entry.
                if let Some(entry) = cwds.get_mut(&cwd) {
                    entry.key = key;
                    entry.resolved_at = Some(Instant::now());
                }
            }
            Some((key, result)) = lookups.next() => {
                let entry = keys.entry(key.clone()).or_default();
                entry.in_flight = false;
                entry.fetched_at = Some(Instant::now());
                match result {
                    Ok(value) => entry.record(value, Instant::now()),
                    Err(error) => {
                        entry.failed = true;
                        if !entry.failure_logged {
                            entry.failure_logged = true;
                            warn!(
                                repo = %escape_control_and_bidi(&key.slug),
                                branch = %escape_control_and_bidi(&key.branch),
                                error = %escape_control_and_bidi(&error),
                                "pull request lookup failed; the agent's card shows no \
                                 pull request until a later lookup succeeds"
                            );
                        }
                    }
                }
            }
        }
        let now = Instant::now();

        // Which directory each live session works in: its own report, else
        // the directory the deck started its agent in.
        let record_cwds: HashMap<String, String> = registry
            .agent_records()
            .into_iter()
            .filter_map(|record| Some((record.id, record.cwd?)))
            .collect();
        let sessions: Vec<(String, Option<PathBuf>)> = {
            let state = state.read().await;
            state
                .sessions
                .values()
                .map(|session| {
                    let cwd = session
                        .cwd
                        .clone()
                        .or_else(|| {
                            session
                                .agent_id
                                .as_ref()
                                .and_then(|id| record_cwds.get(id).cloned())
                        })
                        .map(PathBuf::from);
                    (session.session_id.clone(), cwd)
                })
                .collect()
        };

        for (session_id, cwd) in &sessions {
            let Some(cwd) = cwd else { continue };
            let entry = cwds.entry(cwd.clone()).or_default();
            if kicked_sessions.contains(session_id) {
                entry.kicked = true;
                if let Some(key) = &entry.key {
                    keys.entry(key.clone()).or_default().kicked = true;
                }
            }
        }
        kicked_sessions.clear();

        // Only directories some session still works in are tracked; a probe
        // for one no session references any more is cancelled with it.
        let live_cwds: HashSet<&PathBuf> =
            sessions.iter().filter_map(|(_, c)| c.as_ref()).collect();
        cwds.retain(|cwd, _| live_cwds.contains(cwd));
        probes.retain(|cwd| live_cwds.contains(cwd));

        // Re-read each directory's branch: new, once an interval, or after a
        // turn end once the debounce passed.
        for (cwd, entry) in cwds.iter_mut() {
            if probes.is_running(cwd)
                || !resolve_due(entry.resolved_at, entry.kicked, now, interval)
            {
                continue;
            }
            entry.kicked = false;
            let probe = cwd.clone();
            probes.spawn(cwd.clone(), async move { branch_key(&probe).await });
        }

        // Look up each key in use that is due; a lookup for a key no
        // directory uses any more is cancelled.
        let mut key_cwd: HashMap<PrKey, PathBuf> = HashMap::new();
        for (cwd, entry) in &cwds {
            if let Some(key) = &entry.key {
                key_cwd.entry(key.clone()).or_insert_with(|| cwd.clone());
            }
        }
        keys.retain(|key, _| key_cwd.contains_key(key));
        lookups.retain(|key| key_cwd.contains_key(key));
        for (key, cwd) in &key_cwd {
            let entry = keys.entry(key.clone()).or_default();
            entry.in_flight = lookups.is_running(key);
            if !fetch_due(entry, now, interval) {
                continue;
            }
            entry.in_flight = true;
            entry.kicked = false;
            debug!(
                repo = %escape_control_and_bidi(&key.slug),
                branch = %escape_control_and_bidi(&key.branch),
                "looking up the branch's pull request"
            );
            let (job_key, cwd) = (key.clone(), cwd.clone());
            lookups.spawn(key.clone(), async move { query_gh(cwd, job_key).await });
        }

        // What each session should show now. A session whose directory has no
        // answer yet keeps what it has, and so does one whose key has none yet
        // while that key is the one its value came from — a failed re-read
        // keeps the last answer. A session whose key changed drops the old
        // key's PR at once and shows the new key's when that resolves.
        let live_sessions: HashSet<&String> = sessions.iter().map(|(id, _)| id).collect();
        shown_keys.retain(|id, _| live_sessions.contains(id));
        let mut want: HashMap<String, Option<PullRequestInfo>> = HashMap::new();
        for (session_id, cwd) in &sessions {
            let Some(entry) = cwd.as_ref().and_then(|cwd| cwds.get(cwd)) else {
                continue;
            };
            if entry.resolved_at.is_none() {
                continue;
            }
            let value = match &entry.key {
                None => None,
                Some(key) => match keys.get(key).and_then(|k| k.value.clone()) {
                    Some(value) => value,
                    None if shown_keys.get(session_id) == Some(&entry.key) => continue,
                    None => None,
                },
            };
            shown_keys.insert(session_id.clone(), entry.key.clone());
            want.insert(session_id.clone(), value);
        }
        let needs_write = {
            let state = state.read().await;
            want.iter().any(|(id, value)| {
                state
                    .sessions
                    .get(id)
                    .is_some_and(|session| session.pull_request != *value)
            })
        };
        if needs_write {
            let mut state = state.write().await;
            for event in state.set_pull_requests(&want) {
                let _ = event_tx.send(BroadcastMsg::Event(event));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh(entries: serde_json::Value) -> String {
        entries.to_string()
    }

    fn gh_pr(
        number: u64,
        branch: &str,
        state: &str,
        draft: bool,
        review: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "headRefName": branch,
            "state": state,
            "isDraft": draft,
            "reviewDecision": review,
            "url": format!("https://github.com/o/r/pull/{number}"),
        })
    }

    #[test]
    fn gh_states_and_review_decisions_map_onto_the_badge() {
        use serde_json::json;
        let cases = [
            (
                "OPEN",
                false,
                json!("REVIEW_REQUIRED"),
                PullRequestState::Open,
                Some(PullRequestReview::ReviewRequired),
            ),
            (
                "OPEN",
                true,
                json!("CHANGES_REQUESTED"),
                PullRequestState::Draft,
                Some(PullRequestReview::ChangesRequested),
            ),
            (
                "MERGED",
                false,
                json!("APPROVED"),
                PullRequestState::Merged,
                Some(PullRequestReview::Approved),
            ),
            (
                "CLOSED",
                false,
                serde_json::Value::Null,
                PullRequestState::Closed,
                None,
            ),
            ("OPEN", false, json!(""), PullRequestState::Open, None),
        ];
        for (state, draft, review, want_state, want_review) in cases {
            let pr = parse_pr_list(
                &gh(json!([gh_pr(7, "feat/x", state, draft, review)])),
                "feat/x",
            )
            .unwrap()
            .unwrap();
            assert_eq!(pr.state, want_state, "{state} draft={draft}");
            assert_eq!(pr.review, want_review, "{state}");
            assert_eq!(pr.number, 7);
            assert_eq!(pr.url, "https://github.com/o/r/pull/7");
        }
    }

    #[test]
    fn only_an_exact_branch_match_counts_and_an_open_one_wins() {
        use serde_json::json;
        let out = gh(json!([
            gh_pr(9000, "feat/x-other", "OPEN", false, json!("APPROVED")),
            gh_pr(8000, "feat/x", "MERGED", false, json!("APPROVED")),
            gh_pr(1234, "feat/x", "OPEN", false, json!("REVIEW_REQUIRED")),
            gh_pr(1000, "feat/x", "CLOSED", false, serde_json::Value::Null),
        ]));
        assert_eq!(parse_pr_list(&out, "feat/x").unwrap().unwrap().number, 1234);
        // A draft is open too.
        let out = gh(json!([
            gh_pr(8000, "feat/x", "MERGED", false, json!("APPROVED")),
            gh_pr(1234, "feat/x", "OPEN", true, serde_json::Value::Null),
        ]));
        assert_eq!(parse_pr_list(&out, "feat/x").unwrap().unwrap().number, 1234);
    }

    #[test]
    fn with_no_open_match_the_highest_number_wins() {
        use serde_json::json;
        let out = gh(json!([
            gh_pr(1000, "feat/x", "CLOSED", false, serde_json::Value::Null),
            gh_pr(1234, "feat/x", "MERGED", false, json!("APPROVED")),
            gh_pr(1100, "feat/x", "CLOSED", false, serde_json::Value::Null),
        ]));
        let pr = parse_pr_list(&out, "feat/x").unwrap().unwrap();
        assert_eq!((pr.number, pr.state), (1234, PullRequestState::Merged));
    }

    #[test]
    fn no_match_unknown_states_and_foreign_urls_read_as_no_pr() {
        use serde_json::json;
        assert_eq!(parse_pr_list("[]", "feat/x").unwrap(), None);
        assert_eq!(
            parse_pr_list(
                &gh(json!([gh_pr(1, "other", "OPEN", false, json!(""))])),
                "feat/x"
            )
            .unwrap(),
            None
        );
        assert_eq!(
            parse_pr_list(
                &gh(json!([gh_pr(1, "feat/x", "QUEUED", false, json!(""))])),
                "feat/x"
            )
            .unwrap(),
            None
        );
        let mut foreign = gh_pr(1, "feat/x", "OPEN", false, json!(""));
        foreign["url"] = json!("https://evil.example/pull/1");
        assert_eq!(
            parse_pr_list(&gh(json!([foreign])), "feat/x").unwrap(),
            None
        );
    }

    /// Scenario: a contributor's fork opens a pull request into the
    /// repository from a branch named like the agent's. The badge shows the
    /// agent's own pull request even though the fork's has the higher number,
    /// and with only the fork's it shows none (PR #1680 review).
    #[test]
    fn a_pull_request_from_a_fork_is_not_the_branchs() {
        use serde_json::json;
        assert!(
            GH_PR_FIELDS
                .split(',')
                .any(|field| field == "isCrossRepository"),
            "gh must be asked where each PR's branch lives"
        );
        let mut fork = gh_pr(9000, "feat/x", "OPEN", false, json!(""));
        fork["isCrossRepository"] = json!(true);
        let mut own = gh_pr(1234, "feat/x", "OPEN", false, json!(""));
        own["isCrossRepository"] = json!(false);
        assert_eq!(
            parse_pr_list(&gh(json!([own, fork.clone()])), "feat/x")
                .unwrap()
                .map(|pr| pr.number),
            Some(1234)
        );
        assert_eq!(parse_pr_list(&gh(json!([fork])), "feat/x").unwrap(), None);
    }

    #[test]
    fn unparsable_gh_output_is_an_error_not_no_pr() {
        assert!(parse_pr_list("not json", "feat/x").is_err());
    }

    #[test]
    fn the_default_branch_is_origins_head_or_main_and_master_without_one() {
        assert!(is_default_branch("main", Some("main")));
        assert!(is_default_branch("trunk", Some("trunk")));
        assert!(!is_default_branch("main", Some("trunk")));
        assert!(!is_default_branch("feat/x", Some("main")));
        assert!(is_default_branch("main", None));
        assert!(is_default_branch("master", None));
        assert!(!is_default_branch("feat/x", None));
    }

    #[test]
    fn the_refresh_interval_seam_falls_back_to_the_default() {
        assert_eq!(refresh_interval_from(None), DEFAULT_REFRESH);
        assert_eq!(refresh_interval_from(Some("")), DEFAULT_REFRESH);
        assert_eq!(refresh_interval_from(Some("0")), DEFAULT_REFRESH);
        assert_eq!(refresh_interval_from(Some("soon")), DEFAULT_REFRESH);
        assert_eq!(refresh_interval_from(Some(" 7 ")), Duration::from_secs(7));
    }

    fn fetched(value: Option<Option<PullRequestInfo>>, failed: bool, at: Instant) -> KeyEntry {
        KeyEntry {
            value,
            fetched_at: Some(at),
            failed,
            ..KeyEntry::default()
        }
    }

    fn pr_in(state: PullRequestState) -> Option<Option<PullRequestInfo>> {
        Some(Some(PullRequestInfo {
            number: 1,
            url: "https://github.com/o/r/pull/1".into(),
            state,
            review: None,
            checks: None,
        }))
    }

    #[test]
    fn a_new_key_is_due_and_one_in_flight_never_is() {
        let now = Instant::now();
        let interval = Duration::from_secs(60);
        assert!(fetch_due(&KeyEntry::default(), now, interval));
        let in_flight = KeyEntry {
            in_flight: true,
            ..KeyEntry::default()
        };
        assert!(!fetch_due(&in_flight, now, interval));
    }

    /// Issue #1781 (PR #1783 review): a PR merged or closed while its checks
    /// still run keeps being re-read until they finish, then stops, so its
    /// CI mark does not stay pending for good.
    #[test]
    fn a_merged_or_closed_pr_with_pending_checks_is_polled_until_they_finish() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        let with_checks = |state, checks_state| {
            let mut value = pr_in(state);
            value.as_mut().unwrap().as_mut().unwrap().checks = Some(PullRequestChecks {
                state: checks_state,
                passing: 1,
                failing: 0,
                pending: 1,
                skipped: 0,
            });
            let mut entry = KeyEntry::default();
            entry.record(value.unwrap(), at);
            entry.fetched_at = Some(at);
            entry
        };
        for state in [PullRequestState::Merged, PullRequestState::Closed] {
            let pending = with_checks(state, PullRequestChecksState::Pending);
            assert!(!fetch_due(&pending, at + Duration::from_secs(59), interval));
            assert!(fetch_due(&pending, at + interval, interval), "{state:?}");
            for finished in [
                PullRequestChecksState::Passing,
                PullRequestChecksState::Failing,
            ] {
                let done = with_checks(state, finished);
                assert!(!fetch_due(&done, at + interval * 10, interval), "{state:?}");
            }
        }
    }

    /// Issue #1781 (PR #1783 review): a check that never finishes, such as a
    /// stale run, stops the re-reads of a merged or closed PR once the window
    /// passed, counted from when it first closed, not from the last re-read.
    #[test]
    fn a_closed_pr_whose_checks_never_finish_stops_being_polled() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        let pending = PullRequestChecks {
            state: PullRequestChecksState::Pending,
            passing: 0,
            failing: 0,
            pending: 1,
            skipped: 0,
        };
        let mut closed = pr_in(PullRequestState::Closed).unwrap().unwrap();
        closed.checks = Some(pending);
        let mut entry = KeyEntry::default();
        entry.record(Some(closed.clone()), at);
        // Every later re-read keeps the first close time.
        let later = at + TERMINAL_PENDING_CHECKS_WINDOW - interval * 2;
        entry.record(Some(closed.clone()), later);
        assert_eq!(entry.terminal_since, Some(at));
        entry.fetched_at = Some(later);
        // Inside the window it is still re-read every interval…
        assert!(fetch_due(&entry, later + interval, interval));
        // …and once the window has passed, never again.
        assert!(!fetch_due(
            &entry,
            at + TERMINAL_PENDING_CHECKS_WINDOW + interval,
            interval
        ));
        // Reopened, the PR is open again and the clock starts over.
        let mut open = closed;
        open.state = PullRequestState::Open;
        entry.record(Some(open), later);
        assert_eq!(entry.terminal_since, None);
    }

    #[test]
    fn an_open_or_draft_pr_is_polled_every_interval() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        for state in [PullRequestState::Open, PullRequestState::Draft] {
            let entry = fetched(pr_in(state), false, at);
            assert!(!fetch_due(&entry, at + Duration::from_secs(59), interval));
            assert!(fetch_due(&entry, at + interval, interval));
        }
    }

    #[test]
    fn a_merged_or_closed_pr_is_never_polled_or_kicked() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        for state in [PullRequestState::Merged, PullRequestState::Closed] {
            let mut entry = fetched(pr_in(state), false, at);
            entry.kicked = true;
            assert!(!fetch_due(&entry, at + interval * 100, interval));
        }
    }

    #[test]
    fn a_turn_end_re_reads_after_the_debounce() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        for value in [pr_in(PullRequestState::Open), Some(None)] {
            let mut entry = fetched(value, false, at);
            entry.kicked = true;
            assert!(!fetch_due(&entry, at + Duration::from_secs(9), interval));
            assert!(fetch_due(&entry, at + MAX_KICK_DEBOUNCE, interval));
        }
        // A short interval shortens the debounce with it.
        let mut entry = fetched(Some(None), false, at);
        entry.kicked = true;
        assert!(fetch_due(
            &entry,
            at + Duration::from_secs(1),
            Duration::from_secs(1)
        ));
    }

    /// Scenario: the agent is idle on a branch with no pull request, and the
    /// user opens one from another terminal. No turn ends to prompt a
    /// re-read, yet the badge must appear within about a minute (PRD #1401),
    /// so a branch with no PR is re-read every interval (PR #1680 review).
    #[test]
    fn no_pr_is_polled_every_interval() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        let entry = fetched(Some(None), false, at);
        assert!(!fetch_due(&entry, at + Duration::from_secs(59), interval));
        assert!(fetch_due(&entry, at + interval, interval));
    }

    #[test]
    fn a_failure_backs_off_even_when_kicked() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        let mut entry = fetched(pr_in(PullRequestState::Open), true, at);
        entry.kicked = true;
        assert!(!fetch_due(&entry, at + interval, interval));
        assert!(!fetch_due(
            &entry,
            at + interval * (FAILURE_BACKOFF_FACTOR - 1),
            interval
        ));
        assert!(fetch_due(
            &entry,
            at + interval * FAILURE_BACKOFF_FACTOR,
            interval
        ));
    }

    #[test]
    fn the_gh_query_names_the_branch_the_repo_and_every_field() {
        let args = gh_pr_list_args("feat/x", "o/r", GH_PR_FIELDS);
        assert_eq!(
            args,
            [
                "pr",
                "list",
                "--head",
                "feat/x",
                "--state",
                "all",
                "--repo",
                "o/r",
                "--json",
                "number,state,isDraft,reviewDecision,url,headRefName,isCrossRepository,\
                 statusCheckRollup",
            ]
        );
    }

    /// A check run with a name of its own, so no two count as one check.
    fn check_run(status: &str, conclusion: &str) -> serde_json::Value {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        named_run(
            &format!("job-{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            status,
            conclusion,
            None,
        )
    }

    fn named_run(
        name: &str,
        status: &str,
        conclusion: &str,
        started_at: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "__typename": "CheckRun",
            "name": name,
            "workflowName": "CI",
            "status": status,
            "conclusion": conclusion,
            "startedAt": started_at,
        })
    }

    fn status_context(state: &str) -> serde_json::Value {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        serde_json::json!({
            "__typename": "StatusContext",
            "context": format!("ci/{}", NEXT.fetch_add(1, Ordering::Relaxed)),
            "state": state,
        })
    }

    fn checks_of(rollup: serde_json::Value) -> Option<PullRequestChecks> {
        let mut pr = gh_pr(7, "feat/x", "OPEN", false, serde_json::json!(""));
        pr["statusCheckRollup"] = rollup;
        parse_pr_list(&gh(serde_json::json!([pr])), "feat/x")
            .unwrap()
            .unwrap()
            .checks
    }

    /// Issue #1781: check runs and commit statuses land in GitHub's buckets,
    /// and the overall result is GitHub's: any failure is red, otherwise
    /// anything unfinished is pending, otherwise green.
    #[test]
    fn checks_roll_up_the_way_github_lists_them() {
        use serde_json::json;
        let all_kinds = checks_of(json!([
            check_run("COMPLETED", "SUCCESS"),
            check_run("COMPLETED", "FAILURE"),
            check_run("COMPLETED", "TIMED_OUT"),
            check_run("COMPLETED", "CANCELLED"),
            check_run("COMPLETED", "ACTION_REQUIRED"),
            check_run("COMPLETED", "STARTUP_FAILURE"),
            check_run("COMPLETED", "SKIPPED"),
            check_run("COMPLETED", "NEUTRAL"),
            check_run("IN_PROGRESS", ""),
            check_run("QUEUED", ""),
            status_context("SUCCESS"),
            status_context("ERROR"),
            status_context("FAILURE"),
            status_context("PENDING"),
            status_context("EXPECTED"),
        ]))
        .unwrap();
        assert_eq!(
            all_kinds,
            PullRequestChecks {
                state: PullRequestChecksState::Failing,
                passing: 2,
                failing: 7,
                pending: 4,
                skipped: 2,
            }
        );
        let pending = checks_of(json!([
            check_run("COMPLETED", "SUCCESS"),
            check_run("IN_PROGRESS", ""),
        ]))
        .unwrap();
        assert_eq!(
            (pending.state, pending.passing, pending.pending),
            (PullRequestChecksState::Pending, 1, 1)
        );
        let passing = checks_of(json!([
            check_run("COMPLETED", "SUCCESS"),
            check_run("COMPLETED", "SKIPPED"),
            status_context("SUCCESS"),
        ]))
        .unwrap();
        assert_eq!(
            (passing.state, passing.passing, passing.skipped),
            (PullRequestChecksState::Passing, 2, 1)
        );
    }

    /// Issue #1781 (PR #1783 review): a check that failed and then passed on
    /// a re-run counts once, as its newest run, and a re-run still queued is
    /// newer than any finished run of the same check.
    #[test]
    fn only_the_newest_run_of_a_check_counts() {
        use serde_json::json;
        let rerun_passed = checks_of(json!([
            named_run("test", "COMPLETED", "FAILURE", Some("2026-10-20T10:00:00Z")),
            named_run("test", "COMPLETED", "SUCCESS", Some("2026-10-20T11:00:00Z")),
            named_run(
                "build",
                "COMPLETED",
                "SUCCESS",
                Some("2026-10-20T10:00:00Z")
            ),
        ]))
        .unwrap();
        assert_eq!(
            (
                rerun_passed.state,
                rerun_passed.passing,
                rerun_passed.failing
            ),
            (PullRequestChecksState::Passing, 2, 0)
        );
        let requeued = checks_of(json!([
            named_run("test", "COMPLETED", "SUCCESS", Some("2026-10-20T11:00:00Z")),
            named_run("test", "QUEUED", "", None),
        ]))
        .unwrap();
        assert_eq!(
            (requeued.state, requeued.passing, requeued.pending),
            (PullRequestChecksState::Pending, 0, 1)
        );
        // The same job name in two workflows is two checks.
        let mut other = named_run("test", "COMPLETED", "FAILURE", Some("2026-10-20T09:00:00Z"));
        other["workflowName"] = json!("Docs");
        let two = checks_of(json!([
            named_run("test", "COMPLETED", "SUCCESS", Some("2026-10-20T11:00:00Z")),
            other,
        ]))
        .unwrap();
        assert_eq!((two.passing, two.failing), (1, 1));
    }

    /// Issue #1781 (PR #1783 review): a stale run never finished, so it is
    /// pending, alone or beside passing checks.
    #[test]
    fn a_stale_check_is_pending() {
        use serde_json::json;
        let alone = checks_of(json!([check_run("COMPLETED", "STALE")])).unwrap();
        assert_eq!(
            (alone.state, alone.pending),
            (PullRequestChecksState::Pending, 1)
        );
        let beside = checks_of(json!([
            check_run("COMPLETED", "SUCCESS"),
            check_run("COMPLETED", "STALE"),
        ]))
        .unwrap();
        assert_eq!(
            (beside.state, beside.passing, beside.pending),
            (PullRequestChecksState::Pending, 1, 1)
        );
    }

    /// Issue #1781: a PR with no checks shows no CI icon, whether `gh` sends
    /// an empty rollup, a null one, none at all, or only values this build
    /// cannot place.
    #[test]
    fn no_checks_means_no_ci_summary() {
        use serde_json::json;
        assert_eq!(checks_of(json!([])), None);
        assert_eq!(checks_of(serde_json::Value::Null), None);
        assert_eq!(
            checks_of(json!([check_run("COMPLETED", "SOMETHING_NEW")])),
            None
        );
        let pr = parse_pr_list(
            &gh(json!([gh_pr(7, "feat/x", "OPEN", false, json!(""))])),
            "feat/x",
        )
        .unwrap()
        .unwrap();
        assert_eq!(pr.checks, None);
    }

    /// The report a client applies carries the PR as JSON under the key, and
    /// a cleared PR as no key at all; both decode back through
    /// `pull_request_report`.
    #[test]
    fn a_report_event_round_trips_through_the_wire() {
        let mut state = crate::state::AppState::default();
        let event = |pr: Option<PullRequestInfo>| {
            let mut session = sample_session();
            session.pull_request = pr;
            report_event(&session)
        };
        let info = PullRequestInfo {
            number: 1234,
            url: "https://github.com/o/r/pull/1234".into(),
            state: PullRequestState::Open,
            review: Some(PullRequestReview::ReviewRequired),
            checks: None,
        };
        let open = event(Some(info.clone()));
        assert_eq!(open.event_type, EventType::PullRequest);
        let wire: AgentEvent =
            serde_json::from_str(&serde_json::to_string(&open).unwrap()).unwrap();
        assert_eq!(wire.pull_request_report(), Some(Some(info.clone())));
        let cleared = event(None);
        assert!(!cleared.metadata.contains_key(PULL_REQUEST_METADATA_KEY));
        assert_eq!(cleared.pull_request_report(), Some(None));
        state.sessions.insert("s1".into(), sample_session());
        state.apply_event(open);
        assert_eq!(state.sessions["s1"].pull_request, Some(info));
        state.apply_event(cleared);
        assert_eq!(state.sessions["s1"].pull_request, None);
    }

    fn apply_pr_report_then_stale_snapshot(next: Option<PullRequestInfo>) {
        let mut daemon = crate::state::AppState::default();
        let mut session = sample_session();
        session.pull_request = pr_in(PullRequestState::Open).flatten();
        let stale = session.live_snapshot();
        daemon.sessions.insert("s1".into(), session.clone());
        let mut client = crate::state::AppState::default();
        client.register_pane("pane-1".into());
        client.sessions.insert("s1".into(), session);
        let events = daemon.set_pull_requests(&HashMap::from([("s1".into(), next.clone())]));
        assert_eq!(events.len(), 1);
        client.apply_event(events[0].clone());
        assert_eq!(client.sessions["s1"].pull_request, next, "report applied");
        client.seed_hydrated_session(
            "pane-1".into(),
            Some("/work/repo".into()),
            Some(crate::event::AgentType::ClaudeCode),
            Some("agent-1".into()),
            Some(&stale),
        );
        assert_eq!(
            client.sessions["s1"].pull_request, next,
            "a delayed ListAgents snapshot must not overwrite the newer PR report"
        );
    }

    /// Scenario: Deliver a merged PR event before an older open-PR ListAgents
    /// reply. Hydrating that delayed reply must leave the card merged.
    #[test]
    fn a_stale_snapshot_cannot_reopen_a_merged_pull_request() {
        apply_pr_report_then_stale_snapshot(pr_in(PullRequestState::Merged).flatten());
    }

    /// Scenario: Clear a card's PR by event, then hydrate an older snapshot
    /// containing its former PR. The card must remain without a PR.
    #[test]
    fn a_stale_snapshot_cannot_restore_a_cleared_pull_request() {
        apply_pr_report_then_stale_snapshot(None);
    }

    /// Scenario: Hydrate a card from a current merged-PR snapshot without
    /// receiving a PR event. The current reply must still update its badge.
    #[test]
    fn a_current_snapshot_can_update_a_pull_request_without_a_report() {
        let mut daemon = crate::state::AppState::default();
        let mut session = sample_session();
        session.pull_request = pr_in(PullRequestState::Open).flatten();
        daemon.sessions.insert("s1".into(), session.clone());
        let mut client = crate::state::AppState::default();
        client.register_pane("pane-1".into());
        client.sessions.insert("s1".into(), session);
        let merged = pr_in(PullRequestState::Merged).flatten();
        daemon.set_pull_requests(&HashMap::from([("s1".into(), merged.clone())]));
        let current = daemon.sessions["s1"].live_snapshot();
        client.seed_hydrated_session(
            "pane-1".into(),
            Some("/work/repo".into()),
            Some(crate::event::AgentType::ClaudeCode),
            Some("agent-1".into()),
            Some(&current),
        );
        assert_eq!(client.sessions["s1"].pull_request, merged);
    }

    /// A client holding card `s1` on `pane-1` with an open PR at `revision`,
    /// as a report from the daemon it was attached to left it.
    fn client_holding_revision(revision: u64) -> crate::state::AppState {
        let mut client = crate::state::AppState::default();
        client.register_pane("pane-1".into());
        let mut session = sample_session();
        session.pull_request = pr_in(PullRequestState::Open).flatten();
        session.pull_request_revision = revision;
        client.sessions.insert("s1".into(), session);
        client
    }

    /// A report for `s1` of `pr` at `revision`, or with no revision at all as
    /// from a daemon that sends none.
    fn report_at(pr: Option<PullRequestInfo>, revision: Option<u64>) -> AgentEvent {
        let mut session = sample_session();
        session.pull_request = pr;
        session.pull_request_revision = revision.unwrap_or(0);
        let mut event = report_event(&session);
        if revision.is_none() {
            event.metadata.remove(PULL_REQUEST_REVISION_METADATA_KEY);
        }
        event
    }

    /// Resynchronize `client` after an event-stream gap from a daemon whose
    /// snapshot of `s1` carries `pr` at `revision`.
    fn resync_with(
        client: &mut crate::state::AppState,
        pr: Option<PullRequestInfo>,
        revision: Option<u64>,
    ) {
        let mut session = sample_session();
        session.pull_request = pr;
        let mut snapshot = session.live_snapshot();
        snapshot.pull_request_revision = revision;
        let mut record: crate::agent_pty::AgentRecord =
            serde_json::from_value(serde_json::json!({ "id": "agent-1", "pane_id_env": "pane-1" }))
                .unwrap();
        record.live = Some(snapshot);
        client.resync_after_event_gap(&[record]);
    }

    /// Scenario: A card holds a PR revision from a daemon that was then
    /// replaced by one whose clock is behind. The replacement's resync
    /// snapshot and its later reports must both land on the card.
    #[test]
    fn a_replacement_daemon_with_a_lower_clock_is_adopted() {
        let mut client = client_holding_revision(10_000);
        let merged = pr_in(PullRequestState::Merged).flatten();
        resync_with(&mut client, merged.clone(), Some(500));
        assert_eq!(
            client.sessions["s1"].pull_request, merged,
            "the replacement daemon's snapshot must be adopted"
        );
        let closed = pr_in(PullRequestState::Closed).flatten();
        client.apply_event(report_at(closed.clone(), Some(600)));
        assert_eq!(
            client.sessions["s1"].pull_request, closed,
            "a later report from the replacement daemon must be applied"
        );
    }

    /// Scenario: A card holds an open PR from a daemon that was replaced by
    /// one that knows no PR for it (revision 0). The resync must clear the
    /// badge.
    #[test]
    fn a_cleared_pull_request_on_a_replacement_daemon_is_adopted() {
        let mut client = client_holding_revision(10_000);
        resync_with(&mut client, None, Some(0));
        assert_eq!(client.sessions["s1"].pull_request, None);
        assert_eq!(client.sessions["s1"].pull_request_revision, 0);
    }

    /// Scenario: A card holds a revision from a newer daemon, then takes a
    /// report and a snapshot from an older daemon that sends no revision, then
    /// a report from a newer daemon with a lower clock. No stale floor may
    /// remain to refuse the last one.
    #[test]
    fn a_value_with_no_revision_leaves_no_stale_floor() {
        let merged = pr_in(PullRequestState::Merged).flatten();
        let closed = pr_in(PullRequestState::Closed).flatten();

        let mut client = client_holding_revision(10_000);
        client.apply_event(report_at(merged.clone(), None));
        assert_eq!(client.sessions["s1"].pull_request, merged);
        client.apply_event(report_at(closed.clone(), Some(500)));
        assert_eq!(
            client.sessions["s1"].pull_request, closed,
            "after a report with no revision"
        );

        let mut client = client_holding_revision(10_000);
        let mut session = sample_session();
        session.pull_request = merged.clone();
        let mut snapshot = session.live_snapshot();
        snapshot.pull_request_revision = None;
        client.seed_hydrated_session(
            "pane-1".into(),
            Some("/work/repo".into()),
            Some(crate::event::AgentType::ClaudeCode),
            Some("agent-1".into()),
            Some(&snapshot),
        );
        assert_eq!(client.sessions["s1"].pull_request, merged);
        client.apply_event(report_at(closed.clone(), Some(500)));
        assert_eq!(
            client.sessions["s1"].pull_request, closed,
            "after a snapshot with no revision"
        );
    }

    /// The report moves nothing else on the card — not status, not activity,
    /// not the journal — and never creates a card.
    #[test]
    fn a_report_is_status_neutral_and_creates_no_card() {
        let mut state = crate::state::AppState::default();
        let before = sample_session();
        state.sessions.insert("s1".into(), before.clone());
        let mut with_pr = before.clone();
        with_pr.pull_request = pr_in(PullRequestState::Merged).flatten();
        let mut event = report_event(&with_pr);
        event.timestamp = before.last_activity + chrono::Duration::hours(1);
        state.apply_event(event.clone());
        let after = &state.sessions["s1"];
        assert_eq!(after.status, before.status);
        assert_eq!(after.last_activity, before.last_activity);
        assert_eq!(after.recent_events.len(), before.recent_events.len());
        assert!(after.pull_request.is_some());
        event.session_id = "no-such-card".into();
        event.pane_id = Some("no-such-pane".into());
        state.apply_event(event);
        assert_eq!(state.sessions.len(), 1);
    }

    /// The daemon's setter reports exactly the sessions whose value changed.
    #[test]
    fn set_pull_requests_reports_only_changes() {
        let mut state = crate::state::AppState::default();
        state.sessions.insert("s1".into(), sample_session());
        let mut want = HashMap::new();
        want.insert("s1".to_string(), pr_in(PullRequestState::Open).flatten());
        want.insert("gone".to_string(), None);
        let events = state.set_pull_requests(&want);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].session_id, "s1");
        assert!(
            state.set_pull_requests(&want).is_empty(),
            "no change, no report"
        );
    }

    /// `branch_key` against real repositories: a feature branch with a GitHub
    /// origin has a key; the default branch, a detached HEAD, a non-GitHub
    /// origin and a directory outside git have none.
    #[tokio::test]
    async fn branch_key_reads_the_branch_the_default_and_the_origin() {
        let sandbox = tempfile::tempdir().unwrap();
        let repo = sandbox.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = crate::git_env::fixture_git(&repo, sandbox.path())
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "fixture"]);
        git(&["remote", "add", "origin", "git@github.com:o/r.git"]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        assert_eq!(branch_key(&repo).await, None, "the default branch");
        git(&["checkout", "-q", "-b", "feat/x"]);
        assert_eq!(
            branch_key(&repo).await,
            Some(PrKey {
                slug: "o/r".into(),
                branch: "feat/x".into()
            })
        );
        git(&["remote", "set-url", "origin", "https://gitlab.com/o/r.git"]);
        assert_eq!(branch_key(&repo).await, None, "a non-GitHub origin");
        git(&["remote", "set-url", "origin", "https://github.com/o/r.git"]);
        git(&["checkout", "-q", "--detach"]);
        assert_eq!(branch_key(&repo).await, None, "a detached HEAD");
        let outside = sandbox.path().join("plain");
        std::fs::create_dir(&outside).unwrap();
        assert_eq!(branch_key(&outside).await, None, "outside git");
    }

    #[test]
    fn a_branch_is_re_read_every_interval_and_a_turn_end_only_after_the_debounce() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        assert!(resolve_due(None, false, at, interval), "never read");
        assert!(!resolve_due(
            Some(at),
            false,
            at + Duration::from_secs(59),
            interval
        ));
        assert!(resolve_due(Some(at), false, at + interval, interval));
        // A burst of Idle reports right after a probe does not re-probe.
        assert!(!resolve_due(Some(at), true, at, interval));
        assert!(!resolve_due(
            Some(at),
            true,
            at + Duration::from_secs(9),
            interval
        ));
        assert!(resolve_due(
            Some(at),
            true,
            at + MAX_KICK_DEBOUNCE,
            interval
        ));
    }

    /// Wait (bounded) until `done` holds, yielding to the runtime between
    /// checks.
    async fn eventually(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Two pools sharing one semaphore run at most its permit count of jobs
    /// at once, however many are queued, and every job still finishes.
    #[tokio::test]
    async fn the_job_pools_share_one_concurrency_bound() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let permits = Arc::new(tokio::sync::Semaphore::new(2));
        let mut probes: JobPool<u32, u32> = JobPool::new(permits.clone());
        let mut lookups: JobPool<u32, u32> = JobPool::new(permits);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let job = |id: u32| {
            let (gate, active, peak) = (gate.clone(), active.clone(), peak.clone());
            async move {
                let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let _ = gate.acquire().await.map(|p| p.forget());
                active.fetch_sub(1, Ordering::SeqCst);
                id
            }
        };
        for id in 0..5 {
            probes.spawn(id, job(id));
        }
        for id in 0..3 {
            lookups.spawn(id, job(id));
        }
        eventually("two jobs running", || active.load(Ordering::SeqCst) == 2).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(active.load(Ordering::SeqCst), 2, "only two may run");
        assert_eq!((probes.len(), lookups.len()), (5, 3));
        gate.add_permits(100);
        let mut finished = 0;
        while probes.next().await.is_some() {
            finished += 1;
        }
        while lookups.next().await.is_some() {
            finished += 1;
        }
        assert_eq!(finished, 8);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!((probes.len(), lookups.len()), (0, 0));
    }

    /// A job whose id is retained away, or whose pool is dropped (the monitor
    /// stopping), has its future dropped; a superseded job's result is never
    /// reported.
    #[tokio::test]
    async fn cancelled_superseded_and_dropped_jobs_go_away() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut pool: JobPool<u32, ()> =
            JobPool::new(Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_JOBS)));
        for id in 0..6 {
            let guard = Guard(dropped.clone());
            pool.spawn(id, async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
        }
        pool.retain(|id| id % 2 == 0);
        assert_eq!(pool.len(), 3);
        assert!(!pool.is_running(&1) && pool.is_running(&2));
        eventually("the three cancelled jobs dropped", || {
            dropped.load(Ordering::SeqCst) == 3
        })
        .await;
        drop(pool);
        eventually("every job dropped with the pool", || {
            dropped.load(Ordering::SeqCst) == 6
        })
        .await;

        let mut pool: JobPool<u32, &str> =
            JobPool::new(Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_JOBS)));
        pool.spawn(1, async { "old" });
        tokio::time::sleep(Duration::from_millis(20)).await;
        pool.spawn(1, async { "new" });
        assert_eq!(pool.next().await, Some((1, "new")));
        assert_eq!(pool.next().await, None);
    }

    /// Dropping the pool kills the child a running job started — the
    /// `kill_on_drop` a stuck `git` relies on.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn dropping_the_pool_kills_a_running_jobs_child() {
        let mut pool: JobPool<u32, ()> =
            JobPool::new(Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_JOBS)));
        let (pid_tx, pid_rx) = tokio::sync::oneshot::channel();
        pool.spawn(1, async move {
            let mut child = tokio::process::Command::new("sleep")
                .arg("1000")
                .kill_on_drop(true)
                .spawn()
                .expect("spawn sleep");
            let _ = pid_tx.send(child.id().expect("pid"));
            let _ = child.wait().await;
        });
        let pid = pid_rx.await.expect("the job started its child");
        drop(pool);
        let stat = format!("/proc/{pid}/stat");
        eventually("the child killed", || {
            // Gone, or a zombie waiting for tokio's reaper: either way dead.
            match std::fs::read_to_string(&stat) {
                Err(_) => true,
                Ok(s) => s
                    .rsplit(')')
                    .next()
                    .is_some_and(|rest| rest.trim_start().starts_with('Z')),
            }
        })
        .await;
    }

    #[test]
    fn gh_stderr_is_escaped_and_capped_before_it_is_logged() {
        let hostile = "HTTP 401\n2026-10-10T00:00:00Z WARN forged line\x1b[2J\u{202e}gnp.exe";
        let msg = gh_failure(&"exit status: 1", hostile.as_bytes());
        assert!(!msg.chars().any(|c| c.is_control()), "{msg}");
        assert!(
            !msg.chars().any(crate::untrusted_text::is_bidi_format_char),
            "{msg}"
        );
        assert!(msg.contains("HTTP 401\\n2026"), "{msg}");
        let long = "y".repeat(10_000);
        let msg = gh_failure(&"exit status: 1", long.as_bytes());
        assert_eq!(msg.matches('y').count(), MAX_LOGGED_STDERR_CHARS);
    }

    fn sample_session() -> SessionState {
        let now = chrono::Utc::now();
        SessionState {
            session_id: "s1".into(),
            agent_type: crate::event::AgentType::ClaudeCode,
            cwd: Some("/work/repo".into()),
            status: crate::state::SessionStatus::Thinking,
            blocked: None,
            active_tool: None,
            started_at: now,
            last_activity: now,
            recent_events: std::collections::VecDeque::new(),
            tool_count: 3,
            last_user_prompt: None,
            first_prompts: Vec::new(),
            pane_id: Some("pane-1".into()),
            agent_id: Some("agent-1".into()),
            display_name: None,
            shell_synthetic_working: false,
            native_hooks_reported: false,
            orchestration_orphaned: false,
            subagent_wait: None,
            output_set_status: false,
            pending_question: None,
            pull_request: None,
            pull_request_revision: 0,
        }
    }

    #[test]
    fn known_values_round_trip_in_snake_case() {
        let info = PullRequestInfo {
            number: 1401,
            url: "https://github.com/o/r/pull/1401".to_string(),
            state: PullRequestState::Draft,
            review: Some(PullRequestReview::ChangesRequested),
            checks: None,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["state"], "draft");
        assert_eq!(value["review"], "changes_requested");
        let back: PullRequestInfo = serde_json::from_value(value).unwrap();
        assert_eq!(back, info);
    }

    #[test]
    fn absent_review_is_omitted_and_tolerated() {
        let info = PullRequestInfo {
            number: 7,
            url: "https://github.com/o/r/pull/7".to_string(),
            state: PullRequestState::Open,
            review: None,
            checks: None,
        };
        let value = serde_json::to_value(&info).unwrap();
        assert!(value.get("review").is_none(), "None review must be omitted");
        let back: PullRequestInfo = serde_json::from_value(value).unwrap();
        assert_eq!(back, info);
    }

    /// A newer daemon may send a state, review or check result this build has
    /// never heard of; the record must still deserialize, with the value read
    /// as Unknown.
    #[test]
    fn unknown_state_and_review_values_deserialize_as_unknown() {
        let info: PullRequestInfo = serde_json::from_value(serde_json::json!({
            "number": 9,
            "url": "https://github.com/o/r/pull/9",
            "state": "queued_for_merge",
            "review": "dismissed_by_bot",
            "checks": { "state": "awaiting_approval", "pending": 2 },
        }))
        .unwrap();
        assert_eq!(info.state, PullRequestState::Unknown);
        assert_eq!(info.review, Some(PullRequestReview::Unknown));
        let checks = info.checks.unwrap();
        assert_eq!(checks.state, PullRequestChecksState::Unknown);
        assert_eq!((checks.pending, checks.passing), (2, 0));
    }

    /// Issue #1781: the checks summary is snake_case on the wire, and absent
    /// when there are none, so an older client never sees the key.
    #[test]
    fn checks_round_trip_and_are_omitted_when_absent() {
        let mut info = PullRequestInfo {
            number: 7,
            url: "https://github.com/o/r/pull/7".to_string(),
            state: PullRequestState::Open,
            review: None,
            checks: None,
        };
        assert!(serde_json::to_value(&info).unwrap().get("checks").is_none());
        info.checks = Some(PullRequestChecks {
            state: PullRequestChecksState::Failing,
            passing: 24,
            failing: 1,
            pending: 0,
            skipped: 2,
        });
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(
            value["checks"],
            serde_json::json!({
                "state": "failing", "passing": 24, "failing": 1, "pending": 0, "skipped": 2,
            })
        );
        let back: PullRequestInfo = serde_json::from_value(value).unwrap();
        assert_eq!(back, info);
    }

    /// An older daemon's snapshot has no `pull_request` key; a newer client
    /// must decode it as no PR, with every other field intact.
    #[test]
    fn snapshot_without_pull_request_decodes_as_none() {
        let snap: crate::state::SessionSnapshot = serde_json::from_value(serde_json::json!({
            "status": "Working",
            "tool_count": 3,
        }))
        .unwrap();
        assert!(snap.pull_request.is_none());
        assert_eq!(snap.tool_count, 3);
        let value = serde_json::to_value(&snap).unwrap();
        assert!(
            value.get("pull_request").is_none(),
            "an absent PR must have no key on the wire"
        );
    }

    #[test]
    fn snapshot_with_pull_request_round_trips() {
        let mut snap: crate::state::SessionSnapshot = serde_json::from_value(serde_json::json!({
            "status": "Idle",
            "tool_count": 0,
        }))
        .unwrap();
        let info = PullRequestInfo {
            number: 1401,
            url: "https://github.com/o/r/pull/1401".to_string(),
            state: PullRequestState::Merged,
            review: Some(PullRequestReview::Approved),
            checks: None,
        };
        snap.pull_request = Some(info.clone());
        let json = serde_json::to_string(&snap).unwrap();
        let back: crate::state::SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pull_request, Some(info));
    }

    /// A newer daemon's unknown state must not fail the snapshot it rides on.
    #[test]
    fn snapshot_with_unknown_pull_request_state_still_decodes() {
        let snap: crate::state::SessionSnapshot = serde_json::from_value(serde_json::json!({
            "status": "Idle",
            "tool_count": 0,
            "pull_request": {
                "number": 5,
                "url": "https://github.com/o/r/pull/5",
                "state": "some_future_state",
            },
        }))
        .unwrap();
        let pr = snap.pull_request.unwrap();
        assert_eq!(pr.state, PullRequestState::Unknown);
        assert_eq!(pr.review, None);
    }
}
