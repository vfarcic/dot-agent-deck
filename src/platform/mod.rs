//! Platform abstraction seam (PRD #42 — Foundation, M1).
//!
//! Hides every Unix-specific mechanism behind a `cfg`-dispatched API so a
//! native Windows backend can be added alongside the Unix one without
//! scattering `#[cfg(...)]` through call sites. Each submodule follows the
//! same shape:
//!
//! - `mod.rs` — a `cfg`-dispatched public API (`#[cfg(unix)] pub use unix::*;`
//!   / `#[cfg(windows)] pub use windows::*;`), so each platform compiles
//!   exactly one backend.
//! - `unix.rs` (`#[cfg(unix)]`) — lifts today's behavior **verbatim** (M1 is a
//!   behavior-preserving refactor on Unix).
//! - `windows.rs` (`#[cfg(windows)]`) — the Windows backend. At M1 these are
//!   compiling stubs (return errors, not panics); the real implementations
//!   land in PRD #163.
//!
//! M1 populated the **non-socket** mechanisms (`paths`, `shell`, `detach`,
//! `lock`, `proc`, `fsperm`). M2 adds the IPC/socket transport (`ipc`) and
//! peer-credential PID discovery (`peercred`), porting the 8 socket files onto
//! them so the Unix build is byte-for-byte unchanged and the Windows
//! named-pipe build compiles.

pub mod detach;
pub mod fsperm;
pub mod ipc;
pub mod lock;
pub mod paths;
pub mod peercred;
pub mod proc;
pub mod shell;
