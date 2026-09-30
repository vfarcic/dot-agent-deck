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
    // The landing-page prompt does not name the subcommand, so llms.txt is
    // where an agent sent there learns of it, older-release caveat included.
    assert!(llms.contains("`dot-agent-deck docs"), "{llms}");
    assert!(
        llms.contains("reports an unrecognized subcommand"),
        "{llms}"
    );
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

/// Scenario: `docs/img` is repointed from `site/static/img` at another
/// directory beside `docs/` (a `prds/` holding a PRD). The build fails naming
/// the allowed target, and nothing from `prds/` is published.
#[cfg(unix)]
#[test]
fn generator_rejects_an_image_dir_pointing_at_another_sibling_directory() {
    use std::os::unix::fs::symlink;

    let fx = Fixture::new();
    let root = fx.docs().parent().unwrap().to_path_buf();
    fs::create_dir_all(root.join("site/static")).unwrap();
    fs::rename(fx.docs().join("img"), root.join("site/static/img")).unwrap();
    symlink("../site/static/img", fx.docs().join("img")).unwrap();
    // The repository layout builds.
    let site = build(&fx.config).unwrap();
    assert_eq!(site.files["img/shot.png"], b"PNG-shot");

    fs::create_dir_all(root.join("prds")).unwrap();
    fs::write(root.join("prds/1-plan.md"), "# Private plan\n").unwrap();
    fs::write(root.join("prds/shot.png"), b"PNG-shot").unwrap();
    fs::write(root.join("prds/rel.png"), b"PNG-rel").unwrap();
    fs::remove_file(fx.docs().join("img")).unwrap();
    symlink("../prds", fx.docs().join("img")).unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("site/static/img"), "{err}");
}

/// Scenario: `docs/img` keeps its normal `../site/static/img` symlink, but
/// `site/static/img` is itself a symlink into `docs/develop/` (which holds a
/// `secret.png`), then to a directory outside the checkout, and then
/// `site/static` is a symlink. Each build fails and publishes nothing.
#[cfg(unix)]
#[test]
fn generator_rejects_a_symlinked_site_static_img() {
    use std::os::unix::fs::symlink;

    let outside = tempfile::tempdir().unwrap();
    let fx = Fixture::new();
    let root = fx.docs().parent().unwrap().to_path_buf();
    fs::create_dir_all(root.join("site/static")).unwrap();
    fs::rename(fx.docs().join("img"), root.join("site/static/img")).unwrap();
    symlink("../site/static/img", fx.docs().join("img")).unwrap();
    assert!(build(&fx.config).is_ok());
    let real_img = outside.path().join("real-img");
    fs::rename(root.join("site/static/img"), &real_img).unwrap();

    fs::create_dir_all(fx.docs().join("develop")).unwrap();
    fs::write(fx.docs().join("develop/secret.png"), b"PNG-secret").unwrap();
    // The pages' own image references resolve there too, so the refusal is
    // reached whichever image the build looks at first.
    fs::write(fx.docs().join("develop/shot.png"), b"PNG-secret").unwrap();
    fs::write(fx.docs().join("develop/rel.png"), b"PNG-secret").unwrap();
    symlink("../../docs/develop", root.join("site/static/img")).unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("docs/develop/"), "{err}");

    fs::remove_file(root.join("site/static/img")).unwrap();
    symlink(&real_img, root.join("site/static/img")).unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("site/static/img"), "{err}");

    fs::remove_file(root.join("site/static/img")).unwrap();
    fs::create_dir_all(outside.path().join("static")).unwrap();
    fs::rename(&real_img, outside.path().join("static/img")).unwrap();
    fs::remove_dir_all(root.join("site/static")).unwrap();
    symlink(outside.path().join("static"), root.join("site/static")).unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("site/static/img"), "{err}");
}

/// Scenario: A non-image file (a Markdown note) sits in the image directory.
/// The build fails naming the file instead of publishing it at `/img/`.
#[test]
fn generator_refuses_a_non_image_file_in_the_image_dir() {
    let fx = Fixture::new();
    fs::write(fx.docs().join("img/notes.md"), "# Notes\n").unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("notes.md"), "{err}");
    assert!(err.contains("image extension"), "{err}");
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
    // Every goal in the "start here" list names a real page, so none is
    // dropped from llms.txt.
    for (goal, slug) in START_BY_GOAL {
        assert!(
            pages.iter().any(|p| p.slug == *slug),
            "START_BY_GOAL `{goal}` names `{slug}`, which is not in the manifest"
        );
        assert!(llms.contains(&format!("- {goal}: read [")), "{goal}");
    }
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

// ---------------------------------------------------------------------------
// Redirects and content types
// ---------------------------------------------------------------------------

fn fixture_urls() -> Vec<String> {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/docusaurus-urls.txt"))
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[test]
fn redirects_cover_every_route_form_of_every_page() {
    let fx = Fixture::new();
    let pages = published_docs::read(fx.docs()).unwrap();
    let table = redirects(&pages);
    let get = |from: &str| {
        table
            .iter()
            .find(|r| r.from == from)
            .map(|r| r.to.as_str())
            .unwrap_or_else(|| panic!("no redirect from {from}"))
    };
    for from in ["/docs/start", "/docs/start/", "/docs/start/index.html"] {
        assert_eq!(get(from), "/docs/start.md");
    }
    // `desktop/index.md` was the category index at /docs/desktop/.
    for from in [
        "/docs/desktop",
        "/docs/desktop/",
        "/docs/desktop/index.html",
    ] {
        assert_eq!(get(from), "/docs/desktop/index.md");
    }
    assert_eq!(get("/docs"), DOCS_INDEX);
    assert_eq!(get("/docs/"), DOCS_INDEX);
    // The fixture has no `configuration` page, so the retired page's
    // successor is absent and it gets no redirect rather than a broken one.
    assert!(table.iter().all(|r| !r.from.contains("workspace-modes")));
    let froms: BTreeSet<&str> = table.iter().map(|r| r.from.as_str()).collect();
    assert_eq!(froms.len(), table.len(), "duplicate redirect source");
}

#[test]
fn both_deploy_targets_carry_the_same_redirect_table() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    let pages = published_docs::read(fx.docs()).unwrap();
    let table = redirects(&pages);
    let netlify = site.text("_redirects").unwrap();
    for r in &table {
        assert!(
            netlify
                .lines()
                .any(|l| l == format!("{} {} 301!", r.from, r.to)),
            "_redirects lacks {r:?}:\n{netlify}"
        );
        assert!(
            site.nginx_redirects
                .lines()
                .any(|l| l == format!("location = {} {{ return 301 {}; }}", r.from, r.to)),
            "nginx include lacks {r:?}:\n{}",
            site.nginx_redirects
        );
    }
    assert!(!site.files.keys().any(|k| k.contains("nginx")));
}

#[test]
fn netlify_headers_serve_markdown_and_llms_files_readable() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    let headers = site.text("_headers").unwrap();
    for path in ["/docs/start.md", "/docs/desktop/index.md"] {
        assert!(
            headers.contains(&format!(
                "{path}\n  Content-Type: {MARKDOWN_CONTENT_TYPE}\n"
            )),
            "{headers}"
        );
    }
    for path in ["/llms.txt", "/llms-full.txt", "/robots.txt"] {
        assert!(
            headers.contains(&format!("{path}\n  Content-Type: {LLMS_CONTENT_TYPE}\n")),
            "{headers}"
        );
    }
    assert!(
        headers.contains(&format!(
            "/sitemap.xml\n  Content-Type: {SITEMAP_CONTENT_TYPE}\n"
        )),
        "{headers}"
    );
}

/// The `<loc>` values of a generated sitemap, in order.
fn sitemap_locs(sitemap: &str) -> Vec<&str> {
    sitemap
        .split("<loc>")
        .skip(1)
        .map(|rest| &rest[..rest.find("</loc>").expect("unterminated <loc>")])
        .collect()
}

#[test]
fn sitemap_lists_the_landing_page_and_exactly_the_manifest_pages() {
    let fx = Fixture::new();
    // Without a landing page, the sitemap holds the pages alone.
    let site = build(&fx.config).unwrap();
    let sitemap = site.text("sitemap.xml").unwrap();
    assert!(
        sitemap.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<urlset "),
        "{sitemap}"
    );
    assert_eq!(
        sitemap_locs(sitemap),
        [
            "https://example.test/docs/start.md",
            "https://example.test/docs/desktop/index.md",
        ]
    );
    assert!(!sitemap.contains("develop"), "{sitemap}");

    with_landing(&fx, "<a href=\"{{doc:start}}\">s</a>\n");
    let site = build(&fx.config).unwrap();
    assert_eq!(
        sitemap_locs(site.text("sitemap.xml").unwrap()),
        [
            "https://example.test/",
            "https://example.test/docs/start.md",
            "https://example.test/docs/desktop/index.md",
        ]
    );
}

#[test]
fn robots_txt_allows_everything_and_points_at_the_sitemap() {
    let fx = Fixture::new();
    let site = build(&fx.config).unwrap();
    assert_eq!(
        site.text("robots.txt").unwrap(),
        "User-agent: *\nAllow: /\n\nSitemap: https://example.test/sitemap.xml\n"
    );
}

/// The real sitemap: the landing page plus one URL per manifest page, each a
/// file the site publishes, and nothing from `docs/develop/`.
#[test]
fn the_real_sitemap_lists_the_landing_page_and_every_published_page() {
    let config = SiteConfig::from_workspace(&workspace_root());
    let site = build(&config).unwrap();
    let pages = published_docs::read(&config.docs_dir).unwrap();
    let mut expected = vec![format!("{DEFAULT_BASE_URL}/")];
    expected.extend(
        pages
            .iter()
            .map(|p| format!("{DEFAULT_BASE_URL}/docs/{}", p.file_name())),
    );
    let sitemap = site.text("sitemap.xml").unwrap();
    assert_eq!(sitemap_locs(sitemap), expected);
    for loc in sitemap_locs(sitemap) {
        let path = loc.strip_prefix(DEFAULT_BASE_URL).unwrap();
        let file = if path == "/" {
            "index.html"
        } else {
            &path[1..]
        };
        assert!(site.files.contains_key(file), "{loc} is not published");
    }
    assert!(!sitemap.contains("/docs/develop"), "{sitemap}");
    assert!(
        site.text("robots.txt")
            .unwrap()
            .contains(&format!("\nSitemap: {DEFAULT_BASE_URL}/sitemap.xml\n"))
    );
}

/// The redirect table, checked against the two lists PRD #1419 names: the URLs
/// the last Docusaurus build served, and every `agent-deck.devopstoolkit.ai/docs/…`
/// URL in the repository. Each must redirect to a page the site publishes, or
/// already be a file it publishes.
#[test]
fn the_real_redirects_cover_the_legacy_urls_and_every_url_in_the_repository() {
    let root = workspace_root();
    let config = SiteConfig::from_workspace(&root);
    let site = build(&config).unwrap();
    let pages = published_docs::read(&config.docs_dir).unwrap();
    let table = redirects(&pages);
    let resolves = |path: &str| -> Result<(), String> {
        if let Some(r) = table.iter().find(|r| r.from == path) {
            let target = r.to.trim_start_matches('/');
            return if site.files.contains_key(target) {
                Ok(())
            } else {
                Err(format!(
                    "{path} redirects to {}, which is not published",
                    r.to
                ))
            };
        }
        if site.files.contains_key(path.trim_start_matches('/')) {
            Ok(())
        } else {
            Err(format!("{path} neither redirects nor is published"))
        }
    };

    let mut errors = Vec::new();
    let legacy = fixture_urls();
    assert!(legacy.len() > 40, "fixture looks truncated");
    for url in &legacy {
        if let Err(e) = resolves(url) {
            errors.push(format!("fixture: {e}"));
        }
    }
    // `/docs/workspace-modes` was already a 404 before Docusaurus went, and is
    // redirected to its successor rather than left dead.
    assert_eq!(
        table
            .iter()
            .find(|r| r.from == "/docs/workspace-modes")
            .map(|r| r.to.as_str()),
        Some("/docs/configuration.md")
    );

    let repo_urls = repository_site_doc_urls(&root);
    assert!(
        repo_urls.iter().any(|(_, u)| u == "/docs/workspace-modes"),
        "the CHANGELOG links to /docs/workspace-modes; the scan found none, so it is broken"
    );
    for (file, url) in &repo_urls {
        if let Err(e) = resolves(url) {
            errors.push(format!("{file}: {e}"));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

/// The environment variables through which git's location discovery can be
/// steered from outside the process, each of which outranks `-C`. The same
/// list, for the same reason, as `AMBIENT_LOCATION_VARS` in
/// `xtask/linkage-check/src/repo_state.rs` (issue #834), copied because that
/// one is private to a test module of a crate this one does not depend on.
const AMBIENT_GIT_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// A read-only git command against the checkout at `root`, with the ambient
/// git environment switched off the way `repo_state.rs`'s `Sandbox::git()`
/// switches it off: global and system configuration pointed at a path that
/// does not exist (and configuration injected through the environment
/// cleared), the location-discovery variables above cleared, and
/// `GIT_CEILING_DIRECTORIES` set to `root`'s parent, so git reads `root`'s own
/// repository or none, never an enclosing or ambient one.
fn checkout_git(root: &Path, args: &[&str]) -> std::process::Output {
    let no_config = root.join("target/no-such-gitconfig");
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &no_config)
        .env("GIT_CONFIG_SYSTEM", &no_config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CEILING_DIRECTORIES", root.parent().unwrap_or(root));
    for var in AMBIENT_GIT_LOCATION_VARS {
        cmd.env_remove(var);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("failed to invoke `git {}`: {e}", args.join(" ")))
}

/// Every `agent-deck.devopstoolkit.ai/docs…` URL path in the repository's
/// tracked files, with the file it was found in. Tracked files, because that
/// is the list the PRD asks for and it keeps build output and local scratch
/// out; `git ls-files` is the one reliable way to name them, so this is a
/// read-only git call pinned to the checkout by [`checkout_git`], which also
/// confirms the repository git found is the one at `root`.
fn repository_site_doc_urls(root: &Path) -> Vec<(String, String)> {
    let root = root.canonicalize().expect("resolve the workspace root");
    let root = root.as_path();
    let toplevel = checkout_git(root, &["rev-parse", "--show-toplevel"]);
    assert!(
        toplevel.status.success(),
        "git rev-parse --show-toplevel failed: {}",
        String::from_utf8_lossy(&toplevel.stderr)
    );
    let toplevel = String::from_utf8_lossy(&toplevel.stdout);
    assert_eq!(
        Path::new(toplevel.trim_end_matches('\n'))
            .canonicalize()
            .expect("resolve git's top level"),
        root,
        "git read a repository other than the workspace root"
    );
    let output = checkout_git(root, &["ls-files", "-z"]);
    assert!(
        output.status.success(),
        "git ls-files failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let host = "agent-deck.devopstoolkit.ai";
    let mut found = Vec::new();
    for name in output.stdout.split(|b| *b == 0) {
        let Ok(name) = std::str::from_utf8(name) else {
            continue;
        };
        if name.is_empty() || name == "xtask/site/fixtures/docusaurus-urls.txt" {
            continue;
        }
        let Ok(text) = fs::read_to_string(root.join(name)) else {
            continue; // binary, or deleted in the working tree
        };
        let mut rest = text.as_str();
        while let Some(at) = rest.find(host) {
            let after = &rest[at + host.len()..];
            rest = after;
            if !after.starts_with("/docs") {
                continue;
            }
            let end = after
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(
                            c,
                            ')' | '('
                                | '"'
                                | '\''
                                | '`'
                                | '>'
                                | '<'
                                | ']'
                                | '#'
                                | '?'
                                | ','
                                | '*'
                                | '…'
                        )
                })
                .unwrap_or(after.len());
            let mut path = after[..end].trim_end_matches(['.', ';', ':']);
            if path.len() > 5 && !path[5..].starts_with('/') {
                continue; // `/docsfoo`, not the docs tree
            }
            if path.is_empty() {
                path = "/docs";
            }
            found.push((name.to_string(), path.to_string()));
        }
    }
    found
}

// ---------------------------------------------------------------------------
// The link check
// ---------------------------------------------------------------------------

#[test]
fn link_check_rejects_a_broken_relative_link_or_image() {
    let fx = Fixture::new();
    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\nSee [gone](gone.md) and [ok](desktop/index.md#desktop).\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(
        err.contains("docs/start.md: `gone.md` resolves to `/docs/gone.md`"),
        "{err}"
    );
    assert!(!err.contains("desktop/index.md"), "{err}");

    // A raw <img> the image collector does not publish is still checked.
    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\n<img src=\"img/nope.png\" alt=\"\">\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("`img/nope.png`"), "{err}");
}

#[test]
fn link_check_rejects_links_into_develop() {
    let fx = Fixture::new();
    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\n[secret](develop/secret.md)\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("links into docs/develop/"), "{err}");

    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\n[secret](https://github.com/vfarcic/dot-agent-deck/blob/main/docs/develop/secret.md)\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("links into docs/develop/"), "{err}");
}

#[test]
fn link_check_rejects_a_broken_reference_definition() {
    let fx = Fixture::new();
    for (body, target) in [
        ("See [guide][setup].\n\n[setup]: missing.md\n", "missing.md"),
        (
            "See [guide][setup].\n\n   [setup]: <gone page.md> \"Setup\"\n",
            "gone page.md",
        ),
        // The destination may sit on the line after the label.
        ("See [setup].\n\n[setup]:\n  later.md\n", "later.md"),
        // An unused definition is still checked.
        (
            "No link uses it.\n\n[Unused Label]: nowhere.md 'title'\n",
            "nowhere.md",
        ),
    ] {
        fs::write(fx.docs().join("start.md"), format!("# Start\n\n{body}")).unwrap();
        let err = build(&fx.config).unwrap_err();
        assert!(
            err.contains(&format!(
                "docs/start.md: `{target}` resolves to `/docs/{target}`"
            )),
            "{body:?}: {err}"
        );
    }
}

#[test]
fn link_check_accepts_valid_reference_definitions_and_publishes_their_images() {
    let fx = Fixture::new();
    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\nSee [the app][app], [Shot], ![a relative shot][rel] and ![abs][].\n\n\
         [app]: desktop/index.md#desktop \"The app\"\n\
         [shot]: </img/shot.png>\n\
         [rel]: img/rel.png (relative)\n\
         [abs]: /img/shot.png\n\
         [web]: https://example.com/x\n\n\
         Not definitions: a footnote and prose that merely starts like one.\n\n\
         [^1]: gone.md\n\n\
         [Note]: see gone.md for more\n\n    [indented]: code-block.md\n\n\
         ```md\n[fenced]: gone.md\n```\n",
    )
    .unwrap();
    let site = build(&fx.config).unwrap();
    // The reference-style relative image is published like an inline one.
    assert_eq!(site.files["docs/img/rel.png"], b"PNG-rel");
}

#[test]
fn link_check_rejects_a_reference_definition_into_develop() {
    let fx = Fixture::new();
    for target in [
        "develop/secret.md",
        "<./develop/secret.md>",
        "/docs/develop/secret.md",
        "https://github.com/vfarcic/dot-agent-deck/blob/main/docs/develop/secret.md",
    ] {
        fs::write(
            fx.docs().join("start.md"),
            format!("# Start\n\nSee [secret].\n\n[secret]: {target}\n"),
        )
        .unwrap();
        let err = build(&fx.config).unwrap_err();
        assert!(err.contains("links into docs/develop/"), "{target}: {err}");
    }

    // A reference-style image into develop/ is refused before anything is
    // published, as an inline one is.
    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\n![diagram][d]\n\n[d]: develop/diagram.png\n",
    )
    .unwrap();
    let err = build(&fx.config).unwrap_err();
    assert!(err.contains("is under docs/develop/"), "{err}");
}

#[test]
fn link_check_ignores_code_fences_and_external_links() {
    let fx = Fixture::new();
    fs::write(
        fx.docs().join("start.md"),
        "# Start\n\n[web](https://example.com/x) [mail](mailto:a@b.c) [here](#start)\n\
         ```md\n[not a link](gone.md)\n```\n",
    )
    .unwrap();
    build(&fx.config).unwrap();
}

fn with_landing(fx: &Fixture, html: &str) {
    let dir = fx.config.landing_dir();
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("index.html"), html).unwrap();
    fs::write(dir.join("landing.css"), "body{}").unwrap();
}

#[test]
fn landing_page_links_come_from_the_manifest() {
    let fx = Fixture::new();
    with_landing(
        &fx,
        "<link href=\"/landing.css\"><a href=\"{{doc:start}}\">s</a>\
         <a href=\"{{doc:desktop/index#desktop}}\">d</a><a href=\"#top\" id=\"top\">t</a>\
         <a href=\"/llms.txt\">l</a>\n<ul>\n{{docs-index}}\n</ul>\n",
    );
    let site = build(&fx.config).unwrap();
    let html = site.text("index.html").unwrap();
    assert!(html.contains("<a href=\"/docs/start.md\">s</a>"), "{html}");
    assert!(
        html.contains("<a href=\"/docs/desktop/index.md#desktop\">d</a>"),
        "{html}"
    );
    assert!(
        html.contains("<li><a href=\"/docs/start.md\">Start</a><span>Begin here.</span></li>"),
        "{html}"
    );
    assert!(!html.contains("{{"), "{html}");
    assert_eq!(site.text("landing.css"), Some("body{}"));
}

#[test]
fn landing_page_rejects_an_unpublished_page_a_missing_anchor_or_a_dead_link() {
    let cases = [
        (
            "<a href=\"{{doc:develop/secret}}\">x</a>",
            "not in docs/published.toml",
        ),
        (
            "<a href=\"{{doc:start#nowhere}}\">x</a>",
            "has no heading with that anchor",
        ),
        ("<a href=\"{{nope}}\">x</a>", "unknown placeholder"),
        (
            "<a href=\"/docs/start\">x</a>",
            "which the site does not publish",
        ),
        (
            "<img src=\"/img/gone.png\">",
            "which the site does not publish",
        ),
        ("<a href=\"#missing\">x</a>", "names no element on the page"),
    ];
    for (html, expected) in cases {
        let fx = Fixture::new();
        with_landing(&fx, html);
        let err = build(&fx.config).unwrap_err();
        assert!(err.contains(expected), "{html}: {err}");
    }
}

#[test]
fn heading_anchors_follow_the_github_convention() {
    let anchors = heading_anchors(
        "# Desktop App\n## How it runs\n## How it runs\n### `docs` & more!\n```\n# fenced\n```\n#not\n",
    );
    let expected: BTreeSet<String> = ["desktop-app", "how-it-runs", "how-it-runs-1", "docs--more"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(anchors, expected);
}

/// Where a docs page is readable as GitHub renders it. The site serves the docs
/// as raw Markdown for agents, so a link offered to a person goes here instead.
const RENDERED_DOCS_PREFIX: &str = "https://github.com/vfarcic/dot-agent-deck/blob/main/docs/";

/// The GitHub-rendered installation guide: the link on the landing page for a
/// person who would rather install by hand.
const INSTALLATION_GUIDE_URL: &str =
    "https://github.com/vfarcic/dot-agent-deck/blob/main/docs/installation.md";

/// The GitHub-rendered desktop app page, linked from the clients section.
const DESKTOP_PAGE_URL: &str =
    "https://github.com/vfarcic/dot-agent-deck/blob/main/docs/desktop/index.md";

#[test]
fn the_real_landing_page_hands_off_to_the_docs_in_plain_html() {
    let config = SiteConfig::from_workspace(&workspace_root());
    let site = build(&config).unwrap();
    let html = site.text("index.html").unwrap();
    // The agent prompt is the hero's call to action: it sits inside the hero,
    // between its opening and the end of the hero's <header>.
    let hero_start = html
        .find("<header class=\"hero\">")
        .expect("landing page lost its hero");
    let hero_end = hero_start + html[hero_start..].find("</header>").unwrap();
    let hero = &html[hero_start..hero_end];
    assert!(
        hero.contains("id=\"agent-prompt-title\""),
        "the prompt is not in the hero:\n{hero}"
    );
    assert!(
        !hero.contains("<h2 id=\"agent-prompt-title\" class=\"heroPromptTitle\">Or"),
        "the prompt's heading starts with \"Or\" again"
    );
    // The prompt, copied as is: one line of plain text, no markup and no slot
    // to fill in, pointing the agent at /llms.txt.
    let start = hero
        .find("<pre id=\"agent-prompt-text\">")
        .expect("the hero lost the agent prompt");
    let body = start + "<pre id=\"agent-prompt-text\">".len();
    let end = body + hero[body..].find("</pre>").unwrap();
    assert_eq!(
        &hero[body..end],
        "Read https://agent-deck.devopstoolkit.ai/llms.txt, then install dot-agent-deck, \
         set it up for me, and explain what it does and how to use it."
    );
    assert!(
        hero.contains("id=\"copy-prompt\""),
        "the copy button left the hero"
    );
    assert!(!html.contains("promptSlot"), "the goal slot is back");
    // For agents: where to start, and every published page as a plain
    // <a href> in the served HTML, in the last band above the footer.
    let agents_start = html
        .find("<section class=\"forAgents\" id=\"docs\"")
        .expect("landing page lost the docs list for agents");
    let agents_end = agents_start + html[agents_start..].find("</section>").unwrap();
    let agents = &html[agents_start..agents_end];
    assert!(
        html[agents_end + "</section>".len()..]
            .trim_start()
            .starts_with("</main>"),
        "the docs list for agents is no longer the last band"
    );
    assert!(agents.contains("For agents: every docs page, as Markdown"));
    assert!(agents.contains("An AI agent should start at <a href=\"/llms.txt\">/llms.txt</a>"));
    assert!(agents.contains("<a href=\"/llms-full.txt\">/llms-full.txt</a>"));
    assert!(agents.contains("dot-agent-deck docs"));
    assert!(agents.contains(
        "The docs are Markdown, written for the <strong>coding agent</strong> that sets the deck \
         up for you."
    ));
    assert_eq!(agents.matches("<strong>").count(), 1, "{agents}");
    // No install command and no "get started" call to action: the only link to
    // Getting Started is its entry in the list of every page, for agents.
    // The list is generated from the manifest, whose Installation entry names
    // Homebrew as one of its routes for an agent; everything else is the page.
    let outside_list = format!("{}{}", &html[..agents_start], &html[agents_end..]);
    assert!(
        !outside_list.to_lowercase().contains("brew"),
        "brew is back on the landing page"
    );
    for cta in [
        "Get started",
        "Read the guide",
        "Where to start in the docs",
    ] {
        assert!(!html.contains(cta), "`{cta}` is back on the landing page");
    }
    assert_eq!(
        html.matches("href=\"/docs/getting-started.md").count(),
        1,
        "Getting Started is linked outside the docs list"
    );
    // One readable installation guide for a person, the same URL wherever it
    // appears, and no link to the raw installation page outside the docs list.
    let guide = format!("<a href=\"{INSTALLATION_GUIDE_URL}\">Installation guide →</a>");
    assert!(
        hero.contains(&format!("Prefer to do it yourself? {guide}")),
        "the hero lost the do-it-yourself link"
    );
    assert_eq!(
        html.matches("href=\"/docs/installation.md").count(),
        1,
        "the raw installation page is linked outside the docs list"
    );
    // The desktop app is read about on its rendered page, once, from its card;
    // its raw page is linked only from the docs list.
    assert_eq!(
        outside_list
            .matches(&format!(
                "<a href=\"{DESKTOP_PAGE_URL}\">Read about the desktop app →</a>"
            ))
            .count(),
        1,
        "the desktop app card lost its link to the rendered page"
    );
    assert_eq!(
        html.matches("href=\"/docs/desktop/index.md").count(),
        1,
        "the raw desktop page is linked outside the docs list"
    );
    let pages = published_docs::read(&config.docs_dir).unwrap();
    for page in &pages {
        assert!(
            agents.contains(&format!("<a href=\"/docs/{}\">", page.file_name())),
            "no plain link to {} in the docs list",
            page.slug
        );
    }
    // Outside the docs list, a person is never linked to raw Markdown: every
    // .md link is a GitHub-rendered page, and names a published one.
    let mut rendered = 0;
    for href in outside_list.split("href=\"").skip(1) {
        let href = &href[..href.find('"').unwrap()];
        let path = href.split(['#', '?']).next().unwrap();
        if !path.ends_with(".md") {
            continue;
        }
        let file = path
            .strip_prefix(RENDERED_DOCS_PREFIX)
            .unwrap_or_else(|| panic!("`{href}` links a person to raw Markdown"));
        assert!(
            pages.iter().any(|page| page.file_name() == file),
            "`{href}` is not a published page"
        );
        rendered += 1;
    }
    assert!(
        rendered > 0,
        "no rendered docs link found outside the docs list"
    );
    assert!(!html.contains("{{"));
    assert!(!html.contains("docs/develop"));
    assert!(site.files.contains_key("landing.css"));
}
