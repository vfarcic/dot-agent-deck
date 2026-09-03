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
    /// Windows, and to `$SHELL` else `/bin/sh` with `-lc` otherwise. The deck
    /// writes no per-entry `shell` override, so that default is what runs every
    /// deck hook and the interpreter really does follow the host — and
    /// `codex_home` honours `$CODEX_HOME` on every platform, which is what
    /// makes the Windows arm reachable rather than theoretical.
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
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");

    let (mut file, tmp) = create_temp(dir, name)?;

    // Everything after the create is fallible with a temp file already on disk,
    // so it runs in one closure and shares a single cleanup path. The previous
    // shape leaked the temp file whenever `write_all` or `sync_all` failed — it
    // only removed it when the `rename` did.
    let written = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dest)
                .map(|meta| meta.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
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
/// It carries [`write_atomic`]'s mode rule along with its safety: the backup
/// lands at its own current mode, or 0600 when it is new, rather than
/// `File::create`'s `0666 & !umask` — so a copy of a 0600 config holding an
/// org id or an auth reference is no longer published 0644 beside it (#360,
/// #382).
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
    write_atomic(dir, &backup, bytes).ok().map(|()| backup)
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

#[cfg(test)]
mod tests {
    use super::*;

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
