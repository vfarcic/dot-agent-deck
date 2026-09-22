//! Issue #1200 — the `site-image-refs` rule: every local image reference under
//! `docs/` and `site/src/` resolves to a file in `site/static/img/`.
//!
//! ## What it is for
//!
//! A reference to a file under `site/static/` that does not exist **builds
//! clean**. Docusaurus does not resolve an image path at build time — it is a
//! string that reaches the browser verbatim — so `onBrokenLinks: 'throw'` never
//! sees it, the page ships, and the browser 404s. No compiler, no bundler and
//! no link checker looks at it either, which is the whole reason this is a
//! `linkage-check` rule rather than something a build step would have caught.
//!
//! Issue #1200 was filed off two instances inside one pull request (#1155).
//! One worker re-encoded the images to WebP and deleted `busy-deck.png` while
//! another had just wired a landing-page row to that exact path: green build,
//! broken image, caught only by loading the page. And `docs/orchestration.md`
//! carried a raw `<img src="./img/…">` whose `src` MDX leaves alone, so the
//! built page kept the literal relative path and the browser resolved it
//! against the page URL as `/docs/img/…`, which is never emitted — it had been
//! 404ing on the published site for months, found only because someone was
//! reading that page for another reason.
//!
//! ## One resolution target, both spellings
//!
//! `docs/img` is a **symlink** to `../site/static/img`, so the two spellings
//! this rule matches end at the same bytes on disk by two different routes:
//! Docusaurus serves `/img/x.png` out of `site/static/`, and MDX resolves a
//! relative `./img/x.png` as a file reference next to the source page, which
//! the symlink forwards. Both therefore resolve here against
//! [`IMAGE_ROOT`] alone, and nothing in this rule reads the symlink — which is
//! also what keeps it working on a Windows checkout, where git may materialise
//! a symlink as a text file holding its target.
//!
//! ## Which reference forms are covered, and why
//!
//! The scan is over **text**, not over any one syntax: it matches `/img/…` and
//! `./img/…` wherever they appear, with a known image extension on the end, in
//! every UTF-8 file under the scanned trees. That is deliberate and it is the
//! only version that covers the instances above, which arrived in three
//! different syntaxes — a Markdown image (`![alt](/img/x.png)`), a JavaScript
//! string (`src: '/img/x.png'` in `site/src/data/landing-content.js`) and a raw
//! HTML/JSX `<img src="./img/x.png">`. A syntax-aware rule would have to be
//! taught each one, and the raw-`<img>` instance is precisely the form the
//! Markdown-aware tooling already skips.
//!
//! What is **not** covered, said plainly rather than implied:
//!
//! - **Absolute and protocol-relative URLs.** `https://img.youtube.com/vi/…`
//!   and `https://cdn.example.com/img/x.png` are not ours to resolve. The left
//!   boundary in [`image_ref_re`] is what excludes them: the character before
//!   `/img/` has to be a delimiter, and in a URL it is the tail of the host.
//! - **The bare `img/…` and `../img/…` spellings.** No file in either tree uses
//!   them today. They are left out because every character added to the left of
//!   the match widens what a repo-relative path in prose can trip — `docs/img/`
//!   and `site/static/img/` are written in prose in both trees and must not be
//!   read as references.
//! - **Whether a relative spelling is right for a *nested* page.** A `./img/x.png`
//!   in `docs/develop/` would need `docs/develop/img/`, and this rule would
//!   still resolve it against `site/static/img/` and pass. The published docs
//!   are flat (`docs/*.md`), so the case does not arise today; closing it would
//!   mean resolving through the symlink, which the Windows note above rules out.
//! - **An image under `site/static/img/` that nothing references.** A different
//!   question, and a harmless state rather than a broken page.
//! - **A reference whose case does not match the file's.** The extension is
//!   matched case-insensitively so the reference is *seen*, and the name is
//!   then resolved exactly — so `/img/Modes.png` against `modes.png` is
//!   reported, which is right on the Linux host that serves the site and
//!   stricter than a case-insensitive developer machine would be.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use regex::Regex;

use crate::paths::slash_path;

/// The trees scanned for references.
///
/// `docs/` covers the published pages *and* the maintainer docs under
/// `docs/develop/` — they are the same tree, they resolve against the same
/// directory, and a developer doc with a dead image in it is still a dead
/// image. `site/src/` is where the landing page keeps its screenshots, in a
/// data module rather than in Markdown, which is how #1200's first instance
/// got in.
const SCANNED_DIRS: &[&str] = &["docs", "site/src"];

/// Where every reference resolves. See the module docs for why one target
/// covers both spellings.
const IMAGE_ROOT: &str = "site/static/img";

/// Opt-out marker, on the referencing line.
///
/// In a Markdown file put it in a trailing same-line HTML comment —
/// `![alt](/img/x.png) <!-- linkage-check:allow-missing-image -->` — which
/// renders as nothing. As with `BARE_TEMPDIR_ALLOW`, the exception is
/// declared where it is taken, so review sees it in the diff that needs it
/// rather than in a list somewhere else.
const MISSING_IMAGE_ALLOW: &str = "linkage-check:allow-missing-image";

/// The rule, quoted at every finding.
pub const MISSING_IMAGE_RULE: &str = "so the page ships a 404 instead of failing the build: \
     Docusaurus does not resolve an image path at build time, which is why \
     `onBrokenLinks: 'throw'` never sees this one (issue #1200). Restore the \
     file, re-point the reference, or declare the exception with \
     `linkage-check:allow-missing-image` on the line";

/// File extensions a reference must end in to be one.
///
/// The extension is what keeps the rule off prose. Both trees discuss their own
/// image paths in comments and in sentences — the landing-page data module
/// records why `orchestration-config.png` was deleted, and
/// `docs/develop/linkage-check-rules.md` quotes the two spellings — and a bare
/// `/img/` with no file name after it is not a reference to anything.
///
/// Matched case-insensitively, so an `.PNG` reference is seen; the file name it
/// then resolves to is compared exactly, because that is how the server serves
/// it.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "svg", "gif", "avif", "ico"];

/// `/img/…` or `./img/…` with an image extension, preceded by a delimiter.
///
/// The left boundary is the load-bearing part. `https://cdn.example.com/img/x.png`
/// has `m` before its `/img/`, and `site/static/img/x.png` has `/`; neither is
/// in the set, so neither matches. A reference in every syntax the trees
/// actually use is preceded by one that is: `(` in Markdown and CSS `url()`,
/// `'` or `"` in a JavaScript string or an HTML attribute, `=` in JSX, or
/// whitespace in prose.
fn image_ref_re() -> Regex {
    let exts = IMAGE_EXTENSIONS.join("|");
    Regex::new(&format!(
        r#"(?:^|[\s"'`(\[=,;:>{{])(\.?/img/[A-Za-z0-9._][A-Za-z0-9._/-]*\.(?i:{exts}))\b"#
    ))
    .expect("image reference regex compiles")
}

/// Every reference in one line, without its boundary character.
fn refs_in_line(re: &Regex, line: &str) -> Vec<String> {
    re.captures_iter(line).map(|c| c[1].to_string()).collect()
}

/// The path a reference resolves to, relative to [`IMAGE_ROOT`].
///
/// Both spellings carry the same tail, so this is the tail: `/img/a/b.png` and
/// `./img/a/b.png` both resolve `a/b.png`.
fn resolved_name(reference: &str) -> &str {
    reference
        .trim_start_matches('.')
        .trim_start_matches('/')
        .strip_prefix("img/")
        .unwrap_or(reference)
}

/// One scanned file: its repo-relative display path and its contents.
type TextFile = (String, String);

/// The findings for a set of files against the set of image paths that exist.
///
/// Takes the contents rather than reading them, for the reason
/// `self_contained_violations` in `main.rs` does: a rule whose only coverage is
/// the live checkout tests nothing whenever that checkout is clean, which is
/// its normal state and would be its state on the day it silently stopped
/// matching.
fn check(files: &[TextFile], available: &BTreeSet<String>) -> Vec<String> {
    let re = image_ref_re();
    let mut out = Vec::new();
    let mut seen_any = false;

    for (display, text) in files {
        for (idx, line) in text.lines().enumerate() {
            for reference in refs_in_line(&re, line) {
                seen_any = true;
                if line.contains(MISSING_IMAGE_ALLOW) {
                    continue;
                }
                let name = resolved_name(&reference);
                if available.contains(name) {
                    continue;
                }
                out.push(format!(
                    "{display}:{}: `{reference}` resolves to `{IMAGE_ROOT}/{name}`, which does not exist — {MISSING_IMAGE_RULE}",
                    idx + 1
                ));
            }
        }
    }

    // A scan that matched nothing would report nothing, which is the shape in
    // which this rule dies: the reference forms are text, and text gets
    // reworded. `site/static/img/` holding images that no page in either tree
    // reaches is the tell, and it is cheap to refuse.
    if !seen_any && !available.is_empty() {
        out.push(format!(
            "no `/img/…` or `./img/…` reference found anywhere under {} while `{IMAGE_ROOT}/` \
             holds {} image(s) — this rule now covers nothing. Either every image reference was \
             removed, or the spelling changed and the scan in \
             `xtask/linkage-check/src/site_image_refs.rs` has to learn it (issue #1200)",
            SCANNED_DIRS.join(" and "),
            available.len()
        ));
    }

    out
}

/// Every image path under `<root>/site/static/img`, `/`-joined and relative to
/// it. `None` when the directory itself cannot be read — which is a finding
/// rather than an empty set, since a rule with no resolution target covers
/// nothing.
fn available_images(root: &Path) -> Option<BTreeSet<String>> {
    let base = root.join(IMAGE_ROOT);
    // Probed here rather than inside the recursion so that only the ROOT going
    // missing is fatal: an unreadable subdirectory deeper in leaves the
    // references under it unresolved, which the rule reports one by one.
    let top = std::fs::read_dir(&base).ok()?;
    let mut out = BTreeSet::new();
    collect_images(&base, top, &mut out);
    Some(out)
}

fn collect_images(base: &Path, entries: std::fs::ReadDir, out: &mut BTreeSet<String>) {
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            if let Ok(nested) = std::fs::read_dir(&path) {
                collect_images(base, nested, out);
            }
        } else if let Ok(rel) = path.strip_prefix(base) {
            out.insert(slash_path(rel));
        }
    }
}

/// Every UTF-8 file under the scanned trees, in a stable order, plus one
/// finding per [`SCANNED_DIRS`] entry that could not be read at all.
///
/// Reads by content rather than by an extension allowlist — a reference is a
/// string, and an allowlist silently stops covering a file type someone adds.
/// Anything that is not valid UTF-8 (a checked-in image, say) is skipped rather
/// than reported: it cannot carry a textual reference.
///
/// **The top-level root is probed separately from what it contains**, on
/// purpose — a Greptile finding on #1238 caught the version that was not.
/// `docs` and `site/src` are two independent trees this rule promises to
/// scan; losing one of them silently (missing directory, permissions) used to
/// pass unnoticed as long as the *other* tree still produced a match, because
/// `check`'s vacuous-scan guard only fires when NEITHER tree yields anything.
/// So rule 16 could go green having stopped covering a whole declared input.
/// An unreadable file or subdirectory *inside* a root that opened fine stays a
/// silent skip — the same root-vs-nested asymmetry [`available_images`] draws
/// for the image directory, and for the same reason: a page two directories
/// down going unreadable is a narrower problem than the scan itself failing.
///
/// **Symlinks are never followed**, which is what keeps the walk out of
/// `docs/img` — the symlink into `site/static/img` — so the rule never reads
/// the images it resolves against, and never counts one of them as a page.
fn text_files(root: &Path) -> (Vec<TextFile>, Vec<String>) {
    let mut acc: BTreeMap<String, String> = BTreeMap::new();
    let mut missing_roots = Vec::new();
    for rel in SCANNED_DIRS {
        let dir = root.join(rel);
        match std::fs::read_dir(&dir) {
            Ok(entries) => collect_text_files(root, entries, &mut acc),
            Err(e) => missing_roots.push(format!(
                "{rel}/ is missing or unreadable ({e}) — this rule scans it for `/img/...` and \
                 `./img/...` references, so it now covers nothing under that tree. If it moved, \
                 move `SCANNED_DIRS` in `xtask/linkage-check/src/site_image_refs.rs` with it \
                 (issue #1200)"
            )),
        }
    }
    (acc.into_iter().collect(), missing_roots)
}

fn collect_text_files(root: &Path, entries: std::fs::ReadDir, acc: &mut BTreeMap<String, String>) {
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            if let Ok(nested) = std::fs::read_dir(&path) {
                collect_text_files(root, nested, acc);
            }
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            let display = path
                .strip_prefix(root)
                .map(slash_path)
                .unwrap_or_else(|_| slash_path(&path));
            acc.insert(display, text);
        }
    }
}

/// The rule: read both trees and the image directory, then resolve.
pub fn run(root: &Path) -> Vec<String> {
    let Some(available) = available_images(root) else {
        // Same reasoning as `desktop_project_boundary`'s missing-directory
        // finding: a rule whose input has vanished covers nothing, and that has
        // to be reported rather than quietly passing.
        return vec![format!(
            "{IMAGE_ROOT}/ is missing or unreadable — this rule resolves every image reference \
             against it, so it covers nothing. If the static directory moved, move this rule's \
             `IMAGE_ROOT` with it (issue #1200)"
        )];
    };
    let (files, mut out) = text_files(root);
    out.extend(check(&files, &available));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    fn available(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn file(text: &str) -> Vec<TextFile> {
        vec![("docs/page.md".to_string(), text.to_string())]
    }

    /// The hit, in both spellings: `docs/` writes relative and `site/src/`
    /// writes site-absolute, and `docs/img` being a symlink to
    /// `site/static/img` is why one resolution target settles both.
    #[test]
    fn both_spellings_resolve_against_the_one_image_root() {
        let findings = check(
            &[
                (
                    "docs/getting-started.md".to_string(),
                    "![alt](./img/launch.jpg)\n".to_string(),
                ),
                (
                    "site/src/data/landing-content.js".to_string(),
                    "  src: '/img/launch.jpg',\n".to_string(),
                ),
            ],
            &available(&["launch.jpg"]),
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The miss, in both spellings, each named at its own line.
    #[test]
    fn both_spellings_are_reported_when_the_file_is_gone() {
        let findings = check(
            &[
                (
                    "docs/getting-started.md".to_string(),
                    "intro\n![alt](./img/gone.jpg)\n".to_string(),
                ),
                (
                    "site/src/data/landing-content.js".to_string(),
                    "  src: '/img/gone.webp',\n".to_string(),
                ),
            ],
            &available(&["kept.jpg"]),
        );
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(
            findings[0].starts_with(
                "docs/getting-started.md:2: `./img/gone.jpg` resolves to `site/static/img/gone.jpg`"
            ),
            "{}",
            findings[0]
        );
        assert!(
            findings[1].starts_with(
                "site/src/data/landing-content.js:1: `/img/gone.webp` resolves to `site/static/img/gone.webp`"
            ),
            "{}",
            findings[1]
        );
    }

    /// #1200's second instance: a raw `<img>` tag, the form MDX leaves alone
    /// and every Markdown-aware checker skips. It is covered because the scan
    /// is over text rather than over one syntax.
    #[test]
    fn a_raw_img_tag_is_covered_like_markdown_syntax() {
        let findings = check(
            &file("<img src=\"./img/busy-deck.png\" width=\"480\" />\n"),
            &available(&["busy-deck-real.webp"]),
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("`./img/busy-deck.png`"),
            "{}",
            findings[0]
        );
    }

    /// An absolute URL that happens to contain the same path segment is not
    /// ours to resolve. `docs/orchestration.md` carries the YouTube thumbnail
    /// form in the second assertion.
    #[test]
    fn an_absolute_url_is_not_resolved_locally() {
        let findings = check(
            &file(concat!(
                "<img src=\"https://cdn.example.com/img/hero.png\" />\n",
                "<img src=\"https://img.youtube.com/vi/ZIWWDDu02Ik/maxresdefault.jpg\" />\n",
                "![alt](//cdn.example.com/img/hero.png)\n",
                "![alt](/img/kept.png)\n"
            )),
            &available(&["kept.png"]),
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// A repo-relative path written as prose is not a reference. Both trees do
    /// this: the landing-page data module records why an image was deleted, and
    /// this module's own docs quote the spellings.
    #[test]
    fn a_repo_relative_path_in_prose_is_not_a_reference() {
        let findings = check(
            &file(concat!(
                " * `orchestration-config.png` was deleted from `site/static/img/` in an\n",
                " * earlier round. `grep -rn orchestration-config docs/ site/` returns nothing.\n",
                "the docs/img/old.png route is a symlink\n",
                "![alt](/img/kept.png)\n"
            )),
            &available(&["kept.png"]),
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// Only an image extension makes a reference, which is what keeps a bare
    /// `/img/` in a sentence out of the rule.
    #[test]
    fn only_an_image_extension_makes_a_reference() {
        let findings = check(
            &file(concat!(
                "the rule resolves every `/img/...` and `./img/...` reference\n",
                "see /img/notes.txt and /img/ for the rest\n",
                "![alt](/img/kept.png)\n"
            )),
            &available(&["kept.png"]),
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The extension is matched case-insensitively so an `.PNG` reference is
    /// SEEN, and the name is then resolved exactly — which is what the Linux
    /// host serving the site does, and stricter than a case-insensitive
    /// developer machine would be.
    #[test]
    fn an_uppercase_extension_is_seen_and_the_name_still_resolves_exactly() {
        let findings = check(
            &file("![alt](/img/Modes.PNG)\n"),
            &available(&["modes.png"]),
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("`site/static/img/Modes.PNG`"),
            "{}",
            findings[0]
        );
    }

    /// A nested reference resolves under the root rather than by file name, so
    /// `/img/a/b.png` needs `site/static/img/a/b.png`.
    #[test]
    fn a_nested_reference_resolves_under_the_root() {
        let ok = check(
            &file("![alt](/img/deep/b.png)\n"),
            &available(&["deep/b.png"]),
        );
        assert!(ok.is_empty(), "{ok:?}");
        let bad = check(&file("![alt](/img/deep/b.png)\n"), &available(&["b.png"]));
        assert_eq!(bad.len(), 1, "{bad:?}");
        assert!(
            bad[0].contains("`site/static/img/deep/b.png`"),
            "{}",
            bad[0]
        );
    }

    /// The exception is declared on the line that takes it, and in Markdown a
    /// trailing same-line HTML comment renders as nothing.
    #[test]
    fn the_line_marker_declares_an_exception() {
        let findings = check(
            &file("![alt](/img/gone.png) <!-- linkage-check:allow-missing-image -->\n"),
            &available(&["kept.png"]),
        );
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// And the marker is per line, not per file.
    #[test]
    fn the_line_marker_does_not_cover_the_next_line() {
        let findings = check(
            &file(concat!(
                "![a](/img/gone.png) <!-- linkage-check:allow-missing-image -->\n",
                "![b](/img/also-gone.png)\n"
            )),
            &available(&["kept.png"]),
        );
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("also-gone.png"), "{}", findings[0]);
    }

    /// A scan that matched nothing reports nothing, so it refuses to pass
    /// vacuously — the failure mode of a text rule is that the text got
    /// reworded, not that it started matching the wrong thing.
    #[test]
    fn a_scan_that_resolves_nothing_is_a_finding() {
        let findings = check(&file("no references here\n"), &available(&["kept.png"]));
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].contains("covers nothing"), "{}", findings[0]);

        // …but an empty image directory with no references is consistent, not
        // a finding: there is nothing for the rule to have stopped seeing.
        let quiet = check(&file("no references here\n"), &available(&[]));
        assert!(quiet.is_empty(), "{quiet:?}");
    }

    /// The missing-input case, through the real `run`: an absent image
    /// directory is reported rather than quietly emptying the rule.
    #[test]
    fn a_missing_image_root_is_reported() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("docs")).expect("docs");
        let findings = run(tmp.path());
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].contains("is missing or unreadable"),
            "{}",
            findings[0]
        );
    }

    /// The bug rule 16 shipped with, caught by review on PR #1238: a missing
    /// or unreadable `SCANNED_DIRS` root silently produced no files from THAT
    /// tree while the other one still yielded matches, so `seen_any` stayed
    /// true and `check`'s vacuous-scan guard never fired — the rule could pass
    /// having lost all coverage of a whole declared input tree. `site/src/` is
    /// never created here; `docs/` alone still resolves cleanly, and that must
    /// not be enough for a clean run.
    #[test]
    fn a_missing_scanned_root_is_a_finding_even_when_the_other_tree_resolves_cleanly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join(IMAGE_ROOT)).expect("img root");
        std::fs::write(root.join(IMAGE_ROOT).join("kept.png"), "x").expect("kept");
        std::fs::create_dir_all(root.join("docs")).expect("docs");
        std::fs::write(root.join("docs/page.md"), "![a](/img/kept.png)\n").expect("page");
        // site/src/ is never created.

        let findings = run(root);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(findings[0].starts_with("site/src/"), "{}", findings[0]);
        assert!(
            findings[0].contains("is missing or unreadable"),
            "{}",
            findings[0]
        );
    }

    /// `run` end to end over a synthetic tree, including the walk: the miss is
    /// found in a nested `site/src/` module and reported with a `/`-separated
    /// path.
    #[test]
    fn run_walks_both_trees_and_reports_relative_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join(IMAGE_ROOT)).expect("img root");
        std::fs::write(root.join(IMAGE_ROOT).join("kept.png"), "x").expect("kept");
        std::fs::create_dir_all(root.join("docs")).expect("docs");
        std::fs::write(root.join("docs/page.md"), "![a](./img/kept.png)\n").expect("page");
        std::fs::create_dir_all(root.join("site/src/data")).expect("data");
        std::fs::write(
            root.join("site/src/data/landing-content.js"),
            "  src: '/img/gone.png',\n",
        )
        .expect("landing");

        let findings = run(root);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].starts_with("site/src/data/landing-content.js:1:"),
            "{}",
            findings[0]
        );
    }

    /// The walk does not follow a symlinked directory, which is what keeps it
    /// out of `docs/img` — the symlink into the directory it resolves against.
    /// Without this the rule would read every image as a candidate page.
    #[cfg(unix)]
    #[test]
    fn the_walk_does_not_follow_a_symlinked_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join(IMAGE_ROOT)).expect("img root");
        std::fs::write(
            root.join(IMAGE_ROOT).join("note.svg"),
            "![a](/img/gone.png)\n",
        )
        .expect("svg");
        std::fs::create_dir_all(root.join("docs")).expect("docs");
        std::fs::write(root.join("docs/page.md"), "![a](/img/note.svg)\n").expect("page");
        std::os::unix::fs::symlink("../site/static/img", root.join("docs/img")).expect("symlink");
        std::fs::create_dir_all(root.join("site/src")).expect("site/src");

        let findings = run(root);
        assert!(findings.is_empty(), "{findings:?}");
    }

    /// The tree itself. This is what makes the planted-input tests above mean
    /// something — they prove the scan CAN fail, and this proves the checked-in
    /// pages and landing data do not.
    #[test]
    fn the_checked_in_docs_and_site_references_all_resolve() {
        let findings = run(&repo_root());
        assert!(
            findings.is_empty(),
            "site-image-refs: {}",
            findings.join("\n")
        );
    }
}
