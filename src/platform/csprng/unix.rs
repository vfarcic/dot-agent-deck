//! Unix CSPRNG: `/dev/urandom`.
//!
//! Chosen over `libc::getrandom` because that symbol is Linux-only — macOS
//! exposes `getentropy` instead, and `getentropy` caps a single call at 256
//! bytes. `/dev/urandom` is the one spelling both platforms share, it is the
//! same kernel pool `getrandom(2)` draws from, and it needs no per-OS branch.
//!
//! The file is opened per call rather than cached. The token is minted once per
//! agent spawn — a path that already forks a process and opens a PTY — so an
//! `open`/`read`/`close` is not measurable there, and a cached descriptor would
//! be one more thing a `fork` has to reason about.

use std::fs::File;
use std::io::{self, Read};

/// Fill `buf` with bytes from the OS CSPRNG, or fail.
///
/// `read_exact` is what makes this fail closed: a short read leaves the tail of
/// `buf` as whatever the caller passed in (zeros, for every caller here), so
/// treating a partial fill as success would hand out a token with a predictable
/// suffix.
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let mut f = File::open("/dev/urandom")?;
    f.read_exact(buf)
}
