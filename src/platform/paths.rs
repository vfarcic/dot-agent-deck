//! Home / runtime / state directory and IPC-endpoint path resolution
//! (PRD #42 M1, lifted from `config.rs`).
//!
//! The Unix branch preserves today's behavior byte-for-byte: `$HOME`,
//! `$XDG_RUNTIME_DIR`, `$XDG_CONFIG_HOME`, the per-uid `/tmp` socket fallback,
//! and `getuid(2)` namespacing. The Windows branch resolves
//! `%USERPROFILE%`/`%LOCALAPPDATA%`/`%APPDATA%` via the `dirs` crate and returns
//! named-pipe endpoint strings (`\\.\pipe\dot-agent-deck-{user}-…`, where
//! `{user}` is the current user's SID — see [`endpoint_user_suffix`]). The
//! `DOT_AGENT_DECK_*` env overrides stay authoritative on both platforms:
//! every resolver checks its override before consulting any platform default.
//!
//! Note: only the **path computation** lives here. The socket binding / I/O
//! that consumes these paths stays in `daemon*`/`hook`/`ui` until M2 abstracts
//! the transport.

use std::path::PathBuf;

/// Home directory used to anchor config/state/cache paths.
///
/// Unix: `$HOME`, falling back to `/` (matches the historical
/// `config::dirs_home`). Windows: `%USERPROFILE%`, falling back to `C:\`.
pub fn home_dir() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/"))
    }
    #[cfg(windows)]
    {
        // `dirs::home_dir()` resolves `%USERPROFILE%` (via the known-folder API
        // — more robust than reading the env var directly).
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(r"C:\"))
    }
}

/// Current real uid, used to namespace the `/tmp` fallback sockets per user.
/// Wraps `getuid(2)` so the single `unsafe` lives in one place.
///
/// Unix-only: Windows has no uid concept and namespaces its named-pipe
/// endpoints by username instead (see [`endpoint_user_suffix`]).
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // SAFETY: `getuid(2)` is async-signal-safe and has no failure mode; it
    // simply returns the calling process's real uid.
    unsafe { libc::getuid() }
}

/// Per-user namespacing suffix for the Windows named-pipe endpoints — the
/// Win32 analogue of the per-uid `/tmp` socket suffix.
///
/// **PRD #163, release-gating.** The #42 skeleton read `%USERNAME%` and fell
/// back to the literal `"user"` when it was unset, which *collides across
/// users*: two accounts on one host would compute the same pipe name, so the
/// loser's clients would be handed to the winner's daemon. The uid this
/// replaces never collides, and neither may its Windows counterpart.
///
/// The source is therefore the **current user's SID** (`S-1-5-21-…`), read from
/// the process token — the exact analogue of `getuid(2)` — and not `%USERNAME%`:
///
/// - A SID is unique. `%USERNAME%` is not: `DOMAIN_A\alice` and `DOMAIN_B\alice`
///   logged into the same machine both report `alice`.
/// - A SID cannot be *steered*. An env var can, and the daemon, the ui/attach
///   client, and the hook client (which runs inside an agent's deliberately
///   scrubbed environment) must all derive the *same* endpoint name — an agent
///   whose `%USERNAME%` was unset or rewritten would otherwise compute a
///   different pipe name, or be pointed at a foreign one.
///
/// Resolved once and cached: a process's token user SID cannot change.
///
/// When the SID cannot be read there is no non-colliding source left, so this
/// is a hard error (per the PRD: "a non-colliding fallback **or hard error**")
/// rather than a silent collision. `DOT_AGENT_DECK_SOCKET` /
/// `DOT_AGENT_DECK_ATTACH_SOCKET` are consulted *before* this function and
/// bypass it entirely, so an explicit endpoint name is always available as an
/// escape hatch.
#[cfg(windows)]
pub fn endpoint_user_suffix() -> String {
    static SUFFIX: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SUFFIX
        .get_or_init(|| match current_user_sid() {
            Ok(sid) if is_pipe_name_token(&sid) => sid,
            Ok(sid) => panic!(
                "current-user SID {sid:?} is not usable as a named-pipe segment; \
                 refusing a colliding fallback — set DOT_AGENT_DECK_SOCKET and \
                 DOT_AGENT_DECK_ATTACH_SOCKET to explicit per-user pipe names"
            ),
            Err(err) => panic!(
                "cannot read the current user's SID for the per-user pipe name ({err}); \
                 refusing a colliding fallback — set DOT_AGENT_DECK_SOCKET and \
                 DOT_AGENT_DECK_ATTACH_SOCKET to explicit per-user pipe names"
            ),
        })
        .clone()
}

/// Whether `token` is safe to embed as the per-user segment of a
/// `\\.\pipe\dot-agent-deck-<token>-…` name: non-empty (an empty segment would
/// collide with every other empty one), restricted to characters that cannot
/// escape the pipe namespace (`\` is the pipe-name separator; `/` and whitespace
/// are rejected for the same reason), and short enough that the longest fixed
/// prefix+suffix around it (`\\.\pipe\dot-agent-deck-` + `-attach`, 31 chars)
/// still fits the 256-character named-pipe limit.
///
/// Compiled on every platform — it is pure data, so the rule stays unit-testable
/// on Linux CI where the `#[cfg(windows)]` caller is absent.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_pipe_name_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 200
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The current user's SID in canonical string form, cached, **without**
/// [`endpoint_user_suffix`]'s panic-on-failure (PRD #163 M4).
///
/// `endpoint_user_suffix` must panic when the SID is unreadable: it feeds a pipe
/// *name*, and the only alternative there is a colliding fallback. The filesystem
/// security backend has a better option — fail the individual operation closed —
/// so it needs the same one cached value as a `Result`. Both go through this,
/// which is what keeps the pipe name, the pipe's DACL, the spawn mutex's DACL and
/// the config-file DACLs from ever disagreeing about which user we are.
///
/// A process's token user SID cannot change, so the result (success *or* failure)
/// is resolved once. The error is cached as a string because [`std::io::Error`]
/// is not `Clone`; the kind is not load-bearing — every consumer treats "cannot
/// read our own SID" as fatal-for-this-operation.
#[cfg(windows)]
pub(crate) fn current_user_sid() -> std::io::Result<String> {
    static SID: std::sync::OnceLock<Result<String, String>> = std::sync::OnceLock::new();
    match SID.get_or_init(|| current_user_sid_string().map_err(|err| err.to_string())) {
        Ok(sid) => Ok(sid.clone()),
        Err(message) => Err(std::io::Error::other(message.clone())),
    }
}

/// Read the calling process's user SID and return it in the canonical string
/// form (`S-<revision>-<authority>-<sub-authority>…`).
///
/// Uses the token rather than any env var so the value is identical in the
/// daemon and in every client, however their environments were scrubbed (see
/// [`endpoint_user_suffix`]).
#[cfg(windows)]
fn current_user_sid_string() -> std::io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Closes the opened process token on every exit path below.
    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a successful `OpenProcessToken` and is
            // closed exactly once, here.
            unsafe { CloseHandle(self.0) };
        }
    }

    let mut raw_token: HANDLE = std::ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no release;
    // `raw_token` is a valid out-pointer for the duration of the call.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw_token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = TokenHandle(raw_token);

    // Documented size probe: a null buffer fails with ERROR_INSUFFICIENT_BUFFER
    // and reports the required byte count.
    let mut needed: u32 = 0;
    // SAFETY: null buffer + zero length is the probe form; `needed` is a valid
    // out-pointer.
    unsafe { GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // `TOKEN_USER` leads with a pointer, so the buffer must be pointer-aligned;
    // a `Vec<u8>` is only byte-aligned. `Vec<u64>` is (over-)aligned for every
    // Windows target we build.
    let mut buf = vec![0u64; needed.div_ceil(8) as usize];
    // SAFETY: `buf` owns at least `needed` bytes of writable, 8-byte-aligned
    // storage, and `needed` is passed as its true length.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: on success the buffer holds a `TOKEN_USER` followed by the
    // variable-length SID it points into; both stay valid as long as `buf`.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut wide: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` is the token's SID and `wide` a valid out-pointer; on
    // success the callee hands back a `LocalAlloc`'d NUL-terminated string.
    if unsafe { ConvertSidToStringSidW(sid, &mut wide) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut len = 0usize;
    // SAFETY: `wide` is NUL-terminated, so the scan stops inside the allocation.
    while unsafe { *wide.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` UTF-16 units of the live `LocalAlloc`'d buffer.
    let sid_string = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(wide, len) });
    // SAFETY: frees exactly the buffer `ConvertSidToStringSidW` allocated;
    // `wide` is not read again.
    unsafe { LocalFree(wide.cast()) };

    drop(token);
    Ok(sid_string)
}

/// Hook-ingestion endpoint. Unix: a Unix-domain-socket path
/// (`$XDG_RUNTIME_DIR/dot-agent-deck.sock` else `/tmp/dot-agent-deck-{uid}.sock`).
/// Windows: the named-pipe `\\.\pipe\dot-agent-deck-{user}-hook`, where
/// `{user}` is the non-colliding per-user token from [`endpoint_user_suffix`].
///
/// `DOT_AGENT_DECK_SOCKET` overrides on both platforms.
pub fn socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_SOCKET") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("dot-agent-deck.sock");
        }

        // PRD #93 reviewer REV-2: the `/tmp` fallback must include the uid so
        // two users on the same host can't collide on the same socket path
        // (the daemon is per-user; the 0o600 mode is on the socket inode, but
        // the *path* still has to be unique, otherwise the loser's `bind(2)`
        // sees `EADDRINUSE` against the winner's inode). Same rationale as
        // `attach_socket_path` below.
        PathBuf::from(format!("/tmp/dot-agent-deck-{}.sock", current_uid()))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\dot-agent-deck-{}-hook",
            endpoint_user_suffix()
        ))
    }
}

/// Streaming-attach endpoint (separate from the hook endpoint so the two
/// protocols have disjoint wire formats — hook is line-delimited JSON, attach
/// is a binary frame protocol). Unix: `$XDG_RUNTIME_DIR/dot-agent-deck-attach.sock`
/// else `/tmp/dot-agent-deck-attach-{uid}.sock`. Windows: the named pipe
/// `\\.\pipe\dot-agent-deck-{user}-attach` (`{user}` per
/// [`endpoint_user_suffix`]).
///
/// `DOT_AGENT_DECK_ATTACH_SOCKET` overrides on both platforms.
pub fn attach_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            return PathBuf::from(runtime_dir).join("dot-agent-deck-attach.sock");
        }

        // PRD #93 reviewer REV-2: include the uid in the `/tmp` fallback path so
        // two users on the same host get disjoint sockets (each daemon's
        // `bind(2)` would otherwise collide with the other user's inode), and
        // so the path itself can't be observed by another user to figure out
        // *which* deck process to target. The 0o600 mode on the inode is
        // already enforced; the per-user path is the missing half.
        PathBuf::from(format!("/tmp/dot-agent-deck-attach-{}.sock", current_uid()))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(
            r"\\.\pipe\dot-agent-deck-{}-attach",
            endpoint_user_suffix()
        ))
    }
}

/// Per-user state directory (detached-daemon log, spawn mutex). Resolution
/// order on Unix:
///
/// 1. `DOT_AGENT_DECK_STATE_DIR` — explicit override (tests use this).
/// 2. `$XDG_STATE_HOME/dot-agent-deck` — freedesktop spec default.
/// 3. `$HOME/.local/state/dot-agent-deck` — XDG fallback.
///
/// Windows: the override first, then `%LOCALAPPDATA%\dot-agent-deck` (already
/// per-user ACL'd by default).
pub fn state_dir() -> PathBuf {
    if let Ok(path) = std::env::var("DOT_AGENT_DECK_STATE_DIR") {
        return PathBuf::from(path);
    }

    #[cfg(unix)]
    {
        match std::env::var("XDG_STATE_HOME") {
            Ok(state_home) if !state_home.is_empty() => {
                PathBuf::from(state_home).join("dot-agent-deck")
            }
            _ => home_dir().join(".local/state/dot-agent-deck"),
        }
    }
    #[cfg(windows)]
    {
        // `%LOCALAPPDATA%\dot-agent-deck` (already per-user ACL'd by default).
        state_dir_platform_root()
    }
}

/// Per-user **config** root — the directory holding `config.toml`,
/// `session.toml`, `keybindings.toml`, `remotes.toml`, `schedules.toml` and the
/// small JSON state files that live beside them (PRD #163 M1).
///
/// Unix: `$HOME/.config/dot-agent-deck` — byte-for-byte the historical
/// `dirs_home().join(".config/dot-agent-deck")` every caller used inline.
/// Windows: `%APPDATA%\dot-agent-deck` (`dirs::config_dir()`, resolved via the
/// known-folder API), falling back to `%USERPROFILE%\AppData\Roaming\…`.
/// `%APPDATA%` — not `%USERPROFILE%\.config` — is the conventional Windows
/// per-user config root, and it completes the `%LOCALAPPDATA%`/`%APPDATA%`/
/// `%USERPROFILE%` mapping locked in #42.
///
/// Every caller checks its own `DOT_AGENT_DECK_*` file override *before* calling
/// this, so those overrides stay authoritative on both platforms.
pub fn config_dir() -> PathBuf {
    #[cfg(unix)]
    {
        home_dir().join(".config/dot-agent-deck")
    }
    #[cfg(windows)]
    {
        match dirs::config_dir() {
            Some(config) => config.join("dot-agent-deck"),
            None => home_dir().join("AppData/Roaming/dot-agent-deck"),
        }
    }
}

/// `$XDG_CONFIG_HOME` when set and non-empty, else `None`.
///
/// Windows always returns `None`: the XDG spec has no Windows analogue, and
/// [`config_dir`] already resolves the platform's own per-user config root, so
/// only the `DOT_AGENT_DECK_*` overrides apply there. Keeping the check behind
/// this seam is what lets the callers stay `cfg`-free.
pub fn xdg_config_home() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        match std::env::var("XDG_CONFIG_HOME") {
            Ok(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
            _ => None,
        }
    }
    #[cfg(windows)]
    {
        None
    }
}

/// Platform default root for the daemon's per-endpoint lock files
/// (`{basename}-{hash}.lock`). Callers apply their own overrides — the
/// per-`Daemon` builder override and `DOT_AGENT_DECK_LOCK_DIR` — *before* this,
/// so this is only the platform tail of `daemon::lock_root`.
///
/// Unix: `$XDG_RUNTIME_DIR/dot-agent-deck` when set and non-empty, else
/// `$HOME/.cache/dot-agent-deck` — byte-for-byte the historical resolution.
/// Never `/tmp` (PRD #93 round-4 auditor BLOCKER: a world-writable lock root
/// lets a foreign uid pre-create the lock entry and DoS the target user's daemon
/// startup).
///
/// Windows: `%LOCALAPPDATA%\dot-agent-deck\locks` — there is no
/// `$XDG_RUNTIME_DIR` analogue, and `%LOCALAPPDATA%` carries the same
/// "not world-writable" property the Unix choice exists for. Kept distinct from
/// [`state_dir`] so the lock files stay separable from the daemon log / spawn
/// mutex.
pub fn lock_root_default() -> PathBuf {
    #[cfg(unix)]
    {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
            && !runtime_dir.is_empty()
        {
            return PathBuf::from(runtime_dir).join("dot-agent-deck");
        }
        home_dir().join(".cache").join("dot-agent-deck")
    }
    #[cfg(windows)]
    {
        state_dir_platform_root().join("locks")
    }
}

/// `%LOCALAPPDATA%\dot-agent-deck` — the platform root shared by [`state_dir`]
/// and [`lock_root_default`] on Windows (the former uses it directly, the latter
/// nests `locks` under it). Split out so the `%LOCALAPPDATA%` fallback chain is
/// written once.
#[cfg(windows)]
fn state_dir_platform_root() -> PathBuf {
    match dirs::data_local_dir() {
        Some(local) => local.join("dot-agent-deck"),
        None => home_dir().join("AppData/Local/dot-agent-deck"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Windows per-user pipe segment must be a *non-colliding*, namespace-safe
    /// token (PRD #163). Pure data, so the rule is checked on Linux CI too.
    #[test]
    fn pipe_name_token_accepts_a_sid_and_rejects_unsafe_or_colliding_sources() {
        // The canonical SID string form — what `endpoint_user_suffix` embeds.
        assert!(is_pipe_name_token(
            "S-1-5-21-3623811015-3361044348-30300820-1013"
        ));
        assert!(is_pipe_name_token("alice"));

        // An empty segment collides with every other empty segment — exactly the
        // failure mode the literal `"user"` fallback had.
        assert!(!is_pipe_name_token(""));
        // `\` is the pipe-name separator: a domain-qualified name would escape
        // the `\\.\pipe\dot-agent-deck-…` namespace.
        assert!(!is_pipe_name_token(r"DOMAIN\alice"));
        assert!(!is_pipe_name_token("alice/../bob"));
        assert!(!is_pipe_name_token("first last"));
        assert!(!is_pipe_name_token("üser"));
        // Long enough to push the full pipe name past the 256-char limit.
        assert!(!is_pipe_name_token(&"a".repeat(201)));
    }

    /// The `DOT_AGENT_DECK_*` overrides are authoritative on BOTH platforms:
    /// they are consulted before any platform default, so a set override is
    /// returned verbatim (and, on Windows, short-circuits the per-user pipe-name
    /// derivation entirely).
    #[test]
    fn env_overrides_precede_every_platform_default() {
        let _guard = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_socket = std::env::var("DOT_AGENT_DECK_SOCKET").ok();
        let prev_attach = std::env::var("DOT_AGENT_DECK_ATTACH_SOCKET").ok();
        let prev_state = std::env::var("DOT_AGENT_DECK_STATE_DIR").ok();
        // SAFETY: env-var lock held; every value is restored on the way out.
        unsafe {
            std::env::set_var("DOT_AGENT_DECK_SOCKET", "override-hook");
            std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", "override-attach");
            std::env::set_var("DOT_AGENT_DECK_STATE_DIR", "override-state");
        }

        assert_eq!(socket_path(), PathBuf::from("override-hook"));
        assert_eq!(attach_socket_path(), PathBuf::from("override-attach"));
        assert_eq!(state_dir(), PathBuf::from("override-state"));

        // SAFETY: same lock held; restoring the previous values.
        unsafe {
            match prev_socket {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_SOCKET", v),
                None => std::env::remove_var("DOT_AGENT_DECK_SOCKET"),
            }
            match prev_attach {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_ATTACH_SOCKET", v),
                None => std::env::remove_var("DOT_AGENT_DECK_ATTACH_SOCKET"),
            }
            match prev_state {
                Some(v) => std::env::set_var("DOT_AGENT_DECK_STATE_DIR", v),
                None => std::env::remove_var("DOT_AGENT_DECK_STATE_DIR"),
            }
        }
    }

    /// `config_dir` is the home-anchored config root, NOT an XDG-anchored one:
    /// only `schedules_path` honors `$XDG_CONFIG_HOME`, and it does so itself
    /// (via [`xdg_config_home`]) so that `config.toml`/`session.toml`/… keep
    /// their historical `~/.config/dot-agent-deck` location.
    #[cfg(unix)]
    #[test]
    fn config_dir_is_home_anchored_and_ignores_xdg_config_home() {
        let _guard = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/should/not/anchor/config-dir");
        }

        assert_eq!(config_dir(), home_dir().join(".config/dot-agent-deck"));
        assert_eq!(
            xdg_config_home(),
            Some(PathBuf::from("/should/not/anchor/config-dir"))
        );

        // An empty value is treated as unset (the historical `!is_empty()` guard).
        // SAFETY: same lock held.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "");
        }
        assert_eq!(xdg_config_home(), None);

        // SAFETY: same lock held; restoring the previous value.
        unsafe {
            match prev_xdg {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }
}
