//! The published-docs manifest, `docs/published.toml` (PRD #1419, Decision 5).
//!
//! The manifest is the single list of published pages. The `cargo xtask site`
//! generator (`xtask/site`) and the binary's embedded `docs` topics both read
//! it through this ONE parser, so the site and the binary cannot disagree about
//! what a manifest says. That is why this file depends on nothing but `std` and
//! `toml`: it is compiled into more than one crate by `#[path]` (the way
//! `build_version_resolve.rs` is shared between `build.rs` and a test), and each
//! includer only has to provide `toml`. (The unit tests at the bottom also use
//! `tempfile`; they compile only under `cfg(test)`, which `build.rs` never is.)
//!
//! Parsing is strict. An unknown key, a missing or empty field, a multi-line
//! value, a duplicate slug or a slug outside the grammar below is an error,
//! because a manifest that parses leniently publishes whatever the lenient
//! reading happened to accept.
//!
//! The same two consumers also resolve every file they publish through here
//! ([`page_source`], [`image_source`], [`image_dir`]), because a valid slug is
//! not enough: `docs/alias.md` can be a symlink to `develop/secret.md`. The
//! publication boundary is therefore checked on the CANONICAL path, once, for
//! both the site and the binary.

use std::fmt;
use std::path::{Path, PathBuf};

/// Where the manifest lives, relative to the repository root.
pub const MANIFEST_PATH: &str = "docs/published.toml";

/// The top-level directory under `docs/` that is never published.
pub const UNPUBLISHED_DIR: &str = "develop";

/// The image directory under `docs/`, and the ONE place a published file may
/// resolve outside `docs/`: in this repository `docs/img` is a symlink to
/// `../site/static/img`, the images Docusaurus used to serve. It may resolve
/// to exactly that directory ([`IMAGE_TARGET`]) or be a real directory inside
/// `docs/`, and nowhere else.
pub const IMAGE_DIR: &str = "img";

/// Where `docs/img` may point, relative to the directory holding `docs/` (the
/// workspace root), the same way the landing page's `site/landing/` is found.
pub const IMAGE_TARGET: &str = "site/static/img";

/// The file extensions (lowercase) an image or web asset may have to be
/// published: what `site/static/img` holds (screenshots, the logo and the
/// favicon) plus the other common image formats. Anything else is refused.
pub const IMAGE_EXTENSIONS: &[&str] = &["avif", "gif", "ico", "jpeg", "jpg", "png", "svg", "webp"];

/// Whether `path` has one of the [`IMAGE_EXTENSIONS`], case-insensitively.
pub fn is_image_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

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

/// Why a file was refused at the publication boundary. The message names the
/// page it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundaryError(pub String);

impl fmt::Display for BoundaryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BoundaryError {}

/// The canonical locations the boundary is checked against.
struct Boundary {
    docs: PathBuf,
    /// `docs/develop` as a directory entry of the canonical `docs/`, and, when
    /// it exists, its own canonical path (they differ if it is a symlink).
    develop: Vec<PathBuf>,
}

impl Boundary {
    fn new(docs_dir: &Path) -> Result<Self, BoundaryError> {
        let docs = docs_dir
            .canonicalize()
            .map_err(|e| BoundaryError(format!("cannot resolve {}: {e}", docs_dir.display())))?;
        let lexical = docs.join(UNPUBLISHED_DIR);
        let mut develop = vec![lexical.clone()];
        if let Ok(real) = lexical.canonicalize()
            && real != lexical
        {
            develop.push(real);
        }
        Ok(Self { docs, develop })
    }

    fn is_unpublished(&self, real: &Path) -> bool {
        self.develop.iter().any(|d| real.starts_with(d))
    }

    /// The canonical image directory. It must be either a real directory at
    /// `docs/img` (inside the published tree already) or resolve to exactly
    /// the canonical `<workspace>/site/static/img`, where `<workspace>` is the
    /// directory holding `docs/`. Any other target is refused, because the
    /// generator publishes the whole image directory: `docs/img -> ../prds`
    /// would otherwise publish the PRDs.
    fn image_dir(&self) -> Result<PathBuf, BoundaryError> {
        let lexical = self.docs.join(IMAGE_DIR);
        let real = lexical
            .canonicalize()
            .map_err(|e| BoundaryError(format!("cannot resolve {}: {e}", lexical.display())))?;
        let expected = self
            .docs
            .parent()
            .map(|workspace| workspace.join(IMAGE_TARGET))
            .and_then(|target| target.canonicalize().ok());
        if real == lexical || Some(&real) == expected.as_ref() {
            return Ok(real);
        }
        Err(BoundaryError(format!(
            "docs/{IMAGE_DIR} resolves to {}; it must be a directory inside docs/ or resolve \
             to {IMAGE_TARGET} beside docs/",
            real.display()
        )))
    }

    /// Resolve `docs/<relative>` and refuse it unless its canonical path is
    /// inside `docs/` and outside `docs/develop/` — or, when `allow_images`,
    /// inside the image directory.
    fn resolve(
        &self,
        docs_dir: &Path,
        relative: &str,
        slug: &str,
        what: &str,
        allow_images: bool,
    ) -> Result<PathBuf, BoundaryError> {
        let lexical = docs_dir.join(relative);
        let real = lexical.canonicalize().map_err(|e| {
            BoundaryError(if allow_images {
                format!(
                    "page `{slug}`: {what} {} cannot be read: {e}",
                    lexical.display()
                )
            } else {
                format!(
                    "page `{slug}` is in the manifest but {} cannot be read: {e}",
                    lexical.display()
                )
            })
        })?;
        let via_symlink = if real == self.docs.join(relative) {
            ""
        } else {
            " through a symlink"
        };
        if self.is_unpublished(&real) {
            return Err(BoundaryError(format!(
                "page `{slug}`: {what} docs/{relative} resolves{via_symlink} to {}, under \
                 docs/{UNPUBLISHED_DIR}/, which is never published",
                real.display()
            )));
        }
        if real.starts_with(&self.docs) {
            return Ok(real);
        }
        if allow_images && real.starts_with(self.image_dir()?) {
            return Ok(real);
        }
        Err(BoundaryError(format!(
            "page `{slug}`: {what} docs/{relative} resolves{via_symlink} to {}, outside docs/{}",
            real.display(),
            if allow_images {
                format!(" and outside docs/{IMAGE_DIR}/")
            } else {
                String::new()
            }
        )))
    }
}

/// The file to read `page` from: `<docs_dir>/<slug>.md`, canonicalized, and
/// refused unless it resolves inside `docs/` and outside `docs/develop/`. A
/// symlink that stays in the published part of `docs/` is allowed; one into
/// `docs/develop/` or out of `docs/` is not. Both the site generator and
/// `build.rs` call this, so the binary cannot embed what the site refuses.
pub fn page_source(docs_dir: &Path, page: &Page) -> Result<PathBuf, BoundaryError> {
    Boundary::new(docs_dir)?.resolve(docs_dir, &page.file_name(), &page.slug, "page", false)
}

/// The file to read an image from that `page` references, given as a path
/// relative to `docs/` (already lexically normalized by the caller).
/// Canonicalized and refused unless it resolves inside `docs/` outside
/// `docs/develop/`, or inside [`image_dir`] — the one directory allowed to
/// live outside `docs/`.
/// Also refused unless the file has one of the [`IMAGE_EXTENSIONS`], so an
/// image reference cannot publish a page or a config file.
pub fn image_source(
    docs_dir: &Path,
    page: &Page,
    relative: &str,
) -> Result<PathBuf, BoundaryError> {
    let real = Boundary::new(docs_dir)?.resolve(docs_dir, relative, &page.slug, "image", true)?;
    if !is_image_file(&real) {
        return Err(BoundaryError(format!(
            "page `{}`: image docs/{relative} resolves to {}, which does not have an image \
             extension ({})",
            page.slug,
            real.display(),
            IMAGE_EXTENSIONS.join(", ")
        )));
    }
    Ok(real)
}

/// The canonical image directory (`docs/img`): a real directory inside
/// `docs/`, or a symlink resolving to exactly [`IMAGE_TARGET`] beside `docs/`.
pub fn image_dir(docs_dir: &Path) -> Result<PathBuf, BoundaryError> {
    Boundary::new(docs_dir)?.image_dir()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    fn page(slug: &str) -> Page {
        Page {
            slug: slug.to_string(),
            title: "T".to_string(),
            description: "D".to_string(),
        }
    }

    /// A repository-shaped tree: `docs/` with a maintainer page under
    /// `docs/develop/`, and `docs/img -> ../site/static/img` as in this repo.
    fn tree() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let docs = root.path().join("docs");
        fs::create_dir_all(docs.join("develop")).unwrap();
        fs::write(docs.join("develop/secret.md"), "# Secret\n").unwrap();
        fs::write(docs.join("start.md"), "# Start\n").unwrap();
        fs::create_dir_all(root.path().join("site/static/img")).unwrap();
        fs::write(root.path().join("site/static/img/shot.png"), b"png").unwrap();
        symlink("../site/static/img", docs.join("img")).unwrap();
        (root, docs)
    }

    /// Scenario: `build.rs` resolves each manifest page through
    /// `page_source` before embedding it. A page symlinked to a maintainer
    /// page is refused with an error naming the slug and docs/develop/, so
    /// the binary cannot embed it.
    #[test]
    fn page_source_refuses_symlink_into_develop() {
        let (_root, docs) = tree();
        symlink("develop/secret.md", docs.join("alias.md")).unwrap();
        let err = page_source(&docs, &page("alias")).unwrap_err().to_string();
        assert!(err.contains("`alias`"), "{err}");
        assert!(err.contains("docs/develop/"), "{err}");
        assert!(err.contains("through a symlink"), "{err}");
    }

    #[test]
    fn page_source_refuses_symlink_outside_docs() {
        let (root, docs) = tree();
        fs::write(root.path().join("README.md"), "# Readme\n").unwrap();
        symlink("../README.md", docs.join("readme.md")).unwrap();
        let err = page_source(&docs, &page("readme")).unwrap_err().to_string();
        assert!(err.contains("outside docs/"), "{err}");
        // A page may not live in the image directory, which is outside docs/.
        fs::write(root.path().join("site/static/img/page.md"), "# P\n").unwrap();
        assert!(page_source(&docs, &page("img/page")).is_err());
    }

    #[test]
    fn page_source_refuses_symlinked_develop_directory_target() {
        let (root, docs) = tree();
        // docs/develop itself relocated behind a symlink: its real location is
        // still unpublished.
        fs::rename(docs.join("develop"), root.path().join("maint")).unwrap();
        symlink("../maint", docs.join("develop")).unwrap();
        symlink("develop/secret.md", docs.join("alias.md")).unwrap();
        let err = page_source(&docs, &page("alias")).unwrap_err().to_string();
        assert!(err.contains("docs/develop/"), "{err}");
    }

    #[test]
    fn page_source_allows_plain_pages_and_symlinks_within_published_docs() {
        let (_root, docs) = tree();
        assert_eq!(
            page_source(&docs, &page("start")).unwrap(),
            docs.canonicalize().unwrap().join("start.md")
        );
        symlink("start.md", docs.join("begin.md")).unwrap();
        assert_eq!(
            page_source(&docs, &page("begin")).unwrap(),
            docs.canonicalize().unwrap().join("start.md")
        );
    }

    #[test]
    fn image_source_allows_the_symlinked_image_dir_but_not_develop() {
        let (root, docs) = tree();
        let start = page("start");
        assert_eq!(
            image_source(&docs, &start, "img/shot.png").unwrap(),
            root.path()
                .canonicalize()
                .unwrap()
                .join("site/static/img/shot.png")
        );
        fs::write(docs.join("develop/diagram.png"), b"png").unwrap();
        symlink("develop/diagram.png", docs.join("diagram.png")).unwrap();
        let err = image_source(&docs, &start, "diagram.png")
            .unwrap_err()
            .to_string();
        assert!(err.contains("docs/develop/"), "{err}");
        fs::write(root.path().join("other.png"), b"png").unwrap();
        symlink("../other.png", docs.join("other.png")).unwrap();
        assert!(image_source(&docs, &start, "other.png").is_err());
    }

    #[test]
    fn image_dir_refuses_develop_and_ancestors_of_docs() {
        let (root, docs) = tree();
        assert_eq!(
            image_dir(&docs).unwrap(),
            root.path().canonicalize().unwrap().join("site/static/img")
        );
        fs::remove_file(docs.join("img")).unwrap();
        symlink("develop", docs.join("img")).unwrap();
        assert!(image_dir(&docs).is_err());
        fs::remove_file(docs.join("img")).unwrap();
        symlink("..", docs.join("img")).unwrap();
        assert!(image_dir(&docs).is_err());
    }

    /// Scenario: `docs/img` is repointed at a sibling directory of the
    /// workspace (here `prds/`, and a directory that merely sits inside
    /// `site/static/`). The image directory is refused, so the generator
    /// cannot publish that directory's files at `/img/`.
    #[test]
    fn image_dir_refuses_any_target_but_site_static_img() {
        let (root, docs) = tree();
        fs::create_dir_all(root.path().join("prds")).unwrap();
        fs::write(root.path().join("prds/1-plan.md"), "# Plan\n").unwrap();
        fs::remove_file(docs.join("img")).unwrap();
        symlink("../prds", docs.join("img")).unwrap();
        let err = image_dir(&docs).unwrap_err().to_string();
        assert!(err.contains("site/static/img"), "{err}");
        fs::remove_file(docs.join("img")).unwrap();
        symlink("../site/static", docs.join("img")).unwrap();
        assert!(image_dir(&docs).is_err());
        // A real directory at docs/img is inside the published tree already.
        fs::remove_file(docs.join("img")).unwrap();
        fs::create_dir(docs.join("img")).unwrap();
        assert_eq!(
            image_dir(&docs).unwrap(),
            docs.canonicalize().unwrap().join("img")
        );
    }

    #[test]
    fn image_source_refuses_a_file_without_an_image_extension() {
        let (root, docs) = tree();
        fs::write(root.path().join("site/static/img/notes.md"), "# N\n").unwrap();
        let err = image_source(&docs, &page("start"), "img/notes.md")
            .unwrap_err()
            .to_string();
        assert!(err.contains("image extension"), "{err}");
        assert!(is_image_file(Path::new("a/B.PNG")));
        assert!(!is_image_file(Path::new("a/b")));
    }
}
