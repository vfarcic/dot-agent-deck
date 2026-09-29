//! The published-docs manifest, `docs/published.toml` (PRD #1419, Decision 5).
//!
//! The manifest is the single list of published pages. The `cargo xtask site`
//! generator (`xtask/site`) and the binary's embedded `docs` topics both read
//! it through this ONE parser, so the site and the binary cannot disagree about
//! what a manifest says. That is why this file depends on nothing but `std` and
//! `toml`: it is compiled into more than one crate by `#[path]` (the way
//! `build_version_resolve.rs` is shared between `build.rs` and a test), and each
//! includer only has to provide `toml`.
//!
//! Parsing is strict. An unknown key, a missing or empty field, a multi-line
//! value, a duplicate slug or a slug outside the grammar below is an error,
//! because a manifest that parses leniently publishes whatever the lenient
//! reading happened to accept.

use std::fmt;
use std::path::Path;

/// Where the manifest lives, relative to the repository root.
pub const MANIFEST_PATH: &str = "docs/published.toml";

/// The top-level directory under `docs/` that is never published.
pub const UNPUBLISHED_DIR: &str = "develop";

/// One published page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The page's path under `docs/` without `.md`, e.g. `desktop/voice`.
    pub slug: String,
    /// The page's title, which is also its first `#` heading.
    pub title: String,
    /// One line saying what the page is for.
    pub description: String,
}

impl Page {
    /// The page's source file, relative to the `docs/` directory.
    pub fn file_name(&self) -> String {
        format!("{}.md", self.slug)
    }
}

/// Why a manifest was rejected. The message names the offending entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError(pub String);

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{MANIFEST_PATH}: {}", self.0)
    }
}

impl std::error::Error for ManifestError {}

/// Read and parse `<docs_dir>/published.toml`.
pub fn read(docs_dir: &Path) -> Result<Vec<Page>, ManifestError> {
    let path = docs_dir.join("published.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| ManifestError(format!("cannot read {}: {e}", path.display())))?;
    parse(&text)
}

/// Parse the manifest text into its pages, in manifest order.
pub fn parse(text: &str) -> Result<Vec<Page>, ManifestError> {
    let table: toml::Table = text
        .parse()
        .map_err(|e| ManifestError(format!("not valid TOML: {e}")))?;

    for key in table.keys() {
        if key != "page" {
            return Err(ManifestError(format!(
                "unknown top-level key `{key}` (only `[[page]]` entries are allowed)"
            )));
        }
    }
    let entries = match table.get("page") {
        Some(toml::Value::Array(entries)) => entries,
        Some(_) => {
            return Err(ManifestError(
                "`page` must be an array of tables (`[[page]]`)".to_string(),
            ));
        }
        None => return Err(ManifestError("no `[[page]]` entries".to_string())),
    };
    if entries.is_empty() {
        return Err(ManifestError("no `[[page]]` entries".to_string()));
    }

    let mut pages: Vec<Page> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let number = index + 1;
        let toml::Value::Table(entry) = entry else {
            return Err(ManifestError(format!("page #{number} is not a table")));
        };
        for key in entry.keys() {
            if !matches!(key.as_str(), "slug" | "title" | "description") {
                return Err(ManifestError(format!(
                    "page #{number}: unknown key `{key}` (allowed: slug, title, description)"
                )));
            }
        }
        let field = |name: &str| -> Result<String, ManifestError> {
            let value = match entry.get(name) {
                Some(toml::Value::String(s)) => s,
                Some(_) => {
                    return Err(ManifestError(format!(
                        "page #{number}: `{name}` must be a string"
                    )));
                }
                None => {
                    return Err(ManifestError(format!("page #{number}: missing `{name}`")));
                }
            };
            if value.trim().is_empty() {
                return Err(ManifestError(format!("page #{number}: `{name}` is empty")));
            }
            if value.contains(['\n', '\r']) {
                return Err(ManifestError(format!(
                    "page #{number}: `{name}` must be a single line"
                )));
            }
            if value.trim() != value {
                return Err(ManifestError(format!(
                    "page #{number}: `{name}` has leading or trailing whitespace"
                )));
            }
            Ok(value.clone())
        };
        let slug = field("slug")?;
        let title = field("title")?;
        let description = field("description")?;

        check_slug(&slug).map_err(|why| ManifestError(format!("page #{number}: {why}")))?;
        if let Some(earlier) = pages.iter().position(|p| p.slug == slug) {
            return Err(ManifestError(format!(
                "page #{number}: slug `{slug}` is already used by page #{}",
                earlier + 1
            )));
        }
        pages.push(Page {
            slug,
            title,
            description,
        });
    }
    Ok(pages)
}

/// A slug is one or more `/`-separated segments of lowercase ASCII letters,
/// digits and `-`, each starting with a letter or digit, and its first segment
/// is not [`UNPUBLISHED_DIR`]. That rules out `..`, absolute paths, a `.md`
/// suffix and anything under `docs/develop/`.
fn check_slug(slug: &str) -> Result<(), String> {
    for segment in slug.split('/') {
        let valid = segment
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && segment
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return Err(format!(
                "slug `{slug}` is not a path of lowercase `[a-z0-9-]` segments \
                 (write the path under docs/ without `.md`, e.g. `desktop/voice`)"
            ));
        }
    }
    if slug.split('/').next() == Some(UNPUBLISHED_DIR) {
        return Err(format!(
            "slug `{slug}` is under docs/{UNPUBLISHED_DIR}/, which is never published"
        ));
    }
    Ok(())
}
