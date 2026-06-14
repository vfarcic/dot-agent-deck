//! Detached daemon spawn (PRD #42 M1, lifted from `daemon_attach.rs`).
//!
//! Spawns `dot-agent-deck daemon serve` as a background process that survives
//! the parent's exit. Unix uses `setsid(2)` (session leader, no `SIGHUP`),
//! `O_NOFOLLOW` + 0o600 on the log, and `/dev/null` stdin. Windows uses
//! `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` creation flags and `NUL`
//! stdin (a compiling stub at M1 — full Job-breakaway behavior is PRD #163).

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::spawn_daemon_serve_detached_with_exe;
#[cfg(windows)]
pub use windows::spawn_daemon_serve_detached_with_exe;
