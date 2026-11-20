//! Issue #2418 (D4): reading a pull request or issue reference an agent
//! reports in a card field, strictly, before the daemon looks it up on GitHub
//! or a client opens it as a link.
//!
//! A report field of type `pull_request` or `issue` holds one of the
//! spellings the report validation already accepts (`#123`,
//! `owner/repo#123`, or an `https://` URL). Only these GitHub forms are
//! recognised here:
//!
//! * `#123` — a number in the reporting agent's own repository, which the
//!   caller resolves the way the branch badge does, from the `origin` of the
//!   agent's working directory ([`GithubReference::resolve`]);
//! * `owner/repo#123`;
//! * `https://github.com/<owner>/<repo>/pull/<n>` or
//!   `https://github.com/<owner>/<repo>/issues/<n>`, with at most a trailing
//!   `/`, and nothing else: no other host, port, user, extra path segment,
//!   query or fragment.
//!
//! Anything else is unrecognised: not looked up and not linked. Owner and
//! repository names must use the characters GitHub allows in them, and the
//! number must be a positive decimal with no leading zero.
//!
//! For `#123` and `owner/repo#123` the kind is the field's
//! ([`ReferenceKind::of_field_type`]). A URL names its own kind (`/pull/` or
//! `/issues/`) and that one is used, whatever the field's type, so a pull
//! request URL in an `issue` field is checked as the pull request it is.
//!
//! This module does no I/O.

use serde::{Deserialize, Serialize};

use crate::card_catalog::FieldType;

/// What a reference names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    PullRequest,
    Issue,
}

impl ReferenceKind {
    /// The spelling the catalog's field types use (`pull_request`, `issue`).
    pub fn as_str(self) -> &'static str {
        match self {
            ReferenceKind::PullRequest => "pull_request",
            ReferenceKind::Issue => "issue",
        }
    }

    /// The kind a field of `field_type` holds, or `None` for a type that
    /// holds no reference (D4: only `pull_request` and `issue` fields are
    /// read as references).
    pub fn of_field_type(field_type: FieldType) -> Option<Self> {
        match field_type {
            FieldType::PullRequest => Some(ReferenceKind::PullRequest),
            FieldType::Issue => Some(ReferenceKind::Issue),
            _ => None,
        }
    }

    /// The path segment of this kind's page on github.com.
    fn path_segment(self) -> &'static str {
        match self {
            ReferenceKind::PullRequest => "pull",
            ReferenceKind::Issue => "issues",
        }
    }
}

/// A recognised reference, as written: `repo` is `None` for `#123`, which
/// names the reporting agent's own repository.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GithubReference {
    /// `owner/name`, when the reference names it.
    pub repo: Option<String>,
    pub kind: ReferenceKind,
    pub number: u64,
}

/// A reference with its repository known: what the daemon looks up, caches
/// and links to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedReference {
    /// `owner/name`, as written in the reference or in the agent's `origin`.
    pub repo: String,
    pub kind: ReferenceKind,
    pub number: u64,
}

impl GithubReference {
    /// Read `claim`, the value of a field holding `field_kind`, or `None`
    /// when it is not one of the GitHub forms the module docs list.
    pub fn parse(claim: &str, field_kind: ReferenceKind) -> Option<Self> {
        let claim = claim.trim();
        if let Some(rest) = strip_prefix_ignore_ascii_case(claim, "https://github.com/") {
            return parse_url_path(rest);
        }
        let (repo, number) = claim.split_once('#')?;
        let number = parse_number(number)?;
        let repo = if repo.is_empty() {
            None
        } else {
            let (owner, name) = repo.split_once('/')?;
            if !is_github_owner(owner) || !is_github_repo_name(name) {
                return None;
            }
            Some(repo.to_string())
        };
        Some(Self {
            repo,
            kind: field_kind,
            number,
        })
    }

    /// The reference with its repository known: its own, else
    /// `default_repo` (the agent's `origin`, as `owner/name`). `None` when it
    /// names none and `default_repo` is missing or not a GitHub `owner/name`.
    pub fn resolve(&self, default_repo: Option<&str>) -> Option<ResolvedReference> {
        let repo = match &self.repo {
            Some(repo) => repo.clone(),
            None => default_repo.filter(|repo| is_repo_slug(repo))?.to_string(),
        };
        Some(ResolvedReference {
            repo,
            kind: self.kind,
            number: self.number,
        })
    }
}

impl ResolvedReference {
    /// The reference's page on github.com.
    pub fn url(&self) -> String {
        format!(
            "https://github.com/{}/{}/{}",
            self.repo,
            self.kind.path_segment(),
            self.number
        )
    }
}

/// `<owner>/<repo>/(pull|issues)/<n>` with at most a trailing `/`: what
/// follows `https://github.com/` in a recognised URL. A query or fragment
/// stays inside the last segment, so the number does not parse.
fn parse_url_path(path: &str) -> Option<GithubReference> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let mut parts = path.split('/');
    let (Some(owner), Some(name), Some(segment), Some(number), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return None;
    };
    if !is_github_owner(owner) || !is_github_repo_name(name) {
        return None;
    }
    let kind = [ReferenceKind::PullRequest, ReferenceKind::Issue]
        .into_iter()
        .find(|kind| kind.path_segment() == segment)?;
    Some(GithubReference {
        repo: Some(format!("{owner}/{name}")),
        kind,
        number: parse_number(number)?,
    })
}

/// A positive decimal with no sign and no leading zero.
fn parse_number(s: &str) -> Option<u64> {
    if s.is_empty() || s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
}

/// A GitHub user or organisation name: 1–39 ASCII letters, digits and
/// hyphens, not starting with a hyphen.
pub fn is_github_owner(s: &str) -> bool {
    (1..=39).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A GitHub repository name: 1–100 ASCII letters, digits, `-`, `_` and `.`,
/// other than `.` and `..`.
pub fn is_github_repo_name(s: &str) -> bool {
    (1..=100).contains(&s.len())
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `owner/name` with both parts valid.
pub fn is_repo_slug(s: &str) -> bool {
    s.split_once('/')
        .is_some_and(|(owner, name)| is_github_owner(owner) && is_github_repo_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ReferenceKind::{Issue, PullRequest};

    fn reference(repo: Option<&str>, kind: ReferenceKind, number: u64) -> GithubReference {
        GithubReference {
            repo: repo.map(str::to_string),
            kind,
            number,
        }
    }

    #[test]
    fn the_shorthand_spellings_take_the_fields_kind() {
        assert_eq!(
            GithubReference::parse("#123", PullRequest),
            Some(reference(None, PullRequest, 123))
        );
        assert_eq!(
            GithubReference::parse("#7", Issue),
            Some(reference(None, Issue, 7))
        );
        assert_eq!(
            GithubReference::parse("vfarcic/dot-agent-deck#2378", PullRequest),
            Some(reference(Some("vfarcic/dot-agent-deck"), PullRequest, 2378))
        );
        assert_eq!(
            GithubReference::parse(" o/r.js#1 ", Issue),
            Some(reference(Some("o/r.js"), Issue, 1))
        );
    }

    #[test]
    fn a_github_url_names_its_own_kind() {
        assert_eq!(
            GithubReference::parse("https://github.com/o/r/pull/12", Issue),
            Some(reference(Some("o/r"), PullRequest, 12))
        );
        assert_eq!(
            GithubReference::parse("https://github.com/o/r/issues/12/", PullRequest),
            Some(reference(Some("o/r"), Issue, 12))
        );
        assert_eq!(
            GithubReference::parse("HTTPS://GitHub.com/o/r/pull/3", PullRequest),
            Some(reference(Some("o/r"), PullRequest, 3))
        );
    }

    #[test]
    fn anything_else_is_unrecognised() {
        for claim in [
            "",
            "#",
            "123",
            "#0",
            "#012",
            "#-1",
            "#+1",
            "#1.5",
            "#99999999999999999999999",
            "o#1",
            "o/r/x#1",
            "/r#1",
            "o/#1",
            "-o/r#1",
            "o_o/r#1",
            "o.o/r#1",
            "o/./#1",
            "o/..#1",
            "o/r#1#2",
            "o/r # 1",
            "http://github.com/o/r/pull/1",
            "https://www.github.com/o/r/pull/1",
            "https://github.com.evil.example/o/r/pull/1",
            "https://github.com:443/o/r/pull/1",
            "https://user@github.com/o/r/pull/1",
            "https://gitlab.com/o/r/pull/1",
            "https://github.com/o/r/pulls/1",
            "https://github.com/o/r/Pull/1",
            "https://github.com/o/r/pull/1/files",
            "https://github.com/o/r/pull/1?x=1",
            "https://github.com/o/r/issues/1#issuecomment-5",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pull/1//",
            "https://github.com/o/r",
            "https://github.com/o/r/discussions/1",
            "https://github.com//r/pull/1",
            &format!("{}/r#1", "a".repeat(40)),
            &format!("o/{}#1", "a".repeat(101)),
        ] {
            assert_eq!(
                GithubReference::parse(claim, PullRequest),
                None,
                "{claim:?} must be unrecognised"
            );
        }
    }

    #[test]
    fn a_bare_number_resolves_against_the_agents_origin() {
        let bare = reference(None, Issue, 5);
        assert_eq!(
            bare.resolve(Some("o/r")),
            Some(ResolvedReference {
                repo: "o/r".into(),
                kind: Issue,
                number: 5
            })
        );
        assert_eq!(bare.resolve(None), None);
        assert_eq!(bare.resolve(Some("not a slug")), None);
        assert_eq!(bare.resolve(Some("o/r/x")), None);
        // A reference naming its repository ignores the origin.
        let named = reference(Some("a/b"), PullRequest, 9);
        assert_eq!(
            named.resolve(Some("o/r")).map(|r| r.repo),
            Some("a/b".into())
        );
        assert_eq!(named.resolve(None).map(|r| r.repo), Some("a/b".into()));
    }

    #[test]
    fn a_resolved_reference_links_to_its_github_page() {
        let pr = ResolvedReference {
            repo: "o/r".into(),
            kind: PullRequest,
            number: 12,
        };
        assert_eq!(pr.url(), "https://github.com/o/r/pull/12");
        let issue = ResolvedReference { kind: Issue, ..pr };
        assert_eq!(issue.url(), "https://github.com/o/r/issues/12");
        // The URL a reference produces parses back to the same reference.
        assert_eq!(
            GithubReference::parse(&issue.url(), PullRequest),
            Some(reference(Some("o/r"), Issue, 12))
        );
    }

    #[test]
    fn only_reference_field_types_hold_a_reference() {
        assert_eq!(
            ReferenceKind::of_field_type(FieldType::PullRequest),
            Some(PullRequest)
        );
        assert_eq!(ReferenceKind::of_field_type(FieldType::Issue), Some(Issue));
        for other in FieldType::ALL
            .into_iter()
            .filter(|t| !matches!(t, FieldType::PullRequest | FieldType::Issue))
        {
            assert_eq!(ReferenceKind::of_field_type(other), None, "{other:?}");
        }
        // The kinds' spellings are the field types' own.
        for kind in [PullRequest, Issue] {
            assert_eq!(
                FieldType::parse(kind.as_str()).and_then(ReferenceKind::of_field_type),
                Some(kind)
            );
        }
    }
}
