//! Windows CSPRNG: `BCryptGenRandom` with `BCRYPT_USE_SYSTEM_PREFERRED_RNG`.
//!
//! The flag says "use the system-preferred RNG algorithm", which is what lets
//! the call pass a null algorithm handle and skip the
//! `BCryptOpenAlgorithmProvider`/`BCryptCloseAlgorithmProvider` pair entirely.
//! `ProcessPrng` (bcryptprimitives.dll) is the faster modern spelling and is
//! what `getrandom` uses, but it is Windows 10+ only; `BCryptGenRandom` is
//! available everywhere this project could plausibly run and the token is minted
//! once per spawn, so the cheaper call buys nothing worth a floor on the
//! supported OS range.

use std::io;

use windows_sys::Win32::Foundation::NTSTATUS;
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

/// Fill `buf` with bytes from the OS CSPRNG, or fail.
///
/// Fails closed on a non-`STATUS_SUCCESS` return: the buffer is left as the
/// caller passed it in, and every caller here treats the error as fatal rather
/// than using those bytes.
pub fn fill_random(buf: &mut [u8]) -> io::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    // `cbBuffer` is a `u32`, so a slice longer than that cannot be expressed —
    // refuse rather than silently truncate the length and hand back a buffer
    // whose tail is still the caller's zeros.
    let len = u32::try_from(buf.len())
        .map_err(|_| io::Error::other("BCryptGenRandom: buffer longer than u32::MAX"))?;
    // SAFETY: `buf` is a live, exclusively-borrowed slice and its length is
    // passed alongside the pointer, so the callee cannot write past it. A null
    // algorithm handle is exactly what `BCRYPT_USE_SYSTEM_PREFERRED_RNG`
    // documents as required.
    let status: NTSTATUS = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            len,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{status:08X}"
        )));
    }
    Ok(())
}
