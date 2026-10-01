//! The local deck's address, as this crate reads it.
//!
//! In a normal build [`local_endpoint`] is exactly
//! [`Endpoint::local`]: the address `DOT_AGENT_DECK_ATTACH_SOCKET` and the
//! platform defaults resolve to. Every read of the local deck in this crate goes
//! through here rather than calling [`Endpoint::local`] directly, so that the
//! tests have one seam to move it with.
//!
//! # Why the tests need a seam rather than the environment variable (issue #1078)
//!
//! Several tests have to point the local deck at a socket they control: no
//! settings document can name the local deck, so its address comes from config
//! alone. They used to do it by writing `DOT_AGENT_DECK_ATTACH_SOCKET`, which is
//! process-global. Under `cargo nextest` every test owns its process and that is
//! safe; under a plain `cargo test --lib` the whole crate shares one process,
//! and every sibling test that resolved the local deck while the variable was
//! moved saw a scratch socket instead — one of the causes that kept that
//! command red. A lock around the writes could not fix it, because the readers
//! are production code, and production code has no business taking a test lock.
//!
//! So the override is **per thread**, and exists only under `cfg(test)`. The
//! test harness runs each test on a thread of its own, and a `#[tokio::test]`
//! polls the test body on that same thread (so does every task a
//! current-thread runtime spawns), which is exactly the reach an override set
//! by one test should have. What it does not reach is another thread: a
//! `spawn_blocking` closure or a multi-threaded runtime's worker resolving the
//! local deck there sees the ambient address instead, so resolve it on the
//! test's own thread and pass the endpoint along.
//!
//! [`Endpoint::local`]: dot_agent_deck::daemon_client::Endpoint::local

use dot_agent_deck::daemon_client::Endpoint;

/// The local deck's endpoint: [`Endpoint::local`], unless a test on this thread
/// has pointed it elsewhere with [`test_override::point_local_deck_at`].
///
/// [`Endpoint::local`]: dot_agent_deck::daemon_client::Endpoint::local
pub(crate) fn local_endpoint() -> Endpoint {
    #[cfg(test)]
    if let Some(endpoint) = test_override::current() {
        return endpoint;
    }
    Endpoint::local()
}

#[cfg(test)]
pub(crate) mod test_override {
    use std::cell::RefCell;
    use std::marker::PhantomData;
    use std::path::Path;

    use dot_agent_deck::daemon_client::{Endpoint, LocalEndpoint};

    thread_local! {
        static OVERRIDE: RefCell<Option<Endpoint>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<Endpoint> {
        OVERRIDE.with(|slot| slot.borrow().clone())
    }

    /// Restores the local deck this thread saw before
    /// [`point_local_deck_at`] moved it, when dropped.
    ///
    /// Not `Send`: the override lives on the thread that set it, so a guard
    /// dropped anywhere else would restore the wrong thread's slot.
    #[must_use = "the local deck goes back to its previous address as soon as this is dropped"]
    pub(crate) struct LocalDeckOverride {
        prior: Option<Endpoint>,
        _this_thread: PhantomData<*const ()>,
    }

    impl Drop for LocalDeckOverride {
        fn drop(&mut self) {
            let prior = self.prior.take();
            OVERRIDE.with(|slot| *slot.borrow_mut() = prior);
        }
    }

    /// Point this thread's local deck at `socket` until the guard is dropped —
    /// what writing `DOT_AGENT_DECK_ATTACH_SOCKET` did, without moving it for
    /// every other test in the process.
    pub(crate) fn point_local_deck_at(socket: &Path) -> LocalDeckOverride {
        let endpoint = Endpoint::Local(LocalEndpoint::at(socket));
        let prior = OVERRIDE.with(|slot| slot.borrow_mut().replace(endpoint));
        LocalDeckOverride {
            prior,
            _this_thread: PhantomData,
        }
    }

    mod tests {
        use super::*;
        use crate::local_deck::local_endpoint;

        /// Scenario: a test points the local deck at a scratch socket; on its
        /// own thread the local deck is that socket, on another thread it is
        /// still the ambient one, and dropping the guard puts this thread back.
        #[test]
        fn an_override_moves_only_this_threads_local_deck_and_is_undone_on_drop() {
            let ambient = Endpoint::local();
            assert_eq!(local_endpoint(), ambient, "no override: the ambient deck");

            let socket = Path::new("/nonexistent/dad-local-deck-override.sock");
            let guard = point_local_deck_at(socket);
            assert_eq!(
                local_endpoint(),
                Endpoint::Local(LocalEndpoint::at(socket)),
                "this thread sees the override"
            );
            let elsewhere = std::thread::spawn(local_endpoint)
                .join()
                .expect("the other thread must not panic");
            assert_eq!(
                elsewhere, ambient,
                "another thread's local deck is not moved — the whole point of the seam"
            );

            let nested = point_local_deck_at(Path::new("/nonexistent/nested.sock"));
            drop(nested);
            assert_eq!(
                local_endpoint(),
                Endpoint::Local(LocalEndpoint::at(socket)),
                "an inner override restores the outer one, not the ambient deck"
            );

            drop(guard);
            assert_eq!(local_endpoint(), ambient, "dropping the guard undoes it");
        }

        /// Every `.rs` file under this crate's `src/`, as `(path, contents)`.
        fn crate_sources() -> Vec<(std::path::PathBuf, String)> {
            let mut pending = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
            let mut sources = Vec::new();
            while let Some(dir) = pending.pop() {
                for entry in std::fs::read_dir(&dir).expect("read a source directory") {
                    let path = entry.expect("a directory entry").path();
                    if path.is_dir() {
                        pending.push(path);
                    } else if path.extension().is_some_and(|ext| ext == "rs") {
                        let contents = std::fs::read_to_string(&path).expect("read a source file");
                        sources.push((path, contents));
                    }
                }
            }
            sources
        }

        /// Scenario: every source file in the crate is read, comment lines
        /// skipped. Outside this module no line calls `Endpoint::local()` and
        /// no line names `DOT_AGENT_DECK_ATTACH_SOCKET` other than a
        /// `Command::env` handing it to a spawned child, so no reader bypasses
        /// the seam and no test goes back to moving the local deck for the
        /// whole process.
        #[test]
        fn nothing_reads_the_local_deck_around_the_seam_or_moves_it_process_wide() {
            let this_file = Path::new(file!())
                .file_name()
                .expect("this file has a name");
            let sources = crate_sources();
            assert!(
                sources.iter().any(|(path, _)| path.ends_with("lib.rs")),
                "the sweep must actually find the crate's sources"
            );
            let mut offenders = Vec::new();
            for (path, contents) in &sources {
                if path.file_name() == Some(this_file) {
                    continue;
                }
                for (index, line) in contents.lines().enumerate() {
                    if line.trim_start().starts_with("//") {
                        continue;
                    }
                    // A `Command::env` on a spawned CHILD is exempt: it moves no
                    // deck in this process and reads none, which is what the
                    // sweep protects. `daemon_bridge`'s opt-in older-daemon test
                    // (issue #1472) starts a daemon that way, and `daemon serve`
                    // takes its socket from nothing else.
                    let child_env = line
                        .trim_start()
                        .starts_with(".env(\"DOT_AGENT_DECK_ATTACH_SOCKET\"");
                    if line.contains("Endpoint::local()")
                        || (line.contains("DOT_AGENT_DECK_ATTACH_SOCKET") && !child_env)
                    {
                        offenders.push(format!(
                            "{}:{}: {}",
                            path.display(),
                            index + 1,
                            line.trim()
                        ));
                    }
                }
            }
            assert!(
                offenders.is_empty(),
                "read the local deck through `crate::local_deck::local_endpoint()`, and move it in \
                 a test with `point_local_deck_at` rather than the process-wide environment \
                 variable (issue #1078):\n{}",
                offenders.join("\n")
            );
        }
    }
}
