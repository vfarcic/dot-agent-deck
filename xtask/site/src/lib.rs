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
//!   `site/static/img`), at the `/img/` paths Docusaurus served it at, so
//!   root-absolute `/img/x.png` references and existing image URLs resolve.
//! - `docs/<path>` for each image a page references by a RELATIVE path, so
//!   `img/x.png` in a top-level page (or `../img/x.png` in a desktop page)
//!   resolves on the site exactly as it does on disk.
//! - the landing page, from [`landing_page`] — the hook the "Site replaced"
//!   milestone fills in.
//!
//! Nothing under `docs/develop/` can reach the output: the parser rejects a
//! `develop/` slug, image references are confined to the docs tree outside
//! `develop/`, and [`build`] refuses an output path under `docs/develop/`.

#[path = "../../../src/published_docs.rs"]
pub mod published_docs;

use std::collections::BTreeMap;
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

    fn page_url(&self, page: &Page) -> String {
        format!("{}/docs/{}", self.base_url, page.file_name())
    }
}

/// The generated site: output path (relative, `/`-separated) → bytes.
#[derive(Debug, Default)]
pub struct Site {
    pub files: BTreeMap<String, Vec<u8>>,
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
        let source = config.docs_dir.join(page.file_name());
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
            let bytes = std::fs::read(config.docs_dir.join(&image))
                .map_err(|e| format!("page `{}`: cannot read image `{image}`: {e}", page.slug))?;
            site.insert(format!("docs/{image}"), bytes)?;
        }
        site.insert(
            format!("docs/{}", page.file_name()),
            body.clone().into_bytes(),
        )?;
        bodies.push(body);
    }

    copy_tree(&config.docs_dir.join("img"), "img", &mut site)?;
    site.insert(
        "llms.txt".to_string(),
        llms_txt(config, &pages).into_bytes(),
    )?;
    site.insert(
        "llms-full.txt".to_string(),
        llms_full_txt(config, &pages, &bodies).into_bytes(),
    )?;
    for (path, bytes) in landing_page(config, &pages)? {
        site.insert(path, bytes)?;
    }
    Ok(site)
}

/// The landing page and its assets (PRD #1419, "Site replaced" milestone).
///
/// HOOK: this returns nothing until the static landing page exists. The
/// milestone that ports the #1155 page to static HTML/CSS makes this return
/// `index.html` and its stylesheet/script, generated from the same `pages` so
/// the page's plain `<a href>` docs links come from the manifest. Everything it
/// returns goes through [`Site::insert`], so the `docs/develop/` guard applies.
pub fn landing_page(
    _config: &SiteConfig,
    _pages: &[Page],
) -> Result<Vec<(String, Vec<u8>)>, String> {
    Ok(Vec::new())
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
         below `/docs/` without `.md`, for example `dot-agent-deck docs desktop/voice`.\n\n",
    );
    out.push_str(&format!(
        "Every page in one file: [llms-full.txt]({}/llms-full.txt)\n\n",
        config.base_url
    ));
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

const USAGE: &str = "usage: cargo xtask site <out-dir> [--base-url <url>]";

/// `cargo xtask site` entry point. `root` is the workspace root.
pub fn run(root: &Path, args: &[String]) -> ExitCode {
    let mut out_dir: Option<PathBuf> = None;
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
    let result = build(&config).and_then(|site| write(&site, &out_dir));
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

/// Every Markdown image target (`![alt](target)`) outside code fences.
fn image_targets(body: &str) -> Vec<&str> {
    let mut targets = Vec::new();
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
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("![") {
            let after = &rest[start + 2..];
            let Some(close) = after.find("](") else { break };
            let target_start = &after[close + 2..];
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
/// be published through one. (`dir` itself may be a symlink: `docs/img` is.)
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
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            site.insert(target, bytes)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
