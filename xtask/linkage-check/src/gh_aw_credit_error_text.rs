//! Issues #1774 and #1808: a gh-aw workflow source must not contain the API
//! proxy's credit-exceeded error text anywhere, frontmatter included.
//!
//! gh-aw's conclusion job scans the tail of the agent's stdio log with
//! `MAX_AI_CREDITS_EXCEEDED_STDIO_RE` (gh-aw's
//! `actions/setup/js/ai_credits_context.cjs`, v0.86.2), which matches that
//! error verbatim, and on a match files an "exceeded max AI credits" failure.
//! `pr-review.md` used to quote the error in its prompt body, so a review whose
//! agent echoed its instructions was reported as a credit failure although it
//! finished inside its budget (#1131, #1219, #1613).
//!
//! The frontmatter is not a safe place for the quote either. The runtime
//! import strips it before the prompt is built, but the agent can still read
//! the file, and a tool result lands in the same stdio log. Run 39721247817
//! (#1808) reviewed a pull request that touched `pr-review.md`, grepped the file
//! for the error, printed a frontmatter comment quoting it, and was reported as
//! over budget at 115 of 10000 credits.
//!
//! The phrase is assembled from two pieces below so that this file does not
//! carry it either: an agent reviewing a change here would read it too.

use std::path::{Path, PathBuf};

/// The classifier's pattern, lowercased. The check below collapses whitespace
/// first, so it is a little stricter than the regex, which wants single spaces.
const CREDIT_ERROR_PHRASE: &str = concat!("maximum ai credits", " exceeded");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

/// Every `.md` under `.github/workflows` that has a generated `.lock.yml`
/// beside it, which is what makes it a gh-aw source.
fn gh_aw_sources() -> Vec<PathBuf> {
    let dir = repo_root().join(".github/workflows");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .filter(|p| p.with_extension("lock.yml").exists())
        .collect();
    out.sort();
    out
}

/// 1-based line numbers that carry the phrase.
fn offending_lines(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            line.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase()
                .contains(CREDIT_ERROR_PHRASE)
        })
        .map(|(i, _)| i + 1)
        .collect()
}

#[test]
fn no_gh_aw_source_quotes_the_credit_exceeded_error() {
    let sources = gh_aw_sources();
    assert!(
        sources.iter().any(|p| p.ends_with("pr-review.md")),
        "pr-review.md is no longer a gh-aw source under .github/workflows — has the layout moved?"
    );

    let mut problems = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        for line in offending_lines(&text) {
            problems.push(format!("{}:{line}", path.display()));
        }
    }

    assert!(
        problems.is_empty(),
        "a gh-aw workflow source quotes {CREDIT_ERROR_PHRASE:?}, which gh-aw's conclusion job \
         reads back from the agent's output as a credit failure (issues #1774, #1808). Describe \
         the limit in other words, in the frontmatter comments as well as the body:\n  {}",
        problems.join("\n  ")
    );
}

/// The check on constructed input, so the test above passing on clean files
/// says something about whether it would catch the error coming back.
#[cfg(test)]
mod synthetic {
    use super::{CREDIT_ERROR_PHRASE, offending_lines};

    #[test]
    fn the_phrase_in_the_body_is_caught_in_any_case_and_spacing() {
        let text = "---\non: push\n---\nfine\nreturns `403 MAXIMUM AI  credits\tEXCEEDED`\n";
        assert_eq!(offending_lines(text), vec![5]);
    }

    #[test]
    fn the_phrase_in_a_frontmatter_comment_is_caught() {
        let text = format!(
            "---\n# died at `403 {CREDIT_ERROR_PHRASE} (1011 / 1000)`\non: push\n---\nbody\n"
        );
        assert_eq!(offending_lines(&text), vec![2]);
    }

    #[test]
    fn a_file_that_describes_the_limit_in_other_words_passes() {
        let text = "---\n# died on the proxy's 403 at 1011 of 1000 credits\non: push\n---\nthe API proxy refuses the request with an HTTP 403\n";
        assert!(offending_lines(text).is_empty());
    }
}
