//! `cargo xver` — CLAUDE.md rule 12's cross-version contract check, scripted.
//! `outer.rs` says what it does and why; `docs/develop/cross-version-harness.md`
//! is the operational page.
//!
//! # Linux only, and built everywhere
//!
//! The harness depends on Linux specifically, not on Unix: bubblewrap for the
//! namespace, procfs for process identity and `/proc/net/unix` for socket
//! listeners, and the `linux-amd64` release asset for the old side. So every
//! module, and every dependency in `Cargo.toml`, is gated on
//! `target_os = "linux"`, and on any other target this crate builds to the
//! `main` below, which says so and exits non-zero.
//!
//! It still has to BUILD there: it is a workspace member, and `build-windows`,
//! `build-macos` and `windows-cross-check` all build `--workspace`. Gating on
//! `unix` instead would hand macOS a half-working harness (`libc` spells errno
//! differently there, and it has no procfs) rather than a clear refusal.

#[cfg(target_os = "linux")]
mod buildgate;
#[cfg(target_os = "linux")]
mod buildns;
#[cfg(target_os = "linux")]
mod ctl;
#[cfg(target_os = "linux")]
mod inner;
#[cfg(target_os = "linux")]
mod isolation;
#[cfg(target_os = "linux")]
mod outer;
#[cfg(target_os = "linux")]
mod previous;
#[cfg(target_os = "linux")]
mod probe;
#[cfg(target_os = "linux")]
mod probes;
#[cfg(target_os = "linux")]
mod proc;
#[cfg(target_os = "linux")]
mod pty;
#[cfg(target_os = "linux")]
mod report;
#[cfg(target_os = "linux")]
mod sandbox;
#[cfg(target_os = "linux")]
mod stub;

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    outer::main()
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!(
        "xver: `cargo xver` runs on Linux only — it needs bubblewrap, procfs (`/proc`) and \
         the linux-amd64 release asset. See docs/develop/cross-version-harness.md."
    );
    std::process::ExitCode::FAILURE
}
