//! PRD #1401: the wire types for the pull request an agent's work produced,
//! carried on [`crate::state::SessionSnapshot::pull_request`].
//!
//! Types only, kept apart from [`crate::pull_request`] on purpose. That module
//! is the daemon's resolver — it runs `git` and `gh` against each session's
//! working directory — and the desktop crate must not reach it
//! (`xtask/linkage-check`'s desktop project boundary). Clients need only the
//! value's shape, so the shape lives here, with no I/O at all.
//!
//! Both enums are tolerant of values they do not know: a newer daemon may
//! report a state or review decision an older client has never heard of, and
//! that must read as [`PullRequestState::Unknown`] /
//! [`PullRequestReview::Unknown`] rather than fail the whole agent record it
//! rides on.

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
