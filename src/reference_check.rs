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
//!   The same lookup reads an issue's labels (round 2, D13), bounded by
//!   [`MAX_LABELS`] and [`MAX_LABEL_NAME_CHARS`]; no second call is made.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tracing::{debug, warn};

use crate::agent_cards::IssueLabel;
use crate::agent_pty::AgentPtyRegistry;
use crate::github_refs::{GithubReference, ReferenceKind, ResolvedReference};
use crate::pull_request::{
    GhFailure, GhFailureReason, JobPool, LookupAnswer, LookupEntry, failure_backoff, fetch_due,
    origin_repo, resolve_due, run_gh,
};
use crate::pull_request_info::{PullRequestChecks, PullRequestReview, PullRequestState};
use crate::untrusted_text::escape_control_and_bidi;

/// The `--json` fields of a pull request lookup. `headRefName` and `url` are
/// ones the branch badge's parser requires.
pub const PR_VIEW_FIELDS: &str =
    "number,state,isDraft,reviewDecision,url,headRefName,statusCheckRollup,title";

/// The `--json` fields of an issue lookup.
pub const ISSUE_VIEW_FIELDS: &str = "number,state,title,url,labels";

/// The most labels a checked issue keeps, in the order `gh` lists them.
pub const MAX_LABELS: usize = 20;

/// The longest label name a card carries, in characters. GitHub's own limit
/// is 50, so this cuts only what GitHub would not have let through.
pub const MAX_LABEL_NAME_CHARS: usize = 50;

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
    /// An issue's labels, bounded ([`labels_of`]); empty for a pull request.
    pub labels: Vec<IssueLabel>,
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
        crate::pull_request::checks_still_running(self.ci.as_ref())
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

#[derive(serde::Deserialize)]
struct Labelled {
    #[serde(default)]
    labels: serde_json::Value,
}

/// A label colour as GitHub gives it: exactly six hex digits, no `#`.
fn label_color(value: Option<&serde_json::Value>) -> Option<String> {
    let color = value?.as_str()?;
    (color.len() == 6 && color.bytes().all(|b| b.is_ascii_hexdigit())).then(|| color.to_string())
}

/// The labels in `gh issue view --json labels` output, as a card keeps them
/// (D13): repository text, so each name is trimmed, escaped and cut to
/// [`MAX_LABEL_NAME_CHARS`], a label with no usable name is skipped, at most
/// [`MAX_LABELS`] are kept, and a colour that is not six hex digits is
/// dropped while its label stays. A `labels` value that is not a list reads
/// as none rather than failing the lookup.
pub fn labels_of(json: &str) -> Vec<IssueLabel> {
    let Ok(Labelled { labels }) = serde_json::from_str::<Labelled>(json) else {
        return Vec::new();
    };
    let Some(labels) = labels.as_array() else {
        return Vec::new();
    };
    labels
        .iter()
        .filter_map(|label| {
            let name = label.get("name")?.as_str()?;
            let name = escape_control_and_bidi(name.trim());
            if name.is_empty() {
                return None;
            }
            Some(IssueLabel {
                name: name.chars().take(MAX_LABEL_NAME_CHARS).collect(),
                color: label_color(label.get("color")),
            })
        })
        .take(MAX_LABELS)
        .collect()
}

/// The page a lookup of `asked` answered with `url`, as its canonical
/// github.com URL — review A2: read as a URL only
/// ([`ResolvedReference::from_github_url`]), naming `asked`'s number in
/// `asked`'s repository (whatever its case), and a pull request's page when
/// `pull_request_only`; an issue lookup may answer a pull request's page,
/// since GitHub numbers both in one sequence. A repository renamed on GitHub
/// answers under its new name, which this refuses, so such a reference reads
/// as unavailable rather than as another repository's page.
fn answered_page(
    url: &str,
    asked: &ResolvedReference,
    pull_request_only: bool,
) -> Result<String, String> {
    let page = ResolvedReference::from_github_url(url).ok_or_else(|| {
        "gh answered a link that is not a github.com pull request or issue page".to_string()
    })?;
    if page.number != asked.number {
        return Err(format!(
            "gh answered the page of #{} for #{}",
            page.number, asked.number
        ));
    }
    if pull_request_only && page.kind != ReferenceKind::PullRequest {
        return Err("gh answered an issue page for a pull request".into());
    }
    if !page.repo.eq_ignore_ascii_case(&asked.repo) {
        return Err("gh answered a page of another repository".into());
    }
    Ok(page.url())
}

/// Read `gh pr view --json` [`PR_VIEW_FIELDS`] output for pull request
/// `asked`, answered at `now`.
pub fn parse_pr_reference(
    json: &str,
    asked: &ResolvedReference,
    now: DateTime<Utc>,
) -> Result<RefAnswer, String> {
    let pr = crate::pull_request::parse_pr_view(json)?
        .ok_or_else(|| "gh answered a pull request this build cannot read".to_string())?;
    if pr.number != asked.number {
        return Err(format!(
            "gh answered pull request #{} for #{}",
            pr.number, asked.number
        ));
    }
    let url = answered_page(&pr.url, asked, true)?;
    Ok(RefAnswer {
        state: pr.state,
        ci: pr.checks,
        review: pr.review,
        title: title_of(json),
        labels: Vec::new(),
        url,
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
/// `asked`, answered at `now`. A state this build does not know, or a URL
/// that is not `asked`'s github.com issue or pull request page, is an error.
pub fn parse_issue_reference(
    json: &str,
    asked: &ResolvedReference,
    now: DateTime<Utc>,
) -> Result<RefAnswer, String> {
    let issue: GhIssue =
        serde_json::from_str(json).map_err(|e| format!("could not parse gh output: {e}"))?;
    if issue.number != asked.number {
        return Err(format!(
            "gh answered issue #{} for #{}",
            issue.number, asked.number
        ));
    }
    let state = match issue.state.as_str() {
        "OPEN" => PullRequestState::Open,
        "CLOSED" => PullRequestState::Closed,
        "MERGED" => PullRequestState::Merged,
        _ => return Err("gh answered an issue state this build does not know".into()),
    };
    let url = answered_page(&issue.url, asked, false)?;
    // Issues only (D13): a number GitHub answers as a pull request keeps no
    // labels; a pull request's labels are a later step.
    let is_issue_page = ResolvedReference::from_github_url(&url)
        .is_some_and(|page| page.kind == ReferenceKind::Issue);
    Ok(RefAnswer {
        state,
        ci: None,
        review: None,
        title: title_of(json),
        labels: if is_issue_page {
            labels_of(json)
        } else {
            Vec::new()
        },
        url,
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
        ReferenceKind::PullRequest => parse_pr_reference(&json, reference, Utc::now()),
        ReferenceKind::Issue => parse_issue_reference(&json, reference, Utc::now()),
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
                labels: answer.labels.clone(),
            }
        }
        // Review A3: the card says why in fixed words; what `gh` printed
        // stays in the daemon's log ([`run_reference_monitor`]).
        (None, Some(failure)) => CheckResult::Unavailable {
            reason: failure.reason.as_str().to_string(),
            detail: Some(unavailable_detail(failure.reason).to_string()),
        },
        (None, None) => CheckResult::Pending,
    };
    checked
}

/// The detail a card carries for a lookup that failed for `reason`. Review
/// A3: a fixed sentence per reason, never `gh`'s own output, which can name
/// paths, hosts or tokens of the deck's host and reaches every client,
/// remote ones included. The daemon logs `gh`'s output instead, once per run
/// of failures.
pub fn unavailable_detail(reason: GhFailureReason) -> &'static str {
    match reason {
        GhFailureReason::NoGithubAccess => "gh is missing or not logged in on the deck's host",
        GhFailureReason::NotFound => "not found, or private to the deck's GitHub login",
        GhFailureReason::RateLimited => "GitHub rate limit reached on the deck's host",
        GhFailureReason::Error => "the deck's host could not check it; the deck's log says why",
    }
}

/// [`crate::agent_cards::CheckResult::Unavailable`]'s reason for a value that
/// is not a GitHub reference, or a `#N` with no GitHub repository to read it
/// in.
pub const REASON_UNRECOGNISED: &str = "unrecognised";

const NO_ORIGIN: &str = "no GitHub origin for this agent's folder";

// ---------------------------------------------------------------------------
// Admission (review A1/R4)
// ---------------------------------------------------------------------------
//
// The job bound limits how many lookups run at once, not how many run per
// hour, and a reference's refresh and failure backoff belong to its cache
// entry. What a report names is up to the agent, so the scheduler below
// makes both hold whatever the reports do:
//
// * a daemon-wide token bucket ([`LOOKUP_BURST`], one more every
//   [`LOOKUP_REFILL_EVERY`]) caps the lookups started per hour, however the
//   references change;
// * a `rate_limited` answer stops reference lookups for
//   [`RATE_LIMIT_COOLDOWN`], not only that reference's: none is admitted,
//   and one admitted but still waiting for its permits hands its turn back
//   without running `gh` and is admitted again after the cooldown
//   ([`gated`]). A lookup already running when the answer arrives finishes;
// * a reference no card names any more keeps its entry for a while
//   ([`retired_ttl`]) once a lookup of it has finished, so naming it again
//   finds its last answer, its retry deadline and its terminal state, rather
//   than a fresh entry that is due at once. At most [`MAX_RETIRED`] are
//   kept: past that, the entries holding neither an answer nor a failure
//   still inside its backoff go first, then the oldest;
// * at most [`MAX_QUEUED_LOOKUPS`] lookups are started and unfinished, and
//   the oldest-due reference goes first, so a stream of new references does
//   not hold back the refresh of ones already shown;
// * the lookups and `origin` probes hold at most [`REFERENCE_LANE_PERMITS`]
//   of the job bound's permits ([`JobPool::with_lane`]), so the branch
//   badge's monitor, which shares the bound, still gets the rest.

/// The lookups the bucket allows back to back.
const LOOKUP_BURST: u32 = 16;

/// One more lookup is allowed this often: at most 1200 an hour after the
/// burst, a quarter of the 5000 requests an hour GitHub gives one
/// authenticated user. At the default refresh interval that keeps about 20
/// open references fresh every minute; more are refreshed less often.
const LOOKUP_REFILL_EVERY: Duration = Duration::from_secs(3);

/// How long every reference lookup waits after one answered `rate_limited`.
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// The most reference lookups started and not yet finished.
const MAX_QUEUED_LOOKUPS: usize = 8;

/// The permits of the shared job bound the reference monitor may hold at
/// once, of [`crate::pull_request`]'s four.
const REFERENCE_LANE_PERMITS: usize = 2;

/// The most entries kept for references no card names any more.
const MAX_RETIRED: usize = 512;

/// The shortest time an entry is kept after no card names its reference.
const MIN_RETIRED_TTL: Duration = Duration::from_secs(30 * 60);

/// How long an entry is kept after no card names its reference: at least
/// one failure backoff at `interval`, so naming it again cannot cut a
/// backoff short.
fn retired_ttl(interval: Duration) -> Duration {
    MIN_RETIRED_TTL.max(failure_backoff(interval))
}

/// When the cooldown after a `rate_limited` answer ends. One is shared by
/// the scheduler, which admits no lookup while it runs, and every lookup job,
/// which checks it once it holds its permits and hands its turn back unrun
/// while it runs ([`gated`]).
#[derive(Debug, Clone, Default)]
struct Cooldown(Arc<Mutex<Option<Instant>>>);

impl Cooldown {
    fn lock(&self) -> MutexGuard<'_, Option<Instant>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start a cooldown at `now`, or keep a later-ending one.
    fn start(&self, now: Instant) {
        let until = now + RATE_LIMIT_COOLDOWN;
        let mut current = self.lock();
        if current.is_none_or(|current| current < until) {
            *current = Some(until);
        }
    }

    fn active(&self, now: Instant) -> bool {
        self.lock().is_some_and(|until| now < until)
    }
}

/// The daemon-wide budget of reference lookups: a token bucket and the
/// cooldown after a rate limit.
#[derive(Debug)]
struct Budget {
    tokens: u32,
    refilled_at: Instant,
    cooldown: Cooldown,
}

impl Budget {
    fn new(now: Instant) -> Self {
        Self {
            tokens: LOOKUP_BURST,
            refilled_at: now,
            cooldown: Cooldown::default(),
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.refilled_at);
        let earned = elapsed.as_nanos() / LOOKUP_REFILL_EVERY.as_nanos();
        if earned == 0 {
            return;
        }
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        self.tokens = self.tokens.saturating_add(earned).min(LOOKUP_BURST);
        self.refilled_at = if self.tokens == LOOKUP_BURST {
            now
        } else {
            self.refilled_at + LOOKUP_REFILL_EVERY * earned
        };
    }

    /// Spend one lookup at `now`, or `false` when none is allowed.
    fn take(&mut self, now: Instant) -> bool {
        if self.cooldown.active(now) {
            return false;
        }
        self.refill(now);
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    /// Give back a token spent on a lookup that never ran.
    fn refund(&mut self) {
        self.tokens = self.tokens.saturating_add(1).min(LOOKUP_BURST);
    }

    fn rate_limited(&mut self, now: Instant) {
        self.cooldown.start(now);
    }
}

/// A lookup job's work once it holds its permits: `lookup`, unless a rate
/// limit's cooldown is running, in which case it returns `None` without
/// polling `lookup` — no `gh` runs — and so releases its permits at once
/// rather than sleeping on them. A `rate_limited` answer starts the cooldown
/// here, before the job releases its lane permit, so a lookup waiting for
/// that permit sees it. A lookup already running when the cooldown starts
/// finishes.
async fn gated(
    cooldown: Cooldown,
    lookup: impl std::future::Future<Output = Result<RefAnswer, GhFailure>>,
) -> Option<Result<RefAnswer, GhFailure>> {
    if cooldown.active(Instant::now()) {
        return None;
    }
    let result = lookup.await;
    if matches!(&result, Err(f) if f.reason == GhFailureReason::RateLimited) {
        cooldown.start(Instant::now());
    }
    Some(result)
}

/// The reference monitor's bookkeeping, without I/O: which wanted references
/// to look up at a given moment ([`Self::plan`]) and what a finished lookup
/// changes ([`Self::finished`]).
#[derive(Debug)]
struct Scheduler {
    budget: Budget,
    /// Entries of references no card names any more, with when they left.
    retired: HashMap<ResolvedReference, (LookupEntry<RefAnswer>, Instant)>,
    /// When each wanted reference was first wanted: how long a reference
    /// never looked up has been waiting.
    first_wanted: HashMap<ResolvedReference, Instant>,
    interval: Duration,
}

impl Scheduler {
    fn new(now: Instant, interval: Duration) -> Self {
        Self {
            budget: Budget::new(now),
            retired: HashMap::new(),
            first_wanted: HashMap::new(),
            interval,
        }
    }

    /// Move the entries of `answers` no card wants into the retired ones,
    /// bring back a retired entry whose reference is wanted again, and
    /// return the references to look up now — due ([`fetch_due`]), the
    /// oldest-due first, within the budget and the queue bound. `running`
    /// says whether a lookup is still unfinished, and `queued` how many are.
    fn plan(
        &mut self,
        answers: &mut HashMap<ResolvedReference, LookupEntry<RefAnswer>>,
        wanted: &HashSet<ResolvedReference>,
        running: impl Fn(&ResolvedReference) -> bool,
        mut queued: usize,
        now: Instant,
    ) -> Vec<ResolvedReference> {
        let gone: Vec<ResolvedReference> = answers
            .keys()
            .filter(|r| !wanted.contains(*r))
            .cloned()
            .collect();
        for reference in gone {
            // An entry no lookup ever finished for holds nothing to keep: no
            // answer and no retry deadline. Retiring it would only let a
            // stream of references never looked up push out entries that
            // do. A lookup of it still running records its result in a
            // retired entry of its own ([`Self::finished`]).
            if let Some(entry) = answers.remove(&reference)
                && entry.fetched_at.is_some()
            {
                self.retired.insert(reference, (entry, now));
            }
        }
        let ttl = retired_ttl(self.interval);
        self.retired
            .retain(|_, (_, since)| now.saturating_duration_since(*since) < ttl);
        if self.retired.len() > MAX_RETIRED {
            // The entries that protect nothing go first — no answer, and no
            // failure still inside its backoff — then the oldest.
            let interval = self.interval;
            let mut by_worth: Vec<(bool, Instant, ResolvedReference)> = self
                .retired
                .iter()
                .map(|(r, (entry, since))| {
                    let protects = entry.value.is_some()
                        || (entry.failed
                            && entry.fetched_at.is_some_and(|at| {
                                now.saturating_duration_since(at) < failure_backoff(interval)
                            }));
                    (protects, *since, r.clone())
                })
                .collect();
            by_worth.sort_by_key(|(protects, since, _)| (*protects, *since));
            let excess = self.retired.len() - MAX_RETIRED;
            for (_, _, reference) in by_worth.into_iter().take(excess) {
                self.retired.remove(&reference);
            }
        }
        self.first_wanted.retain(|r, _| wanted.contains(r));

        let mut due: Vec<(Instant, ResolvedReference)> = Vec::new();
        for reference in wanted {
            let since = *self.first_wanted.entry(reference.clone()).or_insert(now);
            let entry = answers.entry(reference.clone()).or_insert_with(|| {
                self.retired
                    .remove(reference)
                    .map(|(entry, _)| entry)
                    .unwrap_or_default()
            });
            entry.in_flight = running(reference);
            if fetch_due(entry, now, self.interval) {
                due.push((entry.fetched_at.unwrap_or(since), reference.clone()));
            }
        }
        due.sort_by(|a, b| {
            a.0.cmp(&b.0).then_with(|| {
                (&a.1.repo, a.1.kind.as_str(), a.1.number).cmp(&(
                    &b.1.repo,
                    b.1.kind.as_str(),
                    b.1.number,
                ))
            })
        });
        let mut start = Vec::new();
        for (_, reference) in due {
            if queued >= MAX_QUEUED_LOOKUPS || !self.budget.take(now) {
                break;
            }
            queued += 1;
            if let Some(entry) = answers.get_mut(&reference) {
                entry.in_flight = true;
                entry.kicked = false;
            }
            start.push(reference);
        }
        start
    }

    /// Take back the lookup of `reference` that was admitted but handed its
    /// turn back unrun, because a rate limit was recorded while it waited
    /// for its permits ([`gated`]). It ran no `gh`, so its token is
    /// refunded, and its entry is left as it was — still due, in its place —
    /// for the first pass after the cooldown.
    fn deferred(
        &mut self,
        answers: &mut HashMap<ResolvedReference, LookupEntry<RefAnswer>>,
        reference: &ResolvedReference,
    ) {
        self.budget.refund();
        if let Some(entry) = answers.get_mut(reference) {
            entry.in_flight = false;
        }
    }

    /// Record the lookup of `reference` that finished at `now` with
    /// `result`, in its entry whether or not a card still names it. Returns
    /// `gh`'s detail when the failure starts a run of failures — the one to
    /// log.
    fn finished(
        &mut self,
        answers: &mut HashMap<ResolvedReference, LookupEntry<RefAnswer>>,
        reference: &ResolvedReference,
        result: Result<RefAnswer, GhFailure>,
        now: Instant,
    ) -> Option<String> {
        if matches!(&result, Err(f) if f.reason == GhFailureReason::RateLimited) {
            self.budget.rate_limited(now);
        }
        let entry = match answers.get_mut(reference) {
            Some(entry) => entry,
            None => {
                &mut self
                    .retired
                    .entry(reference.clone())
                    .or_insert_with(|| (LookupEntry::default(), now))
                    .0
            }
        };
        entry.in_flight = false;
        entry.fetched_at = Some(now);
        match result {
            Ok(answer) => {
                entry.record(answer, now);
                None
            }
            Err(failure) => {
                let detail = failure.detail.clone();
                entry.record_failure(failure).then_some(detail)
            }
        }
    }
}

/// The daemon's background task that resolves the folders and looks up the
/// references the card projection wants ([`ReferenceChecks`]). Runs until
/// aborted; its jobs, and the children they run, go with it. Every `git` and
/// `gh` it runs holds a permit of `permits`, the bound the branch badge's
/// monitor shares, and at most [`REFERENCE_LANE_PERMITS`] of them at once;
/// lookups are admitted as the section above describes.
pub async fn run_reference_monitor(
    registry: Arc<AgentPtyRegistry>,
    interval: Duration,
    permits: Arc<tokio::sync::Semaphore>,
) {
    monitor_references(
        registry.agent_cards().references(),
        interval,
        permits,
        |reference: ResolvedReference| async move { look_up(&reference).await },
    )
    .await;
}

/// [`run_reference_monitor`] over `checks`, with `lookup` the call that
/// checks one reference: [`look_up`] in the daemon, a stand-in in tests.
/// `lookup`'s future does no work until it is first polled, which is after
/// its job holds its permits.
async fn monitor_references<L, F>(
    checks: &ReferenceChecks,
    interval: Duration,
    permits: Arc<tokio::sync::Semaphore>,
    lookup: L,
) where
    L: Fn(ResolvedReference) -> F,
    F: std::future::Future<Output = Result<RefAnswer, GhFailure>> + Send + 'static,
{
    let lane = Arc::new(tokio::sync::Semaphore::new(REFERENCE_LANE_PERMITS));
    let mut probes: JobPool<String, Option<String>> =
        JobPool::with_lane(permits.clone(), lane.clone());
    let mut lookups: JobPool<ResolvedReference, Option<Result<RefAnswer, GhFailure>>> =
        JobPool::with_lane(permits, lane);
    let mut scheduler = Scheduler::new(Instant::now(), interval);
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
            Some((reference, outcome)) = lookups.next() => match outcome {
                // Handed back unrun during a rate limit's cooldown.
                None => scheduler.deferred(&mut checks.lock().view.answers, &reference),
                Some(result) => {
                    let logged = scheduler.finished(
                        &mut checks.lock().view.answers,
                        &reference,
                        result,
                        Instant::now(),
                    );
                    if let Some(detail) = logged {
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
            },
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

        // References: a lookup a card stopped naming is not cancelled, so a
        // call it makes is recorded rather than wasted.
        let due = scheduler.plan(
            &mut checks.lock().view.answers,
            &wanted.refs,
            |reference| lookups.is_running(reference),
            lookups.len(),
            now,
        );
        for reference in due {
            debug!(
                repo = %escape_control_and_bidi(&reference.repo),
                kind = reference.kind.as_str(),
                number = reference.number,
                "checking a reference an agent reported"
            );
            let job = gated(scheduler.budget.cooldown.clone(), lookup(reference.clone()));
            lookups.spawn(reference, job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_cards::CheckResult;
    use crate::pull_request_info::PullRequestChecksState;

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
        let asked = resolved("o/r", ReferenceKind::PullRequest, 7);
        let answer = parse_pr_reference(json, &asked, at()).unwrap();
        assert_eq!(answer.state, PullRequestState::Merged);
        assert_eq!(answer.review, Some(PullRequestReview::Approved));
        assert_eq!(answer.ci.unwrap().state, PullRequestChecksState::Failing);
        assert_eq!(answer.url, "https://github.com/o/r/pull/7");
        assert_eq!(answer.checked_at, at());
        // Control characters in a title are escaped, never passed through.
        assert!(!answer.title.unwrap().contains('\u{1b}'));
        let other = resolved("o/r", ReferenceKind::PullRequest, 8);
        assert!(parse_pr_reference(json, &other, at()).is_err());
    }

    #[test]
    fn an_issue_lookup_reads_state_and_tolerates_a_pull_request_number() {
        let issue = r#"{"number": 3, "state": "CLOSED", "title": "Bug",
            "url": "https://github.com/o/r/issues/3"}"#;
        let asked = resolved("o/r", ReferenceKind::Issue, 3);
        let answer = parse_issue_reference(issue, &asked, at()).unwrap();
        assert_eq!(answer.state, PullRequestState::Closed);
        assert_eq!(answer.title.as_deref(), Some("Bug"));
        // What `gh issue view` answers for a pull request's number.
        let pr = r#"{"number": 4, "state": "MERGED", "title": "PR",
            "url": "https://github.com/o/r/pull/4"}"#;
        let answer =
            parse_issue_reference(pr, &resolved("o/r", ReferenceKind::Issue, 4), at()).unwrap();
        assert_eq!(answer.state, PullRequestState::Merged);
        assert_eq!(answer.url, "https://github.com/o/r/pull/4");
        for bad in [
            r#"{"number": 3, "state": "LOCKED", "url": "https://github.com/o/r/issues/3"}"#,
            r#"{"number": 3, "state": "OPEN", "url": "https://evil.example/o/r/issues/3"}"#,
            r#"{"number": 3, "state": "OPEN", "url": "https://github.com/o/r/issues/4"}"#,
            r#"{"number": 9, "state": "OPEN", "url": "https://github.com/o/r/issues/9"}"#,
        ] {
            assert!(parse_issue_reference(bad, &asked, at()).is_err(), "{bad}");
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
                "number,state,title,url,labels"
            ]
        );
    }

    /// Issue #2418 (D13): an issue's labels come from the same lookup,
    /// bounded and validated as untrusted repository text.
    #[test]
    fn an_issue_lookup_keeps_its_labels_bounded_and_validated() {
        let issue = r##"{"number": 3, "state": "OPEN", "title": "Bug",
            "url": "https://github.com/o/r/issues/3",
            "labels": [
                {"name": "bug", "color": "d73a4a", "id": "x", "description": "d"},
                {"name": "  needs triage  ", "color": "#ededed"},
                {"name": "no colour"},
                {"name": "short", "color": "abc"},
                {"name": "not hex", "color": "zzzzzz"},
                {"name": "", "color": "ffffff"},
                {"name": "   "},
                {"color": "ffffff"},
                {"name": 7},
                {"name": "esc\u001b[31m", "color": "00FF00"}
            ]}"##;
        let asked = resolved("o/r", ReferenceKind::Issue, 3);
        let answer = parse_issue_reference(issue, &asked, at()).unwrap();
        let label = |name: &str, color: Option<&str>| IssueLabel {
            name: name.into(),
            color: color.map(str::to_string),
        };
        assert_eq!(
            answer.labels,
            [
                label("bug", Some("d73a4a")),
                label("needs triage", None),
                label("no colour", None),
                label("short", None),
                label("not hex", None),
                label(&escape_control_and_bidi("esc\u{1b}[31m"), Some("00FF00")),
            ]
        );
        assert!(!answer.labels[5].name.contains('\u{1b}'));

        // A pull request's number answered through the issue lookup keeps
        // none: pull request labels are a later step.
        let pr = r#"{"number": 4, "state": "OPEN", "title": "PR",
            "url": "https://github.com/o/r/pull/4",
            "labels": [{"name": "bug", "color": "d73a4a"}]}"#;
        let answer =
            parse_issue_reference(pr, &resolved("o/r", ReferenceKind::Issue, 4), at()).unwrap();
        assert!(answer.labels.is_empty());

        // No labels key, or one that is not a list, is no labels rather than
        // a failed lookup.
        for json in [
            r#"{"number": 3, "state": "OPEN", "url": "https://github.com/o/r/issues/3"}"#,
            r#"{"number": 3, "state": "OPEN", "url": "https://github.com/o/r/issues/3", "labels": null}"#,
            r#"{"number": 3, "state": "OPEN", "url": "https://github.com/o/r/issues/3", "labels": "bug"}"#,
        ] {
            let answer = parse_issue_reference(json, &asked, at()).expect(json);
            assert!(answer.labels.is_empty(), "{json}");
        }
    }

    #[test]
    fn labels_are_capped_in_count_and_name_length() {
        let long = "x".repeat(MAX_LABEL_NAME_CHARS + 30);
        let mut labels = vec![
            serde_json::json!({"name": ""}),
            serde_json::json!({"name": long, "color": "123456"}),
        ];
        labels.extend((0..MAX_LABELS + 5).map(|i| serde_json::json!({"name": format!("l{i}")})));
        let json = serde_json::json!({"labels": labels}).to_string();
        let kept = labels_of(&json);
        assert_eq!(kept.len(), MAX_LABELS, "skipped labels do not count");
        assert_eq!(kept[0].name.chars().count(), MAX_LABEL_NAME_CHARS);
        assert_eq!(kept[0].color.as_deref(), Some("123456"));
        assert_eq!(kept[1].name, "l0", "kept in gh's order");
        assert_eq!(kept[MAX_LABELS - 1].name, format!("l{}", MAX_LABELS - 2));
        // A multi-byte name is cut on characters, not bytes.
        let wide = "é".repeat(MAX_LABEL_NAME_CHARS + 1);
        let kept = labels_of(&serde_json::json!({"labels": [{"name": wide}]}).to_string());
        assert_eq!(kept[0].name, "é".repeat(MAX_LABEL_NAME_CHARS));
        assert!(labels_of("not json").is_empty());
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
            labels: Vec::new(),
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

    /// Review A2: a lookup's answer is checked as the page of the reference
    /// asked for — a github.com URL, not shorthand or another page, naming
    /// that number, a pull request's page for a pull request, in that
    /// repository whatever its case — and the card links to its canonical
    /// form.
    #[test]
    fn an_answer_must_link_to_the_page_asked_for() {
        let pr_json = |url: &str, number: u64| {
            serde_json::json!({"number": number, "state": "OPEN", "isDraft": false,
                "url": url, "headRefName": "b"})
            .to_string()
        };
        let issue_json = |url: &str, number: u64| {
            serde_json::json!({"number": number, "state": "OPEN", "url": url}).to_string()
        };
        let pr = resolved("o/r", ReferenceKind::PullRequest, 7);
        let issue = resolved("o/r", ReferenceKind::Issue, 7);
        let answer =
            parse_pr_reference(&pr_json("https://github.com/O/R/pull/7/", 7), &pr, at()).unwrap();
        assert_eq!(answer.url, "https://github.com/O/R/pull/7");
        let answer = parse_issue_reference(
            &issue_json("HTTPS://GITHUB.COM/o/r/pull/7", 7),
            &issue,
            at(),
        )
        .unwrap();
        assert_eq!(answer.url, "https://github.com/o/r/pull/7");
        for bad in [
            "o/r#7",
            "#7",
            "https://github.com/login",
            "https://github.com/o/r/pull/8",
            "https://github.com/o/other/pull/7",
            "https://github.com/o/r/pull/%37",
            "https://user:token@github.com/o/r/pull/7",
            "https://github.com/o/r/pull/7?tab=files",
        ] {
            assert!(
                parse_pr_reference(&pr_json(bad, 7), &pr, at()).is_err(),
                "pull request answer {bad:?}"
            );
            assert!(
                parse_issue_reference(&issue_json(bad, 7), &issue, at()).is_err(),
                "issue answer {bad:?}"
            );
        }
        // A pull request lookup answering an issue's page is refused; an
        // issue lookup may answer either.
        assert!(
            parse_pr_reference(&pr_json("https://github.com/o/r/issues/7", 7), &pr, at()).is_err()
        );
        assert!(
            parse_issue_reference(
                &issue_json("https://github.com/o/r/issues/7", 7),
                &issue,
                at()
            )
            .is_ok()
        );
    }

    /// Review R2: a merged pull request with a failed check and another still
    /// running is still settling, though its summary reads failing.
    #[test]
    fn a_reference_with_a_running_check_is_still_settling() {
        let answer = |failing, pending| RefAnswer {
            state: PullRequestState::Merged,
            ci: Some(PullRequestChecks {
                state: PullRequestChecksState::Failing,
                passing: 0,
                failing,
                pending,
                skipped: 0,
            }),
            review: None,
            title: None,
            labels: Vec::new(),
            url: "https://github.com/o/r/pull/1".into(),
            checked_at: at(),
        };
        assert!(answer(1, 1).still_settling());
        assert!(!answer(2, 0).still_settling());
    }

    /// Review A3: what `gh` printed never reaches the card — a client, maybe
    /// remote, sees a fixed sentence per reason, even for a last-known answer
    /// gone stale.
    #[test]
    fn gh_output_never_reaches_the_cards_detail() {
        let secret = "ghp_S3CRETtoken";
        let key = resolved("o/r", ReferenceKind::PullRequest, 5);
        for reason in [
            GhFailureReason::NoGithubAccess,
            GhFailureReason::NotFound,
            GhFailureReason::RateLimited,
            GhFailureReason::Error,
        ] {
            let mut entry = LookupEntry::<RefAnswer>::default();
            entry.record_failure(GhFailure {
                reason,
                detail: format!(
                    "gh pr view failed (exit 1): Post https://x:{secret}@api.github.com/graphql \
                     in /home/someone/.config/gh"
                ),
            });
            let mut view = ReferenceView::default();
            view.answers.insert(key.clone(), entry);
            let c = check_reference(
                "pull-request",
                "o/r#5",
                ReferenceKind::PullRequest,
                None,
                &view,
                &mut WantedRefs::default(),
            );
            let json = serde_json::to_string(&c).unwrap();
            assert!(!json.contains(secret), "{json}");
            assert!(!json.contains("/home/"), "{json}");
            assert!(
                matches!(&c.result, CheckResult::Unavailable { reason: r, detail: Some(d) }
                    if r == reason.as_str() && d == unavailable_detail(reason)),
                "{c:?}"
            );
        }
    }

    // ---- Review A1/R4: admission ----

    fn failed(reason: GhFailureReason) -> Result<RefAnswer, GhFailure> {
        Err(GhFailure {
            reason,
            detail: "failed".into(),
        })
    }

    /// Run `scheduler` for `secs` simulated seconds from `start`, one pass a
    /// second, with `wanted_at(second)` the references wanted then and every
    /// started lookup finishing with `result` before the next pass. Returns
    /// how many lookups started.
    fn simulate(
        scheduler: &mut Scheduler,
        answers: &mut HashMap<ResolvedReference, LookupEntry<RefAnswer>>,
        start: Instant,
        secs: u64,
        wanted_at: impl Fn(u64) -> HashSet<ResolvedReference>,
        result: impl Fn() -> Result<RefAnswer, GhFailure>,
    ) -> usize {
        let mut started = 0;
        for second in 0..secs {
            let now = start + Duration::from_secs(second);
            let due = scheduler.plan(answers, &wanted_at(second), |_| false, 0, now);
            started += due.len();
            for reference in due {
                scheduler.finished(answers, &reference, result(), now);
            }
        }
        started
    }

    /// A report that names new references every second — 32 at a time, the
    /// most a report holds — starts no more lookups in an hour than the
    /// budget allows, however many it named.
    #[test]
    fn rotating_references_stay_inside_the_budget() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let hour = 3600;
        let started = simulate(
            &mut scheduler,
            &mut answers,
            start,
            hour,
            |second| {
                (0..32)
                    .map(|i| resolved("o/r", ReferenceKind::Issue, second * 32 + i + 1))
                    .collect()
            },
            || failed(GhFailureReason::NotFound),
        );
        let budget = LOOKUP_BURST as u64 + hour / LOOKUP_REFILL_EVERY.as_secs();
        assert!(started as u64 <= budget, "{started} lookups > {budget}");
        assert!(started > 0);
        // The entries kept for references no longer named stay bounded.
        assert!(scheduler.retired.len() <= MAX_RETIRED);
    }

    /// Clearing a failing reference and naming it again inside its backoff
    /// finds its failure, not a fresh entry: no lookup until the backoff
    /// passes, however often it is cleared and named again.
    #[test]
    fn clearing_and_renaming_a_failing_reference_keeps_its_backoff() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let key = resolved("o/r", ReferenceKind::PullRequest, 9);
        let backoff = failure_backoff(interval).as_secs();
        let started = simulate(
            &mut scheduler,
            &mut answers,
            start,
            backoff - 1,
            // Named on even seconds, cleared on odd ones.
            |second| {
                if second % 2 == 0 {
                    HashSet::from([key.clone()])
                } else {
                    HashSet::new()
                }
            },
            || failed(GhFailureReason::Error),
        );
        assert_eq!(started, 1, "one lookup, then its backoff");
        // Named again once the backoff passed: looked up again.
        let due = scheduler.plan(
            &mut answers,
            &HashSet::from([key.clone()]),
            |_| false,
            0,
            start + Duration::from_secs(backoff),
        );
        assert_eq!(due, [key]);
    }

    /// A terminal answer outlives a clear: a merged pull request named again
    /// is not looked up again.
    #[test]
    fn a_renamed_terminal_reference_is_not_looked_up_again() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let key = resolved("o/r", ReferenceKind::PullRequest, 2);
        let wanted = HashSet::from([key.clone()]);
        let due = scheduler.plan(&mut answers, &wanted, |_| false, 0, start);
        assert_eq!(due.len(), 1);
        let merged = RefAnswer {
            state: PullRequestState::Merged,
            ci: None,
            review: None,
            title: None,
            labels: Vec::new(),
            url: key.url(),
            checked_at: at(),
        };
        scheduler.finished(&mut answers, &key, Ok(merged), start);
        let later = start + Duration::from_secs(600);
        assert!(
            scheduler
                .plan(&mut answers, &HashSet::new(), |_| false, 0, later)
                .is_empty()
        );
        assert!(answers.is_empty());
        assert!(
            scheduler
                .plan(&mut answers, &wanted, |_| false, 0, later)
                .is_empty()
        );
        assert!(answers[&key].value.is_some(), "its answer is back");
    }

    /// One `rate_limited` answer stops the scheduler admitting any reference
    /// lookup, not only that reference's, for the cooldown. This drives
    /// [`Scheduler::plan`] alone: the lookups already admitted and waiting
    /// for the lane are
    /// `a_rate_limit_holds_the_lookups_queued_behind_it`'s.
    #[test]
    fn a_rate_limit_pauses_every_reference_lookup() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let first = resolved("o/r", ReferenceKind::PullRequest, 1);
        let due = scheduler.plan(
            &mut answers,
            &HashSet::from([first.clone()]),
            |_| false,
            0,
            start,
        );
        assert_eq!(due.len(), 1);
        scheduler.finished(
            &mut answers,
            &first,
            failed(GhFailureReason::RateLimited),
            start,
        );
        let started = simulate(
            &mut scheduler,
            &mut answers,
            start + Duration::from_secs(1),
            RATE_LIMIT_COOLDOWN.as_secs() - 1,
            |second| {
                (0..4)
                    .map(|i| resolved("a/b", ReferenceKind::Issue, second * 4 + i + 1))
                    .collect()
            },
            || failed(GhFailureReason::RateLimited),
        );
        assert_eq!(started, 0);
        let other = resolved("c/d", ReferenceKind::Issue, 1);
        let due = scheduler.plan(
            &mut answers,
            &HashSet::from([other.clone()]),
            |_| false,
            0,
            start + RATE_LIMIT_COOLDOWN,
        );
        assert_eq!(due, [other]);
    }

    /// Wait up to ten seconds for `done`.
    async fn wait_for(what: &str, done: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Review N1: the monitor admits [`MAX_QUEUED_LOOKUPS`] lookups, two of
    /// which run while the rest wait for the lane. Once a running one
    /// answers `rate_limited`, the waiting ones give their turn back without
    /// calling `gh`, the other running one finishes, nothing more starts
    /// during the cooldown, and a pool outside the lane — the branch
    /// badge's — still runs its jobs throughout.
    #[tokio::test]
    async fn a_rate_limit_holds_the_lookups_queued_behind_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let checks = Arc::new(ReferenceChecks::default());
        let permits = crate::pull_request::new_job_bound();
        let started = Arc::new(AtomicUsize::new(0));
        // Each permit lets one running stand-in answer `rate_limited`.
        let answer = Arc::new(tokio::sync::Semaphore::new(0));
        let refs: HashSet<ResolvedReference> = (1..=MAX_QUEUED_LOOKUPS as u64)
            .map(|n| resolved("o/r", ReferenceKind::Issue, n))
            .collect();
        checks.set_wanted(WantedRefs {
            cwds: BTreeSet::new(),
            refs: refs.clone(),
        });
        let monitor = {
            let (checks, permits) = (checks.clone(), permits.clone());
            let (started, answer) = (started.clone(), answer.clone());
            tokio::spawn(async move {
                monitor_references(
                    &checks,
                    Duration::from_secs(60),
                    permits,
                    move |_reference: ResolvedReference| {
                        let (started, answer) = (started.clone(), answer.clone());
                        async move {
                            started.fetch_add(1, Ordering::SeqCst);
                            answer.acquire().await.expect("never closed").forget();
                            failed(GhFailureReason::RateLimited)
                        }
                    },
                )
                .await;
            })
        };
        let branch_jobs_run = |permits: Arc<tokio::sync::Semaphore>| async move {
            let mut branch: JobPool<u32, u32> = JobPool::new(permits);
            for id in 0..10 {
                branch.spawn(id, async move { id });
            }
            let mut served = 0;
            while tokio::time::timeout(Duration::from_secs(10), branch.next())
                .await
                .expect("the branch jobs are served")
                .is_some()
            {
                served += 1;
            }
            assert_eq!(served, 10);
        };

        wait_for("the lane full", || {
            started.load(Ordering::SeqCst) == REFERENCE_LANE_PERMITS
        })
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(started.load(Ordering::SeqCst), REFERENCE_LANE_PERMITS);
        branch_jobs_run(permits.clone()).await;

        // The first running lookup answers `rate_limited`, then the other.
        answer.add_permits(1);
        let settled = |checks: &ReferenceChecks| {
            let view = checks.view();
            let answered = refs
                .iter()
                .filter(|r| view.answers.get(*r).is_some_and(|e| e.fetched_at.is_some()))
                .count();
            let held = refs
                .iter()
                .filter(|r| {
                    view.answers
                        .get(*r)
                        .is_some_and(|e| e.fetched_at.is_none() && !e.in_flight)
                })
                .count();
            (answered, held)
        };
        wait_for("the queued lookups held, or one started", || {
            started.load(Ordering::SeqCst) > REFERENCE_LANE_PERMITS
                || settled(&checks) == (1, MAX_QUEUED_LOOKUPS - REFERENCE_LANE_PERMITS)
        })
        .await;
        assert_eq!(
            started.load(Ordering::SeqCst),
            REFERENCE_LANE_PERMITS,
            "a queued lookup ran gh after the rate limit"
        );
        answer.add_permits(1);
        wait_for("the running lookup finished", || {
            settled(&checks) == (2, MAX_QUEUED_LOOKUPS - REFERENCE_LANE_PERMITS)
        })
        .await;

        // A newly named reference wakes the monitor; nothing starts.
        let mut more = refs.clone();
        more.insert(resolved("o/r", ReferenceKind::Issue, 99));
        checks.set_wanted(WantedRefs {
            cwds: BTreeSet::new(),
            refs: more,
        });
        branch_jobs_run(permits.clone()).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(started.load(Ordering::SeqCst), REFERENCE_LANE_PERMITS);
        monitor.abort();
    }

    /// Review N1: lookups handed back unrun during a cooldown cost no
    /// budget, keep their place, and go first once the cooldown ends.
    #[test]
    fn lookups_held_by_a_rate_limit_go_first_after_it() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let held: HashSet<ResolvedReference> = (1..=MAX_QUEUED_LOOKUPS as u64)
            .map(|n| resolved("o/r", ReferenceKind::Issue, n))
            .collect();
        let due = scheduler.plan(&mut answers, &held, |_| false, 0, start);
        assert_eq!(due.len(), MAX_QUEUED_LOOKUPS);
        let tokens = scheduler.budget.tokens;
        scheduler.finished(
            &mut answers,
            &due[0],
            failed(GhFailureReason::RateLimited),
            start,
        );
        for reference in &due[1..] {
            scheduler.deferred(&mut answers, reference);
        }
        assert_eq!(
            scheduler.budget.tokens,
            tokens + MAX_QUEUED_LOOKUPS as u32 - 1,
            "a lookup that never ran is refunded"
        );
        let mut wanted = held.clone();
        let newer = resolved("o/r", ReferenceKind::Issue, 99);
        wanted.insert(newer.clone());
        let later = start + Duration::from_secs(1);
        assert!(
            scheduler
                .plan(&mut answers, &wanted, |_| false, 0, later)
                .is_empty()
        );
        let after = start + RATE_LIMIT_COOLDOWN;
        let due_after = scheduler.plan(&mut answers, &wanted, |_| false, 0, after);
        assert_eq!(due_after.len(), MAX_QUEUED_LOOKUPS);
        assert!(
            !due_after.contains(&newer),
            "the held lookups go before a newer reference"
        );
        assert!(
            due_after
                .iter()
                .all(|r| due[1..].contains(r) || *r == due[0])
        );
    }

    /// Review N2: references named and dropped without ever being looked up
    /// keep no entry, so however many rotate through, a failed reference
    /// named again inside its backoff still finds its failure.
    #[test]
    fn untouched_references_do_not_evict_a_failed_ones_backoff() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let key = resolved("o/r", ReferenceKind::PullRequest, 9);
        let due = scheduler.plan(
            &mut answers,
            &HashSet::from([key.clone()]),
            |_| false,
            0,
            start,
        );
        assert_eq!(due, std::slice::from_ref(&key));
        scheduler.finished(&mut answers, &key, failed(GhFailureReason::Error), start);
        // The budget is spent, so none of the references below is looked up.
        let rotate_at = start + Duration::from_secs(1);
        while scheduler.budget.take(rotate_at) {}
        let sets = MAX_RETIRED / 32 + 2;
        for set in 0..sets {
            let wanted: HashSet<ResolvedReference> = (0..32)
                .map(|i| resolved("a/b", ReferenceKind::Issue, (set * 32 + i + 1) as u64))
                .collect();
            let now = rotate_at + Duration::from_millis(set as u64);
            assert!(
                scheduler
                    .plan(&mut answers, &wanted, |_| false, 0, now)
                    .is_empty()
            );
        }
        assert!(scheduler.retired.len() <= MAX_RETIRED);
        let again = start + Duration::from_secs(10);
        assert!(again < start + failure_backoff(interval));
        let due = scheduler.plan(
            &mut answers,
            &HashSet::from([key.clone()]),
            |_| false,
            0,
            again,
        );
        assert!(due.is_empty(), "named again inside its backoff: {due:?}");
        assert!(answers[&key].failed, "its failure is back");
    }

    /// Review N2: when more than [`MAX_RETIRED`] entries are kept, the ones
    /// that protect nothing — no answer, and a failure whose backoff has
    /// passed — go before an older one holding an answer.
    #[test]
    fn a_full_retired_set_drops_what_protects_nothing_first() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let kept = resolved("o/r", ReferenceKind::PullRequest, 1);
        let open = RefAnswer {
            state: PullRequestState::Open,
            ci: None,
            review: None,
            title: None,
            labels: Vec::new(),
            url: kept.url(),
            checked_at: at(),
        };
        let answered = LookupEntry {
            value: Some(open),
            fetched_at: Some(start),
            ..LookupEntry::default()
        };
        scheduler.retired.insert(kept.clone(), (answered, start));
        for n in 0..MAX_RETIRED as u64 {
            let spent = LookupEntry {
                failed: true,
                fetched_at: Some(start),
                ..LookupEntry::default()
            };
            scheduler.retired.insert(
                resolved("a/b", ReferenceKind::Issue, n + 1),
                (spent, start + Duration::from_secs(1)),
            );
        }
        let now = start + failure_backoff(interval) + Duration::from_secs(1);
        assert!(now < start + retired_ttl(interval));
        scheduler.plan(&mut answers, &HashSet::new(), |_| false, 0, now);
        assert_eq!(scheduler.retired.len(), MAX_RETIRED);
        assert!(scheduler.retired.contains_key(&kept), "the answer is kept");
    }

    /// At most [`MAX_QUEUED_LOOKUPS`] lookups are unfinished, and the
    /// reference waiting longest goes first: a burst of new references does
    /// not hold back the refresh of one already shown.
    #[test]
    fn the_queue_is_bounded_and_the_oldest_due_goes_first() {
        let interval = Duration::from_secs(60);
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start, interval);
        let mut answers = HashMap::new();
        let shown = resolved("o/r", ReferenceKind::PullRequest, 1);
        let open = RefAnswer {
            state: PullRequestState::Open,
            ci: None,
            review: None,
            title: None,
            labels: Vec::new(),
            url: shown.url(),
            checked_at: at(),
        };
        let due = scheduler.plan(
            &mut answers,
            &HashSet::from([shown.clone()]),
            |_| false,
            0,
            start,
        );
        assert_eq!(due, std::slice::from_ref(&shown));
        scheduler.finished(&mut answers, &shown, Ok(open), start);
        let later = start + interval;
        let mut wanted: HashSet<ResolvedReference> = (2..40)
            .map(|n| resolved("o/r", ReferenceKind::Issue, n))
            .collect();
        wanted.insert(shown.clone());
        let due = scheduler.plan(&mut answers, &wanted, |_| false, 0, later);
        assert_eq!(due.len(), MAX_QUEUED_LOOKUPS);
        assert_eq!(due[0], shown, "the shown reference's refresh goes first");
        // With the queue full, nothing more starts.
        let more = scheduler.plan(
            &mut answers,
            &wanted,
            |r| due.contains(r),
            MAX_QUEUED_LOOKUPS,
            later,
        );
        assert!(more.is_empty());
    }

    /// Review A1: `O/R#1` and `o/r#1` are one lookup.
    #[test]
    fn one_repository_in_two_cases_is_looked_up_once() {
        let view = ReferenceView::default();
        let mut wanted = WantedRefs::default();
        for claim in ["Owner/Repo#1", "owner/repo#1"] {
            check_reference(
                "pull-request",
                claim,
                ReferenceKind::PullRequest,
                None,
                &view,
                &mut wanted,
            );
        }
        assert_eq!(wanted.refs.len(), 1);
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
