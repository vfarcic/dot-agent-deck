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

use std::path::{Path, PathBuf};

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

/// Home directory anchor for the **third-party tool config** writers —
/// `hooks_manage`'s `~/.claude/settings.json` and `opencode_manage`'s
/// `~/.config/opencode` / `~/.opencode` roots.
///
/// Identical to [`home_dir`] except for the Unix `$HOME`-unset fallback, which is
/// `/tmp` here instead of `/`. That is not a preference, it is preservation: both
/// call sites read `std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())`
/// before PRD #163 M1 routed them through this module, and #163's bar is
/// byte-for-byte Unix behavior — including in the `$HOME`-unset case, where the
/// paths would otherwise move from `/tmp/.claude/settings.json` to
/// `/.claude/settings.json` (a different file, and one an unprivileged user
/// cannot even create). The fallback lives here, at the seam, rather than as a
/// `cfg` in each call site.
///
/// Windows: exactly [`home_dir`] — `%USERPROFILE%` via the known-folder API. The
/// `/tmp` fallback has no meaning there (nothing loads `C:\tmp\.claude`), and
/// `$HOME` is normally unset on Windows, which is precisely why these two sites
/// had to come through the seam at all.
pub fn home_dir_with_tmp_fallback() -> PathBuf {
    #[cfg(unix)]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
    }
    #[cfg(windows)]
    {
        home_dir()
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

/// The crate's own package name — the literal fallback [`binary_name`] returns
/// when `current_exe()` is unavailable or genuinely unusable (an error, an
/// empty or non-UTF-8 path), and the single source of truth every other such
/// fallback in the crate should read rather than re-typing the literal
/// `"dot-agent-deck"`.
pub const DEFAULT_BINARY_NAME: &str = env!("CARGO_PKG_NAME");

/// The command name this build was invoked as — the file name component of
/// [`std::env::current_exe`] — for generated text that tells an agent to run
/// the deck **by name through `$PATH`** (the `delegate` / `work-done` CLI
/// examples in `orchestrator_context::build_orchestrator_context` and
/// `state::work_done_footer`). A build installed under a different file name
/// must generate instructions naming ITSELF, not a baked-in literal —
/// otherwise the generated command resolves to a different binary than the
/// one that wrote it.
///
/// **Symlink resolution is platform-dependent — this is a fact about the
/// platform, not a choice this function makes, and any doc comment asserting
/// a single cross-platform behavior here is wrong on one of the two.** On
/// macOS `current_exe()` is backed by `_NSGetExecutablePath`, which reports
/// the path the process was INVOKED as: a symlink stays a symlink, confirmed
/// directly (not assumed) with a four-way probe on this crate's dev machine
/// covering direct invocation, a same-directory symlink, an absolute-target
/// symlink in another directory, and `$PATH` lookup of a symlink name — all
/// four returned the symlink's own path, never the target. On Linux
/// `current_exe()` reads `/proc/self/exe`, which the kernel resolves fully: a
/// symlink returns its TARGET's path. So `~/bin/deck ->
/// /opt/x/dot-agent-deck` generates `deck` (still on `$PATH`) on macOS but
/// `dot-agent-deck` (possibly not on `$PATH` at all) on Linux, for the exact
/// same install.
///
/// Two gates keep the bare file name usable rather than merely well-formed
/// (issue prageethw/dot-agent-deck#253 review/audit, tightened again by a later issue prageethw/dot-agent-deck#253 pass once
/// the review/audit gate itself turned out to prove only *resolvability*, not
/// *identity* — see the `$PATH` identity bullet below for what changed and
/// why the earlier gate was not enough):
///
/// - **`$PATH` identity.** The bare file name is used ONLY when a `$PATH`
///   lookup for it, walked with the SAME first-match semantics a shell uses
///   (the first entry containing an executable of that name wins; a later,
///   truly-matching entry is irrelevant), lands on the exact file THIS
///   PROCESS is running — not merely *some* executable sharing its name
///   ([`resolves_on_path`]). Resolvability alone used to be the whole gate;
///   it is not enough, because "some executable earlier on `$PATH`" can be a
///   stale build, an unrelated program, or — with a `$PATH` entry like `.` —
///   a file an attacker placed in whatever directory the deck process
///   happened to be running from. Identity is proven by canonicalizing both
///   the `$PATH` candidate and `current_exe()` (resolving symlinks on both
///   sides) and comparing the results ([`same_binary_identity`]). An empty or
///   relative `$PATH` entry is never trusted for this comparison even when it
///   contains a matching executable: a shell resolves it against ITS OWN
///   current directory, a value this process cannot observe and cannot
///   assume matches the consuming agent's shell, so no identity claim can be
///   proven through it — this is what closes the `PATH=.:/usr/bin` case.
/// - **Shell safety.** A name outside [`is_safe_binary_name`]'s conservative
///   allowlist is rejected — not quoted — for the same reason `wrap.rs`'s
///   `usable()` rejects rather than quotes: the bare name is interpolated
///   UNQUOTED into ```` ```bash ```` blocks an agent executes verbatim, and
///   quoting an unsafe *bare name* would still resolve to nothing on a normal
///   `$PATH` — converting an injection into a silent no-op rather than a
///   name that at least works.
///
/// When either gate rejects the bare file name, this does **not** fall back to
/// [`DEFAULT_BINARY_NAME`] — the deck process's own `$PATH` is only a *proxy*
/// for the consuming agent's (agents commonly run through a login shell that
/// sources profile files this process never saw), so a bare name this process
/// could not verify may still be perfectly runnable there, and conversely a
/// literal `dot-agent-deck` fallback can name a binary that was never
/// installed at all. Instead this falls back to `current_exe()`'s own
/// **absolute path**, quoted with [`shell_quote_if_needed`] so a path
/// containing whitespace still parses as one argument — a path is independent
/// of whatever `$PATH` the agent's shell ends up with, and it names this
/// exact running binary rather than whatever `$PATH` might resolve that name
/// to, so it resolves correctly regardless of which proxy this process's own
/// `$PATH` turned out to be.
///
/// [`DEFAULT_BINARY_NAME`] remains the fallback only when `current_exe()`
/// itself is unusable: an error, an empty file name, or (Unix) a file name
/// that is not valid UTF-8. The fallback matters more here than at most other
/// `current_exe()` call sites: `delegate` and `work-done` write to the
/// unversioned hook socket, both call sites are fire-and-forget, and the
/// daemon drops any frame it cannot parse without logging it — so a name that
/// resolves to a binary that cannot run produces no error anywhere, only a
/// signal that silently never arrives.
pub fn binary_name() -> String {
    resolve_binary_name(effective_current_exe(), resolves_on_path)
}

/// `current_exe()`, or — only under the `e2e` feature, and only once
/// [`set_test_current_exe_override`] has been called — the injected test
/// override. [`spawn_inprocess_daemon`]'s test harness (`tests/common/mod.rs`)
/// calls the setter with `env!("CARGO_BIN_EXE_dot-agent-deck")` before driving
/// `handle_delegate`, because a `handle_delegate` run entirely in-process
/// makes the CALLING process the libtest binary, not the deck — libtest's own
/// file name is shell-safe but never on `$PATH`, so without this override
/// [`binary_name`] would (correctly, for that process) name the libtest
/// binary itself, and an agent told to run it hits libtest's CLI parser
/// instead of the deck's (issue prageethw/dot-agent-deck#253 round-4 verification, finding 1).
///
/// This mechanism is gated behind the `e2e` Cargo feature rather than a
/// runtime env var — Greptile's P2 on this issue's round 2 was exactly that a
/// prior env-var seam (`DOT_AGENT_DECK_TEST_BINARY_ON_PATH`) stayed
/// "production-active": present, and consultable, in every build. Gating on
/// `e2e` instead means the override, the setter, and this indirection do not
/// exist in the compiled artifact at all for a release build or even a plain
/// `cargo test-fast` run (`e2e` is off for both) — there is no code path for
/// production to accidentally take, because there is no code.
#[cfg(feature = "e2e")]
fn effective_current_exe() -> std::io::Result<PathBuf> {
    match TEST_CURRENT_EXE_OVERRIDE.get() {
        Some(path) => Ok(path.clone()),
        None => std::env::current_exe(),
    }
}

#[cfg(not(feature = "e2e"))]
fn effective_current_exe() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

/// Backing store for [`set_test_current_exe_override`]. A plain [`OnceLock`]
/// is enough: every call site within a given test process passes the same
/// compile-time constant (`env!("CARGO_BIN_EXE_dot-agent-deck")`), so a
/// second `set` call — if it ever happens — is safely redundant, not a race.
/// `cargo nextest` gives each test its own process, so a value set here can
/// never leak into a different test's process.
///
/// [`OnceLock`]: std::sync::OnceLock
#[cfg(feature = "e2e")]
static TEST_CURRENT_EXE_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Test-only: make [`binary_name`] resolve as though `current_exe()` were
/// `path`, for the remainder of this process. Exists only under the `e2e`
/// feature (see [`effective_current_exe`]) and nothing under `src/` calls
/// it — only the e2e test harness does, from `spawn_inprocess_daemon()`.
#[cfg(feature = "e2e")]
pub fn set_test_current_exe_override(path: PathBuf) {
    let _ = TEST_CURRENT_EXE_OVERRIDE.set(path);
}

/// Pure seam behind [`binary_name`]. `path_identity_matches` is injected so
/// both the malformed-input fallback ([`delegate/018`]) and the two bare-name
/// usability gates (shell safety, `$PATH` identity) are unit-testable with a
/// synthetic `current_exe()` and a synthetic resolver, without needing a real
/// unusable `current_exe()` or a real `$PATH` entry. The seam takes both the
/// candidate `name` and the resolved `current_exe()` path — proving identity
/// needs both sides of the comparison, not just the name.
fn resolve_binary_name(
    current_exe: std::io::Result<PathBuf>,
    path_identity_matches: impl Fn(&str, &Path) -> bool,
) -> String {
    let Ok(path) = current_exe else {
        return DEFAULT_BINARY_NAME.to_string();
    };
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return DEFAULT_BINARY_NAME.to_string();
    };
    if is_safe_binary_name(name) && path_identity_matches(name, &path) {
        return name.to_string();
    }
    match path.to_str() {
        Some(path_str) => shell_quote_if_needed(path_str),
        None => DEFAULT_BINARY_NAME.to_string(),
    }
}

/// Whether `name` is safe to interpolate UNQUOTED into the generated `bash`
/// command examples [`binary_name`] feeds (issue prageethw/dot-agent-deck#253 review F2 / audit F1):
/// a conservative ALLOWLIST rather than a denylist, since the failure mode
/// this guards against is an agent's shell reinterpreting whatever falls
/// outside it. Rejects an empty name, a leading `-` (would be read as a flag
/// by whatever runs the generated line), and anything outside ASCII
/// alphanumerics plus `-`, `_`, `.`, `+` — which also rejects the mundane
/// motivating cases (`dot-agent-deck (1)` from a browser download,
/// `dot-agent-deck copy` from a Finder duplicate) alongside the adversarial
/// ones (`;`, `` ` ``, `$`, a literal newline).
fn is_safe_binary_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+'))
}

/// Whether `name`'s `$PATH` lookup identifies the SAME running executable as
/// `exe_path` — the real resolver [`binary_name`] injects into
/// [`resolve_binary_name`] (issue prageethw/dot-agent-deck#253's identity-verification tightening of
/// the earlier resolvability-only gate; see [`binary_name`]'s doc for why
/// resolvability alone was insufficient). The lookup walks `$PATH` with
/// shell-equivalent FIRST-MATCH semantics via [`first_path_match`] — the
/// first entry containing an executable `name` wins, exactly as a shell's
/// command lookup would, so a later, truly-matching entry is irrelevant if an
/// earlier one already shadows it. A match is an identity match only when:
///
/// - it was found via an absolute `$PATH` entry, never an empty or relative
///   one ([`is_untrustworthy_path_entry`]) — a shell resolves those against
///   ITS OWN current directory, a value this process cannot observe and
///   cannot assume matches the consuming agent's shell (this is what closes
///   the `PATH=.:/usr/bin` case: the `.` entry is checked first, and finding
///   an executable there stops the walk without ever claiming a match);
/// - the file it names is a genuinely **executable** file — same exec-bit
///   check as `orchestrator_ext`'s `is_executable_file`: `is_file()` plus, on
///   Unix, at least one exec permission bit; non-Unix has no cheap exec-bit
///   probe, so a regular file is accepted there. Unlike `wrap.rs`'s
///   `usable()`, a bare existence probe (`is_file()`) is not enough on its
///   own: `binary_name()` feeds an agent's shell a bare command name it is
///   expected to *run*, so a readable-but-not-executable regular file of that
///   name earlier on `$PATH` must not report success (issue prageethw/dot-agent-deck#253 review);
///   and
/// - it canonicalizes to the same file as `exe_path`, symlinks resolved on
///   both sides — [`same_binary_identity`].
///
/// No test-only override is needed: under `cargo test`/`cargo nextest`, each
/// test's own throwaway binary under `target/<profile>/deps/` is never on
/// `$PATH` either way, so [`resolve_binary_name`] naturally takes its
/// absolute-path fallback branch — which is itself the RUNNING binary's own
/// path, not the [`DEFAULT_BINARY_NAME`] literal — and that is exactly what
/// `orchestration/delegate/016`–`017` assert.
fn resolves_on_path(name: &str, exe_path: &Path) -> bool {
    match std::env::var_os("PATH") {
        Some(paths) => path_identity_match(&paths, name, exe_path),
        None => false,
    }
}

/// Whether `dir` — a single entry from splitting a `$PATH`-shaped value — is
/// one a shell resolves against ITS OWN current directory rather than a fixed
/// location: an empty entry (POSIX shells treat `PATH=a::b` and a leading or
/// trailing `:` as `.`) or an explicitly relative one (`PATH=bin:/usr/bin`).
/// Neither can be trusted for an identity comparison made from this process,
/// because the consuming agent's shell may have a different current
/// directory than this one — the mechanism the `PATH=.:/usr/bin` case in
/// issue prageethw/dot-agent-deck#253 depends on.
fn is_untrustworthy_path_entry(dir: &Path) -> bool {
    dir.as_os_str().is_empty() || dir.is_relative()
}

/// Outcome of walking a `$PATH`-shaped value for `name` with shell
/// first-match semantics: the walk stops at the first entry containing an
/// executable `name`, exactly as a shell's command lookup would — a later
/// entry is never consulted once an earlier one has matched.
enum FirstPathMatch {
    /// The first match was found via an absolute entry — trustworthy enough
    /// to canonicalize and compare against `current_exe()`.
    Absolute(PathBuf),
    /// The first match was found via an empty or relative entry
    /// ([`is_untrustworthy_path_entry`]): a shell would still select this
    /// file, but this process cannot vouch for which file that is.
    Untrustworthy,
    /// No `$PATH` entry contains an executable `name`.
    None,
}

/// Scan a `PATH`-shaped value for an executable file named `name`, stopping
/// at the first match with shell-equivalent first-match semantics. Pure over
/// its `path` argument (no environment read), matching `orchestrator_ext`'s
/// `path_contains_binary` precedent, so this is unit-testable with a
/// synthetic `PATH` value rather than by mutating the process-global `PATH`
/// env var.
fn first_path_match(path: &std::ffi::OsStr, name: &str) -> FirstPathMatch {
    for dir in std::env::split_paths(path) {
        let candidate = dir.join(name);
        if !is_executable_file(&candidate) {
            continue;
        }
        return if is_untrustworthy_path_entry(&dir) {
            FirstPathMatch::Untrustworthy
        } else {
            FirstPathMatch::Absolute(candidate)
        };
    }
    FirstPathMatch::None
}

/// Whether `path` contains an executable `name` at all, regardless of
/// identity — the resolvability half of the original (issue prageethw/dot-agent-deck#253
/// review/audit) gate, kept so the exec-bit requirement stays testable in
/// isolation from the identity comparison [`path_identity_match`] adds on
/// top of it. Test-only: production code goes through [`path_identity_match`]
/// exclusively, since resolvability without identity is exactly the gate
/// issue prageethw/dot-agent-deck#253's `$PATH`-identity pass closed.
#[cfg(test)]
fn path_contains_executable(path: &std::ffi::OsStr, name: &str) -> bool {
    !matches!(first_path_match(path, name), FirstPathMatch::None)
}

/// Whether `name`'s first match on `path` (shell first-match semantics) is
/// the SAME file as `exe_path`, symlinks resolved on both sides. An
/// untrustworthy first match (empty/relative `$PATH` entry) or no match at
/// all is never an identity match.
fn path_identity_match(path: &std::ffi::OsStr, name: &str, exe_path: &Path) -> bool {
    match first_path_match(path, name) {
        FirstPathMatch::Absolute(candidate) => same_binary_identity(&candidate, exe_path),
        FirstPathMatch::Untrustworthy | FirstPathMatch::None => false,
    }
}

/// Whether `candidate` and `exe_path` name the same underlying file,
/// resolving symlinks on both sides. `std::fs::canonicalize` rather than a
/// raw device+inode comparison: it is available on every target this crate
/// builds for (device+inode is Unix-only and would need a second code path
/// for Windows), and it is sufficient for the threat this closes — a `$PATH`
/// entry pointing at an unrelated file. (A hard link sharing `exe_path`'s
/// inode canonicalizes to a different path and is treated as a non-match;
/// that is conservative, not a gap — a hard link is byte-identical content
/// under a different name, not a spoof.) A canonicalization failure (dangling
/// symlink, permission denied, removed between the executable-bit check and
/// here) is treated as "not a match" rather than propagated: the caller's
/// fallback to the absolute path is always safe, so failing closed here costs
/// nothing.
fn same_binary_identity(candidate: &Path, exe_path: &Path) -> bool {
    match (
        std::fs::canonicalize(candidate),
        std::fs::canonicalize(exe_path),
    ) {
        (Ok(candidate_real), Ok(exe_real)) => candidate_real == exe_real,
        _ => false,
    }
}

/// Whether `candidate` is a regular file that is *also* executable. Same
/// rationale and shape as `orchestrator_ext::is_executable_file`: `is_file()`
/// alone would accept a same-named regular-but-non-executable file earlier on
/// `$PATH`. On Unix this additionally requires at least one exec bit
/// (`mode & 0o111 != 0`); on non-Unix targets there is no cheap exec-bit
/// check, so a regular file is accepted.
fn is_executable_file(candidate: &std::path::Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(candidate) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Single-quote `path` for a POSIX shell only when it contains a character
/// outside a conservative safe set; otherwise return it unchanged. Canonical
/// copy of the identical helper duplicated in `codex_hooks_manage.rs` and
/// `devin_hooks_manage.rs`, which both call this one rather than defining
/// their own (issue prageethw/dot-agent-deck#253: [`binary_name`]'s absolute-path fallback needed the
/// same quoting, so the third call site is what pushed the helper here rather
/// than re-duplicating it a third time).
pub(crate) fn shell_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b'%' | b','
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', r"'\''"))
    }
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
    use spec::spec;

    /// Scenario: Drive `resolve_binary_name` — the pure seam behind
    /// `binary_name` — directly with a synthetic `current_exe()` result for
    /// each unusable case a real call can produce: an `Err`, a path with no
    /// file name (`/`), and (Unix-only) a file name that is not valid UTF-8.
    /// Every case must fall back to `DEFAULT_BINARY_NAME`, never panic or
    /// produce an empty string.
    #[spec("orchestration/delegate/018")]
    #[test]
    fn delegate_018_binary_name_falls_back_to_the_default_literal_when_current_exe_is_unusable() {
        // The resolver is irrelevant to every case here — each fails before
        // `resolve_binary_name` would ever consult it — so an always-true
        // stub isolates that these are genuinely malformed-input failures,
        // not incidental `$PATH`/shell-safety/identity rejections.
        assert_eq!(
            resolve_binary_name(Err(std::io::Error::other("no such process")), |_, _| true),
            DEFAULT_BINARY_NAME,
            "an current_exe() error must fall back to the default literal"
        );
        assert_eq!(
            resolve_binary_name(Ok(PathBuf::from("/")), |_, _| true),
            DEFAULT_BINARY_NAME,
            "a path with no file name component must fall back to the default literal"
        );
        #[cfg(unix)]
        {
            use std::ffi::OsStr;
            use std::os::unix::ffi::OsStrExt;
            // 0xFF is not valid UTF-8 in any position, so `into_string()` fails.
            let invalid = OsStr::from_bytes(&[0xFF]);
            assert_eq!(
                resolve_binary_name(Ok(PathBuf::from("/usr/local/bin").join(invalid)), |_, _| {
                    true
                }),
                DEFAULT_BINARY_NAME,
                "a non-UTF-8 file name must fall back to the default literal"
            );
        }
    }

    /// Reviewer finding F5: nothing previously pinned the SUCCESS branch, so
    /// a `resolve_binary_name` that returned the full path (instead of just
    /// the file name) would have passed the entire suite — every other test
    /// only exercises fallback inputs. This asserts the happy path returns a
    /// BARE file name, not an absolute path.
    #[test]
    fn resolve_binary_name_returns_the_bare_file_name_on_the_success_path() {
        assert_eq!(
            resolve_binary_name(Ok(PathBuf::from("/usr/local/bin/deck-x")), |_, _| true),
            "deck-x",
            "the success branch must return a bare file name, not the full path"
        );
    }

    /// Reviewer F2 / auditor F1, updated for issue prageethw/dot-agent-deck#253's Greptile P1: a
    /// well-formed name that WOULD resolve on `$PATH` must still be rejected
    /// when it is not shell-safe — the shell-safety gate has to reject
    /// independently of the `$PATH` gate, not rely on an unsafe name also
    /// happening to be absent from `$PATH`. It no longer falls back to
    /// [`DEFAULT_BINARY_NAME`], though: since `current_exe()` is otherwise
    /// usable, it falls back to that absolute path instead, quoted exactly
    /// like [`shell_quote_if_needed`] would quote it directly.
    #[test]
    fn resolve_binary_name_falls_back_to_the_absolute_path_when_the_name_is_shell_unsafe() {
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from("/usr/local/bin/dot-agent-deck (1)")),
                |_, _| true
            ),
            "'/usr/local/bin/dot-agent-deck (1)'",
            "a name containing shell metacharacters must fall back to the quoted absolute \
             path even when it resolves on $PATH"
        );
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from("/usr/local/bin/dot-agent-deck copy")),
                |_, _| true
            ),
            "'/usr/local/bin/dot-agent-deck copy'",
            "a name containing whitespace must fall back to the quoted absolute path \
             (the Finder-duplicate case)"
        );
        assert_eq!(
            resolve_binary_name(Ok(PathBuf::from("/usr/local/bin/-rf")), |_, _| true),
            "/usr/local/bin/-rf",
            "a name with a leading '-' must fall back to the absolute path — unquoted, \
             since as a full path argument (not a bare token) a leading '-' in the file \
             name component is not read as a flag"
        );
    }

    /// Reviewer F1 / auditor F1, updated for issue prageethw/dot-agent-deck#253's Greptile P1 and
    /// again for the `$PATH`-identity tightening: a well-formed, shell-safe
    /// name whose `$PATH` lookup does NOT identity-match `current_exe()`
    /// (here: an injected resolver that always reports "no match", standing
    /// in for "not found" as well as "found the wrong file" — both take this
    /// branch) must still avoid emitting an unrunnable or wrong-binary
    /// command — this is the case that regressed from "wrong but runnable by
    /// accident" to "resolves to nothing, or resolves to something else"
    /// before the gate existed. It no longer falls back to
    /// [`DEFAULT_BINARY_NAME`] (a proxy for the CONSUMING agent's `$PATH`,
    /// which the deck process's own `$PATH` cannot reliably stand in for);
    /// it falls back to the absolute `current_exe()` path instead, which
    /// resolves regardless of either process's `$PATH`.
    #[test]
    fn resolve_binary_name_falls_back_to_the_absolute_path_when_the_name_is_not_on_path() {
        assert_eq!(
            resolve_binary_name(Ok(PathBuf::from("/opt/build/worker-agent-deck")), |_, _| {
                false
            }),
            "/opt/build/worker-agent-deck",
            "a well-formed name whose $PATH lookup does not identity-match must fall back \
             to the (unquoted, since it needs no quoting) absolute path"
        );
    }

    /// Issue prageethw/dot-agent-deck#253 Greptile P1: when `current_exe()` itself is fine but
    /// neither gate is satisfied, the fallback must be the absolute path,
    /// never the generic [`DEFAULT_BINARY_NAME`] literal — an absolute path
    /// is independent of whatever `$PATH` the CONSUMING agent's login shell
    /// ends up with, whereas the deck process's own `$PATH` (what the two
    /// gates check) is only a proxy for it and a `DEFAULT_BINARY_NAME`
    /// fallback can name a binary that was never installed under that name
    /// at all.
    #[test]
    fn resolve_binary_name_absolute_path_fallback_is_never_the_default_literal() {
        let fallback =
            resolve_binary_name(Ok(PathBuf::from("/opt/build/worker-agent-deck")), |_, _| {
                false
            });
        assert_ne!(fallback, DEFAULT_BINARY_NAME);
        assert_eq!(fallback, "/opt/build/worker-agent-deck");
    }

    /// Issue prageethw/dot-agent-deck#253 Greptile P1 (the smaller half): [`path_contains_executable`]
    /// (the pure helper behind [`resolves_on_path`]) must require the execute
    /// bit, not just file existence — a readable but non-executable regular
    /// file must not be treated as resolving, since `binary_name()` feeds an
    /// agent's shell a bare name it is expected to *run*. Same distinction
    /// `orchestrator_ext`'s `is_executable_file` already draws for `pi`
    /// discovery. Driven through a synthetic `PATH` value rather than the
    /// real process-global `PATH`, so no env-var lock is needed.
    #[cfg(unix)]
    #[test]
    fn path_contains_executable_requires_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("not-a-real-binary-253");
        std::fs::write(&candidate, b"#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&candidate).unwrap().permissions();
        perms.set_mode(0o644);
        std::fs::set_permissions(&candidate, perms.clone()).unwrap();

        let synthetic_path = tmp.path().as_os_str();
        assert!(
            !path_contains_executable(synthetic_path, "not-a-real-binary-253"),
            "a regular, non-executable file on $PATH must not resolve"
        );

        perms.set_mode(0o755);
        std::fs::set_permissions(&candidate, perms).unwrap();
        assert!(
            path_contains_executable(synthetic_path, "not-a-real-binary-253"),
            "the same file, once executable, must resolve"
        );
    }

    /// Issue prageethw/dot-agent-deck#253's `$PATH`-identity tightening: an empty or relative `$PATH`
    /// entry can never be trusted for an identity comparison (a shell
    /// resolves either against ITS OWN current directory, which this process
    /// cannot observe), while an absolute entry can. Pure data, no
    /// filesystem access needed. The "absolute" fixture is platform-gated:
    /// `Path::is_absolute()` requires a drive or UNC prefix on Windows, so a
    /// bare leading slash is merely rooted there, not absolute.
    #[test]
    fn is_untrustworthy_path_entry_rejects_empty_and_relative_but_accepts_absolute() {
        assert!(is_untrustworthy_path_entry(Path::new("")));
        assert!(is_untrustworthy_path_entry(Path::new(".")));
        assert!(is_untrustworthy_path_entry(Path::new("bin")));
        #[cfg(unix)]
        assert!(!is_untrustworthy_path_entry(Path::new("/usr/local/bin")));
        #[cfg(windows)]
        assert!(!is_untrustworthy_path_entry(Path::new(
            r"C:\Windows\System32"
        )));
    }

    /// Scenario: Build two directories on a synthetic `$PATH`, each holding an
    /// executable file with the SAME basename but different content — a
    /// "shadow" binary listed first and the "real" (`current_exe()`-standing-in)
    /// binary listed second, reproducing the `PATH=.:/usr/bin`-style shadowing
    /// issue prageethw/dot-agent-deck#253 flags. Drive both the pure `path_identity_match` helper and
    /// the full `resolve_binary_name` seam directly with this synthetic `$PATH`
    /// (never the real process-global `PATH`) and assert the shadowing
    /// candidate is rejected — `resolve_binary_name` must fall back to the
    /// quoted absolute path rather than emit a bare name that a consuming
    /// shell would resolve to the wrong (shadow) binary.
    #[spec("orchestration/delegate/019")]
    #[test]
    fn delegate_019_shadowed_path_match_is_rejected_and_falls_back_to_the_absolute_path() {
        let root = tempfile::tempdir().unwrap();
        let shadow_dir = root.path().join("shadow");
        let real_dir = root.path().join("real");
        std::fs::create_dir_all(&shadow_dir).unwrap();
        std::fs::create_dir_all(&real_dir).unwrap();

        let name = "delegate-019-shared-name";
        let shadow_candidate = shadow_dir.join(name);
        let real_candidate = real_dir.join(name);
        std::fs::write(&shadow_candidate, b"#!/bin/sh\necho shadow\n").unwrap();
        std::fs::write(&real_candidate, b"#!/bin/sh\necho real\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for candidate in [&shadow_candidate, &real_candidate] {
                let mut perms = std::fs::metadata(candidate).unwrap().permissions();
                perms.set_mode(0o755);
                std::fs::set_permissions(candidate, perms).unwrap();
            }
        }

        // Shadow first, exactly like `PATH=.:/usr/bin` puts the attacker- (or
        // stale-build-) controlled entry ahead of the real binary's own location.
        let shadow_first = std::env::join_paths([&shadow_dir, &real_dir]).unwrap();

        assert!(
            !path_identity_match(&shadow_first, name, &real_candidate),
            "the same-named file earlier on $PATH must not be treated as an identity match \
             for the running binary merely because the basename matches"
        );

        // Sanity check, roles reversed: with the real binary first on $PATH, identity
        // DOES match — proves the rejection above is genuinely about identity (shadowed
        // vs. not), not merely "the file could not be found at all".
        let real_first = std::env::join_paths([&real_dir, &shadow_dir]).unwrap();
        assert!(
            path_identity_match(&real_first, name, &real_candidate),
            "the running binary's own first-$PATH-match must be recognized as itself"
        );

        // End-to-end: `resolve_binary_name` must reject the shadow and fall back to the
        // absolute path, never the bare name a consuming shell would resolve to the
        // shadowing binary instead.
        let resolved =
            resolve_binary_name(Ok(real_candidate.clone()), |candidate_name, exe_path| {
                path_identity_match(&shadow_first, candidate_name, exe_path)
            });
        assert_eq!(
            resolved,
            shell_quote_if_needed(real_candidate.to_str().unwrap()),
            "a name shadowed earlier on $PATH must fall back to the quoted absolute path, \
             never the bare name a shell would resolve to the shadowing binary instead"
        );
    }

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

    /// PRD #163 review: the two third-party-tool config writers historically fell
    /// back to `/tmp` when `$HOME` was unset, and #163's bar is byte-for-byte Unix
    /// preservation — so the seam has to keep *two* fallbacks apart. With `$HOME`
    /// set both resolvers agree; with it unset `home_dir` yields `/` (the
    /// `config::dirs_home` behavior) and `home_dir_with_tmp_fallback` yields
    /// `/tmp` (the `hooks_manage`/`opencode_manage` behavior). Nothing asserted
    /// this before, which is how the regression got in.
    #[cfg(unix)]
    #[test]
    fn tool_config_home_keeps_the_historical_tmp_fallback() {
        let _guard = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var("HOME").ok();

        // SAFETY: env-var lock held; restored on the way out.
        unsafe {
            std::env::set_var("HOME", "/home/somebody");
        }
        assert_eq!(home_dir(), PathBuf::from("/home/somebody"));
        assert_eq!(
            home_dir_with_tmp_fallback(),
            PathBuf::from("/home/somebody"),
            "with $HOME set the two resolvers must be identical"
        );

        // SAFETY: same lock held.
        unsafe {
            std::env::remove_var("HOME");
        }
        assert_eq!(home_dir(), PathBuf::from("/"));
        assert_eq!(
            home_dir_with_tmp_fallback(),
            PathBuf::from("/tmp"),
            "the tool-config resolver must keep the pre-#163 /tmp fallback"
        );

        // SAFETY: same lock held; restoring the previous value.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// The safe punctuation set [`shell_quote_if_needed`] documents, re-declared
    /// here **independently of the production predicate** (issue #563). A test
    /// that reused `is_safe` would agree with whatever the helper currently
    /// does rather than pin what it is supposed to do; spelling the set out a
    /// second time is what makes a future narrowing *or* widening of it fail an
    /// assertion instead of silently changing every generated command line.
    const EXPECTED_SAFE_PUNCTUATION: &[char] = &['/', '.', '_', '-', '+', '=', ':', '@', '%', ','];

    /// Alphanumerics plus [`EXPECTED_SAFE_PUNCTUATION`] — the bytes that must
    /// survive [`shell_quote_if_needed`] unquoted.
    fn is_expected_safe(c: char) -> bool {
        c.is_ascii_alphanumeric() || EXPECTED_SAFE_PUNCTUATION.contains(&c)
    }

    /// Minimal POSIX word reader used to prove a quoted result is still ONE
    /// shell word whose literal value is the original path. Returns `None` when
    /// the input would split into more than one word, ends inside an
    /// unterminated quote, or leaves a byte unquoted that a real shell would
    /// treat as anything other than a literal.
    ///
    /// Deliberately narrow: it understands exactly the two constructs
    /// [`shell_quote_if_needed`] can emit — bare safe bytes, and single-quoted
    /// runs spliced together with `'\''` — so anything else is reported as
    /// unsafe rather than quietly accepted. Pure data: no shell is spawned.
    fn parse_as_one_shell_word(s: &str) -> Option<String> {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                // Single-quoted run: every byte up to the next `'` is literal,
                // and no escape is recognized inside it (POSIX 2.2.2).
                '\'' => loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => out.push(inner),
                        // Unterminated quote — the shell would keep reading.
                        None => return None,
                    }
                },
                // Outside quotes a backslash escapes exactly the next byte;
                // this is the `\'` in the middle of the `'\''` splice.
                '\\' => out.push(chars.next()?),
                c if is_expected_safe(c) => out.push(c),
                // A word separator, or an unquoted metacharacter the shell
                // would expand or act on rather than pass through literally.
                _ => return None,
            }
        }
        Some(out)
    }

    /// Issue #563: a path built only from safe bytes comes back BYTE-FOR-BYTE
    /// unchanged. The no-gratuitous-quotes half is worth pinning because all
    /// three call sites write the result into text a user reads and an agent
    /// runs — the two hook installers' generated config among it — so starting
    /// to quote paths that never needed it would rewrite every one of those
    /// lines. That should be a deliberate edit, not a side effect.
    #[test]
    fn shell_quote_if_needed_returns_a_safe_path_unchanged() {
        for input in [
            "/usr/local/bin/dot-agent-deck",
            "dot-agent-deck",
            "/home/somebody/.cargo/bin/dot-agent-deck",
            // Every safe punctuation byte at once, in a plausible path shape.
            "/opt/a+b=c:d@e%f,g/bin/x_1-2.3",
            // Backslash-free Windows-style path: the drive colon and the
            // forward slashes are all in the safe set, so this one does NOT
            // quote — see the backslash test below for the contrast.
            "C:/Users/somebody/bin/dot-agent-deck.exe",
        ] {
            assert_eq!(
                shell_quote_if_needed(input),
                input,
                "a path of only safe bytes must be returned unchanged, not quoted"
            );
        }
    }

    /// Issue #563: a path containing a space is single-quoted, and the quoted
    /// form still reads as a single shell word rather than two.
    #[test]
    fn shell_quote_if_needed_single_quotes_a_path_containing_a_space() {
        for input in [
            "/Applications/My App/bin/dot-agent-deck",
            " leading",
            "trailing ",
        ] {
            let quoted = shell_quote_if_needed(input);
            assert_eq!(
                quoted,
                format!("'{input}'"),
                "a space must force the single-quoted form"
            );
            assert_eq!(
                parse_as_one_shell_word(&quoted).as_deref(),
                Some(input),
                "the quoted form must still be one word with the original value"
            );
        }
    }

    /// Issue #563: an embedded single quote is escaped as `'\''` — close the
    /// run, emit an escaped quote, reopen — and the result still parses as ONE
    /// shell word whose value is the original path. This is the one input where
    /// a naive `format!("'{path}'")` would produce a *syntactically broken*
    /// command rather than merely an ugly one.
    #[test]
    fn shell_quote_if_needed_escapes_an_embedded_single_quote_into_one_word() {
        assert_eq!(
            shell_quote_if_needed("/home/o'brien/bin/dot-agent-deck"),
            r"'/home/o'\''brien/bin/dot-agent-deck'",
            "an embedded single quote must be spliced as '\\'', not passed through"
        );

        for input in [
            "/home/o'brien/bin/dot-agent-deck",
            "'",
            "''",
            "'leading",
            "trailing'",
            "a'b'c",
            // The nastiest realistic shape: a quote next to a space, so the
            // splice and the reason for quoting are different bytes.
            "/tmp/it's here/dot-agent-deck",
        ] {
            let quoted = shell_quote_if_needed(input);
            assert_eq!(
                parse_as_one_shell_word(&quoted).as_deref(),
                Some(input),
                "{quoted:?} must parse back to exactly one word equal to {input:?}"
            );
        }
    }

    /// Issue #563: the empty string is quoted (`''`) rather than returned bare.
    /// The `!path.is_empty()` guard in the helper already makes this true;
    /// nothing asserted it. It matters more than its size suggests — a bare
    /// empty string interpolated into command text expands to *nothing*, so the
    /// next word silently becomes the command, which is precisely the class of
    /// failure the shell-safety work exists to prevent.
    #[test]
    fn shell_quote_if_needed_quotes_the_empty_string_rather_than_returning_it_bare() {
        assert_eq!(
            shell_quote_if_needed(""),
            "''",
            "the empty string must survive as an explicit empty word"
        );
    }

    /// Issue #563: every byte of the documented safe set — alphanumerics plus
    /// `/ . _ - + = : @ % ,` — stays unquoted, both on its own and inside a
    /// path. Narrowing the set (dropping, say, `%` or `,`) would otherwise add
    /// quotes to a large share of generated paths with no test noticing.
    #[test]
    fn shell_quote_if_needed_leaves_every_byte_of_the_safe_set_unquoted() {
        let alphanumerics = ['a', 'z', 'A', 'Z', '0', '9'];
        for c in EXPECTED_SAFE_PUNCTUATION.iter().chain(alphanumerics.iter()) {
            let alone = c.to_string();
            assert_eq!(
                shell_quote_if_needed(&alone),
                alone,
                "{c:?} is in the safe set, so it must not be quoted on its own"
            );

            let in_path = format!("/bin/dot{c}agent{c}deck");
            assert_eq!(
                shell_quote_if_needed(&in_path),
                in_path,
                "{c:?} is in the safe set, so a path containing it must not be quoted"
            );
        }
    }

    /// Issue #563, the other direction: sweep the whole ASCII range and assert
    /// the safe/unsafe split matches the documented set exactly. Widening the
    /// set is the more dangerous edit of the two — letting `$`, `` ` ``, `;` or
    /// a space through unquoted turns an interpolated path into shell syntax —
    /// and a per-byte allowlist test alone would not catch it.
    #[test]
    fn shell_quote_if_needed_quotes_every_ascii_byte_outside_the_safe_set() {
        for byte in 0u8..=0x7f {
            let c = byte as char;
            let input = c.to_string();
            let quoted = shell_quote_if_needed(&input);

            if is_expected_safe(c) {
                assert_eq!(
                    quoted, input,
                    "byte {byte:#04x} ({c:?}) is in the documented safe set and must stay unquoted"
                );
                continue;
            }

            assert_ne!(
                quoted, input,
                "byte {byte:#04x} ({c:?}) is outside the documented safe set and must be quoted"
            );
            assert!(
                quoted.starts_with('\'') && quoted.ends_with('\''),
                "byte {byte:#04x} ({c:?}) must produce the single-quoted form, got {quoted:?}"
            );
            assert_eq!(
                parse_as_one_shell_word(&quoted).as_deref(),
                Some(input.as_str()),
                "byte {byte:#04x} ({c:?}) must survive quoting as one word with its original value"
            );
        }
    }

    /// Issue #563: the safe-set predicate works on BYTES while a path is UTF-8,
    /// so every byte of a multi-byte character is >= 0x80 and therefore outside
    /// the set — a non-ASCII path always quotes. Worth its own case because the
    /// ASCII sweep above cannot reach these bytes, and because quoting must
    /// splice around whole characters rather than cutting one in half.
    #[test]
    fn shell_quote_if_needed_quotes_a_non_ascii_path_without_mangling_it() {
        for input in [
            "/home/josé/bin/dot-agent-deck",
            "/srv/项目/bin/dot-agent-deck",
            "/tmp/naïve café/dot-agent-deck",
        ] {
            let quoted = shell_quote_if_needed(input);
            assert_eq!(
                quoted,
                format!("'{input}'"),
                "a non-ASCII path is outside the byte-wise safe set, so it must be quoted"
            );
            assert_eq!(
                parse_as_one_shell_word(&quoted).as_deref(),
                Some(input),
                "quoting must leave every multi-byte character intact"
            );
        }
    }

    /// Issue #563 records the CURRENT treatment of backslashes, not a desired
    /// one: `\` is absent from the safe set, so every Windows-style path takes
    /// the single-quoted branch. That is the property issue #561 is about —
    /// POSIX single-quoting is meaningless to `cmd.exe` and PowerShell — and
    /// #561 is deliberately NOT resolved here. Pinning the observed behavior is
    /// the point: whichever way #561 lands, the change has to show up as a
    /// deliberate edit to this test rather than passing silently.
    #[test]
    fn shell_quote_if_needed_currently_quotes_backslashes_observed_behavior_for_issue_561() {
        assert_eq!(
            shell_quote_if_needed(r"\"),
            r"'\'",
            "backslash is not in the safe set today, so it is quoted"
        );
        assert_eq!(
            shell_quote_if_needed(r"C:\Users\somebody\bin\dot-agent-deck.exe"),
            r"'C:\Users\somebody\bin\dot-agent-deck.exe'",
            "a Windows path is POSIX-single-quoted today (issue #561)"
        );
        assert_eq!(
            shell_quote_if_needed(r"\\server\share\dot-agent-deck.exe"),
            r"'\\server\share\dot-agent-deck.exe'",
            "a UNC path is POSIX-single-quoted today (issue #561)"
        );

        // The quoting is POSIX-correct even though POSIX is arguably the wrong
        // dialect here: a single-quoted run takes no escapes, so each backslash
        // survives literally rather than being consumed as one.
        assert_eq!(
            parse_as_one_shell_word(&shell_quote_if_needed(
                r"C:\Users\somebody\bin\dot-agent-deck.exe"
            ))
            .as_deref(),
            Some(r"C:\Users\somebody\bin\dot-agent-deck.exe"),
            "under POSIX rules the quoted Windows path is one word with its backslashes intact"
        );
    }
}
