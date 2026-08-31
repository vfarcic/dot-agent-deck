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

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

/// Build the deck's hook command for `binary_path`, robustly quoting the
/// executable path so a path containing whitespace or shell metacharacters still
/// produces a valid command that the agent parses to the intended argv. A "safe"
/// path (only path-typical characters) is emitted verbatim so the common case
/// stays human-readable and stable; anything else is single-quoted with embedded
/// single quotes escaped.
///
/// `suffix` is the caller's `HOOK_COMMAND_SUFFIX` — the fixed
/// `hook --agent <agent>` signature that also identifies the resulting command
/// as deck-owned on the way back in, so the two must stay the same string.
pub(crate) fn build_command(binary_path: &str, suffix: &str) -> String {
    format!(
        "{} {suffix}",
        crate::platform::paths::shell_quote_if_needed(binary_path)
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
            build_command("/abs/dot-agent-deck", "hook --agent codex"),
            "/abs/dot-agent-deck hook --agent codex"
        );
        assert_eq!(
            build_command("/with space/dot-agent-deck", "hook --agent devin"),
            "'/with space/dot-agent-deck' hook --agent devin"
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
}
