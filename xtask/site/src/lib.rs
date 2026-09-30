//! `cargo xtask site <out-dir>` — build the website's output from the
//! published-docs manifest (PRD #1419, Decisions 1, 2 and 5).
//!
//! Everything published is decided by `docs/published.toml`, read through the
//! one parser the binary also uses ([`published_docs`]). The output is:
//!
//! - `docs/<slug>.md` — each manifest page's Markdown, byte for byte, at a
//!   stable URL (`/docs/<slug>.md`).
//! - `llms.txt` — an index of every page (title, URL, description) in the
//!   llms.txt format, which also points at the version-matched
//!   `dot-agent-deck docs [topic]` subcommand.
//! - `llms-full.txt` — every page concatenated in manifest order.
//! - `img/…` — the whole image directory (`docs/img`, a symlink to
//!   `site/static/img`, the one target it may have outside `docs/`), at the
//!   `/img/` paths Docusaurus served it at, so root-absolute `/img/x.png`
//!   references and existing image URLs resolve. Only files with an image
//!   extension are published; any other file there fails the build.
//! - `docs/<path>` for each image a page references by a RELATIVE path, so
//!   `img/x.png` in a top-level page (or `../img/x.png` in a desktop page)
//!   resolves on the site exactly as it does on disk.
//! - the landing page, from [`landing_page`]: `index.html` rendered from the
//!   `site/landing/` template, whose docs links come from the manifest, and
//!   its stylesheet.
//! - `sitemap.xml` ([`sitemap_xml`]): the landing page and every published
//!   page's URL, on the `--base-url`; and `robots.txt` ([`robots_txt`]), which
//!   allows everything and names the sitemap's absolute URL.
//! - `_redirects` and `_headers`, which Netlify reads from the published
//!   directory: every URL the Docusaurus site served redirects to its Markdown
//!   successor ([`redirects`]), and `.md`, `llms`, `robots.txt` and
//!   `sitemap.xml` get a readable `Content-Type`.
//! - separately from the published tree, the same redirects as an nginx
//!   `include` ([`nginx_redirects`]), written only when `--nginx-redirects` asks
//!   for it, because a file nginx serves from its root would publish its own
//!   config.
//!
//! [`build`] finishes with a link check ([`check_links`]), the replacement for
//! Docusaurus's `onBrokenLinks: 'throw'`: every relative or root-absolute link
//! and image in the published Markdown, the landing page and `llms.txt` must
//! resolve to a file in the output, and nothing may link into `docs/develop/`.
//!
//! Nothing under `docs/develop/` can reach the output: the parser rejects a
//! `develop/` slug, image references are confined to the docs tree outside
//! `develop/`, every page and image is read from the canonical path the shared
//! boundary check returns ([`published_docs::page_source`],
//! [`published_docs::image_source`], [`published_docs::image_dir`]), so a
//! symlink into `docs/develop/` is refused rather than followed, and [`build`]
//! refuses an output path under `docs/develop/`.

#[path = "../../../src/published_docs.rs"]
pub mod published_docs;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use published_docs::{Page, UNPUBLISHED_DIR};

/// The site's public origin. Page URLs in `llms.txt` / `llms-full.txt` are
/// built from it.
pub const DEFAULT_BASE_URL: &str = "https://agent-deck.devopstoolkit.ai";

/// The one-paragraph summary at the top of both `llms` files.
const SUMMARY: &str = "dot-agent-deck is a dashboard for running several AI coding agents in \
parallel, in a terminal UI (the `dot-agent-deck` binary) or a desktop app. A background daemon \
owns the agents, so both clients show and control the same ones.";

/// The "start here, by goal" list at the top of `llms.txt`: what the user
/// wants, and the manifest slug of the page that sets it up. An agent handed
/// only "install and set it up" used to stop at Getting Started's one running
/// agent; this tells it where each larger goal lives. [`llms_txt`] lists only
/// the entries whose slug is in the manifest, and a test pins every slug to
/// the real one.
pub const START_BY_GOAL: &[(&str, &str)] = &[
    (
        "Install, upgrade or uninstall dot-agent-deck",
        "installation",
    ),
    (
        "Start a first agent and check that the deck shows it",
        "getting-started",
    ),
    (
        "Several agents working together, for example a coder, a reviewer and an orchestrator \
         that hands them work",
        "orchestration",
    ),
    (
        "Isolated background work, each task in its own copy of the repository",
        "dispatcher-mode",
    ),
    (
        "Run a prompt on a schedule, or put agents on open GitHub issues",
        "scheduled-tasks",
    ),
    (
        "Run the agents on another machine over ssh",
        "remote-environments",
    ),
    ("Use the desktop app", "desktop/index"),
    ("Settings and environment variables", "configuration"),
    ("Something is not working", "troubleshooting"),
];

/// The page the legacy `/docs` and `/docs/` URLs redirect to: the index an
/// agent starts from. Docusaurus served nothing there.
pub const DOCS_INDEX: &str = "/llms.txt";

/// Legacy `/docs/<route>` URLs whose page is gone, with the manifest slug of
/// its closest successor.
///
/// `workspace-modes`: the page was deleted with the feature (#1199, #1412)
/// while `CHANGELOG.md` still links it. Workspace modes were a `[[modes]]`
/// block in `.dot-agent-deck.toml`; a leftover block is now ignored with a
/// warning, and `configuration` is the reference for that file.
pub const RETIRED_PAGES: &[(&str, &str)] = &[("workspace-modes", "configuration")];

/// The `Content-Type` a published `.md` page is served with, on both targets.
pub const MARKDOWN_CONTENT_TYPE: &str = "text/markdown; charset=utf-8";
/// The `Content-Type` of `llms.txt`, `llms-full.txt` and `robots.txt`, on
/// both targets.
pub const LLMS_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
/// The `Content-Type` of `sitemap.xml`, on both targets.
pub const SITEMAP_CONTENT_TYPE: &str = "application/xml; charset=utf-8";

/// Inputs to [`build`].
#[derive(Debug, Clone)]
pub struct SiteConfig {
    /// The `docs/` directory holding `published.toml` and the pages.
    pub docs_dir: PathBuf,
    /// The site origin, without a trailing `/`.
    pub base_url: String,
}

impl SiteConfig {
    pub fn from_workspace(root: &Path) -> Self {
        Self {
            docs_dir: root.join("docs"),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// The landing-page template directory: `site/landing/` beside `docs/`.
    /// A docs tree with no such sibling (a test fixture) builds without a
    /// landing page; `run` refuses that for a real build.
    pub fn landing_dir(&self) -> PathBuf {
        self.docs_dir
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join("site")
            .join("landing")
    }

    fn page_url(&self, page: &Page) -> String {
        format!("{}/docs/{}", self.base_url, page.file_name())
    }
}

/// The generated site: output path (relative, `/`-separated) → bytes.
#[derive(Debug, Default)]
pub struct Site {
    pub files: BTreeMap<String, Vec<u8>>,
    /// The redirects as an nginx `include`, kept out of `files` so it is never
    /// published (see the module docs).
    pub nginx_redirects: String,
}

impl Site {
    fn insert(&mut self, path: String, bytes: Vec<u8>) -> Result<(), String> {
        if path == "docs/develop"
            || path.starts_with(&format!("docs/{UNPUBLISHED_DIR}/"))
            || path
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return Err(format!("refusing to publish `{path}`"));
        }
        match self.files.get(&path) {
            Some(existing) if *existing != bytes => Err(format!(
                "two different files would be published at `{path}`"
            )),
            _ => {
                self.files.insert(path, bytes);
                Ok(())
            }
        }
    }

    /// The generated text of one output file, if it is valid UTF-8.
    pub fn text(&self, path: &str) -> Option<&str> {
        self.files
            .get(path)
            .and_then(|b| std::str::from_utf8(b).ok())
    }
}

/// Generate the whole site in memory.
pub fn build(config: &SiteConfig) -> Result<Site, String> {
    let pages = published_docs::read(&config.docs_dir).map_err(|e| e.to_string())?;
    let mut site = Site::default();
    let mut bodies = Vec::with_capacity(pages.len());

    for page in &pages {
        let source =
            published_docs::page_source(&config.docs_dir, page).map_err(|e| e.to_string())?;
        let body = std::fs::read_to_string(&source).map_err(|e| {
            format!(
                "page `{}` is in the manifest but {} cannot be read: {e}",
                page.slug,
                source.display()
            )
        })?;
        match first_heading(&body) {
            Some(heading) if heading == page.title => {}
            Some(heading) => {
                return Err(format!(
                    "page `{}`: manifest title `{}` does not match its first heading `{heading}`",
                    page.slug, page.title
                ));
            }
            None => {
                return Err(format!(
                    "page `{}` has no `# ` heading to match its title",
                    page.slug
                ));
            }
        }
        for image in relative_images(config, page, &body)? {
            let source = published_docs::image_source(&config.docs_dir, page, &image)
                .map_err(|e| e.to_string())?;
            let bytes = std::fs::read(&source)
                .map_err(|e| format!("page `{}`: cannot read image `{image}`: {e}", page.slug))?;
            site.insert(format!("docs/{image}"), bytes)?;
        }
        site.insert(
            format!("docs/{}", page.file_name()),
            body.clone().into_bytes(),
        )?;
        bodies.push(body);
    }

    let images = published_docs::image_dir(&config.docs_dir).map_err(|e| e.to_string())?;
    copy_tree(&images, "img", &mut site)?;
    site.insert(
        "llms.txt".to_string(),
        llms_txt(config, &pages).into_bytes(),
    )?;
    site.insert(
        "llms-full.txt".to_string(),
        llms_full_txt(config, &pages, &bodies).into_bytes(),
    )?;
    let table = redirects(&pages);
    site.insert(
        "_redirects".to_string(),
        netlify_redirects(&table).into_bytes(),
    )?;
    site.insert("_headers".to_string(), netlify_headers(&pages).into_bytes())?;
    site.nginx_redirects = nginx_redirects(&table);
    let anchors: BTreeMap<&str, BTreeSet<String>> = pages
        .iter()
        .zip(&bodies)
        .map(|(page, body)| (page.slug.as_str(), heading_anchors(body)))
        .collect();
    for (path, bytes) in landing_page(config, &pages, &anchors)? {
        site.insert(path, bytes)?;
    }
    let landing = site.files.contains_key("index.html");
    site.insert(
        "sitemap.xml".to_string(),
        sitemap_xml(config, &pages, landing).into_bytes(),
    )?;
    site.insert("robots.txt".to_string(), robots_txt(config).into_bytes())?;
    check_links(&site, &config.base_url)?;
    Ok(site)
}

/// The landing page and its stylesheet (PRD #1419, Decisions 1 and 2).
///
/// `index.html` is rendered from `<landing_dir>/index.html`, a template with
/// two placeholders: `{{doc:<slug>}}` (optionally `{{doc:<slug>#<anchor>}}`)
/// becomes that page's URL, and fails the build unless `<slug>` is in the
/// manifest and `<anchor>` names one of its headings; `{{docs-index}}` becomes
/// one `<li>` per published page, in manifest order. So every docs link on the
/// page is a plain `<a href>` in the served HTML, and cannot point at a page
/// that is not published. `landing.css` is copied as is. Everything returned
/// goes through [`Site::insert`], so the `docs/develop/` guard applies.
pub fn landing_page(
    config: &SiteConfig,
    pages: &[Page],
    anchors: &BTreeMap<&str, BTreeSet<String>>,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    let dir = config.landing_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .map_err(|e| format!("cannot read {}: {e}", dir.join(name).display()))
    };
    let html = render_landing(&read("index.html")?, pages, anchors)?;
    Ok(vec![
        ("index.html".to_string(), html.into_bytes()),
        ("landing.css".to_string(), read("landing.css")?.into_bytes()),
    ])
}

/// Replace the landing template's placeholders (see [`landing_page`]).
pub fn render_landing(
    template: &str,
    pages: &[Page],
    anchors: &BTreeMap<&str, BTreeSet<String>>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| "landing page: unterminated `{{` placeholder".to_string())?;
        let placeholder = &after[..end];
        if placeholder == "docs-index" {
            for page in pages {
                out.push_str(&format!(
                    "          <li><a href=\"{}\">{}</a><span>{}</span></li>\n",
                    page_path(page),
                    html_escape(&page.title),
                    html_escape(&page.description)
                ));
            }
            // The template puts the placeholder on a line of its own; the
            // generated lines already end in newlines.
            rest = after[end + 2..]
                .strip_prefix('\n')
                .unwrap_or(&after[end + 2..]);
            continue;
        }
        let Some(target) = placeholder.strip_prefix("doc:") else {
            return Err(format!(
                "landing page: unknown placeholder `{{{{{placeholder}}}}}`"
            ));
        };
        let (slug, anchor) = match target.split_once('#') {
            Some((slug, anchor)) => (slug, Some(anchor)),
            None => (target, None),
        };
        let page = pages.iter().find(|p| p.slug == slug).ok_or_else(|| {
            format!("landing page links `{slug}`, which is not in docs/published.toml")
        })?;
        out.push_str(&page_path(page));
        if let Some(anchor) = anchor {
            if !anchors.get(slug).is_some_and(|a| a.contains(anchor)) {
                return Err(format!(
                    "landing page links `{slug}#{anchor}`, but `{slug}` has no heading with that anchor"
                ));
            }
            out.push('#');
            out.push_str(anchor);
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// A published page's root-absolute URL path.
fn page_path(page: &Page) -> String {
    format!("/docs/{}", page.file_name())
}

/// `sitemap.xml`: the landing page (when the site has one) and every published
/// page, as absolute URLs on `base_url`, in manifest order. Nothing else is
/// listed: `llms.txt` and the images are reached from the pages, and the
/// redirects' legacy URLs are not pages.
pub fn sitemap_xml(config: &SiteConfig, pages: &[Page], landing: bool) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">\n",
    );
    let urls = landing
        .then(|| format!("{}/", config.base_url))
        .into_iter()
        .chain(pages.iter().map(|page| config.page_url(page)));
    for url in urls {
        out.push_str(&format!("  <url><loc>{}</loc></url>\n", html_escape(&url)));
    }
    out.push_str("</urlset>\n");
    out
}

/// `robots.txt`: every crawler may fetch everything, and the sitemap is at
/// its absolute URL on `base_url`.
pub fn robots_txt(config: &SiteConfig) -> String {
    format!(
        "User-agent: *\nAllow: /\n\nSitemap: {}/sitemap.xml\n",
        config.base_url
    )
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// One redirect: a legacy URL path and the path it moves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub from: String,
    pub to: String,
}

/// Every URL form the Docusaurus site served, redirected to its successor.
///
/// Generated from the manifest, not written by hand: each page's route is its
/// slug, or the directory for an `<dir>/index` page (Docusaurus served
/// `desktop/index.md` at `/docs/desktop/`), and each route is redirected in its
/// slashless, trailing-slash and `/index.html` forms. [`RETIRED_PAGES`] adds
/// the same three forms for a page that no longer exists, and `/docs` plus
/// `/docs/` go to [`DOCS_INDEX`].
pub fn redirects(pages: &[Page]) -> Vec<Redirect> {
    let mut out = Vec::new();
    let mut push_route = |route: &str, to: String| {
        for from in [
            format!("/docs/{route}"),
            format!("/docs/{route}/"),
            format!("/docs/{route}/index.html"),
        ] {
            out.push(Redirect {
                from,
                to: to.clone(),
            });
        }
    };
    for page in pages {
        let route = page.slug.strip_suffix("/index").unwrap_or(&page.slug);
        push_route(route, page_path(page));
    }
    for (route, successor) in RETIRED_PAGES {
        if let Some(page) = pages.iter().find(|p| p.slug == *successor) {
            push_route(route, page_path(page));
        }
    }
    for from in ["/docs", "/docs/"] {
        out.push(Redirect {
            from: from.to_string(),
            to: DOCS_INDEX.to_string(),
        });
    }
    out
}

/// The redirects in Netlify's `_redirects` format. `301!` forces each one, so
/// a file Netlify might otherwise find at the old path never shadows it.
pub fn netlify_redirects(table: &[Redirect]) -> String {
    let mut out =
        String::from("# Generated by `cargo xtask site` from docs/published.toml. Do not edit.\n");
    for r in table {
        out.push_str(&format!("{} {} 301!\n", r.from, r.to));
    }
    out
}

/// The redirects as nginx exact-match locations, for an `include` inside the
/// `server` block of `site/nginx-default.conf`. An exact match outranks the
/// config's prefix and regex locations.
pub fn nginx_redirects(table: &[Redirect]) -> String {
    let mut out =
        String::from("# Generated by `cargo xtask site` from docs/published.toml. Do not edit.\n");
    for r in table {
        out.push_str(&format!(
            "location = {} {{ return 301 {}; }}\n",
            r.from, r.to
        ));
    }
    out
}

/// Netlify's `_headers`: the readable content types, path by path, since
/// Netlify's header paths match by prefix and splat rather than by extension.
pub fn netlify_headers(pages: &[Page]) -> String {
    let mut out =
        String::from("# Generated by `cargo xtask site` from docs/published.toml. Do not edit.\n");
    for page in pages {
        out.push_str(&format!(
            "{}\n  Content-Type: {MARKDOWN_CONTENT_TYPE}\n",
            page_path(page)
        ));
    }
    for path in ["/llms.txt", "/llms-full.txt", "/robots.txt"] {
        out.push_str(&format!("{path}\n  Content-Type: {LLMS_CONTENT_TYPE}\n"));
    }
    out.push_str(&format!(
        "/sitemap.xml\n  Content-Type: {SITEMAP_CONTENT_TYPE}\n"
    ));
    out
}

/// The link check that replaced Docusaurus's `onBrokenLinks: 'throw'`.
///
/// Every link and image target in the published Markdown, in each HTML page
/// and in `llms.txt` (inline `](target)` links and images, and the target of
/// every link reference definition, `[label]: target`, which reference-style
/// links and images use) is resolved against the output: a relative target against
/// the linking file's directory, a root-absolute one against the site root,
/// and an absolute URL on `base_url` by its path. Each must name a file the
/// site publishes (an `#id` on an HTML page must name an element on it), and
/// no target may reach `docs/develop/`, relatively or through an absolute URL.
/// Heading anchors between Markdown pages are checked by the `docs`
/// subcommand's tests (`cli/docs/005`), which read the same files, so this does
/// not repeat that.
pub fn check_links(site: &Site, base_url: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for (path, bytes) in &site.files {
        let kind = if path.ends_with(".md") {
            LinkSource::Markdown
        } else if path.ends_with(".html") {
            LinkSource::Html
        } else if path == "llms.txt" {
            LinkSource::Markdown
        } else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(bytes) else {
            errors.push(format!("{path}: not UTF-8"));
            continue;
        };
        let targets = match kind {
            LinkSource::Markdown => {
                let mut t = markdown_link_targets(text);
                t.extend(
                    reference_definitions(text)
                        .into_iter()
                        .map(|(_, target)| target),
                );
                t.extend(html_attr_targets(text));
                t
            }
            LinkSource::Html => html_attr_targets(text),
        };
        let dir = Path::new(path).parent().unwrap_or_else(|| Path::new(""));
        for target in targets {
            if let Err(e) = check_target(site, base_url, path, dir, kind, text, target) {
                errors.push(e);
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("broken links:\n  {}", errors.join("\n  ")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkSource {
    Markdown,
    Html,
}

fn check_target(
    site: &Site,
    base_url: &str,
    source: &str,
    dir: &Path,
    kind: LinkSource,
    text: &str,
    target: &str,
) -> Result<(), String> {
    let develop = format!("docs/{UNPUBLISHED_DIR}/");
    let local = match target.strip_prefix(base_url) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('#') => {
            if rest.is_empty() { "/" } else { rest }
        }
        _ => target,
    };
    if local.contains("://") || local.starts_with("mailto:") || local.starts_with("data:") {
        if local.contains(&format!("/{develop}")) {
            return Err(format!(
                "{source}: `{target}` links into docs/{UNPUBLISHED_DIR}/"
            ));
        }
        return Ok(());
    }
    let (path, fragment) = match local.split_once('#') {
        Some((p, f)) => (p, Some(f)),
        None => (local, None),
    };
    let path = path.split('?').next().unwrap_or(path);
    if path.is_empty() {
        // An in-page anchor. Markdown heading anchors are `cli/docs/005`'s; an
        // HTML page's must name an element on it.
        if kind == LinkSource::Html {
            let id = fragment.unwrap_or("");
            if !id.is_empty() && !text.contains(&format!("id=\"{id}\"")) {
                return Err(format!("{source}: `{target}` names no element on the page"));
            }
        }
        return Ok(());
    }
    let resolved = if let Some(rooted) = path.strip_prefix('/') {
        if rooted.is_empty() || rooted.ends_with('/') {
            format!("{rooted}index.html")
        } else {
            rooted.to_string()
        }
    } else {
        normalize(dir, path)
            .ok_or_else(|| format!("{source}: `{target}` points outside the site"))?
    };
    if resolved.starts_with(&develop) || resolved == format!("docs/{UNPUBLISHED_DIR}") {
        return Err(format!(
            "{source}: `{target}` links into docs/{UNPUBLISHED_DIR}/"
        ));
    }
    if !site.files.contains_key(&resolved) {
        return Err(format!(
            "{source}: `{target}` resolves to `/{resolved}`, which the site does not publish"
        ));
    }
    Ok(())
}

/// Every `](target)` in Markdown outside code fences — links and images alike.
fn markdown_link_targets(body: &str) -> Vec<&str> {
    let mut targets = Vec::new();
    for line in unfenced_lines(body) {
        let mut rest = line;
        while let Some(start) = rest.find("](") {
            let target_start = &rest[start + 2..];
            let Some(end) = target_start.find(')') else {
                break;
            };
            let target = target_start[..end]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('<')
                .trim_end_matches('>');
            if !target.is_empty() {
                targets.push(target);
            }
            rest = &target_start[end + 1..];
        }
    }
    targets
}

/// Every link reference definition outside code fences, as `(label, target)`
/// with the label normalized the way CommonMark matches it (case-folded,
/// whitespace collapsed). A definition is a line of up to three leading spaces,
/// `[label]:`, a destination (bare or `<…>`, on that line or the next) and an
/// optional quoted or parenthesized title; anything else after the destination
/// makes the line prose, not a definition. Footnote definitions (`[^1]:`) are
/// not links and are skipped. Checking every definition, used or not, is the
/// strict side: an unused one to a missing page still fails the build.
fn reference_definitions(body: &str) -> Vec<(String, &str)> {
    let lines = unfenced_lines(body);
    let mut definitions = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent > 3 {
            continue;
        }
        let Some(after_open) = line[indent..].strip_prefix('[') else {
            continue;
        };
        let Some(label_end) = label_end(after_open) else {
            continue;
        };
        let label = &after_open[..label_end];
        if label.trim().is_empty() || label.starts_with('^') {
            continue;
        }
        let Some(rest) = after_open[label_end + 1..].strip_prefix(':') else {
            continue;
        };
        let rest = if rest.trim().is_empty() {
            match lines.get(i + 1) {
                Some(next) => *next,
                None => continue,
            }
        } else {
            rest
        };
        if let Some(target) = definition_destination(rest) {
            definitions.push((normalize_label(label), target));
        }
    }
    definitions
}

/// The byte offset of the `]` closing a link label that starts just after its
/// `[`, honoring backslash escapes. `None` for an unescaped `[` inside, which
/// CommonMark does not allow in a reference label, or no `]` on the line.
fn label_end(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '[' => return None,
            ']' => return Some(i),
            _ => {}
        }
    }
    None
}

/// A definition's destination, if what follows `[label]:` is one: the
/// destination, then nothing or a title (`"…"`, `'…'` or `(…)`, which may
/// continue on later lines) after whitespace, and nothing after the title.
fn definition_destination(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    let (target, after) = if let Some(bracketed) = rest.strip_prefix('<') {
        let end = bracketed.find(['<', '>'])?;
        if bracketed.as_bytes()[end] != b'>' {
            return None;
        }
        (&bracketed[..end], &bracketed[end + 1..])
    } else {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        (&rest[..end], &rest[end..])
    };
    if target.is_empty() {
        return None;
    }
    let title = after.trim_start();
    if title.is_empty() {
        return Some(target);
    }
    if title.len() == after.len() {
        return None;
    }
    let close = match title.chars().next()? {
        '"' => '"',
        '\'' => '\'',
        '(' => ')',
        _ => return None,
    };
    match title[1..].find(close) {
        Some(end) if !title[1 + end + 1..].trim().is_empty() => None,
        _ => Some(target),
    }
}

/// A link label as CommonMark matches it: case-folded, whitespace collapsed.
fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Every `href="…"` and `src="…"` attribute value, for HTML and for raw HTML
/// inside Markdown. Only double-quoted values, which is all the landing page
/// and the docs write.
fn html_attr_targets(text: &str) -> Vec<&str> {
    let mut targets = Vec::new();
    for attr in [" href=\"", " src=\""] {
        let mut rest = text;
        while let Some(start) = rest.find(attr) {
            let value = &rest[start + attr.len()..];
            let Some(end) = value.find('"') else { break };
            if !value[..end].is_empty() {
                targets.push(&value[..end]);
            }
            rest = &value[end..];
        }
    }
    targets
}

/// The lines of `body` outside ``` and ~~~ code fences.
fn unfenced_lines(body: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut fence: Option<&str> = None;
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
        } else if trimmed.starts_with("~~~") {
            fence = Some("~~~");
        } else {
            lines.push(line);
        }
    }
    lines
}

/// The GitHub-style anchors of a page's headings: lowercased, punctuation
/// other than `-` and `_` dropped, spaces as `-`, and a `-N` suffix on a
/// repeat: the GitHub convention `cli/docs/005` also follows.
pub fn heading_anchors(body: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchors = BTreeSet::new();
    for line in unfenced_lines(body) {
        let hashes = line.chars().take_while(|c| *c == '#').count();
        if hashes == 0 || hashes > 6 || !line[hashes..].starts_with(' ') {
            continue;
        }
        let text = line[hashes..].trim().trim_end_matches('#').trim();
        let base: String = text
            .to_lowercase()
            .chars()
            .filter_map(|c| match c {
                ' ' => Some('-'),
                c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
                _ => None,
            })
            .collect();
        let count = seen.entry(base.clone()).or_insert(0);
        let anchor = if *count == 0 {
            base.clone()
        } else {
            format!("{base}-{count}")
        };
        *count += 1;
        anchors.insert(anchor);
    }
    anchors
}

/// `llms.txt`: the llms.txt-convention index of every published page.
pub fn llms_txt(config: &SiteConfig, pages: &[Page]) -> String {
    let mut out = String::new();
    out.push_str("# dot-agent-deck\n\n");
    out.push_str(&format!("> {SUMMARY}\n\n"));
    out.push_str(
        "The pages below are the user documentation for the latest release, as Markdown. \
         Links between pages are relative `.md` links and resolve against the linking page's URL.\n\n",
    );
    out.push_str(
        "If dot-agent-deck is already installed and its binary has the `docs` subcommand, \
         prefer it: it prints these pages as they are for the installed version, with no network \
         access. `dot-agent-deck docs` lists the topics, `dot-agent-deck docs <topic>` prints one \
         page, and `dot-agent-deck docs --all` prints every page. A topic is the page's path \
         below `/docs/` without `.md`, for example `dot-agent-deck docs desktop/voice`. Older \
         releases do not have it: if `dot-agent-deck docs` reports an unrecognized subcommand, \
         use these pages, which follow the latest release rather than the installed one.\n\n",
    );
    out.push_str(&format!(
        "Every page in one file: [llms-full.txt]({}/llms-full.txt)\n\n",
        config.base_url
    ));
    let by_goal: Vec<String> = START_BY_GOAL
        .iter()
        .filter_map(|(goal, slug)| {
            let page = pages.iter().find(|p| p.slug == *slug)?;
            Some(format!(
                "- {goal}: read [{}]({})\n",
                page.title,
                config.page_url(page)
            ))
        })
        .collect();
    if !by_goal.is_empty() {
        out.push_str("## Start here, by goal\n\n");
        out.push_str(
            "Getting Started ends with one running agent. When the user wants more than that, \
             read the page for their goal as well:\n\n",
        );
        out.extend(by_goal);
        out.push('\n');
    }
    out.push_str("## Docs\n\n");
    for page in pages {
        out.push_str(&format!(
            "- [{}]({}): {}\n",
            page.title,
            config.page_url(page),
            page.description
        ));
    }
    out
}

/// `llms-full.txt`: every page's Markdown in manifest order, each preceded by
/// a marker line naming its topic and URL.
pub fn llms_full_txt(config: &SiteConfig, pages: &[Page], bodies: &[String]) -> String {
    let mut out = String::new();
    out.push_str("# dot-agent-deck documentation, every page\n\n");
    out.push_str(&format!("> {SUMMARY}\n\n"));
    out.push_str(&format!(
        "Every page of the user documentation for the latest release, in reading order. Each \
         page starts after a line of the form `<!-- page: <topic> <url> -->`, and relative links \
         inside a page resolve against that page's URL. The index is {}/llms.txt. An installed \
         binary that has the `docs` subcommand prints the same pages for its own version with \
         `dot-agent-deck docs --all`.\n",
        config.base_url
    ));
    for (page, body) in pages.iter().zip(bodies) {
        out.push_str(&format!(
            "\n<!-- page: {} {} -->\n\n",
            page.slug,
            config.page_url(page)
        ));
        out.push_str(body);
        if !body.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// Write `site` into `out_dir`, which must be absent or empty, so a stale file
/// from an earlier build (or anything else) is never published by accident.
pub fn write(site: &Site, out_dir: &Path) -> Result<usize, String> {
    if out_dir.exists() {
        let mut entries = std::fs::read_dir(out_dir)
            .map_err(|e| format!("cannot read {}: {e}", out_dir.display()))?;
        if entries.next().is_some() {
            return Err(format!(
                "{} is not empty; remove it or pass a new directory",
                out_dir.display()
            ));
        }
    }
    for (path, bytes) in &site.files {
        let target = out_dir.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, bytes)
            .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    Ok(site.files.len())
}

const USAGE: &str =
    "usage: cargo xtask site <out-dir> [--base-url <url>] [--nginx-redirects <file>]";

/// `cargo xtask site` entry point. `root` is the workspace root.
pub fn run(root: &Path, args: &[String]) -> ExitCode {
    let mut out_dir: Option<PathBuf> = None;
    let mut nginx: Option<PathBuf> = None;
    let mut config = SiteConfig::from_workspace(root);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--base-url" => match args.next() {
                Some(url) => config.base_url = url.trim_end_matches('/').to_string(),
                None => {
                    eprintln!("xtask site: --base-url needs a value\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--nginx-redirects" => match args.next() {
                Some(path) => nginx = Some(PathBuf::from(path)),
                None => {
                    eprintln!("xtask site: --nginx-redirects needs a value\n{USAGE}");
                    return ExitCode::from(2);
                }
            },
            other if other.starts_with('-') || out_dir.is_some() => {
                eprintln!("xtask site: unexpected argument {other:?}\n{USAGE}");
                return ExitCode::from(2);
            }
            other => out_dir = Some(PathBuf::from(other)),
        }
    }
    let Some(out_dir) = out_dir else {
        eprintln!("xtask site: missing <out-dir>\n{USAGE}");
        return ExitCode::from(2);
    };
    if !config.landing_dir().is_dir() {
        eprintln!(
            "xtask site: {} is missing; the site needs its landing page",
            config.landing_dir().display()
        );
        return ExitCode::FAILURE;
    }
    let result = build(&config).and_then(|site| {
        let count = write(&site, &out_dir)?;
        if let Some(nginx) = &nginx {
            std::fs::write(nginx, &site.nginx_redirects)
                .map_err(|e| format!("cannot write {}: {e}", nginx.display()))?;
        }
        Ok(count)
    });
    match result {
        Ok(count) => {
            println!("xtask site: wrote {count} files to {}", out_dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask site: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The text of the first `# ` heading, skipping front matter and code fences.
fn first_heading(body: &str) -> Option<&str> {
    let mut lines = body.lines().peekable();
    if lines.peek() == Some(&"---") {
        lines.next();
        for line in lines.by_ref() {
            if line == "---" {
                break;
            }
        }
    }
    let mut fence: Option<&str> = None;
    for line in lines {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
        } else if trimmed.starts_with("~~~") {
            fence = Some("~~~");
        } else if let Some(heading) = line.strip_prefix("# ") {
            return Some(heading.trim());
        }
    }
    None
}

/// Every Markdown image target outside code fences: inline (`![alt](target)`)
/// and reference-style (`![alt][label]`, `![alt][]`, `![alt]`) resolved
/// through the page's link reference definitions. A reference-style image with
/// no matching definition is plain text, not an image.
fn image_targets(body: &str) -> Vec<&str> {
    let definitions: BTreeMap<String, &str> = reference_definitions(body)
        .into_iter()
        .rev() // the first definition of a label wins, as in CommonMark
        .collect();
    let mut targets = Vec::new();
    for line in unfenced_lines(body) {
        let mut rest = line;
        while let Some(start) = rest.find("![") {
            let after = &rest[start + 2..];
            let Some(close) = alt_end(after) else { break };
            let alt = &after[..close];
            let tail = &after[close + 1..];
            if let Some(target_start) = tail.strip_prefix('(') {
                let Some(end) = target_start.find(')') else {
                    break;
                };
                let target = target_start[..end]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches('<')
                    .trim_end_matches('>');
                if !target.is_empty() {
                    targets.push(target);
                }
                rest = &target_start[end + 1..];
                continue;
            }
            let (label, next) = match tail.strip_prefix('[').and_then(|l| {
                let end = label_end(l)?;
                Some((&l[..end], &l[end + 1..]))
            }) {
                Some(("", next)) => (alt, next),
                Some((label, next)) => (label, next),
                None => (alt, tail),
            };
            if let Some(&target) = definitions.get(&normalize_label(label)) {
                targets.push(target);
            }
            rest = next;
        }
    }
    targets
}

/// The byte offset of the `]` closing an image's alt text that starts just
/// after its `![`, allowing nested brackets and backslash escapes.
fn alt_end(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '[' => depth += 1,
            ']' if depth == 0 => return Some(i),
            ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Check every image `page` references and return the RELATIVE ones as paths
/// under `docs/`. Root-absolute `/img/…` references must exist in the image
/// directory, which is published whole at `/img/`; external URLs are skipped.
fn relative_images(config: &SiteConfig, page: &Page, body: &str) -> Result<Vec<String>, String> {
    let mut relative = Vec::new();
    for target in image_targets(body) {
        if target.contains("://") || target.starts_with("data:") {
            continue;
        }
        let path = target.split(['#', '?']).next().unwrap_or(target);
        if let Some(rest) = path.strip_prefix("/img/") {
            let resolved = normalize(Path::new("img"), rest)
                .filter(|p| p.starts_with("img/"))
                .ok_or_else(|| format!("page `{}`: bad image path `{target}`", page.slug))?;
            if !config.docs_dir.join(&resolved).is_file() {
                return Err(format!(
                    "page `{}`: image `{target}` does not exist (looked for docs/{resolved})",
                    page.slug
                ));
            }
            continue;
        }
        if path.starts_with('/') {
            return Err(format!(
                "page `{}`: image `{target}` is root-absolute but not under /img/",
                page.slug
            ));
        }
        let page_dir = Path::new(&page.slug)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let resolved = normalize(page_dir, path).ok_or_else(|| {
            format!(
                "page `{}`: image `{target}` points outside docs/",
                page.slug
            )
        })?;
        if resolved.split('/').next() == Some(UNPUBLISHED_DIR) {
            return Err(format!(
                "page `{}`: image `{target}` is under docs/{UNPUBLISHED_DIR}/",
                page.slug
            ));
        }
        if !config.docs_dir.join(&resolved).is_file() {
            return Err(format!(
                "page `{}`: image `{target}` does not exist (looked for docs/{resolved})",
                page.slug
            ));
        }
        relative.push(resolved);
    }
    Ok(relative)
}

/// Lexically join `rel` onto `base` (both relative), resolving `.` and `..`.
/// `None` if the result escapes `base`'s root or is empty.
fn normalize(base: &Path, rel: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for component in base.join(rel).components() {
        match component {
            Component::Normal(s) => parts.push(s.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Copy every regular file under `dir` into `site` below `prefix`. A symlink
/// inside the tree is refused rather than followed, so nothing outside it can
/// be published through one, and so is a file without one of the
/// [`published_docs::IMAGE_EXTENSIONS`]: the build fails naming it rather than
/// publishing it or dropping it silently. (`dir` is the canonical image
/// directory from [`published_docs::image_dir`], which resolves the `docs/img`
/// symlink.)
fn copy_tree(dir: &Path, prefix: &str, site: &mut Site) -> Result<(), String> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|n| format!("non-UTF-8 file name {n:?} in {}", dir.display()))?;
        let target = format!("{prefix}/{name}");
        let kind = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
            .file_type();
        if kind.is_symlink() {
            return Err(format!(
                "{} is a symlink; the image tree must hold only files and directories",
                path.display()
            ));
        } else if kind.is_dir() {
            copy_tree(&path, &target, site)?;
        } else if kind.is_file() {
            if !published_docs::is_image_file(&path) {
                return Err(format!(
                    "{} is in the image directory but does not have an image extension ({}); \
                     only images are published at /img/",
                    path.display(),
                    published_docs::IMAGE_EXTENSIONS.join(", ")
                ));
            }
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            site.insert(target, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
