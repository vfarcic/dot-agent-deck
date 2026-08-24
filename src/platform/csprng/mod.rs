//! Cryptographically secure random bytes (issue #318).
//!
//! The hook-provenance capability token has to be *unguessable* by a same-user
//! process that does not already hold it, which rules out every id generator
//! already in this tree: [`crate::agent_pty::mint_orchestration_id`] and
//! `ui::mint_delivery_id` are pid+nanos+counter recipes chosen for uniqueness,
//! and a process that knows roughly when a pane was spawned can enumerate
//! those. So the token needs an OS CSPRNG.
//!
//! Deliberately no new crate. `rand`/`getrandom` would each pull a dependency
//! tree onto a security path for one call site, and the two syscalls involved
//! are three lines apiece behind the seam this module already establishes for
//! every other platform-specific mechanism (see [`crate::platform`]).
//!
//! Both backends **fail closed**: they return an error rather than degrading to
//! a weak source. [`crate::hook_ingest::AgentToken::mint`] treats that error as
//! fatal to the spawn, because an agent whose token came from a predictable
//! source is worse than an agent with no token at all — the first silently
//! looks authorized, the second is visibly refused.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::fill_random;
#[cfg(windows)]
pub use windows::fill_random;

#[cfg(test)]
mod tests {
    use super::*;

    /// The one property every caller depends on: the call succeeds and the
    /// buffer is not left as the zeros it started as. Two independent draws
    /// colliding on 32 bytes is a `2^-256` event, so an equality failure here
    /// means the backend is not writing at all — the exact failure mode a
    /// silent "degrade to a constant" fallback would produce.
    #[test]
    fn fill_random_writes_distinct_bytes() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        fill_random(&mut a).expect("the OS CSPRNG must be readable");
        fill_random(&mut b).expect("the OS CSPRNG must be readable");
        assert_ne!(a, [0u8; 32], "buffer left untouched");
        assert_ne!(a, b, "two draws returned identical bytes");
    }

    /// A zero-length request is a no-op, not an error — it is the degenerate
    /// case of the loop in the Unix backend and of the `u32` cast in the
    /// Windows one.
    #[test]
    fn fill_random_accepts_an_empty_buffer() {
        fill_random(&mut []).expect("an empty draw must succeed");
    }
}
