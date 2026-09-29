use std::path::{Path, PathBuf};
use std::process::Command;

// Issue #250: the version/build-id resolution order lives in a shared file so
// a unit test can exercise it. `cargo test` cannot reach code inside a build
// script — this script is compiled as its own binary and the test crate cannot
// import from it — so the pure functions live next door and
// `tests/build_version.rs` declares the same file as a module via `#[path]`.
// Everything impure (the `git` subprocesses and the `cargo:` emission below)
// stays here. A `mod` rather than an `include!` because rustfmt does not follow
// `include!` and the shared file would then escape `cargo fmt --check`.
mod build_version_resolve;

// PRD #1419 Decision 4: the `docs` subcommand's pages are embedded from
// `docs/published.toml` through the SAME parser the `cargo xtask site`
// generator uses, so the binary and the site cannot disagree about what is
// published. Shared by `#[path]` for the same reason as the module above; the
// build script reads only part of its API, hence the `dead_code` allowance.
#[allow(dead_code)]
#[path = "src/published_docs.rs"]
mod published_docs;

use build_version_resolve::{
    VersionSource, escape_for_cargo_warning, is_single_line_directive_value, normalize_build_id,
    normalize_version, resolve_build_id, resolve_version,
};

fn main() {
    // Both values are injectable from the build environment (issue #250), so
    // cargo has to watch them: without these, an injected value would be baked
    // in once and then silently go stale across rebuilds.
    println!("cargo:rerun-if-env-changed=DAD_VERSION");
    println!("cargo:rerun-if-env-changed=DAD_BUILD_ID");

    embed_published_docs();

    let injected_version = build_env("DAD_VERSION");
    let injected_build_id = build_env("DAD_BUILD_ID");

    // Resolution order (issue #250): injected env -> git tag -> CARGO_PKG_VERSION.
    // Git tags look like "v0.7.1" -> "0.7.1", "v0.25.0-alpha.0" -> "0.25.0-alpha.0";
    // an injected DAD_VERSION is validated the same way and falls through when
    // invalid, because `src/version.rs` parses it with `semver` and `.expect()`s.
    // The rejected value is rendered with `escape_for_cargo_warning`, never
    // interpolated raw: it failed validation precisely because it can be
    // anything, and `cargo:warning=` is parsed by the same line-oriented
    // protocol as every other directive.
    if let Some(raw) = injected_version.as_deref()
        && normalize_version(raw).is_none()
    {
        println!(
            "cargo:warning=DAD_VERSION=\"{}\" is not a valid SemVer version and was ignored \
             (`semver::Version::parse` grammar: an X.Y.Z core, an optional `v` prefix, an \
             optional `-<prerelease>` suffix, an optional `+<build>` suffix). Falling back to \
             the git tag / CARGO_PKG_VERSION.",
            escape_for_cargo_warning(raw)
        );
    }
    let version = resolve_version(
        injected_version.as_deref(),
        git_tag().as_deref(),
        env!("CARGO_PKG_VERSION"),
    );
    if version.source == VersionSource::Placeholder {
        println!(
            "cargo:warning=No version available: neither DAD_VERSION nor a git tag resolved, so \
             this build reports the CARGO_PKG_VERSION placeholder `{}` (issue #250). Version \
             negotiation, the upgrade nudge and `remote add` will all misbehave against it. Set \
             DAD_VERSION=<x.y.z> in the build environment, or build from a checkout with tags.",
            version.value
        );
    }
    emit_rustc_env("DAD_VERSION", &version.value);

    // PRD #103 M1.0: emit a finer-grained build identifier alongside DAD_VERSION.
    // Shape: `<DAD_VERSION>-g<short-sha>[-dirty]`, falling back to
    // `<DAD_VERSION>-unknown` when git metadata is unavailable.
    let short_sha = git_short_sha();
    // Only ask git whether the tree is dirty when there is a sha to qualify;
    // without one the answer cannot reach the composed id anyway.
    let dirty = short_sha.is_some() && git_is_dirty();
    // An injected build id outside the bounded alphabet is ignored the same way
    // an invalid version is — it falls through to the composed / `-unknown`
    // value — and the warning escapes it, because an interior newline in this
    // value is exactly what used to inject a second `cargo:` directive.
    if let Some(raw) = injected_build_id.as_deref()
        && normalize_build_id(raw).is_none()
    {
        println!(
            "cargo:warning=DAD_BUILD_ID=\"{}\" was ignored: a build id must be a single line of \
             ASCII alphanumerics, `.`, `-`, `+` or `_` (the shape is \
             `<version>-g<short-sha>[-dirty]`). Falling back to the git-composed build id.",
            escape_for_cargo_warning(raw)
        );
    }
    let build_id = resolve_build_id(
        injected_build_id.as_deref(),
        &version.value,
        short_sha.as_deref(),
        dirty,
    );
    emit_rustc_env("DAD_BUILD_ID", &build_id);

    // Re-run if HEAD changes (new commit, branch switch, detached-HEAD move).
    // `.git/HEAD` alone is necessary but not sufficient on a normal branch —
    // the file contents `ref: refs/heads/<branch>` don't change when commits
    // land on that branch. PRD #103 M1.0 prescribes also watching the
    // resolved ref file, the index (dirty/clean transitions), and
    // packed-refs (post-`git gc` fallback).
    //
    // In a git worktree, `.git` is a *file* containing
    // `gitdir: <main-repo>/.git/worktrees/<name>`, not a directory. The
    // literal `.git/HEAD` / `.git/index` paths then point at nothing and
    // `cargo:rerun-if-changed` silently no-ops, which means cached
    // `DAD_BUILD_ID` doesn't invalidate on commit (violates PRD invariant
    // 6). Resolve each path through `git rev-parse --git-path <name>` so
    // we watch the real file under `.git/worktrees/<name>/...` (or the
    // shared `commondir` for packed-refs). Fall back to the literal path
    // only if `git rev-parse` fails — matches the existing degrade-to-
    // unknown discipline elsewhere in this file.
    emit_rerun_if_changed_git_path("HEAD");
    emit_rerun_if_changed_git_path("index");
    emit_rerun_if_changed_git_path("packed-refs");
    if let Some(ref_path) = parse_head_ref_path() {
        emit_rerun_if_changed_git_path(&ref_path);
    }
}

/// Generate `$OUT_DIR/published_docs.rs`, the `docs` subcommand's pages
/// (PRD #1419 Decision 4): one `EmbeddedPage` per `docs/published.toml` entry,
/// in manifest order, each page's Markdown pulled in by `include_str!` so the
/// binary prints exactly the bytes on disk and never needs the network.
///
/// A manifest that does not parse, or a listed page that is missing or not
/// UTF-8, fails the build: a binary whose `docs` silently lacks pages would
/// print docs that disagree with the site for its own version.
fn embed_published_docs() {
    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"),
    );
    let docs_dir = manifest_dir.join("docs");
    let manifest = docs_dir.join("published.toml");
    emit_rerun_if_changed_path(&manifest);

    let pages = published_docs::read(&docs_dir).unwrap_or_else(|e| panic!("{e}"));

    let mut generated = String::from(
        "// @generated by build.rs from docs/published.toml (PRD #1419). Do not edit.\n\
         pub static PAGES: &[EmbeddedPage] = &[\n",
    );
    for page in &pages {
        let path = docs_dir.join(page.file_name());
        emit_rerun_if_changed_path(&path);
        if let Err(e) = std::fs::read_to_string(&path) {
            panic!(
                "{}: page `{}` is listed but {} cannot be read as UTF-8 text: {e}",
                published_docs::MANIFEST_PATH,
                page.slug,
                path.display()
            );
        }
        let path = path
            .to_str()
            .unwrap_or_else(|| panic!("{} is not valid UTF-8", path.display()));
        // `{:?}` renders a Rust string literal: every escape `str`'s `Debug`
        // emits (`\"`, `\\`, `\n`, `\u{..}`) is also a string-literal escape.
        generated.push_str(&format!(
            "    EmbeddedPage {{ slug: {:?}, title: {:?}, description: {:?}, markdown: include_str!({:?}) }},\n",
            page.slug, page.title, page.description, path
        ));
    }
    generated.push_str("];\n");

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let out = out_dir.join("published_docs.rs");
    std::fs::write(&out, generated)
        .unwrap_or_else(|e| panic!("cannot write {}: {e}", out.display()));
}

/// `cargo:rerun-if-changed` for a file path, under the same line-protocol
/// rule as every other directive this script emits.
fn emit_rerun_if_changed_path(path: &Path) {
    let path = path.to_string_lossy();
    assert!(
        is_single_line_directive_value(&path),
        "refusing to emit cargo:rerun-if-changed for \"{}\": not a single safe line",
        escape_for_cargo_warning(&path)
    );
    println!("cargo:rerun-if-changed={path}");
}

/// Emit `cargo:rustc-env=<name>=<value>` after a final line-protocol check.
///
/// Cargo reads build-script stdout line by line, so a value containing CR/LF
/// would let whoever supplied it append a second directive of their choosing
/// (`rustc-cfg`, `rustc-link-arg`, another `rustc-env`). Every value reaching
/// here has already been validated — `semver` for the version, the bounded
/// alphabet for the build id, Cargo itself for `CARGO_PKG_VERSION`, hex for the
/// short sha — so this is a backstop that only fires if a future edit adds an
/// unvalidated source. Failing the build loudly is the right outcome then:
/// emitting the value is the one thing we must not do.
fn emit_rustc_env(name: &str, value: &str) {
    assert!(
        is_single_line_directive_value(value),
        "refusing to emit cargo:rustc-env={name}: the value is not a single safe line (\"{}\")",
        escape_for_cargo_warning(value)
    );
    println!("cargo:rustc-env={name}={value}");
}

/// Emit `cargo:rerun-if-changed` for a git-internal path resolved via
/// `git rev-parse --git-path <relative>`. In a worktree this returns the
/// real path under `.git/worktrees/<name>/...` (for HEAD/index) or the
/// shared `commondir` for `packed-refs` and `refs/heads/<branch>`.
///
/// Falls back to the literal `.git/<relative>` path if `git rev-parse`
/// fails (no git, shallow tarball, ...). Cargo's `rerun-if-changed`
/// silently tolerates non-existent paths, so the fallback is harmless
/// outside a real repo.
fn emit_rerun_if_changed_git_path(relative: &str) {
    let resolved = Command::new("git")
        .args(["rev-parse", "--git-path", relative])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        // Same line-protocol rule as `emit_rustc_env`: a path is git-supplied,
        // so a CR/LF in it would open a second directive. Fall back to the
        // literal path rather than emitting it.
        .filter(|s| is_single_line_directive_value(s))
        .unwrap_or_else(|| format!(".git/{relative}"));
    if !is_single_line_directive_value(&resolved) {
        // Only reachable if `relative` itself is unsafe, which
        // `parse_head_ref_path` already rules out. Watching nothing is a
        // stale-build-id risk; emitting it is a directive-injection one.
        return;
    }
    println!("cargo:rerun-if-changed={resolved}");
}

/// Read `name` from the build environment, treating unset, empty and
/// all-whitespace alike as "not injected" so `DAD_VERSION=` in a CI script
/// behaves like no injection at all rather than emitting a blank version.
fn build_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The raw `git describe --tags --abbrev=0` output for the build checkout, or
/// `None` when git is unavailable / this is not a repo / no tags are reachable.
/// Validation and the `v` strip happen in [`normalize_version`], which the
/// injected value goes through too.
fn git_tag() -> Option<String> {
    let output = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let tag = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if tag.is_empty() { None } else { Some(tag) }
}

/// The short HEAD sha, or `None` when git metadata is unavailable. Any git
/// failure degrades to `None` rather than aborting the build, so tarball /
/// shallow-clone builds still produce a usable `DAD_BUILD_ID` (the
/// `-unknown` sentinel composed by [`resolve_build_id`]).
fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

fn git_is_dirty() -> bool {
    let Ok(output) = Command::new("git").args(["status", "--porcelain"]).output() else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    !output.stdout.is_empty()
}

/// Resolve the symbolic ref path HEAD points at (e.g. `refs/heads/main`)
/// via `git symbolic-ref -q HEAD`. Returns `None` on detached HEAD (the
/// existing HEAD watch already covers that case) or when git isn't
/// available.
///
/// We can't read `.git/HEAD` directly: in a worktree, `.git` is a *file*
/// containing `gitdir: ...`, not a directory, so the literal path
/// points at nothing. `git symbolic-ref` handles both layouts uniformly
/// and prints just the ref path (or fails silently with a non-zero exit
/// when HEAD is detached).
fn parse_head_ref_path() -> Option<String> {
    let output = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let ref_path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if ref_path.is_empty() || !is_single_line_directive_value(&ref_path) {
        None
    } else {
        Some(ref_path)
    }
}
