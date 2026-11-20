//! Issue #2418 (D4, D5): the daemon's own check of the pull requests and
//! issues agents report on their cards.
//!
//! Two halves, joined by [`ReferenceChecks`], which the card store owns:
//!
//! * the card projection, which does no I/O, reads the cached answers
//!   ([`ReferenceChecks::view`]) and says which references and which agent
//!   folders it needed ([`ReferenceChecks::set_wanted`]);
//! * [`run_reference_monitor`], a background task like the branch badge's
//!   ([`crate::pull_request::run_pull_request_monitor`]), which resolves each
//!   wanted folder's GitHub `origin` and looks each wanted reference up with
//!   `gh` on the daemon's host. It shares the badge's job bound, timeouts and
//!   refresh policy ([`crate::pull_request::fetch_due`]): an open pull request
//!   or issue is read again every interval, a merged or closed one is not
//!   (unless its CI is still settling), and a failure backs off.
//!
//! The lookups it runs, with `<n>` the number and `<owner/name>` the
//! repository:
//!
//! * a pull request: `gh pr view <n> --repo <owner/name> --json`
//!   [`PR_VIEW_FIELDS`];
//! * an issue: `gh issue view <n> --repo <owner/name> --json`
//!   [`ISSUE_VIEW_FIELDS`]. GitHub numbers issues and pull requests in one
//!   sequence, and `gh issue view` answers a pull request's number too (with
//!   a `MERGED` state and a `/pull/` URL), which is read as that pull request.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::agent_pty::AgentPtyRegistry;
use crate::github_refs::{GithubReference, ReferenceKind, ResolvedReference};
use crate::pull_request::{
    GhFailure, JobPool, LookupAnswer, LookupEntry, fetch_due, origin_repo, resolve_due, run_gh,
};
use crate::pull_request_info::{
    PullRequestChecks, PullRequestChecksState, PullRequestReview, PullRequestState,
};
use crate::untrusted_text::escape_control_and_bidi;

/// The `--json` fields of a pull request lookup. `headRefName` and `url` are
/// ones the branch badge's parser requires.
pub const PR_VIEW_FIELDS: &str =
    "number,state,isDraft,reviewDecision,url,headRefName,statusCheckRollup,title";

/// The `--json` fields of an issue lookup.
pub const ISSUE_VIEW_FIELDS: &str = "number,state,title,url";

/// The arguments of `gh issue view` for issue `number` of `repo_slug`.
pub fn gh_issue_view_args<'a>(number: &'a str, repo_slug: &'a str) -> [&'a str; 7] {
    [
        "issue",
        "view",
        number,
        "--repo",
        repo_slug,
        "--json",
        ISSUE_VIEW_FIELDS,
    ]
}

/// What a lookup found about one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefAnswer {
    pub state: PullRequestState,
    pub ci: Option<PullRequestChecks>,
    pub review: Option<PullRequestReview>,
    pub title: Option<String>,
    /// The page GitHub names for it — for an issue number that is a pull
    /// request, the pull request's.
    pub url: String,
    /// When the lookup finished, by the wall clock.
    pub checked_at: DateTime<Utc>,
}

impl LookupAnswer for RefAnswer {
    fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            PullRequestState::Merged | PullRequestState::Closed
        )
    }

    fn still_settling(&self) -> bool {
        self.ci
            .is_some_and(|ci| ci.state == PullRequestChecksState::Pending)
    }

    fn same_subject(&self, _other: &Self) -> bool {
        // The key is the reference itself: one repository, kind and number.
        true
    }
}

#[derive(serde::Deserialize)]
struct Titled {
    #[serde(default)]
    title: Option<String>,
}

/// The longest title a card carries, in characters.
const MAX_TITLE_CHARS: usize = 300;

fn title_of(json: &str) -> Option<String> {
    let title = serde_json::from_str::<Titled>(json).ok()?.title?;
    let title = escape_control_and_bidi(title.trim());
    (!title.is_empty()).then(|| title.chars().take(MAX_TITLE_CHARS).collect())
}

/// Read `gh pr view --json` [`PR_VIEW_FIELDS`] output for pull request
/// `number`, answered at `now`.
pub fn parse_pr_reference(
    json: &str,
    number: u64,
    now: DateTime<Utc>,
) -> Result<RefAnswer, String> {
    let pr = crate::pull_request::parse_pr_view(json)?
        .ok_or_else(|| "gh answered a pull request this build cannot read".to_string())?;
    if pr.number != number {
        return Err(format!(
            "gh answered pull request #{} for #{number}",
            pr.number
        ));
    }
    Ok(RefAnswer {
        state: pr.state,
        ci: pr.checks,
        review: pr.review,
        title: title_of(json),
        url: pr.url,
        checked_at: now,
    })
}

#[derive(serde::Deserialize)]
struct GhIssue {
    number: u64,
    state: String,
    url: String,
}

/// Read `gh issue view --json` [`ISSUE_VIEW_FIELDS`] output for issue
/// `number`, answered at `now`. A state this build does not know, or a URL
/// that is not a github.com issue or pull request page, is an error.
pub fn parse_issue_reference(
    json: &str,
    number: u64,
    now: DateTime<Utc>,
) -> Result<RefAnswer, String> {
    let issue: GhIssue =
        serde_json::from_str(json).map_err(|e| format!("could not parse gh output: {e}"))?;
    if issue.number != number {
        return Err(format!("gh answered issue #{} for #{number}", issue.number));
    }
    let state = match issue.state.as_str() {
        "OPEN" => PullRequestState::Open,
        "CLOSED" => PullRequestState::Closed,
        "MERGED" => PullRequestState::Merged,
        _ => return Err("gh answered an issue state this build does not know".into()),
    };
    if GithubReference::parse(&issue.url, ReferenceKind::Issue)
        .is_none_or(|r| r.number != number || r.repo.is_none())
    {
        return Err("gh answered an issue URL that is not a github.com page".into());
    }
    Ok(RefAnswer {
        state,
        ci: None,
        review: None,
        title: title_of(json),
        url: issue.url,
        checked_at: now,
    })
}

/// Look `reference` up with `gh`, on the daemon's host. Runs in no
/// directory: the reference names its repository.
pub async fn look_up(reference: &ResolvedReference) -> Result<RefAnswer, GhFailure> {
    let number = reference.number.to_string();
    let (what, args) = match reference.kind {
        ReferenceKind::PullRequest => (
            "gh pr view",
            crate::pull_request::gh_pr_view_args(&number, &reference.repo, PR_VIEW_FIELDS),
        ),
        ReferenceKind::Issue => (
            "gh issue view",
            gh_issue_view_args(&number, &reference.repo),
        ),
    };
    let stdout = run_gh(what, None, &args).await?;
    let json = String::from_utf8_lossy(&stdout);
    let parsed = match reference.kind {
        ReferenceKind::PullRequest => parse_pr_reference(&json, reference.number, Utc::now()),
        ReferenceKind::Issue => parse_issue_reference(&json, reference.number, Utc::now()),
    };
    parsed.map_err(GhFailure::error)
}

/// What the projection reads: each probed folder's GitHub `origin`
/// (`None`: it has none), and each looked-up reference's lookup state.
#[derive(Debug, Clone, Default)]
pub struct ReferenceView {
    pub origins: HashMap<String, Option<String>>,
    pub answers: HashMap<ResolvedReference, LookupEntry<RefAnswer>>,
}

/// What the projection needed: the folders of agents that reported a `#N`
/// reference, and the references it could resolve.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WantedRefs {
    pub cwds: BTreeSet<String>,
    pub refs: HashSet<ResolvedReference>,
}

#[derive(Default)]
struct Shared {
    view: ReferenceView,
    wanted: WantedRefs,
}

/// The cache both halves share. In memory only.
#[derive(Default)]
pub struct ReferenceChecks {
    shared: Mutex<Shared>,
    /// Rung when the wanted set changes, so a new reference is looked up at
    /// once rather than at the monitor's next tick.
    wake: tokio::sync::Notify,
}

impl std::fmt::Debug for ReferenceChecks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReferenceChecks").finish_non_exhaustive()
    }
}

impl ReferenceChecks {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of the cached answers, for one projection.
    pub fn view(&self) -> ReferenceView {
        self.lock().view.clone()
    }

    /// Record what a projection needed, waking the monitor when it changed.
    pub fn set_wanted(&self, wanted: WantedRefs) {
        let changed = {
            let mut shared = self.lock();
            let changed = shared.wanted != wanted;
            if changed {
                shared.wanted = wanted;
            }
            changed
        };
        if changed {
            self.wake.notify_one();
        }
    }

    fn wanted(&self) -> WantedRefs {
        self.lock().wanted.clone()
    }
}

/// The projection's half: what reference field `field` (`claim`, of kind
/// `field_kind`) of an agent working in `cwd` checks out as, given `view`.
/// Records in `wanted` what the monitor must resolve or look up for it.
pub fn check_reference(
    field: &str,
    claim: &str,
    field_kind: ReferenceKind,
    cwd: Option<&str>,
    view: &ReferenceView,
    wanted: &mut WantedRefs,
) -> crate::agent_cards::CheckedReference {
    use crate::agent_cards::{CheckResult, CheckedReference};
    let unavailable = |reason: &str, detail: &str| CheckResult::Unavailable {
        reason: reason.to_string(),
        detail: Some(detail.to_string()),
    };
    let mut checked = CheckedReference {
        field: field.to_string(),
        kind: field_kind.as_str().to_string(),
        claim: claim.to_string(),
        repo: None,
        number: None,
        url: None,
        result: CheckResult::Pending,
    };
    let Some(reference) = GithubReference::parse(claim, field_kind) else {
        checked.result = unavailable(
            REASON_UNRECOGNISED,
            "not a GitHub pull request or issue reference",
        );
        return checked;
    };
    checked.kind = reference.kind.as_str().to_string();
    checked.number = Some(reference.number);
    let origin = match (&reference.repo, cwd) {
        (Some(_), _) => None,
        (None, None) => {
            checked.result = unavailable(REASON_UNRECOGNISED, NO_ORIGIN);
            return checked;
        }
        (None, Some(cwd)) => {
            wanted.cwds.insert(cwd.to_string());
            match view.origins.get(cwd) {
                // Not probed yet.
                None => return checked,
                Some(origin) => origin.as_deref(),
            }
        }
    };
    let Some(resolved) = reference.resolve(origin) else {
        checked.result = unavailable(REASON_UNRECOGNISED, NO_ORIGIN);
        return checked;
    };
    checked.repo = Some(resolved.repo.clone());
    checked.url = Some(resolved.url());
    wanted.refs.insert(resolved.clone());
    let Some(entry) = view.answers.get(&resolved) else {
        return checked;
    };
    checked.result = match (&entry.value, &entry.last_failure) {
        (Some(answer), _) => {
            checked.url = Some(answer.url.clone());
            CheckResult::Checked {
                state: answer.state,
                ci: answer.ci,
                review: answer.review,
                title: answer.title.clone(),
                checked_at: answer.checked_at,
                stale: entry.is_stale(),
            }
        }
        (None, Some(failure)) => CheckResult::Unavailable {
            reason: failure.reason.as_str().to_string(),
            detail: Some(failure.detail.clone()),
        },
        (None, None) => CheckResult::Pending,
    };
    checked
}

/// [`crate::agent_cards::CheckResult::Unavailable`]'s reason for a value that
/// is not a GitHub reference, or a `#N` with no GitHub repository to read it
/// in.
pub const REASON_UNRECOGNISED: &str = "unrecognised";

const NO_ORIGIN: &str = "no GitHub origin for this agent's folder";

/// The daemon's background task that resolves the folders and looks up the
/// references the card projection wants ([`ReferenceChecks`]). Runs until
/// aborted; its jobs, and the children they run, go with it. Every `git` and
/// `gh` it runs holds a permit of `permits`, the bound the branch badge's
/// monitor shares.
pub async fn run_reference_monitor(
    registry: Arc<AgentPtyRegistry>,
    interval: Duration,
    permits: Arc<tokio::sync::Semaphore>,
) {
    let checks = registry.agent_cards().references();
    let mut probes: JobPool<String, Option<String>> = JobPool::new(permits.clone());
    let mut lookups: JobPool<ResolvedReference, Result<RefAnswer, GhFailure>> =
        JobPool::new(permits);
    let mut probed_at: HashMap<String, Instant> = HashMap::new();
    let mut ticker = tokio::time::interval(interval.min(Duration::from_secs(5)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = checks.wake.notified() => {}
            Some((cwd, origin)) = probes.next() => {
                probed_at.insert(cwd.clone(), Instant::now());
                checks.lock().view.origins.insert(cwd, origin);
            }
            Some((reference, result)) = lookups.next() => {
                let mut shared = checks.lock();
                let entry = shared.view.answers.entry(reference.clone()).or_default();
                entry.in_flight = false;
                entry.fetched_at = Some(Instant::now());
                match result {
                    Ok(answer) => entry.record(answer, Instant::now()),
                    Err(failure) => {
                        let detail = failure.detail.clone();
                        if entry.record_failure(failure) {
                            warn!(
                                repo = %escape_control_and_bidi(&reference.repo),
                                kind = reference.kind.as_str(),
                                number = reference.number,
                                error = %escape_control_and_bidi(&detail),
                                "checking a reference an agent reported failed; its card \
                                 says so until a later lookup succeeds"
                            );
                        }
                    }
                }
            }
        }
        let now = Instant::now();
        let wanted = checks.wanted();

        // Folders: only those a card still needs, each re-read once an
        // interval.
        probed_at.retain(|cwd, _| wanted.cwds.contains(cwd));
        probes.retain(|cwd| wanted.cwds.contains(cwd));
        checks
            .lock()
            .view
            .origins
            .retain(|cwd, _| wanted.cwds.contains(cwd));
        for cwd in &wanted.cwds {
            if probes.is_running(cwd)
                || !resolve_due(probed_at.get(cwd).copied(), false, now, interval)
            {
                continue;
            }
            let dir = cwd.clone();
            probes.spawn(
                cwd.clone(),
                async move { origin_repo(Path::new(&dir)).await },
            );
        }

        // References: only those a card still names.
        lookups.retain(|reference| wanted.refs.contains(reference));
        let due: Vec<ResolvedReference> = {
            let mut shared = checks.lock();
            shared
                .view
                .answers
                .retain(|reference, _| wanted.refs.contains(reference));
            let mut due = Vec::new();
            for reference in &wanted.refs {
                let entry = shared.view.answers.entry(reference.clone()).or_default();
                entry.in_flight = lookups.is_running(reference);
                if fetch_due(entry, now, interval) {
                    entry.in_flight = true;
                    entry.kicked = false;
                    due.push(reference.clone());
                }
            }
            due
        };
        for reference in due {
            debug!(
                repo = %escape_control_and_bidi(&reference.repo),
                kind = reference.kind.as_str(),
                number = reference.number,
                "checking a reference an agent reported"
            );
            let job = reference.clone();
            lookups.spawn(reference, async move { look_up(&job).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_cards::CheckResult;
    use crate::pull_request::GhFailureReason;

    fn at() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-20T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn resolved(repo: &str, kind: ReferenceKind, number: u64) -> ResolvedReference {
        ResolvedReference {
            repo: repo.into(),
            kind,
            number,
        }
    }

    #[test]
    fn a_pull_request_lookup_reads_state_ci_review_and_title() {
        let json = r#"{"number": 7, "state": "MERGED", "isDraft": false,
            "reviewDecision": "APPROVED", "url": "https://github.com/o/r/pull/7",
            "headRefName": "b", "title": "Fix it\u001b[31m",
            "statusCheckRollup": [{"__typename": "CheckRun", "name": "ci",
                "status": "COMPLETED", "conclusion": "FAILURE"}]}"#;
        let answer = parse_pr_reference(json, 7, at()).unwrap();
        assert_eq!(answer.state, PullRequestState::Merged);
        assert_eq!(answer.review, Some(PullRequestReview::Approved));
        assert_eq!(answer.ci.unwrap().state, PullRequestChecksState::Failing);
        assert_eq!(answer.url, "https://github.com/o/r/pull/7");
        assert_eq!(answer.checked_at, at());
        // Control characters in a title are escaped, never passed through.
        assert!(!answer.title.unwrap().contains('\u{1b}'));
        assert!(parse_pr_reference(json, 8, at()).is_err());
    }

    #[test]
    fn an_issue_lookup_reads_state_and_tolerates_a_pull_request_number() {
        let issue = r#"{"number": 3, "state": "CLOSED", "title": "Bug",
            "url": "https://github.com/o/r/issues/3"}"#;
        let answer = parse_issue_reference(issue, 3, at()).unwrap();
        assert_eq!(answer.state, PullRequestState::Closed);
        assert_eq!(answer.title.as_deref(), Some("Bug"));
        // What `gh issue view` answers for a pull request's number.
        let pr = r#"{"number": 4, "state": "MERGED", "title": "PR",
            "url": "https://github.com/o/r/pull/4"}"#;
        let answer = parse_issue_reference(pr, 4, at()).unwrap();
        assert_eq!(answer.state, PullRequestState::Merged);
        assert_eq!(answer.url, "https://github.com/o/r/pull/4");
        for bad in [
            r#"{"number": 3, "state": "LOCKED", "url": "https://github.com/o/r/issues/3"}"#,
            r#"{"number": 3, "state": "OPEN", "url": "https://evil.example/o/r/issues/3"}"#,
            r#"{"number": 3, "state": "OPEN", "url": "https://github.com/o/r/issues/4"}"#,
            r#"{"number": 9, "state": "OPEN", "url": "https://github.com/o/r/issues/9"}"#,
        ] {
            assert!(parse_issue_reference(bad, 3, at()).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_lookup_arguments_name_the_number_and_repository() {
        assert_eq!(
            gh_issue_view_args("12", "o/r"),
            [
                "issue",
                "view",
                "12",
                "--repo",
                "o/r",
                "--json",
                "number,state,title,url"
            ]
        );
    }

    /// D4/D5: what a reference checks out as, from the claim to the cache.
    #[test]
    fn a_reference_is_pending_checked_stale_or_unavailable() {
        let mut view = ReferenceView::default();
        let mut wanted = WantedRefs::default();
        let check =
            |claim: &str, cwd: Option<&str>, view: &ReferenceView, wanted: &mut WantedRefs| {
                check_reference(
                    "pull-request",
                    claim,
                    ReferenceKind::PullRequest,
                    cwd,
                    view,
                    wanted,
                )
            };

        // Not a GitHub reference: unrecognised, nothing wanted.
        let c = check(
            "https://gitlab.com/o/r/-/merge_requests/1",
            Some("/w"),
            &view,
            &mut wanted,
        );
        assert!(
            matches!(&c.result, CheckResult::Unavailable { reason, .. } if reason == "unrecognised")
        );
        assert_eq!(wanted, WantedRefs::default());

        // `#N` before its folder's origin is known: pending, and the folder
        // is wanted.
        let c = check("#5", Some("/w"), &view, &mut wanted);
        assert_eq!(c.result, CheckResult::Pending);
        assert_eq!(c.number, Some(5));
        assert!(c.repo.is_none());
        assert!(wanted.cwds.contains("/w"));
        // A folder with no GitHub origin, or no folder at all.
        view.origins.insert("/w".into(), None);
        let c = check("#5", Some("/w"), &view, &mut wanted);
        assert!(
            matches!(&c.result, CheckResult::Unavailable { reason, detail }
            if reason == "unrecognised" && detail.as_deref() == Some(NO_ORIGIN))
        );
        let c = check("#5", None, &view, &mut wanted);
        assert!(
            matches!(&c.result, CheckResult::Unavailable { reason, .. } if reason == "unrecognised")
        );

        // Resolved, not looked up yet: pending, with its repository and page.
        view.origins.insert("/w".into(), Some("o/r".into()));
        let mut wanted = WantedRefs::default();
        let c = check("#5", Some("/w"), &view, &mut wanted);
        assert_eq!(c.result, CheckResult::Pending);
        assert_eq!(c.repo.as_deref(), Some("o/r"));
        assert_eq!(c.url.as_deref(), Some("https://github.com/o/r/pull/5"));
        let key = resolved("o/r", ReferenceKind::PullRequest, 5);
        assert!(wanted.refs.contains(&key));

        // A first lookup that failed: unavailable, with gh's reason.
        let mut entry = LookupEntry::<RefAnswer>::default();
        entry.record_failure(GhFailure {
            reason: GhFailureReason::NotFound,
            detail: "gh pr view failed".into(),
        });
        view.answers.insert(key.clone(), entry.clone());
        let c = check("#5", Some("/w"), &view, &mut wanted);
        assert!(
            matches!(&c.result, CheckResult::Unavailable { reason, .. } if reason == "not_found")
        );

        // A success: checked, fresh.
        let answer = RefAnswer {
            state: PullRequestState::Open,
            ci: None,
            review: None,
            title: Some("T".into()),
            url: "https://github.com/o/r/pull/5".into(),
            checked_at: at(),
        };
        entry.record(answer, Instant::now());
        view.answers.insert(key.clone(), entry.clone());
        let c = check("#5", Some("/w"), &view, &mut wanted);
        assert!(
            matches!(&c.result, CheckResult::Checked { stale: false, checked_at, .. } if *checked_at == at())
        );

        // A later failure keeps the answer, marked stale.
        entry.record_failure(GhFailure {
            reason: GhFailureReason::RateLimited,
            detail: "rate limit".into(),
        });
        view.answers.insert(key, entry);
        let c = check("#5", Some("/w"), &view, &mut wanted);
        assert!(matches!(
            &c.result,
            CheckResult::Checked { stale: true, .. }
        ));
    }

    /// D4: a URL names its own kind and repository; `owner/repo#N` names its
    /// repository and takes the field's kind, needing no folder.
    #[test]
    fn a_full_reference_needs_no_folder() {
        let view = ReferenceView::default();
        let mut wanted = WantedRefs::default();
        let c = check_reference(
            "issue",
            "https://github.com/o/r/pull/9",
            ReferenceKind::Issue,
            None,
            &view,
            &mut wanted,
        );
        assert_eq!(c.kind, "pull_request");
        assert_eq!(c.repo.as_deref(), Some("o/r"));
        let c = check_reference(
            "issue",
            "o/r#3",
            ReferenceKind::Issue,
            None,
            &view,
            &mut wanted,
        );
        assert_eq!(c.kind, "issue");
        assert_eq!(c.url.as_deref(), Some("https://github.com/o/r/issues/3"));
        assert!(wanted.cwds.is_empty());
        assert_eq!(wanted.refs.len(), 2);
    }

    #[test]
    fn the_wire_shape_of_a_result() {
        let json = serde_json::to_value(CheckResult::Pending).unwrap();
        assert_eq!(json, serde_json::json!({"status": "pending"}));
        let json = serde_json::to_value(CheckResult::Unavailable {
            reason: "not_found".into(),
            detail: None,
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"status": "unavailable", "reason": "not_found"})
        );
        let newer: CheckResult = serde_json::from_str(r#"{"status": "verified"}"#).unwrap();
        assert_eq!(newer, CheckResult::Unknown);
    }
}
