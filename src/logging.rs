//! Log-filter construction for the optional file logger.
//!
//! Split out of `main.rs` so the `RUST_LOG` precedence rule is reachable from
//! the fast-tier tests: `main.rs` is the binary, and integration tests link the
//! library only.

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
