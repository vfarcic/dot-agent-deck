//! Issue #1516: an in-process stand-in for a handful of environment knobs, for
//! the tests that have to change one while the deck's own tasks are running.
//!
//! A knob such as `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS` is read per
//! delegate, on whichever Tokio worker runs that delegate. A test that changes
//! it between two delegates used to `std::env::set_var` from inside `block_on`,
//! with those workers alive, which is the unsound case `set_var` warns about: it
//! races every thread that reads the environment, including libc code that
//! reads it without std's lock. An override here is behind a lock that both the
//! writer and every reader take, so changing it mid-run is an ordinary
//! synchronised write.
//!
//! **Production reads the environment exactly as before.** Nothing outside the
//! tests calls [`override_for_tests`], so [`ACTIVE`] stays `false` for the life
//! of a release process and [`var`] is `std::env::var(name).ok()` behind one
//! atomic load. The environment variable stays the user-facing knob.
//! The setter is `pub` only because integration tests link the library as an
//! ordinary dependency and cannot see `#[cfg(test)]` items; it is
//! `#[doc(hidden)]` for the same reason the L1 seams in `ui.rs` are.
//!
//! Only the knobs whose readers call [`var`] honour an override. Today those are
//! `DOT_AGENT_DECK_DELEGATE_READINESS_BUFFER_MS`,
//! `DOT_AGENT_DECK_DELEGATE_NO_EVENT_WINDOW_MS`,
//! `DOT_AGENT_DECK_WORKER_RESPONSE_TIMEOUT_MS` and
//! `DOT_AGENT_DECK_SESSION_START_WAIT_MS`. An override of any other name is
//! never read.

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Set once any override has ever been installed in this process, and never
/// cleared. While it is `false`, [`var`] does not touch [`OVERRIDES`].
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Hands each [`Override`] an id of its own.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// One entry per live guard, `(name, guard id, value)`, oldest first. For each
/// name the newest live entry wins, so guards may drop in any order: a guard
/// removes its own entry and nobody else's. A `None` value reads as unset rather
/// than deferring to the environment; a name with no entry defers to it.
static OVERRIDES: RwLock<Vec<(&'static str, u64, Option<String>)>> = RwLock::new(Vec::new());

/// Read `name`: an override when a test installed one, the process environment
/// otherwise. Same result shape as `std::env::var(name).ok()`, which is what
/// every reader called before.
pub(crate) fn var(name: &str) -> Option<String> {
    if ACTIVE.load(Ordering::Acquire) {
        let overrides = OVERRIDES.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, value)) = overrides.iter().rev().find(|(key, _, _)| *key == name) {
            return value.clone();
        }
    }
    std::env::var(name).ok()
}

/// Make [`var`] read `value` for `name` while the returned guard is the newest
/// live one for that name. `None` reads as unset, whatever the environment says.
/// When the guard drops, the next-newest live guard for the name decides again,
/// or the environment does when there is none.
///
/// Process-global, like the environment it stands in for: under plain
/// `cargo test` a test that installs one serialises against its siblings with
/// the same lock it would have used for `set_var`.
#[doc(hidden)]
pub fn override_for_tests(name: &'static str, value: Option<&str>) -> Override {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut overrides = OVERRIDES.write().unwrap_or_else(|e| e.into_inner());
    ACTIVE.store(true, Ordering::Release);
    overrides.push((name, id, value.map(str::to_owned)));
    Override { id }
}

/// Guard returned by [`override_for_tests`].
#[doc(hidden)]
#[must_use = "the override is removed when this guard drops"]
pub struct Override {
    id: u64,
}

impl Override {
    /// Change this guard's value, so a test can compare two values against one
    /// running fixture. It is what [`var`] reads only while this guard is the
    /// newest live one for its name.
    pub fn repoint(&self, value: Option<&str>) {
        let mut overrides = OVERRIDES.write().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, slot)) = overrides.iter_mut().find(|(_, id, _)| *id == self.id) {
            *slot = value.map(str::to_owned);
        }
    }
}

impl Drop for Override {
    fn drop(&mut self) {
        let mut overrides = OVERRIDES.write().unwrap_or_else(|e| e.into_inner());
        overrides.retain(|(_, id, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A name no reader uses, so these tests cannot change what a sibling test
    // in this process reads.
    const NAME: &str = "DOT_AGENT_DECK_ENV_OVERRIDE_SELF_TEST";

    /// The override wins over the environment, `None` reads as unset, a nested
    /// guard restores the outer one, and dropping the last guard hands the
    /// name back to the environment.
    #[test]
    fn an_override_shadows_the_environment_and_unwinds_in_order() {
        assert_eq!(var(NAME), None);
        let outer = override_for_tests(NAME, Some("outer"));
        assert_eq!(var(NAME).as_deref(), Some("outer"));
        outer.repoint(Some("repointed"));
        assert_eq!(var(NAME).as_deref(), Some("repointed"));
        {
            let _inner = override_for_tests(NAME, None);
            assert_eq!(var(NAME), None, "a `None` override reads as unset");
        }
        assert_eq!(
            var(NAME).as_deref(),
            Some("repointed"),
            "dropping the inner guard must restore the outer override"
        );
        drop(outer);
        assert_eq!(var(NAME), None, "no guard left, so the environment decides");
    }

    /// Guards may drop out of order: the newest live guard decides, a guard
    /// removes only its own entry, and an outer guard's `repoint` takes effect
    /// once the inner guard is gone. Review finding on PR #1534.
    #[test]
    fn guards_dropped_out_of_order_leave_no_stale_value() {
        const OTHER: &str = "DOT_AGENT_DECK_ENV_OVERRIDE_ORDER_SELF_TEST";
        let outer = override_for_tests(OTHER, Some("outer"));
        let inner = override_for_tests(OTHER, Some("inner"));
        outer.repoint(Some("outer-repointed"));
        assert_eq!(
            var(OTHER).as_deref(),
            Some("inner"),
            "the newest guard decides"
        );
        drop(outer);
        assert_eq!(
            var(OTHER).as_deref(),
            Some("inner"),
            "dropping the outer guard first must leave the inner one in force"
        );
        drop(inner);
        assert_eq!(
            var(OTHER),
            None,
            "no guard left, so the environment decides"
        );

        let outer = override_for_tests(OTHER, Some("outer"));
        let inner = override_for_tests(OTHER, Some("inner"));
        outer.repoint(Some("outer-repointed"));
        drop(inner);
        assert_eq!(
            var(OTHER).as_deref(),
            Some("outer-repointed"),
            "the outer guard's repoint must survive the inner guard's drop"
        );
        drop(outer);
        assert_eq!(var(OTHER), None);
    }
}
