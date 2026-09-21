//! The sandbox: where a run puts its files, what environment every process in
//! it shares, and the preflight that refuses to start a run that would measure
//! nothing or damage something.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One run's directory tree. Disk-backed by construction — see
/// [`filesystem_of`] and CLAUDE.md rule 14 for why that is checked rather than
/// assumed.
pub struct Sandbox {
    pub root: PathBuf,
    /// The deck's `HOME`.
    pub home: PathBuf,
    /// The directory both decks are launched in, holding the
    /// `.dot-agent-deck.toml` that defines the orchestration.
    pub project: PathBuf,
    pub state: PathBuf,
    pub locks: PathBuf,
    /// `TMPDIR` for every process in the run. Also where a post-#1121 build
    /// puts its endpoint fallback directory, which is what keeps that arm out
    /// of the real `/tmp`.
    pub tmp: PathBuf,
    /// `DOT_AGENT_DECK_LOG`. Resolved separately from [`state`](Self::state):
    /// without it an otherwise-isolated daemon appends into the operator's real
    /// `~/.local/state/dot-agent-deck/deck.log`.
    pub log: PathBuf,
    pub session: PathBuf,
    pub schedules: PathBuf,
    /// Where PTY streams, daemon stdio and the evidence file's raw excerpts go.
    pub artifacts: PathBuf,
}

impl Sandbox {
    pub fn create(root: PathBuf) -> Result<Self, String> {
        let sb = Self {
            home: root.join("home"),
            project: root.join("project"),
            state: root.join("state"),
            locks: root.join("locks"),
            tmp: root.join("tmp"),
            log: root.join("deck.log"),
            session: root.join("session.toml"),
            schedules: root.join("schedules.toml"),
            artifacts: root.join("artifacts"),
            root,
        };
        for dir in [
            &sb.root,
            &sb.home,
            &sb.project,
            &sb.state,
            &sb.locks,
            &sb.tmp,
            &sb.artifacts,
        ] {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        Ok(sb)
    }

    pub fn hook_socket(&self) -> PathBuf {
        self.root.join("hook.sock")
    }

    pub fn attach_socket(&self) -> PathBuf {
        self.root.join("attach.sock")
    }
}

/// How the run addresses the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointMode {
    /// `DOT_AGENT_DECK_SOCKET` / `DOT_AGENT_DECK_ATTACH_SOCKET` point inside the
    /// sandbox. The default, and the right one for any change that does not
    /// touch endpoint resolution: it is the strongest isolation available and
    /// two runs in this mode cannot collide.
    SandboxSockets,
    /// Neither override is set, so both builds resolve their endpoint the way
    /// they would on a real host.
    ///
    /// Needed when the change under test *is* endpoint resolution — those
    /// overrides short-circuit it, so a run that sets them exercises the one arm
    /// such a PR did not touch and measures nothing. The cost is that the
    /// addresses are then process-global: see
    /// [`preflight_resolved_endpoints`].
    Resolved,
}

/// The environment every process in a run shares — the daemon, both TUIs and
/// every CLI call.
///
/// Built as a complete map rather than as a set of overlays on this process's
/// own environment, because two of the things a run has to pin are *absences*
/// (`XDG_RUNTIME_DIR`, and in [`EndpointMode::Resolved`] the two socket
/// overrides), and an absence cannot be expressed by adding a variable.
pub fn run_env(
    sandbox: &Sandbox,
    new_binary_dir: &Path,
    mode: EndpointMode,
    keep_xdg_runtime_dir: bool,
    experimental: bool,
    max_lifetime_secs: u64,
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };

    // A pane that shells a bare `dot-agent-deck` must reach the BRANCH build.
    // That is not a convenience: it models the upgrade this test is about, where
    // the binary on disk has already been replaced while the daemon in memory
    // has not.
    let host_path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_string());
    set(
        "PATH",
        format!("{}:{}", new_binary_dir.display(), host_path),
    );
    set("HOME", sandbox.home.display().to_string());
    set("TMPDIR", sandbox.tmp.display().to_string());
    set("TERM", "xterm-256color".to_string());
    set("LC_ALL", "C.UTF-8".to_string());
    set("COLORTERM", "truecolor".to_string());
    set("SHELL", "/bin/sh".to_string());
    set(
        "XDG_CONFIG_HOME",
        sandbox.home.join(".config").display().to_string(),
    );
    for pass in ["USER", "LOGNAME"] {
        if let Ok(v) = std::env::var(pass) {
            set(pass, v);
        }
    }
    if keep_xdg_runtime_dir && let Ok(v) = std::env::var("XDG_RUNTIME_DIR") {
        set("XDG_RUNTIME_DIR", v);
    }

    set(
        "DOT_AGENT_DECK_STATE_DIR",
        sandbox.state.display().to_string(),
    );
    set(
        "DOT_AGENT_DECK_LOCK_DIR",
        sandbox.locks.display().to_string(),
    );
    set("DOT_AGENT_DECK_LOG", sandbox.log.display().to_string());
    set(
        "DOT_AGENT_DECK_SESSION",
        sandbox.session.display().to_string(),
    );
    // The daemon loads the global `schedules.toml` at startup and fires every
    // enabled entry, so without this a sandbox daemon runs the operator's real
    // scheduled tasks a second time — and a registered schedule also keeps it
    // from ever idling out. An absent file means no schedules.
    set(
        "DOT_AGENT_DECK_SCHEDULES",
        sandbox.schedules.display().to_string(),
    );
    // Project-config discovery walks up from CWD, not from `$HOME`, so a
    // sandbox rooted at a sibling path still reads the operator's real
    // `.dot-agent-deck.toml` feature flags. Pin the flag rather than inherit it.
    set(
        "DOT_AGENT_DECK_EXPERIMENTAL",
        if experimental { "1" } else { "0" }.to_string(),
    );
    // `DEFAULT_IDLE_SHUTDOWN_SECS` is 30, so more than 30 s between
    // `daemon serve` and the first attach and the daemon exits, the TUI replaces
    // it, and the run is silently a same-version test. `0` is the documented
    // "always on" production value.
    set("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0".to_string());
    // Backstop for a stand-in agent that escapes its process group: it
    // self-exits after this many seconds even if this harness is killed outright.
    set(
        "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
        max_lifetime_secs.to_string(),
    );

    if mode == EndpointMode::SandboxSockets {
        set(
            "DOT_AGENT_DECK_SOCKET",
            sandbox.hook_socket().display().to_string(),
        );
        set(
            "DOT_AGENT_DECK_ATTACH_SOCKET",
            sandbox.attach_socket().display().to_string(),
        );
    }

    env.into_iter().collect()
}

/// The filesystem type backing `path`, from `/proc/mounts` (longest matching
/// mount point wins).
pub fn filesystem_of(path: &Path) -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/mounts").ok()?;
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut best: Option<(usize, String)> = None;
    for line in mounts.lines() {
        let mut f = line.split_whitespace();
        let (_dev, mount_point, fstype) = (f.next()?, f.next()?, f.next()?);
        let mount_point = mount_point.replace("\\040", " ");
        if target.starts_with(&mount_point) {
            let len = mount_point.len();
            if best.as_ref().is_none_or(|(b, _)| len > *b) {
                best = Some((len, fstype.to_string()));
            }
        }
    }
    best.map(|(_, fs)| fs)
}

/// Free bytes available to an unprivileged writer at `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a NUL-terminated path and `st` is a valid out-parameter.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// Refuse a directory that is on a tmpfs or short of space.
///
/// CLAUDE.md rule 14: a cargo `target/` built with `--features e2e` is several
/// GB, and whatever occupies a tmpfs occupies RAM — so a build there dies at
/// link time with a misleading `error: linking with 'cc' failed`, or gets its
/// `rustc` OOM-killed, and nothing in either message points at the filesystem.
pub fn require_disk_backed(label: &str, path: &Path, min_free_gib: u64) -> Result<String, String> {
    std::fs::create_dir_all(path).map_err(|e| format!("create {label} {}: {e}", path.display()))?;
    let fs = filesystem_of(path).unwrap_or_else(|| "<unknown>".to_string());
    if fs == "tmpfs" || fs == "ramfs" {
        return Err(format!(
            "{label} {} is on a {fs} — CLAUDE.md rule 14 forbids build artifacts and sandboxes on \
             a RAM-backed filesystem. Pass a disk-backed sibling path instead.",
            path.display()
        ));
    }
    let free = free_bytes(path).ok_or_else(|| format!("statvfs({}) failed", path.display()))?;
    let free_gib = free / (1024 * 1024 * 1024);
    if free_gib < min_free_gib {
        return Err(format!(
            "{label} {} has {free_gib} GiB free, below the {min_free_gib} GiB floor",
            path.display()
        ));
    }
    Ok(format!("{} ({fs}, {free_gib} GiB free)", path.display()))
}

/// The two addresses a pre-#1121 build hardcodes when `XDG_RUNTIME_DIR` is
/// unset. Process-global: they are in the real `/tmp`, not in any sandbox.
pub fn legacy_endpoints() -> [PathBuf; 2] {
    let uid = unsafe { libc::geteuid() };
    [
        PathBuf::from(format!("/tmp/dot-agent-deck-{uid}.sock")),
        PathBuf::from(format!("/tmp/dot-agent-deck-attach-{uid}.sock")),
    ]
}

/// Where a post-#1121 build puts its endpoint directory when `XDG_RUNTIME_DIR`
/// is unset: `<temp dir>/dot-agent-deck-{uid}`.
///
/// Used to observe something the change under test claims about itself — that
/// the compatibility read is READ-ONLY, so a branch build that found the old
/// daemon at the legacy address never binds, creates or unlinks anything at its
/// own spelling.
pub fn fallback_endpoint_dir(temp_dir: &Path) -> PathBuf {
    temp_dir.join(format!("dot-agent-deck-{}", unsafe { libc::geteuid() }))
}

/// Refuse to start an [`EndpointMode::Resolved`] run when something is already
/// at the legacy addresses.
///
/// In this mode the OLD build binds the real `/tmp/dot-agent-deck-{uid}*.sock`,
/// because it hardcodes `/tmp` and ignores `TMPDIR`. That is deliberate — it is
/// exactly the address a post-#1121 build's compatibility read looks for — but
/// it means the run touches process-global paths. Two consequences follow, and
/// both are the caller's to respect: two runs in this mode cannot execute
/// concurrently, and a deck already running in the fallback case on this host
/// would be talked to by mistake.
pub fn preflight_resolved_endpoints() -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for path in legacy_endpoints() {
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(format!(
                    "{} already exists. In --endpoint-mode=resolved the old build binds the real \
                     /tmp addresses, so something is already running in the fallback case on this \
                     host — find it and stop it rather than working around it. Two likely \
                     causes: another cross-version run in this mode (they cannot run \
                     concurrently), or a socket file an earlier run left behind because it \
                     could not attribute it to its own daemon pid. `ss -xlp | grep \
                     dot-agent-deck` says which.",
                    path.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                notes.push(format!("{}: absent (as required)", path.display()));
            }
            Err(e) => return Err(format!("stat {}: {e}", path.display())),
        }
    }
    Ok(notes)
}

/// The `.dot-agent-deck.toml` a run drives.
///
/// Three roles, each a **stand-in** rather than a real agent: this check is
/// about the TUI↔daemon wire, and a credential buys nothing here. What each one
/// is chosen for:
///
/// * `orchestrator` is a shell, because `dot-agent-deck delegate` is
///   orchestrator-only and the faithful way to issue it is from inside the pane
///   — which also means the delegate crosses the new TUI's pane-input wire to
///   the old daemon on its way in, and carries genuine hook provenance (issue
///   #1077) rather than an impersonation the harness had to wave through.
/// * `coder` is `cat`, so whatever the daemon writes into its PTY is echoed
///   back verbatim and a delivered delegate is readable off the screen.
/// * `reviewer` is a shell, because `work-done` and `agent-event` have to be
///   issued from a WORKER pane, and the feedback the daemon writes back lands in
///   the orchestrator's pane where it is a separate, daemon-authored observable.
///
/// Each role prints a unique sentinel first: `PaneLayout::Stacked` draws only
/// the focused role's pane, so the sentinel on screen names which role is
/// focused and proves its process is live rather than an empty rebuilt frame.
pub const FIXTURE_TOML: &str = r#"# Written by `cargo xver`. Stand-in agents only — no credentials are used.
[[orchestrations]]
name = "xver"

[[orchestrations.roles]]
name = "orchestrator"
command = "printf 'XVER_ORCHESTRATOR_SENTINEL\\n'; exec sh -i"
start = true

[[orchestrations.roles]]
name = "coder"
command = "printf 'XVER_CODER_SENTINEL\\n'; exec cat"

[[orchestrations.roles]]
name = "reviewer"
command = "printf 'XVER_REVIEWER_SENTINEL\\n'; exec sh -i"
"#;

/// Write the fixture and make the project directory a git repository, because
/// some deck paths probe `.git`.
///
/// The `git init` runs with location and configuration discovery neutralised
/// (issue #834's shape): an ambient `GIT_DIR` outranks the `current_dir` a
/// command is given, so without this a run started mid-`rebase --exec`, from a
/// pre-commit hook or under `bisect run` would initialise, stage and commit
/// against the **named repository** rather than against the sandbox.
pub fn write_project(sandbox: &Sandbox) -> Result<(), String> {
    std::fs::write(sandbox.project.join(".dot-agent-deck.toml"), FIXTURE_TOML)
        .map_err(|e| format!("write fixture: {e}"))?;
    let mut cmd = std::process::Command::new("git");
    cmd.args(["init", "--quiet"])
        .current_dir(&sandbox.project)
        .env("GIT_CONFIG_GLOBAL", sandbox.root.join("no-such-gitconfig"))
        .env("GIT_CONFIG_SYSTEM", sandbox.root.join("no-such-gitconfig"))
        .env("GIT_CEILING_DIRECTORIES", &sandbox.root)
        .env("HOME", &sandbox.home)
        .env("XDG_CONFIG_HOME", sandbox.home.join(".config"));
    for var in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        cmd.env_remove(var);
    }
    let status = cmd.status().map_err(|e| format!("git init: {e}"))?;
    if !status.success() {
        return Err(format!("git init in the sandbox project failed: {status}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_filesystem_is_identified() {
        assert!(filesystem_of(Path::new("/")).is_some());
    }

    #[test]
    fn free_bytes_reports_something_for_the_root_filesystem() {
        assert!(free_bytes(Path::new("/")).is_some());
    }

    #[test]
    fn a_tmpfs_is_refused_even_with_space_to_spare() {
        // `/dev/shm` is a tmpfs on every Linux this harness runs on; skip
        // cleanly where it is not rather than asserting about the host.
        let shm = Path::new("/dev/shm");
        if filesystem_of(shm).as_deref() != Some("tmpfs") {
            eprintln!("SKIP: /dev/shm is not a tmpfs on this host");
            return;
        }
        let err = require_disk_backed("scratch", shm, 0).expect_err("a tmpfs must be refused");
        assert!(err.contains("rule 14"), "{err}");
    }

    #[test]
    fn a_short_of_space_directory_is_refused() {
        let err = require_disk_backed("scratch", Path::new("/"), u64::MAX / 2)
            .expect_err("an impossible free-space floor must be refused");
        assert!(err.contains("below the"), "{err}");
    }

    #[test]
    fn resolved_mode_pins_both_socket_overrides_absent() {
        let root = std::env::temp_dir().join(format!("xver-env-{}", std::process::id()));
        let sb = Sandbox::create(root.clone()).expect("sandbox");
        let env = run_env(
            &sb,
            Path::new("/opt/bin"),
            EndpointMode::Resolved,
            false,
            false,
            1800,
        );
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"DOT_AGENT_DECK_SOCKET"));
        assert!(!keys.contains(&"DOT_AGENT_DECK_ATTACH_SOCKET"));
        assert!(!keys.contains(&"XDG_RUNTIME_DIR"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn sandbox_mode_sets_both_socket_overrides_inside_the_sandbox() {
        let root = std::env::temp_dir().join(format!("xver-env2-{}", std::process::id()));
        let sb = Sandbox::create(root.clone()).expect("sandbox");
        let env = run_env(
            &sb,
            Path::new("/opt/bin"),
            EndpointMode::SandboxSockets,
            true,
            false,
            1800,
        );
        let map: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(
            map.get("DOT_AGENT_DECK_ATTACH_SOCKET"),
            Some(&sb.attach_socket().display().to_string())
        );
        assert_eq!(
            map.get("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS"),
            Some(&"0".to_string())
        );
        assert_eq!(
            map.get("DOT_AGENT_DECK_LOG"),
            Some(&sb.log.display().to_string())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
