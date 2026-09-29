use std::fs;
use std::path::Path;

use dot_agent_deck::embedded_docs::PAGES;

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Scenario: Inspect every topic embedded in the compiled binary and every
/// maintainer Markdown page on disk. No topic or embedded body may expose a
/// maintainer page, even through a different published slug.
#[test]
fn embedded_topics_exclude_develop_page_content() {
    let docs = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
    let mut pending = vec![docs.join("develop")];
    let mut checked = 0;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "md") {
                continue;
            }
            let body = fs::read(&path).unwrap();
            checked += 1;
            for page in PAGES {
                assert!(
                    !page.slug.starts_with("develop/"),
                    "embedded maintainer topic {}",
                    page.slug
                );
                assert!(
                    !contains_bytes(page.markdown.as_bytes(), &body),
                    "embedded topic {} contains all bytes of {}",
                    page.slug,
                    path.display()
                );
            }
        }
    }
    assert!(checked > 0, "expected maintainer pages under docs/develop/");
}
