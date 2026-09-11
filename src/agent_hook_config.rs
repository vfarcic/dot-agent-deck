//! Helpers shared by the per-agent hook-config adapters — how the deck's hook
//! command is spelled, and how an agent's config file is published.
//!
//! [`crate::codex_hooks_manage`] and [`crate::devin_hooks_manage`] each install
//! the deck's hooks by rewriting a config file a *third-party* tool owns:
//! Codex's `~/.codex/hooks.json` and `~/.codex/config.toml`, Devin's
//! `~/.config/devin/config.json`. The Devin adapter was copied verbatim from the
//! Codex one, so both carried byte-identical copies of these two helpers — which
//! is how the permissions defect below was fixed in one (#360) and left standing
//! in the other (#382). They live here once so the next adapter inherits the fix
//! instead of the bug.
//!
//! **[`crate::hooks_manage`] deliberately keeps its own `write_atomic`.** The
//! Claude adapter's is not a third copy of this one: it takes `dest` alone
//! (deriving the directory). It publishes through `create_new` too (#534), but
//! at the fixed `.<name>.tmp.<pid>` path, unlinking a squatter and retrying
//! once; this one draws an unpredictable name instead (#731). Folding them
//! together would push a rewrite onto an adapter that has not been reviewed for
//! it, so it stays where it is.
//!
//! It does share [`backup_malformed`], though, and all three adapters do — that
//! one is new rather than a rewrite of anything, and the alternative was a
//! fourth hand-rolled copy of the write whose triplicated version is what #731
//! is about.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

/// Which shell will actually EXECUTE the hook command line the deck writes, and
/// therefore which dialect [`build_command`] quotes for. It is a property of
/// the **consuming agent**, not of the machine the deck was compiled on (issue
/// #734).
///
/// Spelled per writer rather than defaulted, because the two writers genuinely
/// differ and a single host-derived answer is wrong for one of them. Deriving
/// it from `cfg!(windows)` for both is the same category error #734 fixed —
/// reading the dialect off the compile target instead of off the interpreter —
/// just one level further down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookShell {
    /// The host's native shell: `cmd.exe` on Windows, a POSIX shell elsewhere.
    ///
    /// **Codex.** Its hooks engine
    /// (`codex-rs/hooks/src/engine/command_runner.rs`, read at 0.149.0) hands
    /// the whole command string to `%COMSPEC%` else `cmd.exe` with `/C` on
    /// Windows, and to `$SHELL` else `/bin/sh` with `-lc` otherwise. There is
    /// no per-entry override to write: measured on 0.149.0, a handler's
    /// `shell`, `cwd`, `env` and `timeoutSec` are silently dropped and do not
    /// even reach `currentHash`, so that default is what runs every deck hook
    /// and the interpreter really does follow the host — and `codex_home`
    /// honours `$CODEX_HOME` on every platform, which is what makes the Windows
    /// arm reachable rather than theoretical.
    Native,
    /// A POSIX shell, whatever the host is.
    ///
    /// **Devin.** `devin_hooks_manage::devin_config_dir` returns `None` off
    /// Unix, so the only machine that can ever read the config this writer
    /// produces is a Unix one, and its interpreter is POSIX by construction.
    ///
    /// The gate that makes that true lives one level up, and
    /// `devin_hooks_manage::install_to` is reachable *without* passing through
    /// it — so a host-derived dialect did not stay theoretical either: it gave
    /// a Windows CI runner double-quoted Devin output and went red, which is
    /// the correct outcome, since that output contradicts the very claim
    /// ("byte-identical on every platform Devin can run on") that justified
    /// leaving the Devin path alone in the first place. Asking for POSIX here
    /// makes the claim true at the call site instead of borrowing it from a
    /// caller.
    Posix,
}

/// Build the deck's hook command for `binary_path`, robustly quoting the
/// executable path so a path containing whitespace or shell metacharacters still
/// produces a valid command that the agent parses to the intended argv. A "safe"
/// path (only path-typical characters) is emitted verbatim so the common case
/// stays human-readable and stable; anything else is quoted in `shell`'s
/// dialect — single quotes for a POSIX shell, double quotes for `cmd.exe`.
///
/// `suffix` is the caller's `HOOK_COMMAND_SUFFIX` — the fixed
/// `hook --agent <agent>` signature that also identifies the resulting command
/// as deck-owned on the way back in, so the two must stay the same string.
///
/// **The quoting follows the interpreter, not the compile target** (issue
/// #734); [`HookShell`] records which writer names which interpreter, and why.
/// Before #734 it was POSIX on every platform, so a Windows Codex user
/// (reachable only via `$CODEX_HOME` — see `codex_hooks_manage::codex_home`)
/// got `'C:\…\dot-agent-deck.exe' hook --agent codex` written into
/// `hooks.json`, which `cmd.exe` cannot run: it reads `'` as an ordinary
/// character and looks for a file whose name literally starts with one.
pub(crate) fn build_command(binary_path: &str, suffix: &str, shell: HookShell) -> String {
    build_command_for(binary_path, suffix, shell, cfg!(windows))
}

/// [`build_command`] with the host as a parameter.
///
/// The split exists for testability and nothing else: production passes
/// `cfg!(windows)`, a compile-time constant, so the branch costs nothing at
/// runtime — but a `#[cfg]` here would leave the Windows spelling of these
/// command lines asserted by nothing on any machine this project is developed
/// or CI-tested on except `build-windows`, which type-checks the arm without
/// ever running it. That is exactly how #734 shipped.
///
/// It is also what lets [`HookShell::Posix`]'s host-independence be *asserted*
/// from Linux rather than trusted, which matters because that property was
/// wrong once already and only a Windows runner noticed.
fn build_command_for(
    binary_path: &str,
    suffix: &str,
    shell: HookShell,
    windows_host: bool,
) -> String {
    let windows_dialect = match shell {
        HookShell::Native => windows_host,
        HookShell::Posix => false,
    };
    format!(
        "{} {suffix}",
        crate::platform::paths::native_shell_command_word(binary_path, windows_dialect)
    )
}

/// Atomically publish `bytes` to `dest` by writing a temp file in `dir` — which
/// must be `dest`'s OWN directory, so `rename(2)` stays on one filesystem and is
/// atomic — and renaming over `dest`. A crash mid-write leaves either the old
/// file or the temp file intact, never a truncated `dest`.
///
/// The temp name carries `dest`'s file name (see [`temp_path`]), so one
/// adapter's `hooks.json` and `config.toml` publishes never race on a single
/// temp path. (The `"config"` fallback is only reachable for a `dest` with no
/// UTF-8 file name, where the `rename` below cannot succeed either; the two
/// adapters spelled that unreachable literal differently before this was
/// extracted.)
///
/// # The temp file is never an entry that already exists (#731)
///
/// This used to build `.<name>.tmp.<pid>` and open it with `File::create`. Both
/// halves were wrong together: the name is fully derivable from the destination
/// and a pid anyone on the box can read, and `File::create` **follows a
/// symlink**. A writer able to add an entry to the agent's config directory —
/// `~/.codex`, `~/.config/devin` — could pre-plant that name pointing anywhere
/// it could write, and the publish would truncate that target, chmod it and
/// fill it with the deck's bytes. The `rename` then moved the *symlink* onto
/// `dest` (rename does not follow one either), so the destination did not even
/// end up holding the evidence.
///
/// The fix is [`create_temp_excl`]'s `create_new` — `O_CREAT|O_EXCL`, which
/// POSIX requires to fail with `EEXIST` when the path names a symlink, dangling
/// or not — over an unpredictable name, retried on collision. Two independent
/// properties, deliberately: `O_EXCL` is what makes following impossible, and
/// it holds even if the name were guessed outright. The unpredictable name is
/// the second layer, and it demotes the remaining attack from a redirected
/// write to a squat that costs one retry.
///
/// # Permissions
///
/// The temp file is published with the destination's OWN mode, or owner-only
/// when the file is new. `File::create` would otherwise apply `0666 & !umask` —
/// 0644 under a typical 022 umask, **0664 (group-writable) under 002** — and the
/// rename would then silently widen a config the user had kept private. That is
/// not theoretical: a real `devin` install ships its config at 0600 and it holds
/// `devin.org_id`, and Codex's `config.toml` holds the user's model choice,
/// hook-trust records and any hand-written settings (#360, #382).
///
/// Creation itself is owner-only on Unix rather than umask-derived, so the file
/// is never briefly group- or world-readable between `open` and the `chmod`
/// below. That is only a tightening of the pre-content window — the mode the
/// publish lands is still the destination's own, applied by `fchmod`, which no
/// umask filters.
pub(crate) fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    publish(dir, dest, bytes, PublishMode::Destination)
}

/// Which mode a [`publish`] lands.
///
/// Named rather than implied because the answer genuinely differs between the
/// two things this module writes, and the wrong one is a confidentiality bug in
/// either direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishMode {
    /// The destination's OWN current mode, or owner-only when it is new — the
    /// rule #360 and #382 exist to hold, so an install never widens a config the
    /// user kept private and never narrows one they deliberately opened.
    ///
    /// The stat behind this FOLLOWS a symlink, which is correct here: a config
    /// legitimately symlinked into a dotfiles checkout should be published at
    /// the mode of the file that actually holds it. What makes that safe is the
    /// caller — `hooks_manage::write_settings` refuses a symlinked destination
    /// outright, and the other adapters write a path the agent owns.
    Destination,
    /// Owner-only, whatever is at the destination.
    ///
    /// For a file whose name is the deck's own scratch convention, where there
    /// is no user intent at the destination to preserve and a pre-existing entry
    /// is far more likely to be a plant than a preference. Greptile's P1 on PR
    /// #855: [`backup_malformed`] used [`PublishMode::Destination`], so a
    /// symlink planted at `<name>.bak` pointing at a world-readable file got to
    /// CHOOSE the mode a copy of the user's config was published with — the
    /// `rename` correctly refused to follow the link for the write, and then the
    /// mode was taken from what it pointed at anyway.
    OwnerOnly,
}

/// [`write_atomic`] with the landed mode spelled by the caller.
fn publish(dir: &Path, dest: &Path, bytes: &[u8], mode: PublishMode) -> io::Result<()> {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");

    let (mut file, tmp) = create_temp(dir, name)?;

    // Windows has no mode bits to set, so the policy is read by nobody there and
    // `-D warnings` fails on the unused parameter. `build-windows` is the only
    // gate that compiles this arm, and it caught exactly that on PR #855 — the
    // divergence CLAUDE.md rule 2 warns about, since the four-flag clippy run a
    // contributor makes locally never builds this configuration.
    #[cfg(not(unix))]
    let _ = mode;

    // Everything after the create is fallible with a temp file already on disk,
    // so it runs in one closure and shares a single cleanup path. The previous
    // shape leaked the temp file whenever `write_all` or `sync_all` failed — it
    // only removed it when the `rename` did.
    let written = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Set explicitly in BOTH arms rather than leaning on
            // `create_temp_excl`'s `mode(0o600)`, which an unusual umask can
            // narrow further; `fchmod` is filtered by no umask.
            let landed = match mode {
                PublishMode::Destination => std::fs::metadata(dest)
                    .map(|meta| meta.permissions().mode() & 0o777)
                    .unwrap_or(0o600),
                PublishMode::OwnerOnly => 0o600,
            };
            file.set_permissions(std::fs::Permissions::from_mode(landed))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);

    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, dest)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Preserve `bytes` — the content of a config file that would not parse — beside
/// the original as `<file name>.bak`, and report where they went.
///
/// Best-effort by contract: every caller is on its way to returning an
/// `InvalidData` error with the user's file left untouched, so a failed copy must
/// not replace that error with its own. The return is a path rather than a `()`
/// so [`preserved_phrase`] can turn it into the clause the caller's message
/// shows, and no message names a backup that was never made — the
/// `let _ = std::fs::write(…)` this replaces could not express that difference.
///
/// # The copy is never written THROUGH a symlink (#731)
///
/// All three adapters spelled this as `std::fs::write` at this same, fully
/// predictable path. That opens with `O_TRUNC` and **follows a symlink**, so a
/// writer able to add an entry to the agent's config directory — `~/.claude`,
/// `~/.codex`, `~/.config/devin` — could plant `<name>.bak` pointing at any file
/// it could write and have the deck truncate that file and fill it with the
/// malformed config's bytes. It is [`write_atomic`]'s own defect one door along:
/// the config directory's other name the deck writes without creating it first.
///
/// Publishing through [`write_atomic`] closes it, because `rename(2)` does not
/// follow a symlink at its destination either — it replaces the link itself, so
/// the planted target is never opened. Replacing rather than refusing is the
/// right branch *here*, unlike `hooks_manage::refuse_symlinked_destination`
/// which guards the real config file: a `.bak` is the deck's own scratch name
/// that nobody stows in a dotfiles checkout, and refusing would discard the very
/// bytes this exists to keep.
///
/// # Permissions
///
/// The backup lands **owner-only, always** ([`PublishMode::OwnerOnly`]) — not at
/// the destination's own mode, which is what every other write here uses. It is
/// a byte-for-byte copy of a config that may hold an org id or an auth
/// reference, so 0600 is the right answer on its merits; `std::fs::write` left
/// it at `0666 & !umask`, typically 0644, beside a Devin config that ships 0600
/// (#360, #382).
///
/// Taking the destination's mode here would ALSO hand the mode back to a
/// planted symlink, which is Greptile's P1 on PR #855 and the reason this arm
/// exists: `rename` does not follow the link for the write, but the stat that
/// chose the mode did, so a link pointing at a world-readable file published the
/// user's config bytes 0666. Not following a symlink and then adopting its
/// target's permissions is worse than either half sounds.
///
/// # The name
///
/// `.bak` is APPENDED to the whole file name. Two of the three adapters spelled
/// this `path.with_extension("json.bak")`, which *replaces* the extension
/// instead — the same answer for every path they actually pass, since
/// `settings.json`, `hooks.json` and `config.json` each reach
/// `<that name>.bak` either way, and a different one only for a destination not
/// named `*.json`, where appending is what keeps the original name legible.
pub(crate) fn backup_malformed(dest: &Path, bytes: &[u8]) -> Option<PathBuf> {
    let mut name = dest.file_name()?.to_os_string();
    name.push(".bak");
    // `dest`'s OWN directory, which is what keeps the publish's `rename` on one
    // filesystem — the same requirement `write_atomic` states for `dir`.
    let dir = match dest.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let backup = dir.join(name);
    publish(dir, &backup, bytes, PublishMode::OwnerOnly)
        .ok()
        .map(|()| backup)
}

/// Spell where [`backup_malformed`]'s bytes went, for the caller's error message.
///
/// One phrasing shared by all three adapters, so the sentence a user reads names
/// a file that exists.
pub(crate) fn preserved_phrase(backup: Option<&Path>) -> String {
    match backup {
        Some(path) => format!("preserved at {}", path.display()),
        None => "not preserved: the copy aside failed".to_string(),
    }
}

/// How many temp names [`create_temp`] draws before giving up. Each attempt
/// draws a fresh unpredictable name, so a natural collision is already
/// vanishingly unlikely at the first; the budget exists for a directory being
/// actively squatted, where retrying is what keeps an attacker from turning a
/// name clash into a refusal to install the deck's hooks at all. Bounded rather
/// than unbounded so a genuinely undrainable directory reports an error instead
/// of spinning.
const TEMP_NAME_ATTEMPTS: usize = 16;

/// Exclusively create a fresh temp file in `dir` for a publish of `name`,
/// redrawing the name on collision. Returns the open file and its path.
fn create_temp(dir: &Path, name: &str) -> io::Result<(std::fs::File, PathBuf)> {
    create_temp_at(std::iter::repeat_with(|| temp_path(dir, name)).take(TEMP_NAME_ATTEMPTS))
}

/// Take the first of `candidates` that does not already exist, exclusively.
///
/// Split from [`create_temp`] so the collision path can be driven by a fixed
/// list of paths in a test — a randomly drawn name cannot be made to collide on
/// purpose, and "retries instead of failing" is the half of #731 that keeps a
/// squatter from turning an unfollowable name into a refusal to install hooks.
///
/// A collision is never resolved by unlinking whatever holds the name: that
/// would let a squatter steer which entry the deck deletes, and there is no need
/// — the next candidate is a different name.
fn create_temp_at(
    candidates: impl Iterator<Item = PathBuf>,
) -> io::Result<(std::fs::File, PathBuf)> {
    let mut last = None;
    for tmp in candidates {
        match create_temp_excl(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not create a temp file for the publish",
        )
    }))
}

/// Open `tmp` with `O_CREAT|O_EXCL` (owner-only on Unix), failing rather than
/// opening anything that is already there.
///
/// This is the whole security property of #731 in one call: `create_new` maps to
/// `O_EXCL` on Unix and `CREATE_NEW` on Windows, and POSIX requires `O_EXCL` to
/// fail with `EEXIST` when the path names a symbolic link — so a pre-planted
/// symlink can never be followed, whether or not its target exists.
fn create_temp_excl(tmp: &Path) -> io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(tmp)
}

/// Draw an unpredictable same-directory temp path for a publish of `name`:
/// `.<name>.tmp.<pid>.<random>`.
///
/// `name` and the pid are kept for what they were always worth — they keep two
/// concurrent publishes in one directory apart by construction and make a
/// leftover attributable to a process — and the random tail is what an outside
/// writer cannot precompute.
///
/// The tail is a keyed hash under [`RandomState`](std::hash::RandomState)'s
/// keys — SipHash-1-3 as the standard library implements it today, though it
/// promises no particular algorithm — and those keys are seeded once per thread
/// from the OS random source. What an outside writer can see is 64 hashed bits
/// of output, never the keys, so it cannot precompute the next name; a
/// process-wide counter is mixed in as well, so no two draws in one run share an
/// input, and the retry loop covers the vanishing chance that two of them
/// nevertheless hash alike.
///
/// This deliberately does not pull in a random-number crate. `O_EXCL` above —
/// not the quality of this tail — is what makes a squatted name unfollowable, so
/// the tail carries only the weaker second-layer job of being unguessable to a
/// writer that cannot observe the keys, which a keyed hash already is.
fn temp_path(dir: &Path, name: &str) -> PathBuf {
    use std::hash::{BuildHasher as _, Hasher as _, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};

    static DRAWS: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(DRAWS.fetch_add(1, Ordering::Relaxed));
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0),
    );
    dir.join(format!(
        ".{name}.tmp.{}.{:016x}",
        std::process::id(),
        hasher.finish()
    ))
}

// ---------------------------------------------------------------------------
// Which hook commands the deck owns, and at what granularity they are removed
// ---------------------------------------------------------------------------
//
// [`crate::hooks_manage`] (Claude) worked this out first, over issues #535,
// #536, #733 and PRD #381, and then kept it to itself: the Codex and Devin
// adapters each carried a one-line `ends_with(SUFFIX)` predicate and a
// **whole-rule** `retain` on top of it. Issue #730 is the two defects that
// follow from the difference — a user's sibling handler sharing a rule object
// with a deck command is deleted along with it, and a deck rule naming a
// different but perfectly valid install is repointed on every launch.
//
// These are the parts all three adapters need, parameterized by the caller's
// own `HOOK_COMMAND_SUFFIX` so the three predicates cannot drift apart again.
// What stays per-adapter is what genuinely differs: Claude's LEGACY
// (`<path> hook`, no `--agent`) rule shape, which Codex and Devin never wrote
// and must not start recognising, and which hook events each installs.

/// Parse `command` as `<executable> <suffix>` — the shape
/// [`build_command`] produces — recovering the executable by parsing from the
/// RIGHT (`strip_suffix`), not by counting whitespace-split tokens, so a quoted
/// (or historically unquoted) executable path containing spaces still
/// round-trips. Returns `None` for a command that is not deck-owned at all, and
/// for one that is nothing *but* the suffix: `hook --agent codex` names some
/// program called `hook` on the agent's `$PATH`, which is not a command this
/// project has ever written.
///
/// The returned token may still be shell-quoted; pass it through
/// [`unquote_if_needed`] before comparing it as a path.
pub(crate) fn command_executable<'a>(command: &'a str, suffix: &str) -> Option<&'a str> {
    let exe = command.trim_end().strip_suffix(suffix)?;
    let exe = exe.strip_suffix(' ')?;
    if exe.is_empty() { None } else { Some(exe) }
}

/// Undo the quoting [`build_command`] applies: strip a single- or
/// double-quoted wrapper and unescape it back to the raw path, or return `exe`
/// unchanged if it was never quoted. Tries BOTH quoting forms regardless of
/// platform — not just the one this platform's writer produces — so a config
/// written on one platform and read on another is not stranded.
pub(crate) fn unquote_if_needed(exe: &str) -> std::borrow::Cow<'_, str> {
    if let Some(inner) = exe.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return std::borrow::Cow::Owned(inner.replace(r"'\''", "'"));
    }
    if let Some(inner) = exe.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return std::borrow::Cow::Owned(inner.replace("\\\"", "\""));
    }
    std::borrow::Cow::Borrowed(exe)
}

/// Whether two executable FILE NAMES name the same binary, judged by the host
/// platform's own conventions rather than by byte equality.
///
/// **Unix: byte equality, unchanged.** [`std::env::consts::EXE_SUFFIX`] is
/// empty, so [`strip_suffix_ignoring_ascii_case`] is a literal no-op and the
/// comparison stays exact and case-sensitive. `foo.exe` on Unix is a genuinely
/// different file name from `foo` and this must keep saying so — which is why
/// the suffix is taken from `EXE_SUFFIX` and never hardcoded as `".exe"`.
///
/// **Windows: the suffix and the case are not part of a program's identity.**
/// `dot-agent-deck` and `dot-agent-deck.exe` are the same binary — that is
/// precisely what `PATHEXT` resolution means — and the filesystem is
/// case-insensitive, so `Dot-Agent-Deck.EXE` is that same binary again.
///
/// PR #733's `build-windows` run is what proved both call sites needed it:
/// `durable_binary_path` always resolves a name carrying `EXE_SUFFIX` while
/// `DEFAULT_BINARY_NAME` never does, so comparing raw basenames could not
/// recognise a legacy Windows pin as ours to repair and issue #536 stayed open
/// on that platform.
pub(crate) fn binary_names_match(a: &str, b: &str) -> bool {
    binary_names_match_under(a, b, std::env::consts::EXE_SUFFIX, cfg!(windows))
}

/// [`binary_names_match`] with the host's two conventions injected instead of
/// read from the target: the executable suffix, and whether file names are
/// case-insensitive.
///
/// Split out **so the arithmetic is testable on any platform**, which is not a
/// stylistic preference here. PR #733's defect was Windows-only, could not be
/// reproduced on the machine that had to fix it (`aws-lc-sys` does not
/// cross-compile), and a `cfg!(windows)` branch covered by no test that runs
/// where its author works is precisely how the first one shipped green.
/// Passing `("", false)` reproduces every Unix exactly — an empty suffix makes
/// [`strip_suffix_ignoring_ascii_case`] the identity, leaving plain `==`.
pub(crate) fn binary_names_match_under(
    a: &str,
    b: &str,
    exe_suffix: &str,
    case_insensitive: bool,
) -> bool {
    let a = strip_suffix_ignoring_ascii_case(a, exe_suffix);
    let b = strip_suffix_ignoring_ascii_case(b, exe_suffix);
    if case_insensitive {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

/// `name` without one trailing `suffix`, matched case-insensitively because
/// Windows spells its executable suffix both `.exe` and `.EXE`. Returns `name`
/// untouched when `suffix` is empty (every Unix), when it is absent, and when
/// the name is nothing BUT the suffix — a file called `.exe` is a name in its
/// own right, not an empty one.
pub(crate) fn strip_suffix_ignoring_ascii_case<'a>(name: &'a str, suffix: &str) -> &'a str {
    if suffix.is_empty() {
        return name;
    }
    match name.len().checked_sub(suffix.len()) {
        // `is_char_boundary` is load-bearing, not defensive: a basename ending
        // in a multi-byte character can put `cut` inside one, and slicing
        // there panics.
        Some(cut)
            if cut > 0
                && name.is_char_boundary(cut)
                && name[cut..].eq_ignore_ascii_case(suffix) =>
        {
            &name[..cut]
        }
        _ => name,
    }
}

/// Whether `existing` and `installing` (both already unquoted) name the SAME
/// binary, so a rule for `existing` should be replaced rather than left
/// alongside a fresh rule for `installing`. Symlinks are resolved first — the
/// real-world case this exists for: a `dot-agent-deck` symlink pointing at a
/// renamed `worker-agent-deck` collapses to one rule. Every path here can fail
/// to resolve (most fixtures are never written to disk), so resolution failure
/// falls back to a literal string comparison; this never panics or unwraps on
/// it.
pub(crate) fn executables_match(existing: &str, installing: &str) -> bool {
    if let (Ok(existing_real), Ok(installing_real)) = (
        Path::new(existing).canonicalize(),
        Path::new(installing).canonicalize(),
    ) {
        return existing_real == installing_real;
    }
    existing == installing
}

/// Whether `exe` — the executable a deck-owned command names, already unquoted
/// — is a STALE SIBLING of the binary currently installing: it shares that
/// binary's own basename ([`binary_names_match`], so the host's
/// executable-suffix and case conventions decide what "same basename" means)
/// and its pin is positively known not to be usable
/// ([`crate::platform::paths::pin_is_repairable`]).
///
/// This is the "repair only when the target is **positively missing**" gate PRD
/// #381 Open Question 3 settles on, and the whole of its conservatism lives in
/// those two conjuncts. The basename half is what keeps a deck rule for a
/// genuinely *different-looking* binary out of it — most hook fixtures name
/// fictional paths that were never on disk, and they must not be swept up just
/// because they do not exist. The `pin_is_repairable` half is what keeps a
/// working binary behind an unmounted volume, or one this process cannot
/// `stat`, out of it: a stat error on a well-formed absolute pin means "leave
/// alone", because deleting a working user's hook is worse than leaving a stale
/// rule.
///
/// Callers must have established deck ownership already — pass only the
/// executable of a command [`command_executable`] (or an adapter's legacy
/// equivalent) claimed. So this never sees a command that fails the suffix
/// test, which is what keeps an ordinary user hook out of it.
///
/// **That is the narrow claim, and the wide one would be false.** Ownership
/// upstream is the *suffix*, and the suffix is a convention, not a capability:
/// a user-authored command that deliberately ends in `hook --agent codex` is
/// indistinguishable from a deck entry under it — which is the whole premise of
/// issue #730. Such a command, under this binary's own basename, with a pin the
/// OS positively reports missing, IS pruned here. The two conjuncts above are
/// what keep that case rare rather than impossible; nothing at this layer makes
/// it impossible.
pub(crate) fn pin_is_dead_sibling(exe: &str, binary_path: &str) -> bool {
    let Some(installing) = Path::new(binary_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        // No basename to compare against (an empty or `..`-terminated
        // installing path, or a non-UTF-8 one). Fail safe: prune nothing.
        return false;
    };
    Path::new(exe)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|existing| binary_names_match(existing, installing))
        && crate::platform::paths::pin_is_repairable(exe)
}

/// Every command string a rule carries, from either JSON shape: the current
/// nested `{"hooks": [{"command": ...}]}` or the legacy flat
/// `{"command": ...}`.
pub(crate) fn rule_commands(rule: &Value) -> impl Iterator<Item = &str> {
    let nested = rule
        .get("hooks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str));
    let flat = rule.get("command").and_then(Value::as_str).into_iter();
    nested.chain(flat)
}

/// Remove every command matching `is_target` from `rules`, dropping a rule
/// object only once it carries no commands at all — the fix for issue #535
/// (Claude) and, for the Codex and Devin adapters, for issue #730.
///
/// A rule's `hooks` array is a LIST of commands sharing one matcher, so a user
/// who put their own hook and the deck's in the same rule object is doing a
/// normal thing. Removal used to be a `retain` over whole rules keyed on an
/// `any()` across that list, so one deck command anywhere in a rule deleted the
/// user's commands with it — measured in #535, where a user's
/// `/usr/local/bin/my-critical-audit.sh` disappeared on `hooks uninstall` and
/// nothing said so. Install has the identical granularity and is the more
/// frequent path, since every adapter's auto-install runs unattended at
/// startup.
///
/// Two deliberate conservatisms, both in the "never delete what we did not
/// write" direction:
///
/// - a rule NOTHING matched in is returned untouched, so an already-empty or
///   command-less rule object is never tidied away as a side effect;
/// - a rule is dropped only when [`rule_commands`] reports nothing left in it,
///   which keeps a rule alive on any command the deck does not claim, in either
///   JSON shape.
///
/// Returns the number of individual commands removed.
pub(crate) fn strip_deck_commands(
    rules: &mut Vec<Value>,
    mut is_target: impl FnMut(&str) -> bool,
) -> usize {
    let mut removed = 0usize;
    rules.retain_mut(|rule| {
        let before = removed;

        // Current shape: `{"hooks": [{"command": …}, …]}` — drop just the
        // matching command objects and leave the rest of the array, and the
        // rule's own `matcher`, exactly as the user wrote them.
        if let Some(hooks) = rule.get_mut("hooks").and_then(Value::as_array_mut) {
            let len = hooks.len();
            hooks.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(&mut is_target)
            });
            removed += len - hooks.len();
        }

        // Legacy flat shape: `{"command": …}` — the command IS the rule, so
        // there is nothing smaller to remove. Take the key out and let the
        // no-commands-left check below decide the rule's fate, rather than
        // assuming it carries nothing else.
        if rule
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(&mut is_target)
        {
            if let Some(obj) = rule.as_object_mut() {
                obj.remove("command");
            }
            removed += 1;
        }

        if removed == before {
            return true;
        }
        rule_commands(rule).next().is_some()
    });
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn build_command_appends_the_agent_suffix_and_quotes_only_when_needed() {
        assert_eq!(
            build_command_for(
                "/abs/dot-agent-deck",
                "hook --agent codex",
                HookShell::Native,
                false
            ),
            "/abs/dot-agent-deck hook --agent codex"
        );
        assert_eq!(
            build_command_for(
                "/with space/dot-agent-deck",
                "hook --agent devin",
                HookShell::Posix,
                false
            ),
            "'/with space/dot-agent-deck' hook --agent devin"
        );
    }

    /// Issue #734. The command written into a Windows Codex user's `hooks.json`
    /// must be one `cmd.exe` can run — Codex hands the whole string to
    /// `%COMSPEC%`/`cmd.exe /C` there. The pre-fix output single-quoted the
    /// path, which `cmd.exe` does not implement as quoting at all: it looked
    /// for a file whose name literally began with `'`, so every deck hook
    /// silently failed.
    ///
    /// Driven through `build_command_for`'s parameter rather than `cfg!`, so
    /// this runs on the Linux box the project is developed on. The one link it
    /// does not cover is `build_command`'s own `cfg!(windows)`, which is a
    /// constant.
    #[test]
    fn build_command_for_a_windows_host_is_runnable_by_cmd_exe() {
        let path = r"C:\Users\somebody\AppData\Local\dot-agent-deck.exe";
        let command = build_command_for(path, "hook --agent codex", HookShell::Native, true);
        assert_eq!(
            command,
            format!(r"{path} hook --agent codex"),
            "an ordinary Windows path is emitted verbatim — the safe set has `\\`"
        );
        assert!(
            !command.starts_with('\''),
            "#734's defect: a single-quoted Windows path is not runnable by cmd.exe; \
             got {command}"
        );

        let spaced = r"C:\Program Files\dot-agent-deck\dot-agent-deck.exe";
        assert_eq!(
            build_command_for(spaced, "hook --agent codex", HookShell::Native, true),
            format!(r#""{spaced}" hook --agent codex"#),
            "a spaced Windows path is double-quoted, the form cmd.exe understands"
        );
    }

    /// The suffix is what both installers use to recognise their own rules
    /// (`command_is_deck_owned` is an `ends_with` on it), so it must survive
    /// the dialect change untouched — that is what makes the repair automatic
    /// for a user who already has a POSIX-quoted rule on disk: the next install
    /// still identifies it, strips it, and writes the runnable spelling.
    #[test]
    fn build_command_ends_with_the_ownership_suffix_in_either_dialect() {
        for windows_host in [true, false] {
            for shell in [HookShell::Native, HookShell::Posix] {
                for path in [
                    "/home/somebody/bin/dot-agent-deck",
                    r"C:\Program Files\deck\dot-agent-deck.exe",
                    "/with space/dot-agent-deck",
                ] {
                    for suffix in ["hook --agent codex", "hook --agent devin"] {
                        let command = build_command_for(path, suffix, shell, windows_host);
                        assert!(
                            command.ends_with(suffix),
                            "quoting must never disturb the ownership suffix; got {command}"
                        );
                    }
                }
            }
        }
    }

    /// The regression `build-windows` caught on PR #782, pinned from Linux.
    ///
    /// Devin's writer must not take its dialect from the host: `install_to` is
    /// reachable without the `devin_config_dir()` gate that confines Devin to
    /// Unix, so a host-derived choice quoted Devin's command for `cmd.exe` on a
    /// Windows runner — contradicting #734's own "byte-identical on every
    /// platform Devin can run on", which is what justified leaving that writer
    /// alone. Asserted as an equality across BOTH hosts rather than against one
    /// spelling, so it states the invariant (the host is not an input) instead
    /// of a snapshot of today's POSIX quoter.
    #[test]
    fn a_posix_writer_ignores_the_host_dialect() {
        for path in [
            "/home/somebody/bin/dot-agent-deck",
            "/Applications/My Deck/dot-agent-deck",
            r"C:\Program Files\deck\dot-agent-deck.exe",
        ] {
            assert_eq!(
                build_command_for(path, "hook --agent devin", HookShell::Posix, true),
                build_command_for(path, "hook --agent devin", HookShell::Posix, false),
                "a POSIX writer's output must not depend on the host; {path} differed"
            );
        }

        assert_eq!(
            build_command_for(
                "/Applications/My Deck/dot-agent-deck",
                "hook --agent devin",
                HookShell::Posix,
                true
            ),
            "'/Applications/My Deck/dot-agent-deck' hook --agent devin",
            "on a Windows host a POSIX writer still single-quotes"
        );

        // And the enum is not inert: the SAME path on the SAME host takes the
        // other dialect for a writer whose interpreter really does follow the
        // host. Without this, a `HookShell` that always returned POSIX would
        // satisfy everything above.
        let spaced = r"C:\Program Files\deck\dot-agent-deck.exe";
        assert_ne!(
            build_command_for(spaced, "hook --agent codex", HookShell::Native, true),
            build_command_for(spaced, "hook --agent codex", HookShell::Posix, true),
            "Native and Posix must differ on a Windows host, or the choice is doing nothing"
        );
    }

    #[test]
    fn write_atomic_replaces_the_destination_without_truncating_it() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let dest = dir.path().join("config.json");
        std::fs::write(&dest, b"old").expect("seed destination");

        write_atomic(dir.path(), &dest, b"new").expect("publish");

        assert_eq!(std::fs::read(&dest).expect("read published"), b"new");
        // The temp file is renamed away, never left beside the destination.
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list dir")
            .map(|e| e.expect("dir entry").file_name())
            .filter(|n| n != "config.json")
            .collect();
        assert!(strays.is_empty(), "temp file left behind: {strays:?}");
    }

    /// The publish must never widen the destination. `File::create` applies
    /// `0666 & !umask`, so without the mode carry-over the rename would replace
    /// a 0600 config with a 0644 (or, under a 002 umask, group-writable 0664)
    /// one the first time the deck installed its hooks.
    #[cfg(unix)]
    #[test]
    fn write_atomic_preserves_the_destination_mode_and_creates_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let mode_of = |path: &Path| {
            std::fs::metadata(path)
                .expect("stat published file")
                .permissions()
                .mode()
                & 0o777
        };

        // A file the deck creates itself is owner-only, not umask-dependent.
        let fresh = dir.path().join("fresh.json");
        write_atomic(dir.path(), &fresh, b"{}").expect("publish fresh");
        assert_eq!(mode_of(&fresh), 0o600, "a new config must be owner-only");

        // An existing file keeps exactly the mode the user chose — both a mode
        // narrower than the umask default and one wider than 0600.
        for existing_mode in [0o600, 0o644] {
            let dest = dir.path().join(format!("existing-{existing_mode:o}.json"));
            std::fs::write(&dest, b"{}").expect("seed destination");
            std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(existing_mode))
                .expect("set destination mode");

            write_atomic(dir.path(), &dest, br#"{"hooks":{}}"#).expect("publish over existing");

            assert_eq!(
                mode_of(&dest),
                existing_mode,
                "publish must reapply the destination's own mode"
            );
        }
    }

    /// The reproduction for issue #731. The temp path used to be
    /// `.<name>.tmp.<pid>` — fully derivable from the destination's file name
    /// and a pid anyone on the box can read — and it was opened with
    /// `File::create`, which follows a symlink. Anyone able to create an entry
    /// in the agent's config directory could therefore pre-plant that name as a
    /// symlink and have the deck truncate, chmod and fill a file of the
    /// attacker's choosing with the deck's bytes, while the destination itself
    /// received nothing.
    #[cfg(unix)]
    #[test]
    fn write_atomic_does_not_follow_a_symlink_planted_at_the_legacy_temp_path() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let dest = dir.path().join("config.json");
        std::fs::write(&dest, b"old").expect("seed destination");

        // The victim lives outside the config directory, exactly as a real
        // redirection would: the point of the attack is to escape it.
        let victim_dir = crate::test_temp::tempdir().expect("victim tempdir");
        let victim = victim_dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");

        let planted = dir
            .path()
            .join(format!(".config.json.tmp.{}", std::process::id()));
        std::os::unix::fs::symlink(&victim, &planted).expect("plant symlink");

        write_atomic(dir.path(), &dest, b"new").expect("publish");

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the publish followed a planted symlink and overwrote the victim"
        );
        assert_eq!(
            std::fs::read(&dest).expect("read published"),
            b"new",
            "the publish must still land the new bytes at the destination"
        );
        assert!(
            !std::fs::symlink_metadata(&dest)
                .expect("stat destination")
                .file_type()
                .is_symlink(),
            "the destination must be a real file, not the renamed symlink"
        );
    }

    /// The other half of #731, driven deterministically: even a temp name an
    /// attacker guessed outright cannot be followed, and a squatted name costs
    /// a retry rather than the whole publish. `create_temp_at` takes a fixed
    /// candidate list here because a randomly drawn name cannot be made to
    /// collide on purpose.
    #[cfg(unix)]
    #[test]
    fn create_temp_at_skips_planted_symlinks_and_lands_on_a_free_name() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let victim_dir = crate::test_temp::tempdir().expect("victim tempdir");

        // Two squatted candidates: a symlink onto a live file, and a dangling
        // one. `O_EXCL` must refuse both — POSIX fails it on a symlink whether
        // or not the target exists.
        let victim = victim_dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");
        let squatted_live = dir.path().join(".config.json.tmp.live");
        std::os::unix::fs::symlink(&victim, &squatted_live).expect("plant live symlink");

        let dangling_target = victim_dir.path().join("absent");
        let squatted_dangling = dir.path().join(".config.json.tmp.dangling");
        std::os::unix::fs::symlink(&dangling_target, &squatted_dangling)
            .expect("plant dangling symlink");

        let free = dir.path().join(".config.json.tmp.free");
        let candidates = vec![
            squatted_live.clone(),
            squatted_dangling.clone(),
            free.clone(),
        ];

        let (file, landed) = create_temp_at(candidates.into_iter()).expect("create temp");
        drop(file);

        assert_eq!(
            landed, free,
            "must skip both squatters and take the free name"
        );
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the exclusive create followed a planted symlink"
        );
        assert!(
            !dangling_target.exists(),
            "the exclusive create created the dangling symlink's target"
        );
        // The squatters are left exactly as they were — never unlinked, so a
        // squatter cannot steer what the deck deletes.
        for squatted in [&squatted_live, &squatted_dangling] {
            assert!(
                std::fs::symlink_metadata(squatted)
                    .expect("stat squatted candidate")
                    .file_type()
                    .is_symlink(),
                "{} must be left untouched",
                squatted.display()
            );
        }
    }

    /// Exhausting every candidate is an error, not a silent write somewhere
    /// else, and it does not disturb what holds the names.
    #[test]
    fn create_temp_at_reports_alreadyexists_when_every_candidate_is_taken() {
        let dir = crate::test_temp::tempdir().expect("publish tempdir");
        let taken: Vec<_> = ["a", "b"]
            .iter()
            .map(|n| {
                let path = dir.path().join(format!(".config.json.tmp.{n}"));
                std::fs::write(&path, b"squatter").expect("seed squatter");
                path
            })
            .collect();

        let err = create_temp_at(taken.clone().into_iter()).expect_err("must refuse");

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        for path in &taken {
            assert_eq!(
                std::fs::read(path).expect("read squatter"),
                b"squatter",
                "a refused create must not have touched {}",
                path.display()
            );
        }
    }

    /// The name must no longer be derivable from the destination and the pid,
    /// which together were the whole of the old `.<name>.tmp.<pid>`.
    #[test]
    fn temp_path_is_unpredictable_and_never_the_legacy_shape() {
        let dir = Path::new("/agent/config");
        let legacy = dir.join(format!(".config.json.tmp.{}", std::process::id()));

        let draws: std::collections::HashSet<PathBuf> =
            (0..64).map(|_| temp_path(dir, "config.json")).collect();

        assert_eq!(draws.len(), 64, "two draws collided: {draws:?}");
        for drawn in &draws {
            assert_ne!(drawn, &legacy, "the legacy predictable name came back");
            assert_eq!(drawn.parent(), Some(dir), "the temp must stay beside dest");
            let name = drawn
                .file_name()
                .and_then(|n| n.to_str())
                .expect("temp file name");
            assert!(
                name.starts_with(&format!(".config.json.tmp.{}.", std::process::id())),
                "unexpected temp name shape: {name}"
            );
        }
    }

    /// The backup half of #731, on the shared helper.
    ///
    /// `<name>.bak` is as predictable as the old temp name was, and the
    /// `std::fs::write` all three adapters used follows a symlink — so a writer
    /// able to add an entry to the agent's config directory could point that
    /// name at any file it could write and have the deck fill it with the
    /// malformed config's bytes. The publish must replace the link instead.
    #[cfg(unix)]
    #[test]
    fn backup_malformed_does_not_follow_a_symlink_planted_at_the_backup_path() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");

        let planted = dir.path().join("config.json.bak");
        std::os::unix::fs::symlink(&victim, &planted).expect("plant symlink");

        let backup = backup_malformed(&dest, b"{ not json").expect("the bytes must be preserved");

        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the copy followed the planted symlink and overwrote the victim"
        );
        assert_eq!(backup, planted, "the backup keeps its conventional name");
        assert!(
            !std::fs::symlink_metadata(&backup)
                .expect("stat backup")
                .file_type()
                .is_symlink(),
            "the backup must be a real file, not the planted symlink"
        );
        assert_eq!(std::fs::read(&backup).expect("read backup"), b"{ not json");
    }

    /// The plain path, and the control for the test above: with nothing planted
    /// the bytes land at `<name>.bak`, a later copy replaces that same file
    /// rather than accumulating beside it, and no temp is left behind.
    ///
    /// Replacing matters more than it looks: `hooks_manage::auto_install` runs on
    /// every deck start, so a config that stays malformed reaches this on every
    /// launch. A collision-safe *new* name each time — the issue's other
    /// sanctioned shape — would grow one file per launch in the user's config
    /// directory.
    #[test]
    fn backup_malformed_replaces_a_previous_backup_without_accumulating() {
        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("settings.json");
        std::fs::write(&dest, b"first malformed").expect("seed destination");

        let first = backup_malformed(&dest, b"first malformed").expect("first backup");
        assert_eq!(first, dir.path().join("settings.json.bak"));

        let second = backup_malformed(&dest, b"second malformed").expect("second backup");
        assert_eq!(second, first, "the backup name is stable across copies");
        assert_eq!(
            std::fs::read(&second).expect("read backup"),
            b"second malformed"
        );

        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list dir")
            .map(|e| e.expect("dir entry").file_name())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                std::ffi::OsString::from("settings.json"),
                std::ffi::OsString::from("settings.json.bak")
            ],
            "the copy left a stray temp or a second backup behind"
        );
    }

    /// A planted symlink must not get to CHOOSE the backup's mode either.
    ///
    /// Greptile's P1 on PR #855, and a real second mouth of the same trap: the
    /// publish is safe because `rename` replaces the link rather than following
    /// it, but the mode it lands came from `std::fs::metadata(dest)` — `stat(2)`,
    /// which DOES follow — so a link pointing at a world-readable file published
    /// the user's config bytes 0666. Not following a symlink for the write and
    /// then taking the symlink target's permissions for it is worse than either
    /// half sounds.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_backup_path_cannot_choose_the_backups_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");

        // The attacker's target, deliberately wide open.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"victim bytes").expect("seed victim");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o666))
            .expect("widen victim");
        std::os::unix::fs::symlink(&victim, dir.path().join("config.json.bak"))
            .expect("plant symlink");

        let backup = backup_malformed(&dest, b"{ not json").expect("backup");

        assert_eq!(
            std::fs::metadata(&backup)
                .expect("stat backup")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "the planted link's target supplied the published backup's mode"
        );
        assert_eq!(
            std::fs::read(&victim).expect("read victim"),
            b"victim bytes",
            "the victim must still be untouched"
        );
    }

    /// A backup is a byte-for-byte copy of the config, so it must not be readable
    /// by accounts the config was not. `std::fs::write` created it at
    /// `0666 & !umask` — 0644 typically — beside a Devin config that ships 0600
    /// and holds `devin.org_id` (#360, #382).
    #[cfg(unix)]
    #[test]
    fn backup_malformed_creates_an_owner_only_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("backup tempdir");
        let dest = dir.path().join("config.json");
        let backup = backup_malformed(&dest, b"{ not json").expect("backup");

        assert_eq!(
            std::fs::metadata(&backup)
                .expect("stat backup")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a fresh backup must be owner-only"
        );
    }

    // ---------------------------------------------------------------------
    // The shared ownership predicates (issue #730).
    //
    // These gate a privilege GRANT for Codex (`trust_deck_hooks_in` builds on
    // `command_executable`) and DELETION for all four adapters, so they are
    // tested here directly rather than only through whichever adapter happens
    // to exercise them. A regression then localises to this module instead of
    // surfacing as a puzzling failure in one adapter's suite.
    // ---------------------------------------------------------------------

    const CODEX: &str = "hook --agent codex";
    const DEVIN: &str = "hook --agent devin";
    const CLAUDE: &str = "hook --agent claude";

    /// The suffix is a PARAMETER, and it is the whole of ownership at this
    /// layer. One adapter must not claim another's command — that is what keeps
    /// `hooks uninstall --agent codex` from deleting the Claude adapter's rules
    /// out of a file both happen to write.
    #[test]
    fn command_executable_claims_only_its_own_agents_suffix() {
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent codex", CODEX),
            Some("/abs/dot-agent-deck")
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent codex", CLAUDE),
            None,
            "a Claude suffix must not claim a Codex command"
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent claude", CODEX),
            None,
            "a Codex suffix must not claim a Claude command"
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent devin", DEVIN),
            Some("/abs/dot-agent-deck")
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent devin", CODEX),
            None,
            "`--agent devin` and `--agent codex` are different agents"
        );
    }

    /// A command that is NOTHING BUT the suffix names some program called
    /// `hook` on the agent's own `$PATH`. This project has never written one,
    /// so claiming it would be claiming a stranger's command — and at the Codex
    /// trust seam, handing it a grant.
    #[test]
    fn command_executable_rejects_a_command_that_is_only_the_suffix() {
        assert_eq!(command_executable("hook --agent codex", CODEX), None);
        assert_eq!(command_executable(" hook --agent codex", CODEX), None);
        assert_eq!(command_executable("hook --agent codex   ", CODEX), None);
    }

    /// Trailing whitespace is trimmed before the suffix test, so a config
    /// hand-edited into `"… hook --agent codex "` is still recognised as the
    /// deck's own on the way back in. Note the deliberate asymmetry with the
    /// Codex TRUST predicate, which compares byte-exactly and does NOT trim:
    /// Codex 0.149.0 echoes a trailing space verbatim and hashes the trimmed
    /// and untrimmed forms differently, so an entry differing only by
    /// whitespace is a genuinely different definition, not a spelling of ours.
    /// Liberal for a mutation, exact for a grant, on purpose (issue #730).
    #[test]
    fn command_executable_trims_trailing_whitespace_but_not_leading() {
        assert_eq!(
            command_executable("/abs/dot-agent-deck hook --agent codex  \t\n", CODEX),
            Some("/abs/dot-agent-deck")
        );
        assert_eq!(
            command_executable("  /abs/dot-agent-deck hook --agent codex", CODEX),
            Some("  /abs/dot-agent-deck"),
            "leading whitespace belongs to the executable token and is left for \
             `unquote_if_needed` and the path comparison to deal with"
        );
        assert_eq!(
            command_executable("/abs/dot-agent-deckhook --agent codex", CODEX),
            None,
            "the space before the suffix is required — no substring match"
        );
    }

    /// Both JSON shapes an agent config can carry, so `strip_deck_commands`
    /// sees every command a rule actually holds.
    #[test]
    fn rule_commands_reads_the_nested_and_the_legacy_flat_shape() {
        let nested = json!({
            "matcher": "Bash",
            "hooks": [ { "command": "a" }, { "command": "b" }, { "type": "command" } ]
        });
        assert_eq!(rule_commands(&nested).collect::<Vec<_>>(), vec!["a", "b"]);

        let flat = json!({ "command": "c" });
        assert_eq!(rule_commands(&flat).collect::<Vec<_>>(), vec!["c"]);

        let both = json!({ "command": "c", "hooks": [ { "command": "a" } ] });
        assert_eq!(rule_commands(&both).collect::<Vec<_>>(), vec!["a", "c"]);

        assert_eq!(rule_commands(&json!({})).count(), 0);
        assert_eq!(rule_commands(&json!("not an object")).count(), 0);
    }

    /// Issue #535/#730: a rule is a list of commands sharing one matcher, so
    /// removal is per COMMAND. The user's sibling handler survives, and so does
    /// the matcher they wrote it under.
    #[test]
    fn strip_deck_commands_keeps_a_sibling_handler_and_its_matcher() {
        let mut rules = vec![json!({
            "matcher": "Bash",
            "hooks": [
                { "type": "command", "command": "/abs/dot-agent-deck hook --agent codex" },
                { "type": "command", "command": "/usr/local/bin/my-critical-audit.sh" }
            ]
        })];

        let removed =
            strip_deck_commands(&mut rules, |cmd| command_executable(cmd, CODEX).is_some());

        assert_eq!(removed, 1);
        assert_eq!(rules.len(), 1, "the rule object must survive: {rules:?}");
        assert_eq!(rules[0]["matcher"], json!("Bash"));
        assert_eq!(
            rule_commands(&rules[0]).collect::<Vec<_>>(),
            vec!["/usr/local/bin/my-critical-audit.sh"]
        );
    }

    /// A rule is dropped only once NOTHING is left in it, in either shape — and
    /// a rule the predicate matched nothing in is returned untouched, so an
    /// already-empty or command-less rule object is never tidied away as a side
    /// effect of installing.
    #[test]
    fn strip_deck_commands_drops_a_rule_only_when_no_command_is_left() {
        let mut rules = vec![
            json!({ "hooks": [ { "command": "/abs/dot-agent-deck hook --agent codex" } ] }),
            json!({ "matcher": "Bash", "hooks": [] }),
            json!({}),
        ];

        let removed =
            strip_deck_commands(&mut rules, |cmd| command_executable(cmd, CODEX).is_some());

        assert_eq!(removed, 1);
        assert_eq!(
            rules,
            vec![json!({ "matcher": "Bash", "hooks": [] }), json!({})],
            "only the emptied rule goes; untouched rules stay as the user wrote them"
        );
    }

    /// The legacy FLAT shape: the command IS the rule, so there is nothing
    /// smaller to remove — but the key is taken out and the rule kept whenever
    /// it still carries a command in the other shape, rather than the whole
    /// object being assumed to hold nothing else.
    #[test]
    fn strip_deck_commands_handles_the_legacy_flat_shape() {
        let mut lone = vec![json!({ "command": "/abs/dot-agent-deck hook --agent codex" })];
        assert_eq!(
            strip_deck_commands(&mut lone, |cmd| command_executable(cmd, CODEX).is_some()),
            1
        );
        assert!(
            lone.is_empty(),
            "a flat rule with nothing left goes: {lone:?}"
        );

        let mut mixed = vec![json!({
            "command": "/abs/dot-agent-deck hook --agent codex",
            "hooks": [ { "command": "/usr/local/bin/my-critical-audit.sh" } ]
        })];
        assert_eq!(
            strip_deck_commands(&mut mixed, |cmd| command_executable(cmd, CODEX).is_some()),
            1
        );
        assert_eq!(mixed.len(), 1, "a command survives in the nested shape");
        assert!(mixed[0].get("command").is_none(), "{mixed:?}");
        assert_eq!(
            rule_commands(&mixed[0]).collect::<Vec<_>>(),
            vec!["/usr/local/bin/my-critical-audit.sh"]
        );
    }

    /// Two spellings of one binary are the same binary — the real case being a
    /// `dot-agent-deck` symlink pointing at a renamed build, which must collapse
    /// to one rule rather than accumulate a second every launch.
    #[cfg(unix)]
    #[test]
    fn executables_match_resolves_symlinks_and_falls_back_to_string_equality() {
        let dir = crate::test_temp::tempdir().expect("exe tempdir");
        let real = dir.path().join("worker-agent-deck");
        std::fs::write(&real, b"#!/bin/sh\nexit 0\n").expect("seed binary");
        let link = dir.path().join("dot-agent-deck");
        std::os::unix::fs::symlink(&real, &link).expect("plant symlink");

        assert!(executables_match(
            link.to_str().expect("utf-8"),
            real.to_str().expect("utf-8")
        ));
        assert!(
            executables_match("/nowhere/dot-agent-deck", "/nowhere/dot-agent-deck"),
            "neither path resolves, so the comparison falls back to the strings"
        );
        assert!(!executables_match(
            "/nowhere/dot-agent-deck",
            "/elsewhere/dot-agent-deck"
        ));
    }

    /// Fail-safe branch 1: the INSTALLING path has no basename to compare
    /// against (empty, `..`-terminated, or non-UTF-8). Prune nothing.
    #[test]
    fn pin_is_dead_sibling_prunes_nothing_without_a_basename_to_compare() {
        assert!(!pin_is_dead_sibling("/nowhere/dot-agent-deck", ""));
        assert!(!pin_is_dead_sibling("/nowhere/dot-agent-deck", "/opt/.."));
        assert!(
            !pin_is_dead_sibling("/opt/..", "/abs/dot-agent-deck"),
            "a pin with no basename is not a sibling of anything either"
        );
    }

    /// Fail-safe branch 2 (the basename half): a deck pin naming a
    /// DIFFERENT-looking binary is left alone however absent it is. Most hook
    /// fixtures name fictional paths that were never on disk, and they must not
    /// be swept up just for not existing.
    #[test]
    fn pin_is_dead_sibling_needs_the_installing_binarys_own_basename() {
        assert!(!pin_is_dead_sibling(
            "/nowhere/some-other-tool",
            "/abs/dot-agent-deck"
        ));
    }

    /// Branch 3, the only one that prunes: positively reported missing by the
    /// OS, under this binary's own basename — the pruned-worktree residue PRD
    /// #381's repair gate exists for.
    #[test]
    fn pin_is_dead_sibling_repairs_a_positively_absent_sibling() {
        let dir = crate::test_temp::tempdir().expect("pin tempdir");
        let gone = dir.path().join("pruned").join("dot-agent-deck");
        assert!(!gone.exists(), "the dead path must genuinely not exist");
        assert!(pin_is_dead_sibling(
            gone.to_str().expect("utf-8"),
            "/abs/dot-agent-deck"
        ));
    }

    /// Fail-safe branch 4: a pin this process cannot STAT — permission denied,
    /// an unmounted or stale mount — is well-formed and might well be a working
    /// binary, so it is left alone. Deleting a working user's hook is worse than
    /// leaving a stale rule (PRD #381 Open Question 3).
    #[cfg(unix)]
    #[test]
    fn pin_is_dead_sibling_leaves_a_pin_it_cannot_stat_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = crate::test_temp::tempdir().expect("pin tempdir");
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).expect("create locked dir");
        let hidden = locked.join("dot-agent-deck");
        let hidden = hidden.to_str().expect("utf-8").to_string();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("close the directory");

        let unstatable = Path::new(&hidden).try_exists().is_err();
        let verdict = pin_is_dead_sibling(&hidden, "/abs/dot-agent-deck");

        // Reopen before asserting, so a failure does not also leave the
        // tempdir undeletable.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700))
            .expect("reopen the directory");

        if !unstatable {
            println!(
                "SKIP: this user can stat through a 0o000 directory (root?), so the \
                 unstatable-pin branch is unreachable here"
            );
            return;
        }
        assert!(
            !verdict,
            "a pin we cannot stat must be left alone, not repaired away"
        );
    }

    /// The message a user reads must not name a file that was never written.
    /// The `let _ = std::fs::write(…)` this replaced always claimed one.
    #[test]
    fn preserved_phrase_names_a_backup_only_when_there_is_one() {
        assert_eq!(
            preserved_phrase(Some(Path::new("/agent/config/hooks.json.bak"))),
            "preserved at /agent/config/hooks.json.bak"
        );
        let none = preserved_phrase(None);
        assert!(
            !none.contains(".bak"),
            "a failed copy must not name a backup path: {none}"
        );
        assert!(none.contains("not preserved"), "{none}");
    }
}
