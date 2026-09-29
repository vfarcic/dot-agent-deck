use super::published_docs::{self, Page};
use super::*;

use std::fs;

// ---------------------------------------------------------------------------
// Manifest parser
// ---------------------------------------------------------------------------

fn parse_err(text: &str) -> String {
    published_docs::parse(text)
        .expect_err("manifest should be rejected")
        .to_string()
}

fn entry(slug: &str) -> String {
    format!("[[page]]\nslug = \"{slug}\"\ntitle = \"T\"\ndescription = \"D\"\n")
}

#[test]
fn parser_returns_pages_in_manifest_order() {
    let text = format!(
        "{}{}{}",
        entry("zeta"),
        entry("desktop/index"),
        entry("alpha")
    );
    let pages = published_docs::parse(&text).unwrap();
    let slugs: Vec<_> = pages.iter().map(|p| p.slug.as_str()).collect();
    assert_eq!(slugs, ["zeta", "desktop/index", "alpha"]);
    assert_eq!(pages[1].file_name(), "desktop/index.md");
    assert_eq!(pages[0].title, "T");
    assert_eq!(pages[0].description, "D");
}

#[test]
fn parser_rejects_invalid_toml() {
    assert!(parse_err("[[page]\nslug = ").contains("not valid TOML"));
}

#[test]
fn parser_rejects_an_empty_manifest() {
    assert!(parse_err("").contains("no `[[page]]` entries"));
    assert!(parse_err("page = []").contains("no `[[page]]` entries"));
}

#[test]
fn parser_rejects_unknown_keys() {
    let top = format!("title = \"x\"\n{}", entry("a"));
    assert!(parse_err(&top).contains("unknown top-level key `title`"));
    let inner = format!("{}order = 1\n", entry("a"));
    assert!(parse_err(&inner).contains("page #1: unknown key `order`"));
}

#[test]
fn parser_rejects_page_that_is_not_an_array_of_tables() {
    assert!(parse_err("page = \"a\"").contains("array of tables"));
    assert!(parse_err("page = [\"a\"]").contains("page #1 is not a table"));
}

#[test]
fn parser_rejects_missing_empty_multiline_and_non_string_fields() {
    let missing = "[[page]]\nslug = \"a\"\ntitle = \"T\"\n";
    assert!(parse_err(missing).contains("page #1: missing `description`"));
    let empty = "[[page]]\nslug = \"a\"\ntitle = \"  \"\ndescription = \"D\"\n";
    assert!(parse_err(empty).contains("page #1: `title` is empty"));
    let multiline = "[[page]]\nslug = \"a\"\ntitle = \"T\"\ndescription = \"\"\"\none\ntwo\"\"\"\n";
    assert!(parse_err(multiline).contains("`description` must be a single line"));
    let padded = "[[page]]\nslug = \"a\"\ntitle = \" T\"\ndescription = \"D\"\n";
    assert!(parse_err(padded).contains("leading or trailing whitespace"));
    let number = "[[page]]\nslug = 3\ntitle = \"T\"\ndescription = \"D\"\n";
    assert!(parse_err(number).contains("`slug` must be a string"));
}

#[test]
fn parser_rejects_a_duplicate_slug() {
    let text = format!("{}{}{}", entry("a"), entry("b"), entry("a"));
    assert!(parse_err(&text).contains("page #3: slug `a` is already used by page #1"));
}

#[test]
fn parser_rejects_slugs_outside_the_grammar() {
    for slug in [
        "../secrets",
        "/abs",
        "page.md",
        "Upper",
        "a//b",
        "a/",
        "-lead",
        "a b",
        "desktop/../x",
    ] {
        let err = parse_err(&entry(slug));
        assert!(err.contains("is not a path of lowercase"), "{slug}: {err}");
    }
}

#[test]
fn parser_rejects_every_develop_slug() {
    for slug in ["develop", "develop/versioning", "develop/nested/page"] {
        let err = parse_err(&entry(slug));
        assert!(err.contains("never published"), "{slug}: {err}");
    }
    // A page whose name merely starts with "develop" is not under docs/develop/.
    assert!(published_docs::parse(&entry("developing-agents")).is_ok());
}

// ---------------------------------------------------------------------------
// Generator, against a synthetic docs tree
// ---------------------------------------------------------------------------

struct Fixture {
    _dir: tempfile::TempDir,
    config: SiteConfig,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        fs::create_dir_all(docs.join("img")).unwrap();
        fs::create_dir_all(docs.join("desktop")).unwrap();
        fs::create_dir_all(docs.join("develop")).unwrap();
        fs::write(docs.join("img/shot.png"), b"PNG-shot").unwrap();
        fs::write(docs.join("img/rel.png"), b"PNG-rel").unwrap();
        fs::write(
            docs.join("published.toml"),
            "[[page]]\nslug = \"start\"\ntitle = \"Start\"\ndescription = \"Begin here.\"\n\
             [[page]]\nslug = \"desktop/index\"\ntitle = \"Desktop\"\ndescription = \"The app.\"\n",
        )
        .unwrap();
        fs::write(
            docs.join("start.md"),
            "---\ntitle: Start\n---\n\n# Start\n\n![a shot](/img/shot.png)\n\
             See [desktop](desktop/index.md).\n",
        )
        .unwrap();
        fs::write(
            docs.join("desktop/index.md"),
            "# Desktop\n\n```md\n# not a heading\n![not an image](missing.png)\n```\n\
             ![rel](../img/rel.png \"title\")\n",
        )
        .unwrap();
        // A maintainer page and one of its images: present on disk, never listed.
        fs::write(docs.join("develop/secret.md"), "# Secret\n").unwrap();
        fs::write(docs.join("develop/diagram.png"), b"PNG-dev").unwrap();
        let config = SiteConfig {
            docs_dir: docs,
            base_url: "https://example.test".to_string(),
        };
        Self { _dir: dir, config }
    }

    fn docs(&self) -> &Path {
        &self.config.docs_dir
    }
}

#[test]
fn generator_publishes_each_manifest_page_byte_for_byte() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    for slug in ["start", "desktop/index"] {
        let on_disk = fs::read(fx.docs().join(format!("{slug}.md"))).unwrap();
        assert_eq!(site.files[&format!("docs/{slug}.md")], on_disk, "{slug}");
    }
}

#[test]
fn llms_txt_lists_exactly_the_manifest_pages_in_order() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    let llms = site.text("llms.txt").unwrap();
    assert!(llms.starts_with("# dot-agent-deck\n\n> "), "{llms}");
    assert!(llms.contains("`dot-agent-deck docs"), "{llms}");
    assert!(
        llms.contains("https://example.test/llms-full.txt"),
        "{llms}"
    );
    let listed: Vec<&str> = llms.lines().filter(|l| l.starts_with("- [")).collect();
    assert_eq!(
        listed,
        [
            "- [Start](https://example.test/docs/start.md): Begin here.",
            "- [Desktop](https://example.test/docs/desktop/index.md): The app.",
        ]
    );
}

#[test]
fn llms_full_txt_concatenates_every_page_in_manifest_order() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    let full = site.text("llms-full.txt").unwrap();
    let start_marker = "<!-- page: start https://example.test/docs/start.md -->";
    let desktop_marker = "<!-- page: desktop/index https://example.test/docs/desktop/index.md -->";
    let start_at = full.find(start_marker).expect("start marker");
    let desktop_at = full.find(desktop_marker).expect("desktop marker");
    assert!(start_at < desktop_at);
    let start_body = fs::read_to_string(fx.docs().join("start.md")).unwrap();
    let desktop_body = fs::read_to_string(fx.docs().join("desktop/index.md")).unwrap();
    assert!(full[start_at..desktop_at].contains(&start_body));
    assert!(full[desktop_at..].ends_with(&desktop_body));
    let markers = full
        .lines()
        .filter(|l| l.starts_with("<!-- page: "))
        .count();
    assert_eq!(markers, 2);
}

#[test]
fn nothing_from_develop_reaches_the_output() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    for path in site.files.keys() {
        assert!(!path.contains("develop"), "published {path}");
    }
    for (path, bytes) in &site.files {
        assert!(
            !bytes.windows(b"PNG-dev".len()).any(|w| w == b"PNG-dev"),
            "{path} carries a docs/develop/ image"
        );
    }
    for file in ["llms.txt", "llms-full.txt"] {
        let text = site.text(file).unwrap();
        assert!(
            !text.contains("develop"),
            "{file} mentions develop:\n{text}"
        );
        assert!(!text.contains("Secret"), "{file} carries the develop page");
    }
}

#[test]
fn images_are_published_at_both_image_locations() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    // The whole image directory, where Docusaurus served it.
    assert_eq!(site.files["img/shot.png"], b"PNG-shot");
    assert_eq!(site.files["img/rel.png"], b"PNG-rel");
    // A relative reference resolves beside the published Markdown, as on disk.
    assert_eq!(site.files["docs/img/rel.png"], b"PNG-rel");
    // A root-absolute reference is served from /img/ only.
    assert!(!site.files.contains_key("docs/img/shot.png"));
}

#[test]
fn generator_rejects_a_manifest_page_with_no_file() {
    let fx = Fixture::new();
    fs::remove_file(fx.docs().join("start.md")).unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("page `start` is in the manifest"), "{err}");
}

#[test]
fn generator_rejects_a_title_that_differs_from_the_first_heading() {
    let fx = Fixture::new();
    fs::write(fx.docs().join("start.md"), "# Starting\n").unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(
        err.contains("does not match its first heading `Starting`"),
        "{err}"
    );
}

#[test]
fn generator_rejects_a_missing_or_escaping_image() {
    let fx = Fixture::new();
    fs::write(fx.docs().join("start.md"), "# Start\n![x](/img/gone.png)\n").unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(
        err.contains("image `/img/gone.png` does not exist"),
        "{err}"
    );

    fs::write(
        fx.docs().join("start.md"),
        "# Start\n![x](develop/diagram.png)\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("is under docs/develop/"), "{err}");

    fs::write(
        fx.docs().join("start.md"),
        "# Start\n![x](../../etc/x.png)\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("points outside docs/"), "{err}");
}

#[test]
fn write_refuses_a_non_empty_output_directory() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    let out = tempfile::tempdir().unwrap();
    fs::write(out.path().join("stale.html"), "old").unwrap();
    let err = write(&site, out.path()).unwrap_err();
    assert!(err.contains("is not empty"), "{err}");

    let fresh = out.path().join("site");
    let count = write(&site, &fresh).unwrap();
    assert_eq!(count, site.files.len());
    assert_eq!(
        fs::read(fresh.join("docs/desktop/index.md")).unwrap(),
        site.files["docs/desktop/index.md"]
    );
    assert!(fresh.join("llms.txt").is_file());
}

#[test]
fn insert_refuses_a_develop_path() {
    let mut site = Site::default();
    let err = site
        .insert("docs/develop/x.md".to_string(), Vec::new())
        .unwrap_err();
    assert!(err.contains("refusing"), "{err}");
    assert!(site.insert("docs/../x".to_string(), Vec::new()).is_err());
}

// ---------------------------------------------------------------------------
// The real checkout
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `docs/**/*.md` outside `docs/develop/`, as slugs.
fn user_pages_on_disk(docs: &Path) -> Vec<String> {
    fn walk(dir: &Path, docs: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(docs).unwrap();
            if rel == Path::new(UNPUBLISHED_DIR) {
                continue;
            }
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                walk(&path, docs, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                let slug = rel.with_extension("");
                out.push(slug.to_str().unwrap().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(docs, docs, &mut out);
    out.sort();
    out
}

#[test]
fn the_real_manifest_lists_every_user_page_and_nothing_else() {
    let docs = workspace_root().join("docs");
    let pages: Vec<Page> = published_docs::read(&docs).unwrap();
    let mut listed: Vec<String> = pages.iter().map(|p| p.slug.clone()).collect();
    listed.sort();
    assert_eq!(listed, user_pages_on_disk(&docs));
}

#[test]
fn the_real_site_builds_without_anything_from_develop() {
    let config = SiteConfig::from_workspace(&workspace_root());
    let site = build(&config).unwrap();
    let pages = published_docs::read(&config.docs_dir).unwrap();
    for page in &pages {
        assert!(
            site.files
                .contains_key(&format!("docs/{}", page.file_name()))
        );
    }
    let md: Vec<&String> = site.files.keys().filter(|p| p.ends_with(".md")).collect();
    assert_eq!(md.len(), pages.len(), "{md:?}");
    assert!(site.files.keys().all(|p| !p.starts_with("docs/develop")));
    let llms = site.text("llms.txt").unwrap();
    assert_eq!(
        llms.lines().filter(|l| l.starts_with("- [")).count(),
        pages.len()
    );
    assert!(!llms.contains("/docs/develop/"));
    assert!(
        !site
            .text("llms-full.txt")
            .unwrap()
            .contains("/docs/develop/")
    );
}

/// Every legacy page URL maps to a manifest page, so a redirect generated from
/// the manifest can cover it. (`/docs` itself was never a page.)
#[test]
fn every_legacy_docusaurus_page_url_has_a_manifest_page() {
    let fixture = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/docusaurus-urls.txt"),
    )
    .unwrap();
    let urls: Vec<&str> = fixture
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert!(urls.len() > 2, "fixture is empty");
    let pages = published_docs::read(&workspace_root().join("docs")).unwrap();
    for url in urls {
        let Some(route) = url.strip_prefix("/docs/") else {
            assert_eq!(url, "/docs", "not a /docs URL");
            continue;
        };
        let route = route.trim_end_matches('/');
        if route.is_empty() {
            continue;
        }
        let found = pages
            .iter()
            .any(|p| p.slug == route || p.slug == format!("{route}/index"));
        assert!(found, "legacy URL {url} has no manifest page");
    }
}
