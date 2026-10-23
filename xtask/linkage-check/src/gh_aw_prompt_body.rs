//! Issue #1774: a gh-aw prompt body must not contain the API proxy's
//! credit-exceeded error text.
//!
//! gh-aw's conclusion job scans the tail of the agent's stdio log for
//! `/maximum ai credits exceeded/i` (`MAX_AI_CREDITS_EXCEEDED_STDIO_RE` in
//! gh-aw's `actions/setup/js/ai_credits_context.cjs`, v0.86.2) and, on a match,
//! files an "exceeded max AI credits" failure. `pr-review.md` used to quote
//! that error in its prompt, so a review whose agent echoed its instructions
//! was reported as a credit failure although it finished inside its budget
//! (#1131, #1219, #1613).
//!
//! Only the body reaches the agent: each lock imports its source with
//! `{{#runtime-import …}}`, and gh-aw's `runtime_import.cjs` strips a workflow
//! file's frontmatter first. So the frontmatter's comments may quote the error
//! (pr-review.md's does, to explain all this), and the body may not.

use std::path::{Path, PathBuf};

/// The classifier's pattern, lowercased. The check below collapses whitespace
/// first, so it is a little stricter than the regex, which wants single spaces.
const CREDIT_ERROR_PHRASE: &str = "maximum ai credits exceeded";

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

/// What the runtime import keeps: everything after the second `---` line when
/// the file opens with frontmatter, else the whole file. Mirrors the stripping
/// loop in gh-aw's `runtime_import.cjs`.
fn prompt_body(text: &str) -> &str {
    if !text.trim_start().starts_with("---\n") && !text.trim_start().starts_with("---\r\n") {
        return text;
    }
    let mut delimiters = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        offset += line.len();
        if line.trim() == "---" {
            delimiters += 1;
            if delimiters == 2 {
                return &text[offset..];
            }
        }
    }
    // An unterminated frontmatter block imports as nothing.
    ""
}

/// 1-based line numbers, counted within the body, that carry the phrase.
fn offending_lines(body: &str) -> Vec<usize> {
    body.lines()
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
fn no_gh_aw_prompt_body_quotes_the_credit_exceeded_error() {
    let sources = gh_aw_sources();
    assert!(
        sources.iter().any(|p| p.ends_with("pr-review.md")),
        "pr-review.md is no longer a gh-aw source under .github/workflows — has the layout moved?"
    );

    let mut problems = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        for line in offending_lines(prompt_body(&text)) {
            problems.push(format!("{} (body line {line})", path.display()));
        }
    }

    assert!(
        problems.is_empty(),
        "a gh-aw prompt body quotes {CREDIT_ERROR_PHRASE:?}, which gh-aw's conclusion job reads \
         back from the agent's output as a credit failure (issue #1774). Describe the limit in \
         other words; a frontmatter comment may quote it:\n  {}",
        problems.join("\n  ")
    );
}

/// The check on constructed input, so the test above passing on clean files
/// says something about whether it would catch the error coming back.
#[cfg(test)]
mod synthetic {
    use super::{offending_lines, prompt_body};

    #[test]
    fn the_phrase_in_the_body_is_caught_in_any_case_and_spacing() {
        let text = "---\non: push\n---\nfine\nreturns `403 MAXIMUM AI  credits\texceeded`\n";
        assert_eq!(offending_lines(prompt_body(text)), vec![2]);
    }

    #[test]
    fn the_phrase_in_a_frontmatter_comment_is_allowed() {
        let text =
            "---\n# died at `403 Maximum AI credits exceeded (1011 / 1000)`\non: push\n---\nbody\n";
        assert!(offending_lines(prompt_body(text)).is_empty());
    }

    #[test]
    fn a_file_without_frontmatter_is_all_body() {
        let text = "Maximum AI credits exceeded\n";
        assert_eq!(offending_lines(prompt_body(text)), vec![1]);
    }

    #[test]
    fn a_body_that_describes_the_limit_in_other_words_passes() {
        let text = "---\non: push\n---\nthe API proxy refuses the request with an HTTP 403\n";
        assert!(offending_lines(prompt_body(text)).is_empty());
    }
}
