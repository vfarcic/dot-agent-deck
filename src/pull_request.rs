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
//! not read again until the branch changes; a turn ending or an agent stopping
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
//! [`PULL_REQUEST_METADATA_KEY`], both under one `AppState` write lock, so a
//! `ListAgents` reply can never be older than a report a client already
//! applied. Clients apply it in [`crate::state::AppState::apply_event`] (the TUI
//! and the desktop's fold both run that), and hydration copies the snapshot
//! field. An older client decodes the event type as `Unknown`, a no-op, which is
//! why no `PROTOCOL_VERSION` bump is needed.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::event::{AgentEvent, BroadcastMsg, EventType, PULL_REQUEST_METADATA_KEY};
use crate::git_env::git_at;
use crate::state::{SessionState, SharedState};

/// A pull request linked to an agent session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PullRequestInfo {
    /// The PR number on its forge (e.g. `1401`).
    pub number: u64,
    /// The PR's web URL.
    pub url: String,
    /// Where the PR is in its lifecycle.
    pub state: PullRequestState,
    /// The review decision, when the forge reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<PullRequestReview>,
}

/// A pull request's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
    /// A state this build does not know, sent by a newer daemon. Never
    /// produced by this build; it only exists so deserialization succeeds.
    #[serde(other)]
    Unknown,
}

/// A pull request's review decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestReview {
    Approved,
    ChangesRequested,
    ReviewRequired,
    /// A review decision this build does not know, sent by a newer daemon.
    /// Never produced by this build; it only exists so deserialization
    /// succeeds.
    #[serde(other)]
    Unknown,
}

/// The `--json` fields the badge needs from `gh pr list`.
pub(crate) const GH_PR_FIELDS: &str = "number,state,isDraft,reviewDecision,url,headRefName";

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

/// A branch with no PR is re-read this many intervals apart (a PR an agent
/// opens is normally picked up sooner, by the turn-end re-read).
const NO_PR_POLL_FACTOR: u32 = 5;

/// A failed lookup is retried this many intervals later, whatever happens in
/// between.
const FAILURE_BACKOFF_FACTOR: u32 = 5;

/// The longest gap a turn-end re-read waits for after the previous lookup.
const MAX_KICK_DEBOUNCE: Duration = Duration::from_secs(10);

/// How long one `gh` may run before it counts as a failure.
const GH_TIMEOUT: Duration = Duration::from_secs(30);

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

fn git_line(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = git_at(cwd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!line.is_empty()).then_some(line)
}

/// The key `cwd`'s PR is looked up by, or `None` when it gets no badge: not a
/// git repository, a detached `HEAD`, the remote's default branch, or an
/// `origin` that is missing or not on GitHub. Blocking (it runs `git`); the
/// monitor calls it on a blocking thread.
pub(crate) fn branch_key(cwd: &Path) -> Option<PrKey> {
    // `symbolic-ref` fails on a detached HEAD and outside a repository alike.
    let branch = git_line(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    let default = git_line(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    );
    let default = default
        .as_deref()
        .map(|d| d.strip_prefix("origin/").unwrap_or(d));
    if is_default_branch(&branch, default) {
        return None;
    }
    let slug = crate::worktree_reclaim::derive_repo_slug(cwd)?;
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
        Some(PullRequestInfo {
            number: self.number,
            url: self.url,
            state,
            review,
        })
    }
}

/// The PR `gh pr list --json` output reports for exactly `branch`, or `None`
/// when it reports none. Only an exact `headRefName` match counts. Of several,
/// an open (or draft) one wins, then the highest number. An entry with a state
/// this build does not know, or a URL off `https://github.com/`, is skipped.
pub fn parse_pr_list(json: &str, branch: &str) -> Result<Option<PullRequestInfo>, String> {
    let entries: Vec<GhPr> =
        serde_json::from_str(json).map_err(|e| format!("could not parse gh output: {e}"))?;
    Ok(entries
        .into_iter()
        .filter(|pr| pr.head_ref_name == branch)
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
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr: String = stderr.trim().chars().take(300).collect();
        return Err(format!("gh pr list failed ({}): {stderr}", out.status));
    }
    parse_pr_list(&String::from_utf8_lossy(&out.stdout), &key.branch)
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
}

/// Whether `entry` needs a lookup at `now`. The refresh policy, pure:
///
/// * never two at once, and a key never looked up is due at once;
/// * after a failure, only once [`FAILURE_BACKOFF_FACTOR`] intervals passed;
/// * a merged or closed PR, never (a new branch is a new key);
/// * an open or draft PR, every `interval`, or after a turn end once the
///   debounce passed;
/// * no PR, every [`NO_PR_POLL_FACTOR`] intervals, or after a turn end once
///   the debounce passed.
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
            ) =>
        {
            false
        }
        Some(Some(_)) => age >= interval || kicked,
        Some(None) => age >= interval * NO_PR_POLL_FACTOR || kicked,
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
    resolving: bool,
    kicked: bool,
}

enum Finished {
    Resolved(PathBuf, Option<PrKey>),
    Fetched(PrKey, Result<Option<PullRequestInfo>, String>),
}

/// PRD #1401: the daemon's background task that resolves every live
/// session's pull request and reports changes — see the module docs. Runs
/// until aborted (`run_daemon_with`'s cleanup) or the broadcast closes.
pub async fn run_pull_request_monitor(
    registry: Arc<AgentPtyRegistry>,
    state: SharedState,
    event_tx: broadcast::Sender<BroadcastMsg>,
    interval: Duration,
) {
    let mut events = event_tx.subscribe();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Finished>();
    let tick = interval.min(Duration::from_secs(5));
    let mut cwds: HashMap<PathBuf, CwdEntry> = HashMap::new();
    let mut keys: HashMap<PrKey, KeyEntry> = HashMap::new();
    let mut kicked_sessions: HashSet<String> = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(tick) => {}
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
            Some(done) = done_rx.recv() => match done {
                Finished::Resolved(cwd, key) => {
                    let entry = cwds.entry(cwd).or_default();
                    entry.key = key;
                    entry.resolved_at = Some(Instant::now());
                    entry.resolving = false;
                }
                Finished::Fetched(key, result) => {
                    let entry = keys.entry(key.clone()).or_default();
                    entry.in_flight = false;
                    entry.fetched_at = Some(Instant::now());
                    match result {
                        Ok(value) => {
                            entry.value = Some(value);
                            entry.failed = false;
                            entry.failure_logged = false;
                        }
                        Err(error) => {
                            entry.failed = true;
                            if !entry.failure_logged {
                                entry.failure_logged = true;
                                warn!(
                                    repo = %key.slug,
                                    branch = %key.branch,
                                    error = %error,
                                    "pull request lookup failed; the agent's card shows no \
                                     pull request until a later lookup succeeds"
                                );
                            }
                        }
                    }
                }
            },
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

        // Re-read each directory's branch: new, kicked, or once an interval.
        let live_cwds: HashSet<&PathBuf> =
            sessions.iter().filter_map(|(_, c)| c.as_ref()).collect();
        cwds.retain(|cwd, entry| entry.resolving || live_cwds.contains(cwd));
        for (cwd, entry) in cwds.iter_mut() {
            let due = entry
                .resolved_at
                .is_none_or(|at| entry.kicked || now.saturating_duration_since(at) >= interval);
            if entry.resolving || !due {
                continue;
            }
            entry.resolving = true;
            entry.kicked = false;
            let cwd = cwd.clone();
            let done_tx = done_tx.clone();
            tokio::spawn(async move {
                let probe = cwd.clone();
                let key = tokio::task::spawn_blocking(move || branch_key(&probe))
                    .await
                    .ok()
                    .flatten();
                let _ = done_tx.send(Finished::Resolved(cwd, key));
            });
        }

        // Look up each key in use that is due.
        let mut key_cwd: HashMap<PrKey, PathBuf> = HashMap::new();
        for (cwd, entry) in &cwds {
            if let Some(key) = &entry.key {
                key_cwd.entry(key.clone()).or_insert_with(|| cwd.clone());
            }
        }
        keys.retain(|key, entry| entry.in_flight || key_cwd.contains_key(key));
        for (key, cwd) in &key_cwd {
            let entry = keys.entry(key.clone()).or_default();
            if !fetch_due(entry, now, interval) {
                continue;
            }
            entry.in_flight = true;
            entry.kicked = false;
            debug!(repo = %key.slug, branch = %key.branch, "looking up the branch's pull request");
            let (key, cwd, done_tx) = (key.clone(), cwd.clone(), done_tx.clone());
            tokio::spawn(async move {
                let result = query_gh(cwd, key.clone()).await;
                let _ = done_tx.send(Finished::Fetched(key, result));
            });
        }

        // What each session should show now. A session whose directory or key
        // has no answer yet keeps what it has.
        let want: HashMap<String, Option<PullRequestInfo>> = sessions
            .iter()
            .filter_map(|(session_id, cwd)| {
                let entry = cwds.get(cwd.as_ref()?)?;
                entry.resolved_at?;
                let value = match &entry.key {
                    None => None,
                    Some(key) => keys.get(key)?.value.clone()?,
                };
                Some((session_id.clone(), value))
            })
            .collect();
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

    #[test]
    fn no_pr_is_polled_slowly() {
        let at = Instant::now();
        let interval = Duration::from_secs(60);
        let entry = fetched(Some(None), false, at);
        assert!(!fetch_due(&entry, at + interval, interval));
        assert!(fetch_due(
            &entry,
            at + interval * NO_PR_POLL_FACTOR,
            interval
        ));
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
                "number,state,isDraft,reviewDecision,url,headRefName",
            ]
        );
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
    #[test]
    fn branch_key_reads_the_branch_the_default_and_the_origin() {
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
        assert_eq!(branch_key(&repo), None, "the default branch");
        git(&["checkout", "-q", "-b", "feat/x"]);
        assert_eq!(
            branch_key(&repo),
            Some(PrKey {
                slug: "o/r".into(),
                branch: "feat/x".into()
            })
        );
        git(&["remote", "set-url", "origin", "https://gitlab.com/o/r.git"]);
        assert_eq!(branch_key(&repo), None, "a non-GitHub origin");
        git(&["remote", "set-url", "origin", "https://github.com/o/r.git"]);
        git(&["checkout", "-q", "--detach"]);
        assert_eq!(branch_key(&repo), None, "a detached HEAD");
        let outside = sandbox.path().join("plain");
        std::fs::create_dir(&outside).unwrap();
        assert_eq!(branch_key(&outside), None, "outside git");
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
            orchestration_orphaned: false,
            subagent_wait: None,
            prompt_reports_unavailable: false,
            prompt_reports_declared: false,
            output_set_status: false,
            pull_request: None,
        }
    }

    #[test]
    fn known_values_round_trip_in_snake_case() {
        let info = PullRequestInfo {
            number: 1401,
            url: "https://github.com/o/r/pull/1401".to_string(),
            state: PullRequestState::Draft,
            review: Some(PullRequestReview::ChangesRequested),
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
        };
        let value = serde_json::to_value(&info).unwrap();
        assert!(value.get("review").is_none(), "None review must be omitted");
        let back: PullRequestInfo = serde_json::from_value(value).unwrap();
        assert_eq!(back, info);
    }

    /// A newer daemon may send a state or review this build has never heard
    /// of; the record must still deserialize, with the value read as Unknown.
    #[test]
    fn unknown_state_and_review_values_deserialize_as_unknown() {
        let info: PullRequestInfo = serde_json::from_value(serde_json::json!({
            "number": 9,
            "url": "https://github.com/o/r/pull/9",
            "state": "queued_for_merge",
            "review": "dismissed_by_bot",
        }))
        .unwrap();
        assert_eq!(info.state, PullRequestState::Unknown);
        assert_eq!(info.review, Some(PullRequestReview::Unknown));
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
