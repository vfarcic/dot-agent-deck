//! Which release is "the previous release".
//!
//! `--previous` used to default to a hardcoded tag. That was correct on the day
//! it was written and silently wrong from the next release on: a default run
//! would test the branch against a release that was no longer the previous one,
//! and nothing in its output would say so — the quiet false coverage this
//! harness exists to remove. So when `--previous` is not given, the tag is
//! resolved from the repository's own release listing, and when it cannot be
//! resolved the run stops and asks for one. There is no hardcoded fallback.
//!
//! # The rule, and why it is this one
//!
//! `release.yml` creates its releases as non-drafts, and flags one a
//! prerelease exactly when its version contains a `-` — how a SemVer
//! prerelease suffix (`-alpha.0`, `-rc.1`) begins — routing those to a
//! separate `-beta` Homebrew formula. GitHub marks each newly published
//! stable release **Latest** by default. So "the previous release" is the
//! highest `vMAJOR.MINOR.PATCH` among the releases that are neither drafts nor
//! prereleases, and it must also be the one GitHub marks Latest. On 2026-09-21
//! the two agreed (`v0.41.0`), and all 99 stable releases then listed had been
//! published in version order. They disagree when a release is published out
//! of version order — a patch on an older line, say, which the default marks
//! Latest — or when Latest is moved by hand, and then which one a user has is
//! a judgement this harness does not make: it refuses, names both, and asks for
//! `--previous`.

use std::process::Command;

use serde::Deserialize;

/// How many of the most recently created releases the listing asks for. A
/// release marked Latest that falls outside them is not guessed at: `choose`
/// then finds none marked, and refuses.
const LISTING_LIMIT: &str = "100";

/// The previous release a run tests against, and where that tag came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Previous {
    pub tag: String,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// `--previous <tag>` on the command line.
    Explicit,
    /// Resolved from `gh release list --repo <repo>` at `queried_at` (UTC).
    Resolved { repo: String, queried_at: String },
}

impl Previous {
    /// The evidence file's account of where the tag came from, so a reader can
    /// tell a tag somebody typed from one the harness chose.
    pub fn describe(&self) -> String {
        match &self.source {
            Source::Explicit => "given explicitly with `--previous`".to_string(),
            Source::Resolved { repo, queried_at } => format!(
                "resolved, not given: the highest `vMAJOR.MINOR.PATCH` release of `{repo}` that \
                 is neither a draft nor a prerelease, and the one GitHub marks Latest, read \
                 from `gh release list` at {queried_at}"
            ),
        }
    }
}

/// One row of `gh release list --json tagName,isDraft,isPrerelease,isLatest`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    tag_name: String,
    is_draft: bool,
    is_prerelease: bool,
    is_latest: bool,
}

/// `vMAJOR.MINOR.PATCH` and nothing else — the shape of every stable release
/// tag this repository publishes.
fn stable_version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v')?.split('.');
    let mut next = || -> Option<u64> {
        let p = parts.next()?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        p.parse().ok()
    };
    let v = (next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

/// Choose the previous release from a `gh release list` JSON listing, or say
/// why it cannot be chosen. Pure, so every refusal is unit-tested.
pub fn choose(listing: &str) -> Result<String, String> {
    let rows: Vec<Row> = serde_json::from_str(listing)
        .map_err(|e| format!("the release listing is not the JSON expected: {e}"))?;
    let highest = rows
        .iter()
        .filter(|r| !r.is_draft && !r.is_prerelease)
        .filter_map(|r| stable_version(&r.tag_name).map(|v| (v, r.tag_name.as_str())))
        .max_by_key(|(v, _)| *v)
        .map(|(_, tag)| tag)
        .ok_or_else(|| {
            format!(
                "none of the {} release(s) listed is published (not a draft), stable (not a \
                 prerelease) and tagged `vMAJOR.MINOR.PATCH`",
                rows.len()
            )
        })?;
    let latest: Vec<&str> = rows
        .iter()
        .filter(|r| r.is_latest)
        .map(|r| r.tag_name.as_str())
        .collect();
    match latest.as_slice() {
        [] => Err(format!(
            "no release in the listing is marked Latest, so the highest stable release \
             `{highest}` cannot be confirmed as the current one"
        )),
        [one] if *one == highest => Ok(highest.to_string()),
        [one] => Err(format!(
            "GitHub marks `{one}` Latest, but the highest stable release is `{highest}` — a \
             release published out of version order makes \"the previous release\" ambiguous"
        )),
        many => Err(format!(
            "{} releases are marked Latest ({})",
            many.len(),
            many.join(", ")
        )),
    }
}

/// The refusal every unresolvable case ends in.
fn fail_closed(repo: &str, why: &str) -> String {
    format!(
        "cannot resolve the previous release of `{repo}`: {why}. Pass the release to test \
         against explicitly with `--previous <tag>` (`gh release list --repo {repo}` lists them) \
         — this harness does not fall back to a hardcoded tag"
    )
}

/// Resolve `--previous`: the tag as given, or else the repository's current
/// release, or else an error that stops the run.
///
/// `--old-binary` without `--previous` is refused rather than resolved: that
/// binary's version is recorded but not enforced, so resolving would label an
/// arbitrary binary with a release tag it was never checked against.
pub fn resolve(
    explicit: Option<&str>,
    old_binary_given: bool,
    repo: &str,
    queried_at: impl FnOnce() -> String,
) -> Result<Previous, String> {
    if let Some(tag) = explicit {
        return Ok(Previous {
            tag: tag.to_string(),
            source: Source::Explicit,
        });
    }
    if old_binary_given {
        return Err(
            "`--old-binary` needs an explicit `--previous` naming the release that binary is: its \
             version is recorded but not enforced, so the harness will not label it with a \
             resolved tag"
                .to_string(),
        );
    }
    let queried_at = queried_at();
    let out = Command::new("gh")
        .args([
            "release",
            "list",
            "--repo",
            repo,
            "--limit",
            LISTING_LIMIT,
            "--json",
            "tagName,isDraft,isPrerelease,isLatest",
        ])
        .output()
        .map_err(|e| fail_closed(repo, &format!("could not run `gh`: {e}")))?;
    if !out.status.success() {
        return Err(fail_closed(
            repo,
            &format!(
                "`gh release list` failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    let tag =
        choose(&String::from_utf8_lossy(&out.stdout)).map_err(|why| fail_closed(repo, &why))?;
    Ok(Previous {
        tag,
        source: Source::Resolved {
            repo: repo.to_string(),
            queried_at,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tag: &str, draft: bool, pre: bool, latest: bool) -> String {
        format!(
            r#"{{"tagName":"{tag}","isDraft":{draft},"isPrerelease":{pre},"isLatest":{latest}}}"#
        )
    }

    fn listing(rows: &[String]) -> String {
        format!("[{}]", rows.join(","))
    }

    #[test]
    fn the_highest_stable_release_is_chosen_when_github_marks_it_latest() {
        // The shape of the real listing on 2026-09-21, including the one
        // prerelease this repository has published.
        let l = listing(&[
            row("v0.41.0", false, false, true),
            row("v0.40.2", false, false, false),
            row("v0.40.10", false, false, false),
            row("v0.25.0-alpha.0", false, true, false),
        ]);
        assert_eq!(choose(&l), Ok("v0.41.0".to_string()));
    }

    #[test]
    fn versions_compare_numerically_not_as_strings() {
        let l = listing(&[
            row("v0.9.0", false, false, false),
            row("v0.10.0", false, false, true),
        ]);
        assert_eq!(choose(&l), Ok("v0.10.0".to_string()));
    }

    #[test]
    fn drafts_and_prereleases_are_never_chosen_even_when_higher() {
        let l = listing(&[
            row("v0.43.0", true, false, false),
            row("v0.42.0-rc.1", false, true, false),
            row("v0.41.0", false, false, true),
        ]);
        assert_eq!(choose(&l), Ok("v0.41.0".to_string()));
    }

    #[test]
    fn a_release_published_out_of_version_order_is_refused_naming_both() {
        // A patch on an older line, published after the newer minor: GitHub
        // marks it Latest by default.
        let l = listing(&[
            row("v0.40.3", false, false, true),
            row("v0.41.0", false, false, false),
        ]);
        let err = choose(&l).unwrap_err();
        assert!(
            err.contains("`v0.40.3` Latest") && err.contains("`v0.41.0`"),
            "{err}"
        );
    }

    #[test]
    fn no_release_marked_latest_is_refused() {
        let l = listing(&[row("v0.41.0", false, false, false)]);
        let err = choose(&l).unwrap_err();
        assert!(err.contains("marked Latest"), "{err}");
    }

    #[test]
    fn a_listing_with_no_stable_release_is_refused() {
        for l in [
            listing(&[]),
            listing(&[
                row("v0.42.0", true, false, false),
                row("v0.25.0-alpha.0", false, true, false),
                row("nightly", false, false, true),
            ]),
        ] {
            let err = choose(&l).unwrap_err();
            assert!(err.contains("none of the"), "{err}");
        }
    }

    #[test]
    fn a_latest_tag_that_is_not_a_plain_version_is_refused() {
        let l = listing(&[
            row("v0.41.0", false, false, false),
            row("release-42", false, false, true),
        ]);
        assert!(choose(&l).is_err());
    }

    #[test]
    fn output_that_is_not_the_listing_is_refused() {
        for l in [
            "",
            "not json",
            r#"{"tagName":"v0.41.0"}"#,
            r#"[{"tagName":"v0.41.0"}]"#,
        ] {
            let err = choose(l).unwrap_err();
            assert!(err.contains("not the JSON expected"), "{l:?}: {err}");
        }
    }

    #[test]
    fn only_a_plain_v_major_minor_patch_tag_is_a_stable_version() {
        assert_eq!(stable_version("v0.41.0"), Some((0, 41, 0)));
        assert_eq!(stable_version("v1.2.30"), Some((1, 2, 30)));
        for t in [
            "0.41.0",
            "v0.41",
            "v0.41.0.1",
            "v0.41.0-rc.1",
            "v0.41.0+build",
            "v0..0",
            "v-1.0.0",
            "v",
        ] {
            assert_eq!(stable_version(t), None, "{t}");
        }
    }

    #[test]
    fn every_refusal_asks_for_an_explicit_previous_and_rules_out_a_fallback() {
        let msg = fail_closed("vfarcic/dot-agent-deck", "why");
        assert!(msg.contains("--previous <tag>"), "{msg}");
        assert!(
            msg.contains("does not fall back to a hardcoded tag"),
            "{msg}"
        );
    }

    #[test]
    fn an_explicit_previous_is_used_as_given_without_querying_anything() {
        let p = resolve(Some("v0.39.4"), false, "vfarcic/dot-agent-deck", || {
            panic!("an explicit --previous must not query the release listing")
        })
        .unwrap();
        assert_eq!(p.tag, "v0.39.4");
        assert_eq!(p.source, Source::Explicit);
        assert!(p.describe().contains("explicitly"));
        // --old-binary with an explicit tag is today's behaviour, unchanged.
        let p = resolve(Some("v0.39.4"), true, "r/r", || panic!()).unwrap();
        assert_eq!(p.source, Source::Explicit);
    }

    #[test]
    fn an_old_binary_without_an_explicit_previous_is_refused_rather_than_labelled() {
        let err = resolve(None, true, "vfarcic/dot-agent-deck", || {
            panic!("refused before any query")
        })
        .unwrap_err();
        assert!(
            err.contains("`--old-binary` needs an explicit `--previous`"),
            "{err}"
        );
    }

    #[test]
    fn a_resolved_tag_says_where_it_came_from() {
        let p = Previous {
            tag: "v0.41.0".into(),
            source: Source::Resolved {
                repo: "vfarcic/dot-agent-deck".into(),
                queried_at: "2026-09-21T00:00:00Z".into(),
            },
        };
        let d = p.describe();
        assert!(d.starts_with("resolved, not given"), "{d}");
        assert!(
            d.contains("`vfarcic/dot-agent-deck`") && d.contains("2026-09-21T00:00:00Z"),
            "{d}"
        );
        assert!(
            d.contains("marks Latest") && d.contains("gh release list"),
            "{d}"
        );
    }
}
