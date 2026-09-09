//! Printing a path the way this repository writes paths.
//!
//! One helper, used by the two modules that build a **repo-relative** string
//! out of a directory walk and then print it beside forward-slashed literals:
//! `desktop_palette`'s findings and `list_tests`' inventory tables.
//!
//! The rule it encodes: *a walked path is safe to compare, and unsafe to
//! stringify* (issue #831). Issue #831's survey found the crate's path
//! *comparisons* already immune — they go through `Path`/`PathBuf`, which
//! compares components — and found the crate's other `display()` sites
//! unaffected, because they print **absolute** paths with no forward-slashed
//! literal glued on, so their output is self-consistently native. What is left
//! is the case here: the moment a walked path becomes a repo-relative `String`
//! it carries the native separator into text that promises `/`.
//!
//! It arrived in `desktop_palette` (`25b39b5`), which is where the failure was
//! first observed, and moved here when `list_tests` needed the same thing.

use std::path::Path;

/// A path as a report should print it: `/`-separated on every platform.
///
/// [`Path::display`] and [`std::ffi::OsStr::to_string_lossy`] emit the
/// **native** separator, so on Windows a palette finding read
/// `desktop/src/c\Tile.tsx` — the literal prefix forward-slashed and the walked
/// tail back-slashed, which is not a path anything in this repository prints.
/// That string is not just a test fixture: it is what a developer reads in a
/// guard's failure message or in `cargo xtask list-tests`' inventory table, and
/// what they paste or click. `git`, every CI log and every other path written
/// here use `/`, so a Windows contributor and a Linux contributor debugging the
/// same output have to see the same text.
///
/// Hence normalising here, where the path is built, rather than teaching each
/// assertion to accept either separator — that would leave the tests asserting
/// something weaker than the property that matters, and would leave the output
/// platform-variant for no gain.
///
/// Walks components rather than replacing `\` blindly, because on Unix a
/// backslash is a legal character **in a file name** and rewriting it would
/// report a path that does not exist.
pub(crate) fn slash_path(path: &Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        if component == std::path::Component::RootDir {
            // The leading separator itself; `as_os_str` here is the native one.
            if !out.ends_with('/') {
                out.push('/');
            }
            continue;
        }
        if !out.is_empty() && !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows failure this helper exists for, pinned directly rather than
    /// inferred from callers that happen to compare whole path strings.
    ///
    /// The joins matter: `Path::join` uses the **native** separator, so on
    /// Windows these inputs really are `themes\styles.css` and `c\ui\Tile.tsx`,
    /// and `display()` — what the palette guard used to call — returns them
    /// with those backslashes intact. On Unix the assertion is true either way;
    /// it is a tripwire for the platform where it can fail, which is the most a
    /// Linux host can offer for a Windows-only path defect.
    #[test]
    fn a_printed_path_is_forward_slashed_whatever_the_native_separator_is() {
        assert_eq!(
            slash_path(&Path::new("themes").join("styles.css")),
            "themes/styles.css"
        );
        assert_eq!(
            slash_path(&Path::new("c").join("ui").join("Tile.tsx")),
            "c/ui/Tile.tsx"
        );
        assert_eq!(slash_path(Path::new("styles.css")), "styles.css");
        assert!(
            !slash_path(&Path::new("a").join("b")).contains(std::path::MAIN_SEPARATOR)
                || std::path::MAIN_SEPARATOR == '/'
        );
    }

    /// And it is a component walk rather than a blind `\` → `/` replacement,
    /// which this host *can* prove: on Unix a backslash is a legal character in
    /// a file name, so rewriting one would report a path that does not exist.
    #[cfg(unix)]
    #[test]
    fn a_backslash_inside_a_unix_file_name_is_not_a_separator() {
        assert_eq!(slash_path(Path::new(r"a\b.tsx")), r"a\b.tsx");
        assert_eq!(slash_path(Path::new("/abs/x.tsx")), "/abs/x.tsx");
    }
}
