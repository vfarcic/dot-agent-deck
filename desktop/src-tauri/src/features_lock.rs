//! Tests only: the lock every test that writes the process-global
//! `dot_agent_deck::features::Features` holds, and the guard that puts the
//! value back. Under nextest every test is its own process, so the lock only
//! matters to a plain `cargo test`, where the tests share one — the same
//! shape as the root crate's `tests/features.rs`.

/// Serialises the tests that write or depend on the process-global
/// `Features`. Hold it for the whole test.
pub(crate) static FEATURES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`FEATURES_LOCK`], recovering it from a test that panicked while
/// holding it (that test's [`RestoreFeatures`] already put the value back).
pub(crate) fn lock_features() -> std::sync::MutexGuard<'static, ()> {
    FEATURES_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Puts back whatever `Features` a test found, even when it panics.
pub(crate) struct RestoreFeatures(pub(crate) dot_agent_deck::features::Features);

impl RestoreFeatures {
    /// Remember the `Features` in force now.
    pub(crate) fn current() -> Self {
        Self(dot_agent_deck::features::current())
    }
}

impl Drop for RestoreFeatures {
    fn drop(&mut self) {
        dot_agent_deck::features::set_for_test(self.0);
    }
}
