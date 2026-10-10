//! Platform-absolute fixture paths for unit tests (issue #1637).
//!
//! A fixture written Unix-style, such as `/opt/old/dot-agent-deck`, is not
//! absolute on Windows: it has a root but no drive, so `Path::is_absolute` is
//! false there. Code that classifies a path as absolute or relative — hook
//! pins, `deck_exe`, `is_valid_deck_exe` — then takes its relative branch on
//! Windows only, and a test written against the absolute branch fails on
//! `build-windows` alone. Three rounds of PR #1656 each lost a CI trip to it.

/// `path`, written Unix-style, as an absolute path on this platform:
/// unchanged on Unix, and under `C:\` with backslashes on Windows.
pub(crate) fn abs(path: &str) -> String {
    if cfg!(windows) {
        format!("C:{}", path.replace('/', "\\"))
    } else {
        path.to_string()
    }
}

mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn abs_is_absolute_on_this_platform() {
        for path in ["/", "/opt/old/dot-agent-deck", "/a b/x"] {
            assert!(Path::new(&abs(path)).is_absolute(), "{path}");
        }
    }
}
