//! Log-filter and log-path construction for the optional file logger.
//!
//! Split out of `main.rs` so the `RUST_LOG` precedence rule and the log path's
//! test containment are reachable from the fast-tier tests: `main.rs` is the
//! binary, and integration tests link the library only.

use std::path::PathBuf;

use tracing_subscriber::EnvFilter;

/// The crate-level verbosity the deck applies when the user has not asked for
/// something else.
const CRATE_DEFAULT: &str = "dot_agent_deck=info";

/// The verbosity everything *else* — dependency crates — is held to. This
/// restates the default directive [`EnvFilter::from_default_env`] installs when
/// `RUST_LOG` is unset, which is what the deck used to get for free and now has
/// to name, because the assembled directive string is never empty.
const DEPENDENCY_FLOOR: &str = "error";

/// Build the `EnvFilter` for the file logger from a raw `RUST_LOG` value
/// (`None` when unset).
///
/// Issue #605: the deck's defaults go in FIRST and the user's `RUST_LOG` is
/// layered on top, which is the precedence users expect. The obvious spelling —
/// `EnvFilter::from_default_env().add_directive("dot_agent_deck=info")` — has it
/// backwards: `add_directive` *replaces* an equally-specific directive, so a
/// user's `RUST_LOG=dot_agent_deck=debug` was silently discarded and only more
/// specific targets such as `dot_agent_deck::daemon=debug` survived. Same
/// parser, same replacement rule, opposite order: the user now wins.
pub fn env_filter(rust_log: Option<&str>) -> EnvFilter {
    // An unset or empty `RUST_LOG` leaves a trailing empty segment, which the
    // parser drops. Lossy, exactly as `from_default_env` was: one malformed
    // directive warns on stderr and is skipped rather than taking logging down.
    EnvFilter::new(format!(
        "{DEPENDENCY_FLOOR},{CRATE_DEFAULT},{}",
        rust_log.unwrap_or_default()
    ))
}

/// [`env_filter`] reading `RUST_LOG` from the process environment.
pub fn env_filter_from_env() -> EnvFilter {
    env_filter(std::env::var("RUST_LOG").ok().as_deref())
}

/// The file the logger opens for a `DOT_AGENT_DECK_LOG` value, or `None` when
/// this is a test process and the file is not one it owns.
///
/// Issue #1135: the default (an empty value or `1`) is resolved per platform,
/// not spelled as a Unix literal. `/tmp/...` is rooted but driveless on
/// Windows, so it resolved against the current drive as
/// `\tmp\dot-agent-deck.log` — a directory that usually does not exist, and the
/// logger's open creates the file but never its parent, so the subscriber was
/// never installed and nothing was logged. An explicit value is still used
/// verbatim.
///
/// Issues #1791 and #1796: a test run inherits `DOT_AGENT_DECK_LOG` from the
/// shell that started it — a developer's, or the deck pane's the tests were
/// started in — and every deck binary a test spawned without clearing its
/// environment appended to that real `deck.log`, where its lines read as the
/// production daemon's own. So a process the agent-config containment treats
/// as a test refuses a log outside its owned roots, and the deck's default
/// debug log as well, since that lives in the temp dir that is itself a
/// default root ([`crate::config_write_guard::log_write_allowed`]). A user's
/// process is not a test process, and gets the path unchanged.
pub fn log_path(value: &str) -> Option<PathBuf> {
    let path = if value.is_empty() || value == "1" {
        crate::platform::paths::default_debug_log_path()
    } else {
        PathBuf::from(value)
    };
    crate::config_write_guard::log_write_allowed(&path).then_some(path)
}

// Unix-only: the outside path below is spelled as a Unix root.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Scenario: in this unit-test process, which is a test process by
    /// construction, a log in a test scratch directory is opened as given,
    /// while a log outside every owned root and the deck's default debug log
    /// (an empty value or `1`) are refused.
    #[test]
    fn a_test_process_opens_only_a_log_it_owns() {
        let fixture = crate::test_temp::tempdir().unwrap();
        let owned = fixture.path().join("deck.log");
        assert_eq!(
            log_path(owned.to_str().unwrap()),
            Some(owned.clone()),
            "a log in a test scratch directory is the test's own"
        );
        assert_eq!(log_path("/deck.log"), None, "outside every owned root");
        assert_eq!(log_path("1"), None, "the default debug log is shared");
        assert_eq!(log_path(""), None, "an empty value names the default too");
        assert!(!owned.exists(), "deciding the path must not create the log");
    }
}
