//! Filesystem security: owner-only dirs/files, the socket-bind umask dance,
//! and endpoint trust verification (PRD #42 M1, lifted from `daemon.rs`,
//! `daemon_attach.rs`, `remote.rs`, `schedule_cli.rs`; real Windows behavior in
//! PRD #163 M4).
//!
//! Unix is the uniform mode-bit model (`umask`/0o700/0o600/`is_socket`+uid
//! checks). Windows splits this into a pipe security descriptor + explicit
//! protected DACLs on the state/config dirs and the secret-bearing files +
//! server-SID verification on every client connect.
//!
//! Each `cfg(unix)` permission site was audited individually rather than
//! blanket-`cfg`'d away. The audit is not just prose: [`SITE_AUDIT`] is the
//! machine-readable table, one row per exported function, and
//! [`tests::every_permission_site_has_a_windows_counterpart_or_a_justification`]
//! fails the build if any row claims a Windows no-op without saying why.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

// NOTE: adding an entry here means adding a row to [`SITE_AUDIT`] below — the
// PRD #163 requirement is that no permission site silently loses a security
// property on Windows.
#[cfg(unix)]
pub use unix::{
    create_owner_only_dir, ensure_owner_only_dir, set_create_mode_owner_only,
    set_endpoint_mode_owner_only, set_file_owner_only, verify_endpoint_trusted, with_socket_umask,
};
// The Windows list carries three extras with no Unix counterpart to export —
// `pipe_security_descriptor` / `OwnedSecurityDescriptor` /
// `verify_object_owner_is_current_user`, which together close PRD #163's
// [BLOCKER]. On Unix those same two properties *are* the socket's 0o600 mode and
// `verify_endpoint_trusted`'s `stat`, so there is nothing to name.
#[cfg(windows)]
pub use windows::{
    OwnedSecurityDescriptor, create_owner_only_dir, ensure_owner_only_dir,
    pipe_security_descriptor, set_create_mode_owner_only, set_endpoint_mode_owner_only,
    set_file_owner_only, verify_endpoint_trusted, verify_object_owner_is_current_user,
    with_socket_umask,
};

/// Decide whether an endpoint owned by `owner_sid` is trusted, given that we are
/// `our_sid` (PRD #163 M4). Both are canonical SID strings (`S-1-5-21-…`).
///
/// This is the pure-data core of the Windows [BLOCKER] check — the analogue of
/// Unix's `metadata.uid() != our_uid` comparison — split out so its fail-closed
/// rules are unit-testable on every platform, including the Linux CI job where
/// the `#[cfg(windows)]` caller does not exist.
///
/// Fails closed on anything it cannot affirmatively vouch for: an empty owner, an
/// empty "us", or any mismatch. Comparison is ASCII-case-insensitive purely as
/// belt and braces — `ConvertSidToStringSidW` renders SIDs uppercase and both
/// sides come from it, so the two can only differ by *value*.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn endpoint_owner_is_trusted(owner_sid: &str, our_sid: &str) -> Result<(), String> {
    if our_sid.is_empty() {
        return Err("the current user's SID is empty".to_string());
    }
    if owner_sid.is_empty() {
        return Err("the endpoint has no owner SID".to_string());
    }
    if !owner_sid.eq_ignore_ascii_case(our_sid) {
        return Err(format!(
            "owned by SID {owner_sid} (expected {our_sid}) — another user may have created this \
             endpoint"
        ));
    }
    Ok(())
}

/// How a permission site upholds its security property on Windows.
///
/// The audit below is documentation the compiler can hold us to rather than
/// prose that drifts, so it is only *read* by its test — hence the allow.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowsCounterpart {
    /// A real Win32 mechanism enforces the same property.
    Enforced(&'static str),
    /// Deliberately does nothing on Windows. The payload must say *why* the
    /// property is not lost — see the audit test.
    JustifiedNoOp(&'static str),
}

/// One `cfg(unix)` permission site and how each platform upholds it.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct PermissionSite {
    /// The exported seam function.
    pub(crate) function: &'static str,
    /// The Unix mechanism it lifts.
    pub(crate) unix: &'static str,
    /// The Windows side of the story.
    pub(crate) windows: WindowsCounterpart,
}

/// **The PRD #163 per-site audit.** "Each `cfg(unix)` permission site must get a
/// Windows counterpart or a justified no-op — easy to under-implement and silently
/// drop a security property" (Edge Cases). One row per exported function; the test
/// below is the guard.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const SITE_AUDIT: &[PermissionSite] = &[
    PermissionSite {
        function: "with_socket_umask",
        unix: "umask(0o177) around bind(2) so the socket inode is created at 0o600 with no \
               world-readable window",
        windows: WindowsCounterpart::JustifiedNoOp(
            "there is no window to close: the pipe's owner-only security descriptor is an \
             argument to CreateNamedPipeW, so the object never exists with any other DACL. \
             Strictly stronger than the umask dance, and there is no umask on Windows.",
        ),
    },
    PermissionSite {
        function: "ensure_owner_only_dir",
        unix: "DirBuilder::mode(0o700) plus an unconditional set_permissions(0o700) that repairs \
               a pre-existing loose directory",
        windows: WindowsCounterpart::Enforced(
            "create_dir_all plus an unconditional SetNamedSecurityInfoW protected \
             current-user-only DACL — the same repair-an-existing-dir semantics",
        ),
    },
    PermissionSite {
        function: "create_owner_only_dir",
        unix: "DirBuilder::mode(0o700) only — an existing directory keeps its mode (PRD #127 S2)",
        windows: WindowsCounterpart::Enforced(
            "the same protected current-user-only DACL, applied only when we created the \
             directory, so an existing shared directory is never surprise-tightened",
        ),
    },
    PermissionSite {
        function: "set_create_mode_owner_only",
        unix: "OpenOptionsExt::mode(0o600) so the file is never created group/other-readable",
        windows: WindowsCounterpart::JustifiedNoOp(
            "std::fs::OpenOptions has no SECURITY_ATTRIBUTES hook, so create-time enforcement is \
             not expressible. The property is not lost: every caller applies \
             set_file_owner_only to the fresh handle immediately after open, before the first \
             content byte, so only an empty file can exist under a loose inherited ACL.",
        ),
    },
    PermissionSite {
        function: "set_file_owner_only",
        unix: "fchmod(0o600) on the open handle, re-asserted before the atomic rename",
        windows: WindowsCounterpart::Enforced(
            "handle-based SetSecurityInfo with a protected current-user-only DACL — no second \
             name resolution for an attacker to redirect",
        ),
    },
    PermissionSite {
        function: "set_endpoint_mode_owner_only",
        unix: "post-bind set_permissions(0o600) on the socket inode, defense in depth behind the \
               umask dance",
        windows: WindowsCounterpart::JustifiedNoOp(
            "a named pipe has no inode mode to restate after the fact; the owner-only property \
             is established at creation by the pipe security descriptor.",
        ),
    },
    PermissionSite {
        function: "verify_endpoint_trusted",
        unix: "stat: is a socket, owned by our uid, mode 0o600 — out-of-band, because the \
               connect that follows is unguarded",
        windows: WindowsCounterpart::Enforced(
            "GetSecurityInfo on the pipe handle compares the server's owner SID with ours. Also \
             welded into BOTH client entry points (IpcStream::connect and IpcClient::connect), \
             so on Windows every connection is verified whether or not a caller asks",
        ),
    },
];

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const SID: &str = "S-1-5-21-3623811015-3361044348-30300820-1013";
    const OTHER_SID: &str = "S-1-5-21-3623811015-3361044348-30300820-1014";

    /// PRD #163 Edge Cases, mechanized: every permission site either has a real
    /// Windows mechanism or says in writing why doing nothing loses no security
    /// property. A future `JustifiedNoOp("")` — the shape a silently-dropped
    /// property takes — fails here.
    ///
    /// The typed bindings pin the table to the real exports: renaming or deleting
    /// a seam function stops this test *compiling*, which is the only way a table
    /// of strings can stay honest.
    #[test]
    fn every_permission_site_has_a_windows_counterpart_or_a_justification() {
        let _: fn(&Path) -> std::io::Result<()> = ensure_owner_only_dir;
        let _: fn(&Path) -> std::io::Result<()> = create_owner_only_dir;
        let _: fn(&mut std::fs::OpenOptions) = set_create_mode_owner_only;
        let _: fn(&std::fs::File) -> std::io::Result<()> = set_file_owner_only;
        let _: fn(&Path) -> std::io::Result<()> = set_endpoint_mode_owner_only;
        let _: fn(&Path) -> Result<(), String> = verify_endpoint_trusted;
        assert_eq!(with_socket_umask(|| 7), 7);

        assert_eq!(
            SITE_AUDIT.len(),
            7,
            "every exported fsperm seam function needs exactly one audit row"
        );
        for site in SITE_AUDIT {
            assert!(
                !site.unix.is_empty(),
                "{}: no Unix mechanism",
                site.function
            );
            match site.windows {
                WindowsCounterpart::Enforced(how) => assert!(
                    !how.is_empty(),
                    "{}: claims enforcement but names no mechanism",
                    site.function
                ),
                WindowsCounterpart::JustifiedNoOp(why) => assert!(
                    why.len() > 40,
                    "{}: a Windows no-op needs a real justification, not a shrug",
                    site.function
                ),
            }
        }

        let mut names: Vec<_> = SITE_AUDIT.iter().map(|s| s.function).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SITE_AUDIT.len(), "duplicate audit rows");
    }

    /// The exact sites the PRD calls out for the config-secret ACLs
    /// (`remotes.toml` / `schedules.toml` / `session.toml`) must be *enforced* on
    /// Windows, not no-ops — that item is release-gating, so "documented as
    /// missing" is not an acceptable resolution for it.
    #[test]
    fn the_config_secret_write_path_is_enforced_on_windows() {
        let row = |name: &str| {
            SITE_AUDIT
                .iter()
                .find(|s| s.function == name)
                .unwrap_or_else(|| panic!("no audit row for {name}"))
        };
        for name in [
            "set_file_owner_only",
            "ensure_owner_only_dir",
            "create_owner_only_dir",
            "verify_endpoint_trusted",
        ] {
            assert!(
                matches!(row(name).windows, WindowsCounterpart::Enforced(_)),
                "{name} must have a real Windows mechanism"
            );
        }
    }

    /// The pure-data core of the Windows [BLOCKER]: the *only* accepted case is
    /// "the endpoint's owner SID is exactly ours". This is the foreign-user
    /// denial, simulated at the level where it is decidable on any host.
    #[test]
    fn endpoint_owner_trust_accepts_only_our_own_sid() {
        endpoint_owner_is_trusted(SID, SID).expect("our own SID must be trusted");
        // Rendering case can't differ in practice, but must not be the thing that
        // decides a security question either way.
        endpoint_owner_is_trusted(&SID.to_lowercase(), SID).expect("case must not matter");
    }

    /// …and everything else is refused, with the foreign SID named in the reason
    /// so the operator can see who squatted the endpoint.
    #[test]
    fn endpoint_owner_trust_refuses_a_foreign_or_missing_owner() {
        let err = endpoint_owner_is_trusted(OTHER_SID, SID).expect_err("a foreign SID must fail");
        assert!(err.contains(OTHER_SID), "{err}");
        assert!(err.contains(SID), "{err}");

        // A one-digit difference is still a different user.
        assert!(endpoint_owner_is_trusted("S-1-5-18", SID).is_err());
        // Fail closed rather than treating "unknown" as "fine".
        assert!(endpoint_owner_is_trusted("", SID).is_err());
        assert!(endpoint_owner_is_trusted(SID, "").is_err());
        assert!(endpoint_owner_is_trusted("", "").is_err());
    }
}
