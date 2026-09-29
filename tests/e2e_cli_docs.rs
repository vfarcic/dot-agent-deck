#![cfg(feature = "e2e")]

//! The version-matched documentation CLI, driven through the real binary.

mod common;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use regex::Regex;
use serde::Deserialize;
use spec::spec;

#[derive(Deserialize)]
struct Manifest {
    page: Vec<Page>,
}

#[derive(Deserialize)]
struct Page {
    slug: String,
    description: String,
}

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs")
}

fn manifest() -> Manifest {
    let path = docs_dir().join("published.toml");
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let manifest: Manifest = toml::from_str(&source)
        .unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()));
    assert!(
        !manifest.page.is_empty(),
        "{} must list published pages",
        path.display()
    );
    manifest
}

fn page_bytes(slug: &str) -> Vec<u8> {
    let path = docs_dir().join(format!("{slug}.md"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn run_docs(args: &[&str]) -> Output {
    let home = common::harness_tempdir().expect("create isolated CLI home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_dot-agent-deck"));
    command.args(args).env_clear().env("HOME", home.path());
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command.env("TERM", "xterm-256color");
    command.env("DOT_AGENT_DECK_SOCKET", home.path().join("hook.sock"));
    command.env(
        "DOT_AGENT_DECK_ATTACH_SOCKET",
        home.path().join("attach.sock"),
    );
    command.env("DOT_AGENT_DECK_STATE_DIR", home.path().join("state"));
    command.env("DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS", "10");
    command.stdin(Stdio::null());
    command.output().expect("spawn dot-agent-deck")
}

fn output_text(output: &Output) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Scenario: Ask the installed binary to list its documentation topics. Each
/// published manifest slug and description appears on its own line in order.
#[spec("cli/docs/001")]
#[test]
fn docs_001_lists_manifest_topics_and_descriptions() {
    let manifest = manifest();
    let output = run_docs(&["docs"]);
    assert!(
        output.status.success(),
        "`dot-agent-deck docs` exited {:?}; {}",
        output.status.code(),
        output_text(&output)
    );
    let stdout = String::from_utf8(output.stdout).expect("docs list is UTF-8");
    let lines: Vec<_> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        manifest.page.len(),
        "expected one line per manifest page; stdout:\n{stdout}"
    );
    for (line, page) in lines.iter().zip(&manifest.page) {
        assert!(
            line.contains(&page.slug) && line.contains(&page.description),
            "expected slug {:?} and description {:?} on one line; got {line:?}",
            page.slug,
            page.description
        );
    }
}

/// Scenario: Request both a top-level and a nested documentation topic from
/// the binary. Each output starts with one link-resolution preamble line and
/// then reproduces the source Markdown bytes exactly.
#[spec("cli/docs/002")]
#[test]
fn docs_002_prints_top_level_and_nested_pages_unmodified() {
    for slug in ["orchestration", "desktop/voice"] {
        let output = run_docs(&["docs", slug]);
        assert!(
            output.status.success(),
            "`dot-agent-deck docs {slug}` exited {:?}; {}",
            output.status.code(),
            output_text(&output)
        );
        let Some(newline) = output.stdout.iter().position(|byte| *byte == b'\n') else {
            panic!(
                "`docs {slug}` has no one-line preamble; {}",
                output_text(&output)
            );
        };
        let preamble = std::str::from_utf8(&output.stdout[..newline]).expect("UTF-8 preamble");
        assert!(
            preamble.contains("dot-agent-deck docs"),
            "`docs {slug}` preamble must explain how to follow docs links: {preamble:?}"
        );
        assert_eq!(
            &output.stdout[newline + 1..],
            page_bytes(slug),
            "`docs {slug}` must print the source Markdown unmodified after the preamble"
        );
    }
}

/// Scenario: Request a topic absent from the manifest. The command fails and
/// its diagnostic gives the reader every valid topic to choose from.
#[spec("cli/docs/003")]
#[test]
fn docs_003_unknown_topic_lists_valid_topics() {
    let manifest = manifest();
    let output = run_docs(&["docs", "not-a-published-topic"]);
    assert!(
        !output.status.success(),
        "unknown topic unexpectedly succeeded; {}",
        output_text(&output)
    );
    let text = output_text(&output);
    for page in &manifest.page {
        assert!(
            text.contains(&page.slug),
            "unknown-topic diagnostic omitted valid topic {:?}; {text}",
            page.slug
        );
    }
}

/// Scenario: Request the complete documentation corpus through `docs --all`.
/// Every published page's original Markdown appears in manifest order.
#[spec("cli/docs/004")]
#[test]
fn docs_004_all_prints_pages_in_manifest_order() {
    let manifest = manifest();
    let output = run_docs(&["docs", "--all"]);
    assert!(
        output.status.success(),
        "`dot-agent-deck docs --all` exited {:?}; {}",
        output.status.code(),
        output_text(&output)
    );
    let mut remaining = output.stdout.as_slice();
    for page in &manifest.page {
        let content = page_bytes(&page.slug);
        assert!(!content.is_empty(), "{} has empty Markdown", page.slug);
        let Some(start) = remaining
            .windows(content.len())
            .position(|window| window == content)
        else {
            panic!(
                "`docs --all` omitted or reordered the complete Markdown for {:?}",
                page.slug
            );
        };
        remaining = &remaining[start + content.len()..];
    }
}

fn github_heading_slug(heading: &str) -> String {
    let mut slug = String::new();
    for character in heading.to_lowercase().chars() {
        if character.is_alphanumeric() || character == '-' || character == '_' {
            slug.push(character);
        } else if character.is_whitespace() {
            slug.push('-');
        }
    }
    slug
}

fn heading_anchors(markdown: &str) -> HashSet<String> {
    let mut anchors = HashSet::new();
    let mut duplicates: HashMap<String, usize> = HashMap::new();
    let mut in_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let Some(heading) = trimmed.strip_prefix('#') else {
            continue;
        };
        let heading = heading.trim_start_matches('#');
        if !heading.starts_with(char::is_whitespace) {
            continue;
        }
        let base = github_heading_slug(heading.trim().trim_end_matches('#').trim());
        let count = duplicates.entry(base.clone()).or_insert(0);
        let slug = if *count == 0 {
            base
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        anchors.insert(slug);
    }
    anchors
}

fn resolved_slug(source: &str, relative: &str) -> String {
    let parent = Path::new(source).parent().unwrap_or_else(|| Path::new(""));
    let mut parts = Vec::new();
    for component in parent.join(relative).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                assert!(
                    parts.pop().is_some(),
                    "link from {source} escapes docs/: {relative}"
                );
            }
            Component::Normal(segment) => parts.push(segment.to_string_lossy().into_owned()),
            _ => panic!("link from {source} is not relative: {relative}"),
        }
    }
    parts.join("/").trim_end_matches(".md").to_string()
}

/// Scenario: Read every published Markdown page and follow its relative page
/// links and heading anchors as a reader of the CLI output would. Each target
/// remains in the manifest and each anchor names a GitHub-style heading.
#[spec("cli/docs/005")]
#[test]
fn docs_005_relative_markdown_links_resolve_to_published_headings() {
    let manifest = manifest();
    let published: HashSet<_> = manifest
        .page
        .iter()
        .map(|page| page.slug.as_str())
        .collect();
    let link = Regex::new(r"\]\(([^)\s]+)(?:\s+[^)]*)?\)").expect("valid link regex");
    let mut pages = HashMap::new();
    for page in &manifest.page {
        let markdown = String::from_utf8(page_bytes(&page.slug))
            .unwrap_or_else(|error| panic!("{} is not UTF-8: {error}", page.slug));
        pages.insert(page.slug.as_str(), markdown);
    }
    for page in &manifest.page {
        let markdown = &pages[page.slug.as_str()];
        let mut in_fence = false;
        for (line_number, line_text) in markdown.lines().enumerate() {
            let trimmed = line_text.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
            for capture in link.captures_iter(line_text) {
                let destination = &capture[1];
                let (path, anchor) = destination.split_once('#').unwrap_or((destination, ""));
                if !(path.ends_with(".md") || path.is_empty() && !anchor.is_empty()) {
                    continue;
                }
                let target = if path.is_empty() {
                    page.slug.clone()
                } else {
                    resolved_slug(&page.slug, path)
                };
                assert!(
                    published.contains(target.as_str()),
                    "{}:{} link {:?} resolves to unpublished topic {:?}",
                    page.slug,
                    line_number + 1,
                    destination,
                    target
                );
                if !anchor.is_empty() {
                    let target_markdown = &pages[target.as_str()];
                    assert!(
                        heading_anchors(target_markdown).contains(anchor),
                        "{}:{} link {:?} has no heading #{anchor} in {:?}",
                        page.slug,
                        line_number + 1,
                        destination,
                        target
                    );
                }
            }
        }
    }
}
