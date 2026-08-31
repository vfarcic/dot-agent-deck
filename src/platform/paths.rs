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
/// **absolute path**, spelled and quoted for a POSIX shell by
/// [`posix_command_word`] so a path containing whitespace still parses as one
/// argument — a path is independent of whatever `$PATH` *or cwd* the agent's
/// shell ends up with, and it names this exact running binary rather than
/// whatever `$PATH` might resolve that name to, so it resolves correctly
/// regardless of which proxy this process's own `$PATH` turned out to be.
///
/// **That last claim is only true because the path is absolutised here, and
/// it was not before (issue #560).** `current_exe()` is not documented to
/// return an absolute path and on macOS does not: it is backed by
/// `_NSGetExecutablePath`, which reports the path the process was INVOKED as
/// (the same platform fact the symlink paragraph above records), so a deck
/// launched as `./target/release/dot-agent-deck` used to emit exactly that
/// relative word into the worker task footer. The worker then resolved it
/// against ITS OWN cwd — an orchestration directory or a git worktree, never
/// the deck's launch directory — and the command failed, silently, for the
/// reason the last paragraph below gives. Linux never exhibited it, because
/// `/proc/self/exe` is kernel-resolved and therefore always absolute; the
/// defect was invisible on the platform the project develops on.
/// [`std::path::absolute`] is what closes it: purely lexical plus the cwd, no
/// filesystem access, and — unlike [`std::fs::canonicalize`] — it does not
/// resolve symlinks, so it makes "absolute" true by construction without
/// silently taking a position on the platform-dependent symlink behaviour
/// documented above.
///
/// **The emitted word targets a POSIX shell on every platform, including
/// Windows (issue #561).** That is not a default — it is what the text this
/// word is interpolated into already says: both consumers fence it in
/// ```` ```bash ```` and `state::work_done_footer` instructs the worker in
/// prose to run it "via Bash". `cmd.exe` and PowerShell are deliberately NOT
/// targeted, and neither could be by quoting alone: PowerShell needs the `&`
/// call operator before a quoted string for it to be a command at all, and
/// this repo implements no PowerShell quoting anywhere to borrow from.
/// (`hooks_manage`'s `#[cfg(windows)]` `shell_quote_if_needed` is a `cmd.exe`
/// quoter, but it is for a different consumer — a hook command line Claude
/// Code hands to the *native* shell — and its own doc records that `cmd.exe`
/// expands `%VAR%` even inside double quotes, which quoting cannot undo.)
///
/// Targeting POSIX is not enough on its own, though, because a POSIX shell
/// will not treat a backslash-separated Windows path as a **path** however
/// well it is quoted: POSIX (XCU 2.9.1.1) makes a command word containing at
/// least one `/` a pathname and every other command word a `$PATH` lookup, so
/// `'C:\Users\me\dot-agent-deck.exe'` is looked up in `$PATH` and reported as
/// `command not found` — measured against real bash, not assumed. So the
/// fallback respells a Windows path with `/` separators before quoting it,
/// which is lossless (`/` is not a legal character in a Windows file name),
/// is the spelling `shell_quote_if_needed`'s own safe set already treats as
/// needing no quotes at all, and is what git-bash / WSL / MSYS want.
///
/// [`DEFAULT_BINARY_NAME`] remains the fallback only when `current_exe()`
/// itself is unusable: an error, an empty file name, (Unix) a file name that
/// is not valid UTF-8, a path that cannot be made absolute, or a Windows path
/// with no POSIX spelling (see [`posix_command_word`]). The fallback matters
/// more here than at most other `current_exe()` call sites: `delegate` and
/// `work-done` write to the unversioned hook socket, both call sites are
/// fire-and-forget, and the daemon drops any frame it cannot parse without
/// logging it — so a name that resolves to a binary that cannot run produces
/// no error anywhere, only a signal that silently never arrives.
pub fn binary_name() -> String {
    resolve_binary_name(effective_current_exe(), resolves_on_path)
}

/// The absolute path this build should write into **another program's
/// persistent configuration** — a hook command in `~/.claude/settings.json` or
/// `~/.codex/hooks.json`, Devin's `config.json`, or the OpenCode plugin's
/// `BINARY_PATH` (PRD #381).
///
/// This is deliberately **not** [`binary_name`] and deliberately not built on
/// it. `binary_name()` answers "what text tells an agent to run the deck", and
/// its best answer is often a BARE name the agent's own shell resolves through
/// `$PATH`. That answer is never acceptable here: these commands run under
/// `/bin/sh` with an environment the deck does not control, and the field
/// failure this function exists for was exactly a `sh` `$PATH` miss
/// (`/bin/sh: 1: …/target/release/dot-agent-deck: not found`). Everything this
/// returns is an absolute path to a file that existed at resolution time —
/// never a bare name, never relative.
///
/// Resolution order:
///
/// 1. `current_exe()`, when it is a usable absolute path to an executable file
///    that is **not** a cargo build artifact ([`is_build_artifact_path`]). An
///    installed binary performing its own install is the normal, correct case
///    and must keep working.
/// 2. It IS a build artifact — gitignored, deleted by `cargo clean`, and gone
///    the moment its worktree is pruned, so it must never be persisted:
///    - **2a.** `<home>/.local/bin/dot-agent-deck`, when that exists and is
///      executable — the same choice `remote.rs`'s remote install already
///      makes ("Use the absolute path consistently");
///    - **2b.** otherwise the first `dot-agent-deck` reachable through an
///      absolute, non-artifact `$PATH` entry, as its own absolute path;
///    - **2c.** otherwise **refuse**: return `Err`, and the caller writes
///      nothing at all.
///
///    A 2a or 2b candidate must additionally be owner-writable only
///    ([`write_mode_is_owner_only`]); one the group or the world can rewrite
///    is skipped and the walk continues. That check is scoped to 2a and 2b,
///    and deliberately does not extend to owners, ancestor directories or
///    symlink targets — see issue #732.
/// 3. `current_exe()` failing outright is also a refusal, **never** a fallback
///    to [`DEFAULT_BINARY_NAME`] (issue #536). A bare `dot-agent-deck` in a
///    file Claude Code hands to `/bin/sh` re-opens the same `$PATH` miss in
///    the one place the deck can least afford it, and unlike [`binary_name`]'s
///    consumers there is no shell here whose `$PATH` might still save it.
///
/// **The 2a candidate is deliberately NOT canonicalized.** On Linux
/// `current_exe()` reads `/proc/self/exe`, which the kernel resolves fully, so
/// a user who runs `~/.local/bin/dot-agent-deck` where that is a symlink into
/// a cargo target directory arrives at step 1 holding a `target/` path, falls
/// through to 2a, and gets the durable **symlink** path written. That is the
/// desired outcome — canonicalizing 2a would resolve it straight back to the
/// artifact and defeat the fix. It is also what lets the e2e harness point a
/// sandbox `~/.local/bin` at the binary under test.
///
/// **"Exists and is executable" is the whole gate — a candidate is never
/// executed** (PRD #381 Open Question 5, decided deliberately). This does a
/// `stat` and, on Unix, checks the executable bit; it spawns nothing. Running
/// a candidate to prove it works would put a subprocess on the silent
/// dashboard-startup path and add a new failure mode there, to catch a
/// version-skew problem the PRD puts out of scope: an installed deck of a
/// different version is still a *durable* path, which is all this function
/// claims to find.
pub fn durable_binary_path() -> Result<String, String> {
    durable_binary_path_with(
        effective_current_exe(),
        &home_dir(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`durable_binary_path`] with its three environmental inputs injected: the
/// running executable, the home anchor step 2a hangs off, and the `$PATH`
/// value step 2b walks.
///
/// Public because it is the seam PRD #381 M3 exists to open. `hooks_manage`'s
/// auto-install seam used to hardcode `let binary_path =
/// "dot-agent-deck".to_string();`, so **no test ever executed the derivation
/// that produced the field defect** — the PRD calls closing that its
/// highest-value milestone. A test has to be able to drive this with a
/// `…/target/release/dot-agent-deck` `current_exe()` of its own choosing, and
/// a real unusable `current_exe()` cannot be manufactured on demand.
///
/// Only the **inputs** are synthetic. The existence and executable-bit checks
/// are the real ones against the real filesystem, and the `$PATH` walk is the
/// real one — same precedent as [`first_path_match`], which is likewise pure
/// over its `path` argument — so a test that passes a `tempfile` home and a
/// `tempfile`-backed `$PATH` exercises production logic rather than a parallel
/// copy of it.
pub fn durable_binary_path_with(
    current_exe: std::io::Result<PathBuf>,
    home: &Path,
    path_value: Option<&std::ffi::OsStr>,
) -> Result<String, String> {
    let name = durable_binary_file_name();
    let installed = home.join(".local").join("bin").join(&name);

    let exe = match current_exe {
        Ok(exe) => exe,
        // Issue #536: this is a refusal, NOT a fall back to the bare crate
        // name. See the doc comment above.
        Err(e) => {
            return Err(format!(
                "refusing to write a dot-agent-deck hook command: the running executable's own \
                 path is unavailable ({e}), so there is no absolute path to write and a bare \
                 command name would be resolved by whatever `$PATH` the agent's `/bin/sh` \
                 happens to have. {}",
                repair_advice(&installed)
            ));
        }
    };
    // `current_exe()` is only guaranteed absolute on Linux (`/proc/self/exe`);
    // on macOS it reports the invocation path (issue #560), so absolutise
    // before deciding anything about it. Purely lexical plus the cwd, and
    // deliberately not `canonicalize` — resolving symlinks here would turn a
    // durable `~/.local/bin` launch into the artifact it points at.
    let absolute = std::path::absolute(&exe).unwrap_or(exe);

    if !is_build_artifact_path(&absolute)
        && is_executable_file(&absolute)
        && let Some(path) = durable_path_string(&absolute)
    {
        return Ok(path);
    }

    // `write_mode_is_owner_only` is step 2a's and 2b's, never step 1's — see
    // that function, and issue #732 for the checks deliberately left out of it.
    if !is_build_artifact_path(&installed)
        && is_executable_file(&installed)
        && write_mode_is_owner_only(&installed)
        && let Some(path) = durable_path_string(&installed)
    {
        return Ok(path);
    }

    if let Some(path_value) = path_value
        && let Some(found) = first_durable_path_match(path_value, &name)
        && let Some(path) = durable_path_string(&found)
    {
        return Ok(path);
    }

    Err(format!(
        "refusing to write `{}` into agent hook config: {}. No durable dot-agent-deck was found \
         at `{}` or on `$PATH`, and a hook command pointing at a path that will not exist is \
         worse than no hook at all. {}",
        absolute.display(),
        rejection_reason(&absolute),
        installed.display(),
        repair_advice(&installed)
    ))
}

/// Why `exe` was not usable as the written path, for the refusal message.
fn rejection_reason(exe: &Path) -> String {
    if is_build_artifact_path(exe) {
        "it is a cargo build artifact — gitignored, removed by `cargo clean`, and gone the \
         moment its worktree is pruned"
            .to_string()
    } else {
        "it is not a usable absolute path to an executable file".to_string()
    }
}

/// The actionable half of every refusal message: what the operator can do.
fn repair_advice(installed: &Path) -> String {
    format!(
        "Install the deck to a durable location — `cargo install --path .`, or copy the binary to \
         `{}` — or put `dot-agent-deck` on your `PATH`, then run `dot-agent-deck hooks install`.",
        installed.display()
    )
}

/// The file name step 2a and 2b look for: the crate's own package name plus the
/// platform's executable suffix (`.exe` on Windows, empty elsewhere).
///
/// Deliberately [`DEFAULT_BINARY_NAME`] rather than `current_exe()`'s own file
/// name. The two differ only for a deck renamed on disk, and there the
/// question this function answers is "where is the *installed* deck", whose
/// answer is the name `remote.rs` and every install path already use. A
/// renamed build looking for a renamed install would find nothing on the one
/// machine layout the project actually ships.
fn durable_binary_file_name() -> String {
    format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX)
}

/// `path` as an owned string, but only when it satisfies the contract
/// [`durable_binary_path`] promises: absolute, and spellable as UTF-8 (every
/// consumer interpolates it into a text config file). Anything else yields
/// `None`, so the invariant is true by construction rather than by assumption
/// about what shapes `current_exe()` and `$HOME` can take.
fn durable_path_string(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    path.to_str().map(str::to_string)
}

/// Whether `path` runs through a cargo build-output directory: a path
/// **component** `debug` or `release` whose immediate parent component is
/// `target`.
///
/// Component-wise, not a substring search, and that is load-bearing rather
/// than fastidious. `path.contains("target/debug")` would also catch a user
/// whose home directory is literally named `target`, a deck kept under
/// `/opt/target/release-notes/`, or (on Windows) miss the same shapes because
/// the separator is `\`. Matching components makes the test mean what it says
/// on both platforms.
///
/// Known and accepted limitation: it recognises the **default** layout only. A
/// `CARGO_TARGET_DIR=/tmp/build` puts artifacts at `/tmp/build/debug/…`, whose
/// `debug` has no `target` parent, so such a build is treated as durable. PRD
/// #381 defines the check as `target/debug` / `target/release`, which is the
/// layout every path in the field report had; widening it to "any `debug` or
/// `release` component" would reject legitimate install prefixes.
pub(crate) fn is_build_artifact_path(path: &Path) -> bool {
    use std::ffi::OsStr;
    use std::path::Component;

    let mut parent: Option<&OsStr> = None;
    for component in path.components() {
        let Component::Normal(name) = component else {
            parent = None;
            continue;
        };
        if parent == Some(OsStr::new("target"))
            && matches!(name.to_str(), Some("debug" | "release"))
        {
            return true;
        }
        parent = Some(name);
    }
    false
}

/// The first `name` reachable through an absolute, non-artifact entry of a
/// `$PATH`-shaped value, as an absolute path — step 2b of
/// [`durable_binary_path`]. Pure over its `path` argument (no environment
/// read), matching [`first_path_match`]'s precedent.
///
/// Two deliberate differences from [`first_path_match`], both because this
/// answers a different question. That function reproduces a **shell's** lookup
/// — first match wins, and a match found through an empty or relative entry
/// STOPS the walk without being claimed, because a shell would have selected
/// it. Here nothing is being predicted about a shell: the goal is simply to
/// find a durable absolute location, so an untrustworthy entry (which no
/// absolute path can be built from — [`is_untrustworthy_path_entry`]) and a
/// build-artifact candidate (which is exactly what this whole resolver
/// refuses, and `$PATH` entries pointing into `target/debug` are routine on a
/// developer's machine) are both **skipped** and the walk continues. A
/// candidate the group or the world can rewrite ([`write_mode_is_owner_only`],
/// and issue #732 for its deliberate limits) is skipped the same way.
fn first_durable_path_match(path: &std::ffi::OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !is_untrustworthy_path_entry(dir))
        .map(|dir| dir.join(name))
        .find(|candidate| {
            !is_build_artifact_path(candidate)
                && is_executable_file(candidate)
                && write_mode_is_owner_only(candidate)
        })
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
    // Issue #560: absolutise BEFORE quoting. `current_exe()` is only
    // guaranteed absolute on Linux (`/proc/self/exe`); on macOS it reports the
    // invocation path, so this is where a relative `./target/release/…` would
    // otherwise reach the generated command word. Purely lexical plus the cwd,
    // and deliberately not `canonicalize` — see [`binary_name`]'s doc.
    let Ok(absolute) = std::path::absolute(&path) else {
        return DEFAULT_BINARY_NAME.to_string();
    };
    match absolute.to_str() {
        Some(path_str) => posix_command_word(path_str, cfg!(windows))
            .unwrap_or_else(|| DEFAULT_BINARY_NAME.to_string()),
        None => DEFAULT_BINARY_NAME.to_string(),
    }
}

/// Spell `path` — already absolute — as a command word a **POSIX shell** will
/// execute, or `None` when it has no such spelling. Issue #561.
///
/// `windows_host` says whether `path` is in the Windows dialect. It is a
/// parameter rather than a `#[cfg]` so both branches are unit-testable from
/// any host: neither defect in this function's history could be reproduced on
/// the platform this project is developed on, and a `#[cfg(windows)]` branch
/// would have been type-checked by CI but exercised by nothing. Production
/// passes `cfg!(windows)`, which is a compile-time constant, so the branch
/// costs nothing at runtime.
///
/// POSIX is the target on every platform because that is what the text this
/// word lands in already promises: `state::work_done_footer` and
/// `orchestrator_context::build_orchestrator_context` both fence it in
/// ```` ```bash ```` and the former tells the worker in prose to run it "via
/// Bash". See [`binary_name`]'s doc for why `cmd.exe` and PowerShell are not
/// targeted and cannot be reached by quoting anyway.
///
/// On a Windows path two things happen before [`shell_quote_if_needed`]:
///
/// - **A verbatim or device path is refused** (`\\?\…`, `\\.\…`). Those
///   prefixes are defined to disable all path normalization, so `/` is *not*
///   accepted as a separator inside them and respelling one changes which
///   file it names. There is no POSIX-shell spelling of such a path, and
///   [`resolve_binary_name`] therefore falls back to [`DEFAULT_BINARY_NAME`]
///   rather than emit a word that would be misparsed — a bare name the
///   agent's `$PATH` may well resolve beats a path that is silently wrong.
/// - **`\` becomes `/`.** Lossless, because `/` is not a legal character in a
///   Windows file name, and necessary rather than cosmetic: a POSIX shell
///   picks pathname-vs-`$PATH`-lookup on whether the word contains at least
///   one `/` (POSIX XCU 2.9.1.1), so a backslash path is looked up in `$PATH`
///   and reported `command not found` no matter how correctly it is quoted.
///   `C:\Users\me\deck.exe`
///   becomes `C:/Users/me/deck.exe`, which needs no quoting at all under
///   `shell_quote_if_needed`'s existing safe set, and `\\server\share\…`
///   becomes `//server/share/…`, which is the UNC spelling MSYS/Cygwin use.
///
/// The final `contains('/')` guard makes "this is a pathname, not a `$PATH`
/// lookup" true by construction rather than by assumption about what shapes
/// `current_exe()` can return.
fn posix_command_word(path: &str, windows_host: bool) -> Option<String> {
    if !windows_host {
        return Some(shell_quote_if_needed(path));
    }
    if path.starts_with(r"\\?\") || path.starts_with(r"\\.\") {
        return None;
    }
    let respelled = path.replace('\\', "/");
    if !respelled.contains('/') {
        return None;
    }
    Some(shell_quote_if_needed(&respelled))
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

/// Whether `candidate` is a regular file that **this user can actually
/// execute**. Same shape and purpose as `orchestrator_ext::is_executable_file`
/// — `is_file()` alone would accept a same-named regular-but-non-executable
/// file earlier on `$PATH` — but the Unix half asks `access(2)` rather than
/// reading the mode.
///
/// `mode & 0o111 != 0` was the obvious spelling and the wrong question: a file
/// owned by another user with mode `0100` has an exec bit set and is still not
/// executable by us. The resolver would then STOP at that candidate instead of
/// continuing to a usable later one, and persist a hook command that fails with
/// permission denied on every single hook, indefinitely (PRD #381 audit,
/// LOW-2). `access(X_OK)` answers the owner/group/other question the kernel
/// will answer at exec time, and consults ACLs where the filesystem has them.
///
/// `access(2)` tests the REAL uid/gid rather than the effective one. That is
/// the same answer here: the deck is never installed setuid or setgid, so the
/// two are equal in every process this runs in, and `access` is POSIX on every
/// Unix the crate builds for while the effective-uid variants (`eaccess`,
/// `faccessat(…, AT_EACCESS)`) are not uniformly spelled. The
/// [`std::path::Path::is_file`] check stays in front of it because `access`
/// alone would accept a *directory* — every traversable directory answers
/// `X_OK`.
///
/// On non-Unix targets there is no cheap equivalent, so a regular file is
/// accepted.
fn is_executable_file(candidate: &std::path::Path) -> bool {
    if !candidate.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt as _;

        // An interior NUL cannot name a real file, so it cannot be executable.
        let Ok(c_path) = CString::new(candidate.as_os_str().as_bytes()) else {
            return false;
        };
        // SAFETY: `c_path` is a valid NUL-terminated C string that outlives the
        // call, and `access(2)` only reads through the pointer.
        unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Whether write access to `candidate` is held by its owner alone — neither
/// group- nor other-writable. A candidate failing this is skipped by
/// resolution steps 2a and 2b, which move on to the next candidate or refuse
/// (PRD #381 audit, HIGH — **partially** accepted).
///
/// **Scope, deliberately narrow; the rest is [issue #732].** Only the
/// candidate's own mode is consulted. Owner checks, ancestor-directory
/// writability, and canonical-symlink-target validation are NOT done here and
/// must not be added without the design decision #732 exists to make: the
/// failure mode is a **hard refusal to install**, so a false positive breaks a
/// legitimate user outright, and `/usr/local/bin` is group-writable by `admin`
/// on stock macOS.
///
/// Three consequences of that scope worth stating rather than discovering:
///
/// - **A symlink is exempt, not judged.** `symlink_metadata` is used so the
///   mode read is the candidate's OWN, and a symlink's own mode is `0777` on
///   every Unix this ships for — it says nothing about anything. Reading
///   *through* it to the target's mode would be canonical-target validation,
///   which is #732's, and it would also defeat step 2a's whole point: the
///   stable `~/.local/bin` name the user controls is the durable thing, not
///   whatever it currently points at.
/// - **Step 1 (`current_exe()`) is not checked.** Refusing to install from the
///   binary the user is *already running* converts a loose mode on a
///   legitimate install prefix into a refusal, which is the failure #732 has
///   to weigh first.
/// - **A stat failure counts as untrusted.** Callers reach this only after
///   [`is_executable_file`] has already stat'd the candidate successfully, so
///   a failure here means it changed underneath us; skipping to the next
///   candidate is free and correct.
fn write_mode_is_owner_only(candidate: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::symlink_metadata(candidate) {
            Ok(meta) if meta.file_type().is_symlink() => true,
            Ok(meta) => meta.permissions().mode() & 0o022 == 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = candidate;
        true
    }
}

/// Whether a deck-owned pin **already sitting in an agent's configuration** —
/// the executable path a previously written hook command names — must be
/// replaced by a freshly resolved durable path, rather than carried forward.
///
/// This is the read side of [`durable_binary_path`], and it exists because
/// issue #536 was not closed by fixing the write side alone (PRD #381 audit,
/// MEDIUM-1). Both self-heal checks used to ask nothing but
/// [`std::path::Path::try_exists`]. For a legacy pin of the BARE
/// `dot-agent-deck` — exactly what the old code wrote — `try_exists` resolves
/// **relative to the process cwd**, so launching the deck from any directory
/// that happens to contain a file of that name made the bare pin look alive,
/// and it was preserved. At hook-fire time `/bin/sh` and Node's `execFileSync`
/// resolve that same persisted bare name through the **agent's** `$PATH`, not
/// against the cwd-relative file that suppressed the repair — which is #536's
/// arbitrary-execution vector, surviving the change that claims to close it.
/// A relative path, a directory, a non-executable file and a live
/// `target/{debug,release}` path all slipped through the same gate, for the
/// same reason: it only ever asked "is this not `Ok(false)`".
///
/// So a pin is preserved only when it satisfies the invariant a freshly
/// resolved path satisfies: absolute, present, a regular file, executable by
/// this user, and not a build artifact.
///
/// **The one benefit of the doubt is kept, and narrowed.** A stat that returns
/// `Err` — permission denied, an unmounted or stale mount — on an otherwise
/// **well-formed absolute** pin still means "leave alone", because deleting a
/// working user's hook is worse than leaving a stale one, and that is the
/// fail-safe direction PRD #381 Open Question 3 settles on. What changed is
/// that a MALFORMED pin — bare, or relative — is no longer eligible for it at
/// all: there is no reading under which such a value is a path the deck should
/// keep.
///
/// Note this is deliberately NOT [`write_mode_is_owner_only`]'s question. That
/// check picks between candidates the resolver is free to reject; this one
/// decides whether to overwrite a value the user may have put there on
/// purpose, so it stays at "is this a usable absolute executable".
pub(crate) fn pin_is_repairable(pin: &str) -> bool {
    let path = Path::new(pin);
    // Bare or relative: #536's own shape, and cwd-dependent by construction.
    if !path.is_absolute() {
        return true;
    }
    match path.try_exists() {
        // Positively reported missing by the OS.
        Ok(false) => true,
        // Could not determine. Well-formed, so leave it alone.
        Err(_) => false,
        Ok(true) => !is_executable_file(path) || is_build_artifact_path(path),
    }
}

/// Single-quote `path` for a POSIX shell only when it contains a character
/// outside a conservative safe set; otherwise return it unchanged. Canonical
/// copy of the identical helper duplicated in `codex_hooks_manage.rs` and
/// `devin_hooks_manage.rs`, which both reach it through
/// [`native_shell_command_word`] rather than defining their own (issue
/// prageethw/dot-agent-deck#253: [`binary_name`]'s absolute-path fallback needed the
/// same quoting, so the third call site is what pushed the helper here rather
/// than re-duplicating it a third time).
///
/// This is the POSIX arm only. It is NOT the right quoter for a command line
/// `cmd.exe` will run — see [`cmd_quote_if_needed`] and the selector above it
/// (issue #734) — and it stays the unconditional one for
/// [`posix_command_word`], whose consumer is a bash-fenced example on every
/// platform.
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

/// Double-quote `path` for `cmd.exe` only when it contains a character outside
/// a conservative safe set; otherwise return it unchanged. The Windows-dialect
/// sibling of [`shell_quote_if_needed`], selected by
/// [`native_shell_command_word`] (issue #734).
///
/// The safe set and the quoting are deliberately a copy of `hooks_manage`'s own
/// `#[cfg(windows)]` arm rather than a second scheme: the deck has three hook
/// writers landing command strings in three third-party config files, and both
/// read-back paths that exist — `hooks_manage::unquote_if_needed` and
/// `tests/durable_hook_binary_path.rs`'s `unquoted_command` — try BOTH quoting
/// forms on every platform, so a config written by one writer must be readable
/// as the other's. A third dialect would be a third thing to unquote.
///
/// `\` **is** in the safe set, which is the substance of the fix: a Windows
/// path is `\`-separated, [`shell_quote_if_needed`] excludes `\`, and the
/// result was that every Codex hook command written on Windows came out
/// single-quoted — a form `cmd.exe` does not implement at all, since it treats
/// `'` as an ordinary character and would look for a file literally named
/// `'C:\…\dot-agent-deck.exe'`.
///
/// `~` is likewise safe here and is not in the POSIX set, where it triggers
/// home-directory expansion: a real Windows path such as
/// `C:\Users\RUNNER~1\AppData\…\dot-agent-deck.exe` (an 8.3 short name, which
/// is what CI runners hand out) needs no quoting at all.
///
/// `%` and `!` are excluded from the safe set, but excluding them does NOT
/// resolve them — the same over-claim `hooks_manage`'s copy warns about.
/// `cmd.exe` expands `%VAR%` even *inside* double quotes, and `!VAR!` under
/// delayed expansion; wrapping the path here changes neither. What the quoting
/// buys is limited to spaces and the other characters outside the safe set.
///
/// The `"` escape is carried over from that copy for the same
/// both-forms-readable reason, and is unreachable in practice: `"` is not a
/// legal character in a Win32 file name, so no real `path` contains one.
fn cmd_quote_if_needed(path: &str) -> String {
    fn is_safe(b: u8) -> bool {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'\\' | b'/' | b'.' | b'_' | b'-' | b'+' | b'=' | b':' | b'@' | b',' | b'~'
            )
    }
    if !path.is_empty() && path.bytes().all(is_safe) {
        path.to_string()
    } else {
        format!("\"{}\"", path.replace('"', "\\\""))
    }
}

/// Spell `path` as the leading command word of a hook command line, quoted for
/// the shell that will actually EXECUTE it: `cmd.exe` on a Windows host, a
/// POSIX shell everywhere else. Issue #734.
///
/// **The question this answers is which interpreter runs the string, not which
/// OS compiled the deck** — the two only coincide because the hook command is
/// written into config the agent reads on this same machine, from
/// [`durable_binary_path`], which is this process's own executable and so is in
/// the host's own dialect.
///
/// The interpreter is a *measured* fact about each consuming agent, not an
/// assumption:
///
/// - **Codex** (`codex-rs/hooks/src/engine/command_runner.rs`, read at 0.149.0)
///   hands the whole command string to a shell. Its `default_shell_command` is
///   `%COMSPEC%` else `cmd.exe` with `/C` on Windows, and `$SHELL` else
///   `/bin/sh` with `-lc` otherwise; the deck writes no `shell` override into
///   its entries, so that default is what runs them. The Windows arm passes the
///   line through `raw_arg` wrapped in one extra pair of double quotes, i.e.
///   `cmd.exe /C ""<our line>""`. That is the standard idiom and it composes
///   with the quoting here: with four quote characters on the line `cmd.exe`'s
///   preserve-quotes rule cannot apply, so it strips the leading quote and the
///   final quote (`cmd /?`, "old behavior"), handing the parser back exactly
///   the line written here.
/// - **Claude Code** invokes via the platform's native shell too, which is what
///   `hooks_manage`'s `#[cfg(windows)]` quoter has always been for.
/// - **Devin** never sees this selector: `devin_hooks_manage::install_to` asks
///   `agent_hook_config::build_command` for `HookShell::Posix` outright, so
///   `windows_host` is not consulted for that writer and its output is
///   byte-identical to what it has always been on every host. That is a
///   *deliberate* pin rather than a consequence of
///   `devin_hooks_manage::devin_config_dir` returning `None` off Unix — the
///   gate is real, but `install_to` is reachable without passing through it,
///   so relying on it left the Windows arm live for a writer that should never
///   take it, and `build-windows` said so.
///
/// So the reachable user-visible change is Codex's, and narrowly: `codex_home`
/// honours `$CODEX_HOME` on every platform, so a Windows user who sets it got a
/// single-quoted path written into `hooks.json` and every deck hook silently
/// failed to run. Re-installing repairs it without migration code —
/// `codex_hooks_manage::install_impl` strips deck-owned entries by the command
/// SUFFIX, which no quoting scheme touches, before re-adding the fresh one.
///
/// `windows_host` is a parameter rather than a `#[cfg]` for the reason
/// [`posix_command_word`] gives, and this defect is the evidence for it: the
/// broken spelling existed only on a platform no test could observe from the
/// box this project is developed on, was type-checked by `build-windows`, and
/// so shipped.
///
/// This is deliberately NOT [`posix_command_word`], whose always-POSIX
/// behaviour is correct for its own consumer and stays untouched: the word it
/// builds is interpolated into text fenced as ```` ```bash ```` and handed to
/// an agent to run "via Bash", so the shell that executes *that* string is a
/// POSIX one whatever the host is.
pub(crate) fn native_shell_command_word(path: &str, windows_host: bool) -> String {
    if windows_host {
        cmd_quote_if_needed(path)
    } else {
        shell_quote_if_needed(path)
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
    ///
    /// **Split by host dialect since #560.** The injected path has to be
    /// absolute IN THE HOST'S DIALECT, because [`std::path::absolute`] is the
    /// host's: a driveless `/usr/local/bin/x` is rooted but NOT absolute on
    /// Windows, where it acquires the current drive and comes back as
    /// `D:/usr/local/bin/x`. Before #560 nothing absolutised, so one set of
    /// POSIX-shaped literals happened to pass on every platform; that is no
    /// longer true and pretending otherwise is what `build-windows` caught.
    /// Each arm keeps hand-written expected strings rather than composing them
    /// through the production helpers, for the reason
    /// [`EXPECTED_SAFE_PUNCTUATION`] gives.
    #[cfg(unix)]
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
        // Issue #560: every case above injects a path that is ALREADY absolute,
        // so all three passed before anything enforced absoluteness. A relative
        // `current_exe()` is the shape macOS actually produces, and the name
        // "falls back to the absolute path" has to hold for it too.
        assert_eq!(
            resolve_binary_name(Ok(PathBuf::from("./bin/dot-agent-deck copy")), |_, _| true),
            format!(
                "'{}'",
                std::env::current_dir()
                    .expect("a cwd")
                    .join("bin/dot-agent-deck copy")
                    .display()
            ),
            "a relative current_exe() must be absolutised before quoting, not emitted as-is"
        );
    }

    /// Windows arm of the test above (#560/#561). Same three gate cases with
    /// drive-qualified inputs, and the expected strings carry the forward-slash
    /// respelling `posix_command_word` applies — which is the whole of #561
    /// observed at the seam rather than in the helper.
    #[cfg(windows)]
    #[test]
    fn resolve_binary_name_falls_back_to_the_absolute_path_when_the_name_is_shell_unsafe() {
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from(
                    r"C:\Program Files\deck\dot-agent-deck (1).exe"
                )),
                |_, _| true
            ),
            "'C:/Program Files/deck/dot-agent-deck (1).exe'",
            "a name containing shell metacharacters must fall back to the absolute path, \
             respelled with '/' and quoted for the space"
        );
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from(r"C:\deck\dot-agent-deck copy.exe")),
                |_, _| true
            ),
            "'C:/deck/dot-agent-deck copy.exe'",
            "a name containing whitespace must fall back to the respelled, quoted path"
        );
        assert_eq!(
            resolve_binary_name(Ok(PathBuf::from(r"C:\deck\-rf.exe")), |_, _| true),
            "C:/deck/-rf.exe",
            "a name with a leading '-' must fall back to the respelled path — unquoted, \
             since as a full path argument a leading '-' in the file name is not a flag"
        );
        // Issue #560's half, in the Windows dialect: a relative `current_exe()`
        // must be anchored before it is spelled.
        let expected = std::env::current_dir()
            .expect("a cwd")
            .join(r"bin\dot-agent-deck copy.exe")
            .to_str()
            .expect("a UTF-8 cwd")
            .replace('\\', "/");
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from(r".\bin\dot-agent-deck copy.exe")),
                |_, _| true
            ),
            format!("'{expected}'"),
            "a relative current_exe() must be absolutised before spelling, not emitted as-is"
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
    ///
    /// Split by host dialect since #560, for the reason the shell-unsafe test
    /// above records: the injected path must be absolute in the HOST's dialect.
    #[cfg(unix)]
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
        // Issue #560, on the branch that matters most: this is the exact case
        // the fallback exists to serve (a build that is not on `$PATH`), and it
        // is the one a macOS `./target/release/dot-agent-deck` launch lands in.
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from("./target/release/dot-agent-deck")),
                |_, _| false
            ),
            std::env::current_dir()
                .expect("a cwd")
                .join("target/release/dot-agent-deck")
                .display()
                .to_string(),
            "the emitted word must not be resolvable against the WORKER's cwd — it has to \
             be absolute so it means the same thing in every directory"
        );
    }

    /// Windows arm of the test above (#560/#561).
    #[cfg(windows)]
    #[test]
    fn resolve_binary_name_falls_back_to_the_absolute_path_when_the_name_is_not_on_path() {
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from(r"C:\build\worker-agent-deck.exe")),
                |_, _| false
            ),
            "C:/build/worker-agent-deck.exe",
            "a well-formed name whose $PATH lookup does not identity-match must fall back \
             to the respelled absolute path, which needs no quoting"
        );
        let expected = std::env::current_dir()
            .expect("a cwd")
            .join(r"target\release\dot-agent-deck.exe")
            .to_str()
            .expect("a UTF-8 cwd")
            .replace('\\', "/");
        assert_eq!(
            resolve_binary_name(
                Ok(PathBuf::from(r".\target\release\dot-agent-deck.exe")),
                |_, _| false
            ),
            expected,
            "the emitted word must not be resolvable against the WORKER's cwd — it has to \
             be absolute so it means the same thing in every directory"
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
    ///
    /// The injected literal is host-dialect since #560 (see the shell-unsafe
    /// test above); the property being asserted is identical on both.
    #[test]
    fn resolve_binary_name_absolute_path_fallback_is_never_the_default_literal() {
        #[cfg(unix)]
        let (injected, expected) = (
            "/opt/build/worker-agent-deck",
            "/opt/build/worker-agent-deck",
        );
        #[cfg(windows)]
        let (injected, expected) = (
            r"C:\build\worker-agent-deck.exe",
            "C:/build/worker-agent-deck.exe",
        );

        let fallback = resolve_binary_name(Ok(PathBuf::from(injected)), |_, _| false);
        assert_ne!(fallback, DEFAULT_BINARY_NAME);
        assert_eq!(fallback, expected);
    }

    /// Issue #560, stated as the invariant rather than as one example: whatever
    /// shape `current_exe()` comes back in, the fallback command word must
    /// resolve to the same file from any working directory. That is the whole
    /// justification the doc comment, the #520 changelog entry and the review
    /// thread all give for preferring a path over [`DEFAULT_BINARY_NAME`], and
    /// until this landed nothing enforced it — every existing test injected an
    /// already-absolute path, so a `current_exe()` of the shape macOS actually
    /// returns (`_NSGetExecutablePath` reports the INVOCATION path) sailed
    /// through and the worker resolved it against its own cwd.
    ///
    /// **`..` is handled differently per platform, and that is why it is only
    /// checked for absoluteness here.** [`std::path::absolute`] is purely
    /// lexical on Unix and deliberately KEEPS `..`, because collapsing it would
    /// change which file the path names when a component is a symlink; on
    /// Windows it follows `GetFullPathNameW` and DOES collapse it, so the
    /// result is no longer anchored under the cwd at all. Absoluteness holds
    /// either way, and absoluteness is the property #560 is about — so the
    /// cwd-anchoring assertion is applied only to the shapes where "anchored at
    /// the cwd" is well defined on both platforms.
    ///
    /// The comparison is against a dialect-appropriate prefix: on Windows the
    /// emitted word carries `/` separators (#561) while `current_dir()` returns
    /// `\`, so the raw cwd string is not a prefix of it.
    #[test]
    fn resolve_binary_name_fallback_is_absolute_for_every_relative_current_exe_shape() {
        let cwd = std::env::current_dir().expect("a cwd");
        let cwd_str = cwd.to_str().expect("a UTF-8 cwd");
        let cwd_prefix = if cfg!(windows) {
            cwd_str.replace('\\', "/")
        } else {
            cwd_str.to_string()
        };

        for (relative, anchored_at_cwd) in [
            ("./target/release/dot-agent-deck", true),
            ("target/release/dot-agent-deck", true),
            ("dot-agent-deck", true),
            // Absolute on both platforms; anchored under the cwd only where
            // `..` survives, i.e. not on Windows — see the doc above.
            ("../sibling/dot-agent-deck", false),
        ] {
            let fallback = resolve_binary_name(Ok(PathBuf::from(relative)), |_, _| false);
            let unquoted = parse_as_one_shell_word(&fallback)
                .unwrap_or_else(|| panic!("{fallback} must parse as exactly one POSIX word"));
            assert!(
                Path::new(&unquoted).is_absolute(),
                "current_exe() of {relative:?} produced {fallback}, which is not absolute — a \
                 worker would resolve it against ITS OWN cwd"
            );
            if anchored_at_cwd {
                assert!(
                    unquoted.starts_with(&cwd_prefix),
                    "{fallback} must be {relative:?} anchored at this process's cwd"
                );
            }
            assert_ne!(
                fallback, DEFAULT_BINARY_NAME,
                "absolutising must not degrade the fallback to the generic literal"
            );
        }
    }

    /// Issues #560 and #561 together, on the helper that emits the word: the
    /// two defects were in one expression and the fallback is only correct when
    /// both hold at once, so this asserts the combined post-condition across
    /// both host dialects. Whatever the dialect, the emitted word must be
    /// exactly one POSIX shell word, absolute, and containing a `/` — the last
    /// because a POSIX shell resolves a `/`-free command word through `$PATH`
    /// instead of as a path, which is what made a correctly-quoted Windows path
    /// unrunnable even in git-bash. (The absoluteness of what reaches this
    /// helper is [`resolve_binary_name`]'s job and is asserted by the test
    /// above; here the inputs stand in for what it passes down.)
    #[test]
    fn posix_command_word_always_emits_one_absolute_pathname_word() {
        for (path, windows_host) in [
            ("/opt/my deck/dot-agent-deck", false),
            ("/opt/build/dot-agent-deck (1)", false),
            (r"C:\Users\somebody\bin\dot-agent-deck.exe", true),
            (r"C:\Program Files\deck\dot-agent-deck.exe", true),
            (r"\\server\share\dot-agent-deck.exe", true),
        ] {
            let word = posix_command_word(path, windows_host)
                .unwrap_or_else(|| panic!("{path} must have a POSIX spelling"));
            let literal = parse_as_one_shell_word(&word)
                .unwrap_or_else(|| panic!("{word} must parse as exactly one POSIX word"));
            assert!(
                literal.contains('/'),
                "{word} must resolve as a pathname, not a $PATH lookup"
            );
            assert!(
                literal.starts_with('/') || literal.as_bytes().get(1) == Some(&b':'),
                "{word} must be absolute — rooted, or drive-qualified on Windows"
            );
        }
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
        // #561: on Windows the fallback carries the forward-slash respelling, so
        // the expectation is spelled here rather than taken from the raw path —
        // deriving it through `posix_command_word` would make this agree with
        // whatever that helper does instead of pinning what it should do.
        let expected_path = if cfg!(windows) {
            real_candidate.to_str().unwrap().replace('\\', "/")
        } else {
            real_candidate.to_str().unwrap().to_string()
        };
        assert_eq!(
            resolved,
            shell_quote_if_needed(&expected_path),
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

    /// Issue #563 pinned the treatment of backslashes here as merely *observed*
    /// pending issue #561; #561 is now resolved, and this is the assertion of
    /// the settled behavior. The settled behavior is that
    /// [`shell_quote_if_needed`] keeps doing exactly this: `\` stays out of
    /// the safe set, so a backslash-bearing path is single-quoted, and a
    /// single-quoted POSIX run takes no escapes so every backslash survives
    /// literally rather than being consumed as one. That is *correct* POSIX
    /// quoting and the other call site depends on it:
    /// `agent_hook_config::build_command` writes the Codex and Devin hook
    /// command lines. Devin's installer is `#[cfg(unix)]` and can only ever see
    /// a POSIX path; Codex's is NOT — `codex_home` honours `$CODEX_HOME` on
    /// every platform — so that one can be reached on Windows. Whether a Codex
    /// hook command needs a different dialect there was left out of scope for
    /// #561, which is about `binary_name`'s fallback; **#734 answered it: it
    /// does.** Codex runs the line through `cmd.exe /C` on Windows, so
    /// `build_command` no longer reaches this function on that host — it goes
    /// through [`native_shell_command_word`] to [`cmd_quote_if_needed`]. What
    /// is asserted below is unchanged and still load-bearing: this remains the
    /// POSIX quoter, and [`posix_command_word`] still depends on exactly this
    /// treatment of `\`.
    ///
    /// What #561 actually diagnosed is one layer up, and is fixed there rather
    /// than here: quoting alone never made a Windows path *runnable*, because a
    /// POSIX shell decides pathname-vs-`$PATH`-lookup on whether the command
    /// word contains a `/`. So the perfectly-quoted result asserted below is
    /// still `command not found` when used as a command word — which is why
    /// [`posix_command_word`] respells the separators before calling this, and
    /// why the fix did NOT belong in the quoter.
    #[test]
    fn shell_quote_if_needed_keeps_backslashes_literal_in_a_posix_word() {
        assert_eq!(
            shell_quote_if_needed(r"\"),
            r"'\'",
            "backslash is outside the safe set, so it is quoted"
        );
        assert_eq!(
            shell_quote_if_needed(r"C:\Users\somebody\bin\dot-agent-deck.exe"),
            r"'C:\Users\somebody\bin\dot-agent-deck.exe'",
            "a backslash-bearing path is single-quoted"
        );
        assert_eq!(
            shell_quote_if_needed(r"\\server\share\dot-agent-deck.exe"),
            r"'\\server\share\dot-agent-deck.exe'",
            "a UNC path is single-quoted"
        );

        // A single-quoted run takes no escapes (POSIX 2.2.2), so each backslash
        // survives literally rather than being consumed as one.
        assert_eq!(
            parse_as_one_shell_word(&shell_quote_if_needed(
                r"C:\Users\somebody\bin\dot-agent-deck.exe"
            ))
            .as_deref(),
            Some(r"C:\Users\somebody\bin\dot-agent-deck.exe"),
            "the quoted Windows path is one word with its backslashes intact"
        );

        // The half that made #561 a defect rather than a style question: the
        // word above is a correctly-quoted *literal*, and a correctly-quoted
        // literal with no `/` in it is a `$PATH` lookup, not a path. Nothing
        // this function can do changes that, which is what moves the fix to
        // `posix_command_word`.
        assert!(
            !parse_as_one_shell_word(&shell_quote_if_needed(
                r"C:\Users\somebody\bin\dot-agent-deck.exe"
            ))
            .expect("the quoted form parses as one word")
            .contains('/'),
            "the quoted Windows path contains no '/', so a POSIX shell resolves it \
             through $PATH rather than as a pathname"
        );
    }

    /// The safe punctuation set [`cmd_quote_if_needed`] documents, re-declared
    /// here independently of the production predicate for the reason
    /// [`EXPECTED_SAFE_PUNCTUATION`] gives. Note what it does and does not
    /// share with the POSIX set: `\` and `~` are safe here and not there, `%`
    /// is safe there and not here.
    const EXPECTED_CMD_SAFE_PUNCTUATION: &[char] =
        &['\\', '/', '.', '_', '-', '+', '=', ':', '@', ',', '~'];

    fn is_expected_cmd_safe(c: char) -> bool {
        c.is_ascii_alphanumeric() || EXPECTED_CMD_SAFE_PUNCTUATION.contains(&c)
    }

    /// The command line `cmd.exe` is left holding after Codex's Windows arm
    /// hands it `/C ""<line>""` — i.e. the model of the one composition step
    /// between what [`native_shell_command_word`] emits and what actually runs.
    ///
    /// `cmd /?` documents two rules. The first PRESERVES the quotes, but needs
    /// *exactly two* quote characters on the line with the text between them
    /// naming an executable file; neither shape the deck writes can satisfy it
    /// (a quoted path puts four quotes on the line, and an unquoted one leaves
    /// the deck's own `hook --agent …` arguments inside the pair, so the text
    /// between the quotes is not a file name). So the second always applies
    /// here: strip the leading quote and the LAST quote on the line, keeping
    /// any text after it. Modelling only that is what keeps this honest — it is
    /// the rule these inputs reach, not a general `cmd.exe` parser.
    fn cmd_c_line(deck_line: &str) -> String {
        let wrapped = format!("\"{deck_line}\"");
        let stripped = wrapped.strip_prefix('"').expect("codex wraps in quotes");
        let last = stripped.rfind('"').expect("the wrap adds a closing quote");
        format!("{}{}", &stripped[..last], &stripped[last + 1..])
    }

    /// The literal value of the first (command) word of a `cmd.exe` command
    /// line, or `None` when the line does not yield one.
    ///
    /// Deliberately as narrow as [`parse_as_one_shell_word`]: it understands
    /// exactly the two constructs [`cmd_quote_if_needed`] can emit — a run of
    /// safe bytes ended by a space, and a double-quoted run — so anything else
    /// is reported unusable rather than quietly accepted. In particular `'` is
    /// NOT a quoting character to `cmd.exe`; it is an ordinary byte of the file
    /// name, which is the whole of #734. Pure data: no shell is spawned.
    fn parse_as_one_cmd_word(line: &str) -> Option<String> {
        let mut out = String::new();
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            match c {
                // A quoted run: literal up to the closing quote.
                '"' => loop {
                    match chars.next() {
                        Some('"') => break,
                        Some(inner) => out.push(inner),
                        // Unterminated — cmd would keep reading.
                        None => return None,
                    }
                },
                // Unquoted whitespace ends the command word.
                ' ' => return Some(out),
                c if is_expected_cmd_safe(c) => out.push(c),
                // Anything else outside quotes is a byte cmd.exe would act on
                // (`&`, `|`, `>`, `^`) or expand (`%`, `!`), so the word's
                // literal value is not knowable from the text alone.
                _ => return None,
            }
        }
        Some(out)
    }

    /// Issue #734. The Windows dialect of the hook command word: an ordinary
    /// `\`-separated path is emitted VERBATIM (the safe set contains `\`), and
    /// what it must never be is single-quoted — `cmd.exe` reads `'` as an
    /// ordinary character, so the pre-fix output named a file whose name began
    /// with a quote and every deck hook silently failed to run.
    #[test]
    fn cmd_quote_if_needed_leaves_an_ordinary_windows_path_unquoted() {
        for path in [
            r"C:\Users\somebody\bin\dot-agent-deck.exe",
            r"C:\Users\RUNNER~1\AppData\Local\dot-agent-deck.exe",
            r"\\server\share\dot-agent-deck.exe",
            r"D:\deck\dot-agent-deck.exe",
        ] {
            let word = cmd_quote_if_needed(path);
            assert_eq!(word, path, "an ordinary Windows path needs no quoting");
            assert!(
                !word.starts_with('\''),
                "#734: a single-quoted path is not runnable by cmd.exe; got {word}"
            );
            assert_eq!(
                parse_as_one_cmd_word(&cmd_c_line(&format!("{word} hook --agent codex")))
                    .as_deref(),
                Some(path),
                "the command line cmd.exe is left holding must name exactly this binary"
            );
        }
    }

    /// A Windows path containing a space — `C:\Program Files\…` is the ordinary
    /// case, not an exotic one — is DOUBLE-quoted, and the result still names
    /// exactly that binary once Codex's outer wrap and `cmd /C`'s quote
    /// stripping have both been applied.
    #[test]
    fn cmd_quote_if_needed_double_quotes_a_path_containing_a_space() {
        let path = r"C:\Program Files\dot-agent-deck\dot-agent-deck.exe";
        let word = cmd_quote_if_needed(path);
        assert_eq!(
            word,
            format!("\"{path}\""),
            "a spaced path is double-quoted"
        );
        assert_eq!(
            parse_as_one_cmd_word(&cmd_c_line(&format!("{word} hook --agent codex"))).as_deref(),
            Some(path),
            "the quoted path survives the wrap and names one binary, not two words"
        );
    }

    /// `%` and `!` fall outside the Windows safe set and therefore force
    /// quoting — while `~`, which is safe here and NOT safe for POSIX, does
    /// not. The quoting does not *resolve* either character: `cmd.exe` expands
    /// `%VAR%` inside double quotes and `!VAR!` under delayed expansion, and
    /// nothing this function does prevents that. Mirrors the Claude-side
    /// `hook_rule_identification_015`, which asserts the same property but is
    /// `#[cfg(windows)]` and so runs on no machine here.
    #[test]
    fn cmd_quote_if_needed_quotes_percent_and_bang_but_not_tilde() {
        assert_eq!(
            cmd_quote_if_needed(r"C:\Tools\RUNNER~1\dot-agent-deck.exe"),
            r"C:\Tools\RUNNER~1\dot-agent-deck.exe",
            "'~' is not special to cmd.exe, so it does not force quoting"
        );
        for path in [
            r"C:\Tools\100%\dot-agent-deck.exe",
            r"C:\Tools\bang!\dot-agent-deck.exe",
        ] {
            assert_eq!(
                cmd_quote_if_needed(path),
                format!("\"{path}\""),
                "'%' and '!' are outside the safe set and force quoting"
            );
        }
    }

    /// The safe set is pinned in both directions, the same way the POSIX one
    /// is: every byte of it survives unquoted, and every other ASCII byte
    /// forces quoting. A future widening or narrowing of it rewrites every
    /// Windows hook command line, so it should fail an assertion rather than
    /// happen silently.
    #[test]
    fn cmd_quote_if_needed_pins_its_safe_set_in_both_directions() {
        for c in EXPECTED_CMD_SAFE_PUNCTUATION
            .iter()
            .copied()
            .chain('a'..='z')
        {
            let in_path = format!(r"C:\bin\dot-agent-deck{c}.exe");
            assert_eq!(
                cmd_quote_if_needed(&in_path),
                in_path,
                "'{c}' is in the safe set and must not force quoting"
            );
        }
        for byte in 0x20u8..0x7f {
            let c = byte as char;
            if is_expected_cmd_safe(c) {
                continue;
            }
            let input = format!(r"C:\bin\deck{c}.exe");
            let quoted = cmd_quote_if_needed(&input);
            assert!(
                quoted.starts_with('"') && quoted.ends_with('"'),
                "'{c}' is outside the safe set and must be quoted; got {quoted}"
            );
        }
        assert_eq!(
            cmd_quote_if_needed(""),
            "\"\"",
            "the empty string is quoted rather than emitted as nothing, so it \
             stays one (empty) command word instead of vanishing"
        );
    }

    /// Issue #734's actual fix: the selector routes by the shell that will RUN
    /// the line, and the two dialects genuinely differ on the input that
    /// matters — a `\`-separated path, which POSIX quotes (correctly, for a
    /// POSIX shell) and `cmd.exe` must not.
    #[test]
    fn native_shell_command_word_picks_the_dialect_of_the_running_shell() {
        let windows_path = r"C:\Users\somebody\bin\dot-agent-deck.exe";
        assert_eq!(
            native_shell_command_word(windows_path, true),
            windows_path,
            "on a Windows host the path is spelled for cmd.exe"
        );
        assert_eq!(
            native_shell_command_word(windows_path, false),
            format!("'{windows_path}'"),
            "on a POSIX host the POSIX quoter is unchanged"
        );

        let posix_path = "/home/somebody/bin/dot-agent-deck";
        for windows_host in [true, false] {
            assert_eq!(
                native_shell_command_word(posix_path, windows_host),
                posix_path,
                "an ordinary POSIX path is safe in both dialects and stays verbatim"
            );
        }

        // The POSIX arm is a pass-through, byte for byte: every other caller of
        // the POSIX quoter (`posix_command_word`, and therefore `binary_name`)
        // must be unaffected by this fix.
        for path in [
            "/home/o'brien/bin/dot-agent-deck",
            "/with space/dot-agent-deck",
            "",
            windows_path,
        ] {
            assert_eq!(
                native_shell_command_word(path, false),
                shell_quote_if_needed(path),
                "the non-Windows arm must be exactly the POSIX quoter"
            );
        }
    }

    /// Issue #561, the fix side: [`posix_command_word`] is what turns an
    /// absolute path into a word a POSIX shell will actually execute, and the
    /// `windows_host` parameter is what makes the Windows branch reachable from
    /// a Linux CI host. On a POSIX host it is a pass-through to
    /// [`shell_quote_if_needed`]; on a Windows path it respells `\` as `/`
    /// FIRST, which both makes the word a pathname and — for an ordinary
    /// drive-letter path — removes the need to quote it at all.
    #[test]
    fn posix_command_word_respells_a_windows_path_with_forward_slashes() {
        assert_eq!(
            posix_command_word(r"C:\Users\somebody\bin\dot-agent-deck.exe", true).as_deref(),
            Some("C:/Users/somebody/bin/dot-agent-deck.exe"),
            "a drive-letter path is respelled and then needs no quoting"
        );
        assert_eq!(
            posix_command_word(r"\\server\share\dot-agent-deck.exe", true).as_deref(),
            Some("//server/share/dot-agent-deck.exe"),
            "a UNC path becomes the //server/share form MSYS and Cygwin use"
        );
        assert_eq!(
            posix_command_word(r"C:\Program Files\deck\dot-agent-deck.exe", true).as_deref(),
            Some("'C:/Program Files/deck/dot-agent-deck.exe'"),
            "a respelled path containing a space is still single-quoted"
        );

        // Every emitted word is one shell word whose literal value contains a
        // `/` — i.e. a pathname, not a $PATH lookup. This is the property the
        // test above proves the quoter alone cannot deliver.
        for windows_path in [
            r"C:\Users\somebody\bin\dot-agent-deck.exe",
            r"\\server\share\dot-agent-deck.exe",
            r"C:\Program Files\deck\dot-agent-deck.exe",
            r"C:\Users\o'brien\dot-agent-deck.exe",
        ] {
            let word = posix_command_word(windows_path, true).expect("a spellable Windows path");
            let literal = parse_as_one_shell_word(&word)
                .unwrap_or_else(|| panic!("{word} must parse as exactly one POSIX word"));
            assert!(
                literal.contains('/'),
                "{word} must resolve as a pathname, not a $PATH lookup"
            );
            assert!(
                !literal.contains('\\'),
                "{word} must carry no backslash separators into the shell"
            );
        }
    }

    /// Issue #561: a `\\?\` verbatim or `\\.\` device path is REFUSED rather
    /// than respelled. Those prefixes are defined to disable path
    /// normalization, so `/` is not a separator inside them and swapping the
    /// separators would name a different file — emitting something that might
    /// be misparsed into a command an agent executes is worse than declining.
    /// [`resolve_binary_name`] turns the `None` into [`DEFAULT_BINARY_NAME`],
    /// whose worst case is a `$PATH` lookup that may well succeed.
    #[test]
    fn posix_command_word_refuses_a_windows_path_with_no_posix_spelling() {
        for unspellable in [
            r"\\?\C:\Users\somebody\dot-agent-deck.exe",
            r"\\?\UNC\server\share\dot-agent-deck.exe",
            r"\\.\C:\Users\somebody\dot-agent-deck.exe",
            // No separator at all: respelling would leave a bare word, which a
            // POSIX shell resolves through $PATH rather than as a path.
            "dot-agent-deck.exe",
        ] {
            assert_eq!(
                posix_command_word(unspellable, true),
                None,
                "{unspellable} has no POSIX-shell spelling and must be refused"
            );
        }
    }

    /// Issue #561: on a POSIX host nothing changes — [`posix_command_word`] is
    /// a pass-through to [`shell_quote_if_needed`], including for the paths a
    /// Unix filesystem genuinely allows to contain a backslash. Pinning this is
    /// the guard that the Windows branch never leaks onto Unix: `\` is a legal
    /// character in a Unix file name, so respelling one there would name a
    /// different file.
    #[test]
    fn posix_command_word_is_a_pass_through_on_a_posix_host() {
        for input in [
            "/usr/local/bin/dot-agent-deck",
            "/opt/my deck/dot-agent-deck",
            r"/opt/back\slash/dot-agent-deck",
            "/home/o'brien/bin/dot-agent-deck",
        ] {
            assert_eq!(
                posix_command_word(input, false).as_deref(),
                Some(shell_quote_if_needed(input).as_str()),
                "on a POSIX host the word is exactly what shell_quote_if_needed produces"
            );
        }
        assert_eq!(
            posix_command_word(r"/opt/back\slash/dot-agent-deck", false).as_deref(),
            Some(r"'/opt/back\slash/dot-agent-deck'"),
            "a backslash in a Unix path is quoted, never respelled"
        );
    }

    /// Issue #561: the four characters whose handling has to be stated exactly,
    /// because this word is written into a task file an agent then EXECUTES and
    /// a mis-quote there is a command-injection surface rather than a display
    /// bug. Pinned as literal expected strings, in both dialects, so any future
    /// change to the safe set or the respelling has to restate them.
    ///
    /// - **space** — outside the safe set, so the whole word is single-quoted
    ///   and stays one argument.
    /// - **`'`** — ends the quoted run, so it is spliced as `'\''`: close,
    ///   escaped literal quote, reopen. Still one word.
    /// - **`\`** — a legal character in a Unix file name and never a separator
    ///   there, so on Unix it is quoted and preserved verbatim (a single-quoted
    ///   POSIX run takes no escapes). On Windows it can only ever be a
    ///   separator (it is not legal in a file name), so it is respelled to `/`
    ///   and no literal backslash reaches the shell at all.
    /// - **`%`** — in the safe set and left bare, which is correct because a
    ///   POSIX shell gives `%` no meaning in a command word. This is precisely
    ///   where the POSIX target is load-bearing: `cmd.exe` expands `%VAR%` even
    ///   inside double quotes, so no amount of quoting would make this word
    ///   safe there — see [`binary_name`]'s doc for why `cmd.exe` is not the
    ///   target.
    #[test]
    fn posix_command_word_handles_space_quote_backslash_and_percent() {
        let unix = r"/opt/my deck/o'brien/50%/back\slash/dot-agent-deck";
        assert_eq!(
            posix_command_word(unix, false).as_deref(),
            Some(r"'/opt/my deck/o'\''brien/50%/back\slash/dot-agent-deck'"),
            "on Unix: space and quote force quoting, the backslash is preserved verbatim, \
             and % is inert"
        );
        assert_eq!(
            parse_as_one_shell_word(&posix_command_word(unix, false).expect("a POSIX spelling"))
                .as_deref(),
            Some(unix),
            "the quoted Unix path is exactly one word whose literal value is the path"
        );

        let windows = r"C:\Program Files\o'brien\50%\dot-agent-deck.exe";
        assert_eq!(
            posix_command_word(windows, true).as_deref(),
            Some(r"'C:/Program Files/o'\''brien/50%/dot-agent-deck.exe'"),
            "on Windows: separators become '/', space and quote force quoting, % is inert"
        );
        assert_eq!(
            parse_as_one_shell_word(&posix_command_word(windows, true).expect("a POSIX spelling"))
                .as_deref(),
            Some("C:/Program Files/o'brien/50%/dot-agent-deck.exe"),
            "the quoted Windows path is one word carrying no backslash into the shell"
        );

        assert_eq!(
            posix_command_word(r"C:\Users\50%\dot-agent-deck.exe", true).as_deref(),
            Some("C:/Users/50%/dot-agent-deck.exe"),
            "with no space and no quote, a respelled Windows path needs no quoting even \
             though it carries a %"
        );
    }

    // ---------------------------------------------------------------------
    // PRD #381 — the durable hook-binary-path resolver.
    //
    // Plain `#[test]`s, not `#[spec]` catalog entries: these are lib units,
    // like the `resolve_binary_name` tests above. The catalog entries for this
    // PRD are the two L2 tests in `tests/e2e_hook_binary_path.rs`.
    //
    // Every case drives `durable_binary_path_with`, whose three environmental
    // inputs (`current_exe()`, the home anchor, the `$PATH` value) are
    // injected. The filesystem checks are the REAL ones, so the fixtures below
    // create real executables in a real scratch directory rather than stubbing
    // out `is_executable_file`.
    // ---------------------------------------------------------------------

    /// A real executable file at `path`, parents created. Unix sets the exec
    /// bit, which the resolver's step-2a/2b gate genuinely requires.
    fn write_stub_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("candidate has a parent"))
            .expect("create candidate dir");
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write candidate");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod candidate");
        }
    }

    /// Every `Ok` this resolver can return has to satisfy its whole contract at
    /// once, so each case below funnels through here rather than asserting one
    /// property and trusting the rest: absolute, currently on disk, and never a
    /// bare command name (the shape issue #536 is about).
    fn assert_durable(resolved: &Result<String, String>) -> &String {
        let path = resolved
            .as_ref()
            .unwrap_or_else(|e| panic!("expected a durable path, got a refusal: {e}"));
        assert!(
            Path::new(path).is_absolute(),
            "resolved {path} is not absolute — a hook command runs under /bin/sh with a cwd the \
             deck does not control"
        );
        assert!(
            Path::new(path).exists(),
            "resolved {path} does not exist on disk"
        );
        assert_ne!(
            path, DEFAULT_BINARY_NAME,
            "resolved the bare crate name — issue #536: /bin/sh would resolve it through \
             whatever $PATH the agent has"
        );
        assert!(
            Path::new(path).file_name().is_some() && path.contains(std::path::MAIN_SEPARATOR),
            "resolved {path} is a bare command name, not a path"
        );
        path
    }

    /// Step 1: an installed binary performing its own install keeps working —
    /// `current_exe()` is returned unchanged.
    #[test]
    fn durable_binary_path_returns_a_non_artifact_current_exe_unchanged() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let installed = dir
            .path()
            .join("usr")
            .join("local")
            .join("bin")
            .join(format!(
                "{DEFAULT_BINARY_NAME}{}",
                std::env::consts::EXE_SUFFIX
            ));
        write_stub_executable(&installed);

        let resolved = durable_binary_path_with(Ok(installed.clone()), dir.path(), None);

        assert_eq!(
            assert_durable(&resolved),
            installed.to_str().expect("candidate path is UTF-8"),
            "a durable current_exe() must be used as-is, not re-resolved"
        );
    }

    /// Step 2a: the running binary IS a build artifact, and
    /// `<home>/.local/bin/<name>` exists and is executable — that path wins,
    /// and the artifact never appears.
    #[test]
    fn durable_binary_path_prefers_the_installed_home_candidate_over_a_build_artifact() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        let installed = home.join(".local").join("bin").join(format!(
            "{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        ));
        write_stub_executable(&installed);
        let artifact = dir
            .path()
            .join("checkout")
            .join("target")
            .join("release")
            .join(DEFAULT_BINARY_NAME);
        write_stub_executable(&artifact);

        let resolved = durable_binary_path_with(Ok(artifact.clone()), &home, None);

        assert_eq!(
            assert_durable(&resolved),
            installed.to_str().expect("installed path is UTF-8")
        );
        assert!(
            !resolved.as_ref().expect("resolved").contains("target"),
            "the build artifact leaked into the resolved path: {resolved:?}"
        );
        // The 2a candidate is deliberately NOT canonicalized, which is what
        // makes a `~/.local/bin` symlink into a cargo target dir resolve to the
        // durable spelling. Prove the returned value is the candidate path
        // itself even when it IS such a symlink.
        #[cfg(unix)]
        {
            let linked_home = dir.path().join("linked-home");
            let link = linked_home.join(".local").join("bin").join(format!(
                "{DEFAULT_BINARY_NAME}{}",
                std::env::consts::EXE_SUFFIX
            ));
            std::fs::create_dir_all(link.parent().expect("link parent")).expect("create link dir");
            std::os::unix::fs::symlink(&artifact, &link).expect("symlink into target dir");

            let via_link = durable_binary_path_with(Ok(artifact.clone()), &linked_home, None);
            assert_eq!(
                assert_durable(&via_link),
                link.to_str().expect("link path is UTF-8"),
                "canonicalizing the 2a candidate would resolve a durable symlink straight back \
                 to the artifact it points at"
            );
        }
    }

    /// Step 2b: no `~/.local/bin` candidate, but the name is on `$PATH` — its
    /// absolute path is used. An untrustworthy (relative) entry earlier on the
    /// same `$PATH` is skipped, and so is one pointing into a cargo target
    /// directory, which is the routine shape on a developer's machine.
    #[test]
    fn durable_binary_path_falls_back_to_an_absolute_path_lookup() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let name = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);

        let artifact_dir = dir.path().join("checkout").join("target").join("debug");
        write_stub_executable(&artifact_dir.join(&name));
        let installed_dir = dir.path().join("opt").join("bin");
        write_stub_executable(&installed_dir.join(&name));

        // Order matters: a relative entry first (a shell would search it, this
        // resolver must not trust it), then the artifact dir, then the durable
        // one. Only the last is an acceptable answer.
        let path_value = std::env::join_paths([
            PathBuf::from("relative-bin"),
            artifact_dir.clone(),
            installed_dir.clone(),
        ])
        .expect("join synthetic PATH");

        let resolved = durable_binary_path_with(
            Ok(artifact_dir.join(&name)),
            &home,
            Some(path_value.as_os_str()),
        );

        assert_eq!(
            assert_durable(&resolved),
            installed_dir.join(&name).to_str().expect("path is UTF-8"),
            "the PATH fallback must skip relative and build-artifact entries"
        );
    }

    /// Step 2c: a build artifact with neither a `~/.local/bin` candidate nor a
    /// `$PATH` hit is a REFUSAL — and the message has to be actionable, naming
    /// the rejected path and what to do about it.
    #[test]
    fn durable_binary_path_refuses_when_no_durable_candidate_exists() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let artifact = dir
            .path()
            .join("checkout")
            .join("target")
            .join("debug")
            .join(DEFAULT_BINARY_NAME);
        write_stub_executable(&artifact);
        let empty = dir.path().join("empty-bin");
        std::fs::create_dir_all(&empty).expect("create empty PATH dir");
        let path_value = std::env::join_paths([empty]).expect("join synthetic PATH");

        let err =
            durable_binary_path_with(Ok(artifact.clone()), &home, Some(path_value.as_os_str()))
                .expect_err("a build artifact with no durable candidate must refuse");

        assert!(
            err.contains(artifact.to_str().expect("artifact path is UTF-8")),
            "the refusal must name the rejected current_exe() path: {err}"
        );
        assert!(
            err.contains("cargo install --path ."),
            "the refusal must say what to do about it: {err}"
        );
        assert!(
            err.contains(".local"),
            "the refusal must name the durable location it looked in: {err}"
        );
    }

    /// Issue #536: a `current_exe()` that fails is a refusal too, and
    /// specifically NOT a fall back to `DEFAULT_BINARY_NAME`. `binary_name()`
    /// legitimately returns that literal in the same situation — writing it
    /// into a file Claude Code hands to `/bin/sh` is what re-opens the very
    /// `$PATH` miss this PRD exists to close.
    #[test]
    fn durable_binary_path_refuses_rather_than_naming_the_bare_binary() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        // Seeded deliberately: even with a perfectly good durable candidate
        // available, an unknown `current_exe()` must not silently install
        // hooks — and even if it did, it must never be the bare name.
        let installed = dir.path().join(".local").join("bin").join(format!(
            "{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        ));
        write_stub_executable(&installed);

        let resolved = durable_binary_path_with(
            Err(std::io::Error::other("no such process")),
            dir.path(),
            None,
        );

        let err = resolved.expect_err("an unresolvable current_exe() must refuse");
        assert!(
            !err.is_empty() && err != DEFAULT_BINARY_NAME,
            "issue #536: the bare crate name is never an acceptable answer here"
        );
        assert!(
            err.contains("cargo install --path ."),
            "the refusal must stay actionable: {err}"
        );
    }

    /// The build-artifact test is on path COMPONENTS, not a substring — so a
    /// home directory literally named `target`, a deck kept under
    /// `/opt/target/release-notes/`, and a `debug` directory whose parent is
    /// not `target` are all accepted, while a real `target/debug` or
    /// `target/release` is caught. A substring check would fail every
    /// near-miss here.
    #[test]
    fn is_build_artifact_path_matches_components_not_substrings() {
        for artifact in [
            "/home/u/code/deck/target/debug/dot-agent-deck",
            "/home/u/code/deck/target/release/dot-agent-deck",
            "/home/u/code/deck/target/debug/deps/dot-agent-deck",
            "target/debug/dot-agent-deck",
        ] {
            assert!(
                is_build_artifact_path(Path::new(artifact)),
                "{artifact} is a cargo build artifact"
            );
        }
        for durable in [
            "/home/target/.local/bin/dot-agent-deck",
            "/opt/target/release-notes/dot-agent-deck",
            "/opt/target/debugger/dot-agent-deck",
            "/srv/build/debug/dot-agent-deck",
            "/srv/release/dot-agent-deck",
            "/usr/local/bin/dot-agent-deck",
            "/home/u/targets/debug/dot-agent-deck",
        ] {
            assert!(
                !is_build_artifact_path(Path::new(durable)),
                "{durable} is not a cargo build artifact — a substring check would \
                 misclassify it"
            );
        }
    }

    /// A near-miss end to end: a deck genuinely installed under a directory
    /// named `target` is returned by step 1, not refused.
    #[test]
    fn durable_binary_path_accepts_an_install_under_a_directory_named_target() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let installed = dir
            .path()
            .join("target")
            .join("release-notes")
            .join(DEFAULT_BINARY_NAME);
        write_stub_executable(&installed);

        let resolved = durable_binary_path_with(Ok(installed.clone()), dir.path(), None);

        assert_eq!(
            assert_durable(&resolved),
            installed.to_str().expect("installed path is UTF-8")
        );
    }

    /// A `current_exe()` that resolves but is no longer on disk (an upgrade
    /// replaced it, or Linux reported `…/dot-agent-deck (deleted)`) is not
    /// written either: the contract is a path that exists, so the resolver
    /// falls through to the durable candidate.
    #[test]
    fn durable_binary_path_skips_a_current_exe_that_is_no_longer_on_disk() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let installed = dir.path().join(".local").join("bin").join(format!(
            "{DEFAULT_BINARY_NAME}{}",
            std::env::consts::EXE_SUFFIX
        ));
        write_stub_executable(&installed);
        let gone = dir.path().join("gone").join(DEFAULT_BINARY_NAME);

        let resolved = durable_binary_path_with(Ok(gone), dir.path(), None);

        assert_eq!(
            assert_durable(&resolved),
            installed.to_str().expect("installed path is UTF-8")
        );
    }

    /// PRD #381 audit, LOW-2. A candidate with mode `0o011` has an exec bit
    /// set, so the old `mode & 0o111 != 0` gate accepted it — but the file is
    /// owned by this test's own user, and for the OWNER the kernel consults the
    /// owner triad alone, which here has no `x`. `access(X_OK)` says so; the
    /// mode bitmask does not. The resolver must therefore step over it and keep
    /// walking, instead of stopping there and persisting a command that fails
    /// with permission denied on every hook, indefinitely.
    ///
    /// (`0o011` rather than the audit's cross-user `0o100`: same class of
    /// defect, same fix, and reproducible without a second uid.)
    #[cfg(unix)]
    #[test]
    fn durable_binary_path_skips_a_candidate_this_user_cannot_execute() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let name = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);

        let artifact_dir = dir.path().join("checkout").join("target").join("debug");
        write_stub_executable(&artifact_dir.join(&name));

        // Step 2a: an exec bit is set, but not one that applies to us.
        let unusable = home.join(".local").join("bin").join(&name);
        write_stub_executable(&unusable);
        std::fs::set_permissions(&unusable, std::fs::Permissions::from_mode(0o011))
            .expect("chmod the unusable candidate");
        assert_ne!(
            std::fs::metadata(&unusable)
                .expect("stat the unusable candidate")
                .permissions()
                .mode()
                & 0o111,
            0,
            "the fixture must still satisfy the OLD `mode & 0o111` check, or it \
             proves nothing"
        );

        // Step 2b: a candidate that really is executable by us.
        let usable_dir = dir.path().join("opt").join("bin");
        write_stub_executable(&usable_dir.join(&name));
        let path_value = std::env::join_paths([usable_dir.clone()]).expect("join synthetic PATH");

        let resolved = durable_binary_path_with(
            Ok(artifact_dir.join(&name)),
            &home,
            Some(path_value.as_os_str()),
        );

        assert_eq!(
            assert_durable(&resolved),
            usable_dir.join(&name).to_str().expect("path is UTF-8"),
            "a candidate this user cannot execute must be skipped, not persisted"
        );
    }

    /// PRD #381 audit, HIGH (the accepted half). A step-2a candidate the group
    /// can rewrite is not a path worth pinning into four agents' persistent
    /// configuration, so resolution steps over it and continues. See
    /// [`write_mode_is_owner_only`] for what is deliberately NOT checked here
    /// (owners, ancestor directories, symlink targets — issue #732).
    #[cfg(unix)]
    #[test]
    fn durable_binary_path_skips_a_group_writable_installed_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let name = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);

        let artifact_dir = dir.path().join("checkout").join("target").join("debug");
        write_stub_executable(&artifact_dir.join(&name));

        let loose = home.join(".local").join("bin").join(&name);
        write_stub_executable(&loose);
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o775))
            .expect("chmod the group-writable candidate");

        let tight_dir = dir.path().join("opt").join("bin");
        write_stub_executable(&tight_dir.join(&name));
        let path_value = std::env::join_paths([tight_dir.clone()]).expect("join synthetic PATH");

        let resolved = durable_binary_path_with(
            Ok(artifact_dir.join(&name)),
            &home,
            Some(path_value.as_os_str()),
        );

        assert_eq!(
            assert_durable(&resolved),
            tight_dir.join(&name).to_str().expect("path is UTF-8"),
            "a group-writable ~/.local/bin candidate must be skipped"
        );
    }

    /// The same rule on step 2b: a world-writable `$PATH` candidate is skipped
    /// and the walk continues to a later owner-only one, rather than the first
    /// executable hit winning.
    #[cfg(unix)]
    #[test]
    fn durable_binary_path_skips_a_world_writable_path_candidate() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let name = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);

        let artifact_dir = dir.path().join("checkout").join("target").join("debug");
        write_stub_executable(&artifact_dir.join(&name));

        let shared_dir = dir.path().join("srv").join("shared-bin");
        let planted = shared_dir.join(&name);
        write_stub_executable(&planted);
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o777))
            .expect("chmod the world-writable candidate");

        let tight_dir = dir.path().join("usr").join("bin");
        write_stub_executable(&tight_dir.join(&name));

        let path_value = std::env::join_paths([shared_dir.clone(), tight_dir.clone()])
            .expect("join synthetic PATH");

        let resolved = durable_binary_path_with(
            Ok(artifact_dir.join(&name)),
            &home,
            Some(path_value.as_os_str()),
        );

        assert_eq!(
            assert_durable(&resolved),
            tight_dir.join(&name).to_str().expect("path is UTF-8"),
            "a world-writable $PATH candidate must be skipped, not pinned"
        );
    }

    /// A symlink candidate is exempt from the write-mode check rather than
    /// judged by it: its own mode is `0777` on every Unix this ships for, and
    /// reading through it to the target's mode is the canonical-target
    /// validation issue #732 owns. Step 2a's whole point is that the stable
    /// `~/.local/bin` name is the durable thing — this pins that the F4 check
    /// did not quietly undo it.
    #[cfg(unix)]
    #[test]
    fn durable_binary_path_still_accepts_a_symlinked_installed_candidate() {
        let dir = crate::test_temp::tempdir().expect("resolver tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let name = format!("{DEFAULT_BINARY_NAME}{}", std::env::consts::EXE_SUFFIX);

        let artifact_dir = dir.path().join("checkout").join("target").join("debug");
        let artifact = artifact_dir.join(&name);
        write_stub_executable(&artifact);

        let link = home.join(".local").join("bin").join(&name);
        std::fs::create_dir_all(link.parent().expect("link has a parent"))
            .expect("create ~/.local/bin");
        std::os::unix::fs::symlink(&artifact, &link).expect("symlink the durable name");

        let resolved = durable_binary_path_with(Ok(artifact.clone()), &home, None);

        assert_eq!(
            assert_durable(&resolved),
            link.to_str().expect("link path is UTF-8"),
            "the symlink spelling is the durable answer and must survive the \
             write-mode check"
        );
    }

    /// PRD #381 audit, MEDIUM-1, at the unit level: [`pin_is_repairable`] is
    /// the read side of this resolver, and the whole point of it is that a BARE
    /// pin is repairable no matter what the process cwd happens to contain.
    /// `Path::try_exists("dot-agent-deck")` is cwd-relative; the agent's
    /// `/bin/sh` resolves the same string through `$PATH`. The two are not the
    /// same question, and only the second one is the one that runs.
    #[test]
    fn pin_is_repairable_rejects_a_bare_or_relative_pin() {
        assert!(
            pin_is_repairable(DEFAULT_BINARY_NAME),
            "a bare command name is issue #536's own shape and is always repairable"
        );
        assert!(
            pin_is_repairable("./dot-agent-deck"),
            "a cwd-relative pin is resolved by the agent, not by us"
        );
        assert!(
            pin_is_repairable("target/debug/dot-agent-deck"),
            "a relative build-artifact pin is repairable twice over"
        );
    }

    /// The other half of the same predicate, and the reason it is not simply
    /// "is this absolute": an absolute pin still has to be a regular file this
    /// user can execute, and still must not be a build artifact.
    #[test]
    fn pin_is_repairable_judges_an_absolute_pin_on_the_resolver_invariant() {
        let dir = crate::test_temp::tempdir().expect("pin tempdir");

        let live = dir.path().join("opt").join(DEFAULT_BINARY_NAME);
        write_stub_executable(&live);
        assert!(
            !pin_is_repairable(live.to_str().expect("UTF-8")),
            "a usable absolute pin is preserved — repairing what merely differs \
             is what PRD #381 Open Question 3 rules out"
        );

        let gone = dir.path().join("pruned").join(DEFAULT_BINARY_NAME);
        assert!(
            pin_is_repairable(gone.to_str().expect("UTF-8")),
            "a positively-missing pin is the original repair trigger"
        );

        let artifact = dir
            .path()
            .join("checkout")
            .join("target")
            .join("release")
            .join(DEFAULT_BINARY_NAME);
        write_stub_executable(&artifact);
        assert!(
            pin_is_repairable(artifact.to_str().expect("UTF-8")),
            "a LIVE build artifact is repairable: it is exactly what this PRD \
             refuses to write, and it disappears with its worktree"
        );

        let a_directory = dir.path().join("opt");
        assert!(
            pin_is_repairable(a_directory.to_str().expect("UTF-8")),
            "a directory exists but is not an executable file"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let not_executable = dir.path().join("data").join(DEFAULT_BINARY_NAME);
            write_stub_executable(&not_executable);
            std::fs::set_permissions(&not_executable, std::fs::Permissions::from_mode(0o644))
                .expect("chmod");
            assert!(
                pin_is_repairable(not_executable.to_str().expect("UTF-8")),
                "a non-executable file cannot be a hook command"
            );
        }
    }
}
