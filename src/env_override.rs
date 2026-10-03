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
use std::sync::atomic::{AtomicBool, Ordering};

/// Set once any override has ever been installed in this process, and never
/// cleared. While it is `false`, [`var`] does not touch [`OVERRIDES`].
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// `(name, value)`, where a `None` value means "read as unset" rather than
/// "defer to the environment". A name that is absent defers to the environment.
static OVERRIDES: RwLock<Vec<(&'static str, Option<String>)>> = RwLock::new(Vec::new());

/// Read `name`: an override when a test installed one, the process environment
/// otherwise. Same result shape as `std::env::var(name).ok()`, which is what
/// every reader called before.
pub(crate) fn var(name: &str) -> Option<String> {
    if ACTIVE.load(Ordering::Acquire) {
        let overrides = OVERRIDES.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, value)) = overrides.iter().find(|(key, _)| *key == name) {
            return value.clone();
        }
    }
    std::env::var(name).ok()
}

/// Make [`var`] read `value` for `name` until the returned guard drops, which
/// puts back whatever override was there before (or none). `None` reads as
/// unset, whatever the environment says.
///
/// Process-global, like the environment it stands in for: under plain
/// `cargo test` a test that installs one serialises against its siblings with
/// the same lock it would have used for `set_var`.
#[doc(hidden)]
pub fn override_for_tests(name: &'static str, value: Option<&str>) -> Override {
    let previous = install(name, value.map(str::to_owned));
    Override { name, previous }
}

/// Guard returned by [`override_for_tests`].
#[doc(hidden)]
#[must_use = "the override is removed when this guard drops"]
pub struct Override {
    name: &'static str,
    /// The entry this guard replaced: `None` when there was none.
    previous: Option<Option<String>>,
}

impl Override {
    /// Change the overridden value while the guard is alive, so a test can
    /// compare two values against one running fixture.
    pub fn repoint(&self, value: Option<&str>) {
        install(self.name, value.map(str::to_owned));
    }
}

impl Drop for Override {
    fn drop(&mut self) {
        let mut overrides = OVERRIDES.write().unwrap_or_else(|e| e.into_inner());
        overrides.retain(|(key, _)| *key != self.name);
        if let Some(previous) = self.previous.take() {
            overrides.push((self.name, previous));
        }
    }
}

/// Install `value` for `name`, returning the entry it replaced.
fn install(name: &'static str, value: Option<String>) -> Option<Option<String>> {
    let mut overrides = OVERRIDES.write().unwrap_or_else(|e| e.into_inner());
    ACTIVE.store(true, Ordering::Release);
    match overrides.iter_mut().find(|(key, _)| *key == name) {
        Some((_, slot)) => Some(std::mem::replace(slot, value)),
        None => {
            overrides.push((name, value));
            None
        }
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
}
