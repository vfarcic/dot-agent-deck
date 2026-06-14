//! Filesystem security: owner-only dirs/files, the socket-bind umask dance,
//! and endpoint trust verification (PRD #42 M1, lifted from `daemon.rs`,
//! `daemon_attach.rs`, `remote.rs`, `schedule_cli.rs`).
//!
//! Unix is the uniform mode-bit model (`umask`/0o700/0o600/`is_socket`+uid
//! checks). Windows splits this into pipe security descriptors + directory
//! ACLs + reliance on the per-user `%LOCALAPPDATA%` default ACL; at M1 the
//! Windows side is justified no-ops / skeletons (real behavior is PRD #163).
//!
//! Each `cfg(unix)` permission site was audited individually rather than
//! blanket-`cfg`'d away — see the per-function docs for what security property
//! each one upholds and how Windows is expected to uphold it.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{
    create_owner_only_dir, ensure_owner_only_dir, set_create_mode_owner_only,
    set_endpoint_mode_owner_only, set_file_owner_only, verify_endpoint_trusted, with_socket_umask,
};
#[cfg(windows)]
pub use windows::{
    create_owner_only_dir, ensure_owner_only_dir, set_create_mode_owner_only,
    set_endpoint_mode_owner_only, set_file_owner_only, verify_endpoint_trusted, with_socket_umask,
};
