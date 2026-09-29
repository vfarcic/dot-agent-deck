//! `dot-agent-deck docs [topic]` — the user docs for this build, embedded at
//! build time (PRD #1419, Decision 4).
//!
//! `build.rs` reads `docs/published.toml` through the shared
//! `src/published_docs.rs` parser and generates [`PAGES`], one entry per
//! manifest page in manifest order, each page's Markdown pulled in with
//! `include_str!`. So the binary prints the docs for its own version, needs no
//! network, and never contacts a daemon.
//!
//! The rendering functions take the page list as an argument so they can be
//! tested against synthetic pages; the CLI passes [`PAGES`].

use std::io::{self, Write};

/// One embedded published page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedPage {
    /// The page's path under `docs/` without `.md` — also its topic name.
    pub slug: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// The page's Markdown, byte for byte as it is on disk.
    pub markdown: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/published_docs.rs"));

/// The one line printed before a page, telling a reader of stdout (which has
/// no base path) how the page's relative links map onto topics.
pub fn link_preamble(slug: &str) -> String {
    let base = match slug.rsplit_once('/') {
        Some((dir, _)) => format!("`{dir}/`"),
        None => "the docs root".to_string(),
    };
    format!(
        "[dot-agent-deck docs {slug}] Relative links resolve as files under docs/ would: a link \
         to `path/page.md#heading` means `dot-agent-deck docs <topic>` at that heading, where \
         <topic> is the link's path resolved against this page's directory ({base}) without \
         `.md`; `dot-agent-deck docs` lists the topics."
    )
}

/// The topic list: one line per page, slug then description, in manifest order.
pub fn topic_list(pages: &[EmbeddedPage]) -> String {
    let width = pages.iter().map(|p| p.slug.len()).max().unwrap_or(0);
    let mut out = String::new();
    for page in pages {
        out.push_str(&format!(
            "{:<width$}  {}\n",
            page.slug,
            page.description,
            width = width
        ));
    }
    out
}

/// One page: the link preamble, then the Markdown unmodified.
pub fn render_topic(page: &EmbeddedPage) -> String {
    format!("{}\n{}", link_preamble(page.slug), page.markdown)
}

/// Every page in manifest order, each after a marker line naming its topic —
/// the CLI counterpart of the site's `llms-full.txt`.
pub fn render_all(pages: &[EmbeddedPage]) -> String {
    let mut out = String::from(
        "dot-agent-deck documentation, every page in reading order. Each page starts after a \
         line of the form `<!-- page: <topic> -->`; a relative link `path/page.md#heading` in a \
         page means `dot-agent-deck docs <topic>` at that heading, with the path resolved \
         against that page's directory as files under docs/ would.\n",
    );
    for page in pages {
        out.push_str(&format!("\n<!-- page: {} -->\n\n", page.slug));
        out.push_str(page.markdown);
        if !page.markdown.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The diagnostic for a topic that is not published: names it and lists every
/// valid topic. The topic is echoed in its Debug-quoted form, so an escape
/// sequence or control character in it reaches the terminal as text
/// (`"\u{1b}[31m"`) rather than as the byte itself.
pub fn unknown_topic(pages: &[EmbeddedPage], topic: &str) -> String {
    format!(
        "error: unknown docs topic {topic:?}. Valid topics:\n\n{}",
        topic_list(pages)
    )
}

/// What `dot-agent-deck docs` prints and how it exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocsOutput {
    /// Print this to stdout and exit 0.
    Stdout(String),
    /// Print this to stderr and exit non-zero.
    Error(String),
}

/// Resolve a `docs` invocation against `pages`. `all` and `topic` are
/// mutually exclusive at the CLI; `all` wins if both are set.
pub fn run(pages: &[EmbeddedPage], topic: Option<&str>, all: bool) -> DocsOutput {
    if all {
        return DocsOutput::Stdout(render_all(pages));
    }
    match topic {
        None => DocsOutput::Stdout(topic_list(pages)),
        Some(topic) => match pages.iter().find(|p| p.slug == topic) {
            Some(page) => DocsOutput::Stdout(render_topic(page)),
            None => DocsOutput::Error(unknown_topic(pages, topic)),
        },
    }
}

/// Write `text` to `out`, treating a closed pipe (`docs --all | head`) as a
/// normal end of output rather than an error.
pub fn write_output(mut out: impl Write, text: &str) -> io::Result<()> {
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOP: EmbeddedPage = EmbeddedPage {
        slug: "orchestration",
        title: "Orchestration",
        description: "Run a team of agents.",
        markdown: "# Orchestration\n\nSee [voice](desktop/voice.md#setup).\n",
    };
    const NESTED: EmbeddedPage = EmbeddedPage {
        slug: "desktop/voice",
        title: "Voice",
        description: "Talk to agents.",
        markdown: "# Voice\n\n## Setup\n\nno trailing newline",
    };
    const PAGES_FIXTURE: [EmbeddedPage; 2] = [TOP, NESTED];

    #[test]
    fn embedded_pages_follow_the_manifest() {
        let manifest =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/published.toml"))
                .expect("read docs/published.toml");
        let table: toml::Table = manifest.parse().expect("manifest parses");
        let slugs: Vec<&str> = table["page"]
            .as_array()
            .expect("[[page]] array")
            .iter()
            .map(|p| p["slug"].as_str().expect("slug"))
            .collect();
        let embedded: Vec<&str> = PAGES.iter().map(|p| p.slug).collect();
        assert_eq!(
            embedded, slugs,
            "embedded topics must be the manifest, in order"
        );
        for page in PAGES {
            assert!(
                !page.slug.starts_with("develop/"),
                "{} is unpublished",
                page.slug
            );
            assert!(!page.markdown.is_empty(), "{} embedded empty", page.slug);
        }
    }

    #[test]
    fn preamble_is_one_line_naming_the_page_directory() {
        let top = link_preamble("orchestration");
        let nested = link_preamble("desktop/voice");
        assert!(!top.contains('\n') && !nested.contains('\n'));
        assert!(top.contains("dot-agent-deck docs orchestration"));
        assert!(top.contains("the docs root"));
        assert!(nested.contains("`desktop/`"));
    }

    #[test]
    fn topic_list_aligns_descriptions_in_manifest_order() {
        let list = topic_list(&PAGES_FIXTURE);
        assert_eq!(
            list,
            "orchestration  Run a team of agents.\n\
             desktop/voice  Talk to agents.\n"
        );
    }

    #[test]
    fn topic_prints_preamble_then_markdown_unmodified() {
        let DocsOutput::Stdout(out) = run(&PAGES_FIXTURE, Some("desktop/voice"), false) else {
            panic!("known topic must succeed");
        };
        let (first, rest) = out.split_once('\n').expect("preamble line");
        assert_eq!(first, link_preamble("desktop/voice"));
        assert_eq!(rest, NESTED.markdown);
    }

    #[test]
    fn unknown_topic_is_an_error_listing_every_topic() {
        let DocsOutput::Error(err) = run(&PAGES_FIXTURE, Some("nope"), false) else {
            panic!("unknown topic must fail");
        };
        assert!(err.contains("\"nope\""));
        assert!(err.contains("orchestration") && err.contains("desktop/voice"));
    }

    #[test]
    fn unknown_topic_echoes_no_control_bytes() {
        let DocsOutput::Error(err) = run(&PAGES_FIXTURE, Some("a\x1b[31mb\rc\nd"), false) else {
            panic!("unknown topic must fail");
        };
        let first_line = err.lines().next().expect("diagnostic line");
        assert_eq!(
            first_line,
            r#"error: unknown docs topic "a\u{1b}[31mb\rc\nd". Valid topics:"#
        );
        let echoed = err
            .split("\n\n")
            .next()
            .expect("diagnostic before the list");
        assert!(
            !echoed.contains(['\x1b', '\r', '\n']),
            "raw control byte in {echoed:?}"
        );
    }

    #[test]
    fn a_develop_path_is_not_a_topic() {
        assert!(matches!(
            run(&PAGES_FIXTURE, Some("develop/versioning"), false),
            DocsOutput::Error(_)
        ));
    }

    #[test]
    fn all_prints_every_page_in_order_with_markers_and_terminated_lines() {
        let DocsOutput::Stdout(out) = run(&PAGES_FIXTURE, None, true) else {
            panic!("--all must succeed");
        };
        let top = out.find(TOP.markdown).expect("top page present");
        let nested = out.find(NESTED.markdown).expect("nested page present");
        assert!(top < nested, "manifest order");
        assert!(out.contains("<!-- page: orchestration -->"));
        assert!(out.contains("<!-- page: desktop/voice -->"));
        assert!(out.ends_with("no trailing newline\n"));
    }

    #[test]
    fn no_topic_lists_topics() {
        assert_eq!(
            run(&PAGES_FIXTURE, None, false),
            DocsOutput::Stdout(topic_list(&PAGES_FIXTURE))
        );
    }

    struct ClosedPipe;
    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_ends_output_quietly() {
        assert!(write_output(ClosedPipe, "text").is_ok());
    }
}
