use std::fs;
use std::path::Path;

use dot_agent_deck::embedded_docs::PAGES;

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }

    let last = needle.len() - 1;
    let mut shifts = [needle.len(); 256];
    for (index, &byte) in needle[..last].iter().enumerate() {
        shifts[usize::from(byte)] = last - index;
    }

    let mut offset = 0;
    while offset <= haystack.len() - needle.len() {
        let window = &haystack[offset..offset + needle.len()];
        if window[last] == needle[last] && window == needle {
            return true;
        }
        offset += shifts[usize::from(window[last])];
    }
    false
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
