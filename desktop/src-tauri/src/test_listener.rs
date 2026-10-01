//! Binding a production attach listener in a test without touching the process
//! umask (issue #1078).
//!
//! `bind_attach_listener` — and `IpcListener::bind` under it — creates the
//! socket inode owner-only by flipping the **process** umask to `0o177` around
//! `bind(2)`. Under `cargo nextest` every test owns its process and that is
//! invisible; under a plain `cargo test --lib` the whole crate shares one, and a
//! sibling test's `create_dir_all` or `tempdir` landing inside the flip made a
//! directory with no execute bit, so its next `bind` failed `EACCES`. That was
//! one of the causes that kept `cargo test --lib` red.
//!
//! So the tests that run the production attach server bind through
//! `IpcListener::bind_in_owner_only_dir`, which makes the socket's directory
//! owner-only first and then binds at the ambient umask. The inode the client's
//! trust check sees is the same — this user's socket at exactly `0o600` — and
//! the scratch directory it sits in is tightened to `0o700`.

use std::path::Path;

use dot_agent_deck::platform::ipc::IpcListener;

/// An attach listener at `socket`, owner-only, bound without the umask flip.
///
/// Unlike `bind_attach_listener` it does not clear a stale inode first: every
/// caller binds in a fresh scratch directory, where there is none.
pub(crate) fn bind_owner_only(socket: &Path) -> std::io::Result<IpcListener> {
    IpcListener::bind_in_owner_only_dir(socket)
}
