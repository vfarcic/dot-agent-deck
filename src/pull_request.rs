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

#[cfg(test)]
mod tests {
    use super::*;

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
