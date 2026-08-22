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
//! (deriving the directory), and it publishes through `create_new` so a leftover
//! temp file cannot be a symlink the write follows out of the directory (#534).
//! Folding it in here would either drop that hardening or push it onto two
//! adapters that have not been reviewed for it, so it stays where it is.

use std::io::{self, Write as _};
use std::path::Path;

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
/// The temp name is derived from `dest`'s file name (`.<name>.tmp.<pid>`), so
/// one adapter's `hooks.json` and `config.toml` publishes never race on a single
/// temp path. (The `"config"` fallback is only reachable for a `dest` with no
/// UTF-8 file name, where the `rename` below cannot succeed either; the two
/// adapters spelled that unreachable literal differently before this was
/// extracted.)
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
pub(crate) fn write_atomic(dir: &Path, dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dest)
                .map(|meta| meta.permissions().mode() & 0o777)
                .unwrap_or(0o600);
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    match std::fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
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
}
