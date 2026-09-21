//! The sandbox: where a run puts its files, what environment every process in
//! it shares, which endpoints it expects, and the preflight that refuses to
//! start a run that would measure nothing or damage something.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The longest `sun_path` a Unix socket address can carry, including its NUL.
const SUN_PATH_MAX: usize = 108;

/// One run's directory tree, `$S`. Disk-backed by construction — see
/// [`filesystem_of`] and CLAUDE.md rule 14 for why that is checked rather than
/// assumed — and owner-only (`0700`) at every level the harness creates.
#[derive(Clone, Debug)]
pub struct Sandbox {
    pub root: PathBuf,
    /// The deck's `HOME`.
    pub home: PathBuf,
    /// `XDG_CONFIG_HOME`, and where `DOT_AGENT_DECK_CONFIG` / `_SESSION` /
    /// `_SCHEDULES` point. Those three files are never created: an absent
    /// schedules file is "no schedules", which is the point.
    pub config: PathBuf,
    pub xdg_state: PathBuf,
    pub xdg_data: PathBuf,
    pub xdg_cache: PathBuf,
    /// The directory both decks are launched in, holding the
    /// `.dot-agent-deck.toml` that defines the orchestration.
    pub project: PathBuf,
    pub state: PathBuf,
    pub locks: PathBuf,
    /// Bound over `/tmp` inside the run's mount namespace.
    pub fallback_tmp: PathBuf,
    /// Bound over `/run/user/<uid>` inside the run's mount namespace.
    pub run_user: PathBuf,
    /// Bound over `/var/tmp` inside the run's mount namespace.
    pub var_tmp: PathBuf,
    /// `DOT_AGENT_DECK_LOG`. Resolved separately from [`state`](Self::state):
    /// without it an otherwise-isolated daemon appends into the operator's real
    /// `~/.local/state/dot-agent-deck/deck.log`.
    pub log: PathBuf,
    /// Where PTY streams, daemon stdio and the inner run's evidence go.
    pub artifacts: PathBuf,
    /// The branch build, staged as `dot-agent-deck` so a pane that shells a
    /// bare `dot-agent-deck` reaches it. First on the run's `PATH`.
    pub bin: PathBuf,
    /// The previous release's binary, staged. Deliberately NOT on `PATH`.
    pub old: PathBuf,
    /// Where the branch build is staged in a REVERSE run, where `bin` holds the
    /// previous release instead (see [`Direction`]). Empty in a forward run.
    pub branch: PathBuf,
    /// Synthetic agents some reverse probes need — a hard link of this harness
    /// named `claude`, which is what makes the deck type it as Claude Code.
    /// Empty unless a probe stages one.
    pub stub: PathBuf,
    /// This harness's own binary, staged so the namespace can run it.
    pub harness: PathBuf,
}

impl Sandbox {
    /// The layout under `root`, without touching the filesystem.
    pub fn at(root: PathBuf) -> Self {
        Self {
            home: root.join("home"),
            config: root.join("config"),
            xdg_state: root.join("xdg-state"),
            xdg_data: root.join("xdg-data"),
            xdg_cache: root.join("xdg-cache"),
            project: root.join("project"),
            state: root.join("state"),
            locks: root.join("locks"),
            fallback_tmp: root.join("fallback-tmp"),
            run_user: root.join("run-user"),
            var_tmp: root.join("var-tmp"),
            log: root.join("deck.log"),
            artifacts: root.join("artifacts"),
            bin: root.join("bin"),
            old: root.join("old"),
            branch: root.join("branch"),
            stub: root.join("stub"),
            harness: root.join("harness"),
            root,
        }
    }

    /// Create a fresh sandbox at `<runs_root>/<name>`.
    ///
    /// Refuses unless the root is brand new (created here, not reused — a
    /// pre-existing entry could be a symlink or somebody else's directory), is
    /// directly under the canonical runs root, and ends up a real directory
    /// owned by this uid at `0700`.
    pub fn create(runs_root: &Path, name: &str) -> Result<Self, String> {
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(format!("refusing sandbox name {name:?}"));
        }
        let runs_root = std::fs::canonicalize(runs_root)
            .map_err(|e| format!("canonicalize runs root {}: {e}", runs_root.display()))?;
        let root = runs_root.join(name);
        mkdir_private(&root)?;
        require_private_dir(&root)?;
        if std::fs::canonicalize(&root).ok().as_deref() != Some(root.as_path()) {
            return Err(format!(
                "{} does not canonicalize to itself",
                root.display()
            ));
        }
        let sb = Self::at(root);
        for dir in [
            &sb.home,
            &sb.config,
            &sb.xdg_state,
            &sb.xdg_data,
            &sb.xdg_cache,
            &sb.project,
            &sb.state,
            &sb.locks,
            &sb.fallback_tmp,
            &sb.run_user,
            &sb.var_tmp,
            &sb.artifacts,
            &sb.bin,
            &sb.old,
            &sb.branch,
            &sb.stub,
            &sb.harness,
        ] {
            mkdir_private(dir)?;
        }
        Ok(sb)
    }

    pub fn hook_socket(&self) -> PathBuf {
        self.root.join("hook.sock")
    }

    pub fn attach_socket(&self) -> PathBuf {
        self.root.join("attach.sock")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn session_file(&self) -> PathBuf {
        self.config.join("session.toml")
    }

    pub fn schedules_file(&self) -> PathBuf {
        self.config.join("schedules.toml")
    }

    /// The branch build, where `direction` stages it: first on `PATH` in a
    /// forward run, beside it in a reverse one.
    pub fn new_bin(&self, direction: Direction) -> PathBuf {
        match direction {
            Direction::Forward => self.path_bin(),
            Direction::Reverse => self.branch.join("dot-agent-deck"),
        }
    }

    /// What a pane that shells a bare `dot-agent-deck` reaches: the CLIENT
    /// side's build — the branch in a forward run, the previous release in a
    /// reverse one.
    pub fn path_bin(&self) -> PathBuf {
        self.bin.join("dot-agent-deck")
    }

    pub fn old_bin(&self) -> PathBuf {
        self.old.join("dot-agent-deck-linux-amd64")
    }

    /// The synthetic Claude Code stand-in (see [`Sandbox::stub`]).
    pub fn stub_claude(&self) -> PathBuf {
        self.stub.join("claude")
    }

    pub fn harness_bin(&self) -> PathBuf {
        self.harness.join("xtask-cross-version")
    }

    pub fn plan_file(&self) -> PathBuf {
        self.root.join("inner-plan.json")
    }

    pub fn inner_evidence(&self) -> PathBuf {
        self.artifacts.join("inner-evidence.json")
    }
}

/// `mkdir` at `0700`, failing if anything already exists at `path`.
fn mkdir_private(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("create {} (must not already exist): {e}", path.display()))
}

/// Require `path` to be a real directory (not a symlink), owned by this uid,
/// with no group or other permission bits.
pub fn require_private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let md =
        std::fs::symlink_metadata(path).map_err(|e| format!("lstat {}: {e}", path.display()))?;
    if !md.file_type().is_dir() {
        return Err(format!("{} is not a real directory", path.display()));
    }
    if md.uid() != current_uid() {
        return Err(format!("{} is owned by uid {}", path.display(), md.uid()));
    }
    if md.mode() & 0o077 != 0 {
        return Err(format!(
            "{} has mode {:o}; it must be owner-only",
            path.display(),
            md.mode() & 0o777
        ));
    }
    Ok(())
}

pub fn current_uid() -> u32 {
    // SAFETY: `geteuid` cannot fail and has no preconditions.
    unsafe { libc::geteuid() }
}

/// This uid's login name and home directory, from the password database rather
/// than from `$USER` / `$HOME` — the run pins both, and the home it masks must
/// be the real one whatever the caller's environment says.
pub fn passwd_entry() -> Option<(String, PathBuf)> {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: every pointer is valid for the call and `buf` outlives the reads
    // of `pwd`'s fields below.
    let rc = unsafe {
        libc::getpwuid_r(
            current_uid(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: `getpwuid_r` succeeded, so both fields are NUL-terminated strings
    // inside `buf`.
    let (name, dir) = unsafe {
        (
            std::ffi::CStr::from_ptr(pwd.pw_name)
                .to_string_lossy()
                .into_owned(),
            std::ffi::CStr::from_ptr(pwd.pw_dir)
                .to_string_lossy()
                .into_owned(),
        )
    };
    Some((name, PathBuf::from(dir)))
}

/// How the run addresses the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointMode {
    /// `DOT_AGENT_DECK_SOCKET` / `DOT_AGENT_DECK_ATTACH_SOCKET` point inside the
    /// sandbox. The default, and the right one for any change that does not
    /// touch endpoint resolution.
    SandboxSockets,
    /// Neither override is set, so both builds resolve their endpoint the way
    /// they would on a real host — into the run's PRIVATE `/tmp` and
    /// `/run/user/<uid>`, which are what the mount namespace provides.
    ///
    /// Needed when the change under test *is* endpoint resolution — those
    /// overrides short-circuit it, so a run that sets them exercises the one arm
    /// such a PR did not touch and measures nothing.
    Resolved,
}

/// Which build serves the daemon and which one attaches to it.
///
/// Rule 12 prescribes exactly one pairing, `Forward`. For a change that lives in
/// the DAEMON — most of what rule 12's trigger list names — that pairing runs the
/// previous release's daemon code and never executes a changed line: it proves
/// the branch client can drive an old daemon, which is real and is what the rule
/// asks, and says nothing about the branch's own daemon. `Reverse` is the pairing
/// that does: the branch daemon, with the previous release's TUI and CLI attached
/// to it — the downgrade, or a stale binary left on disk beside a new daemon.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Previous-release daemon, branch TUI and CLI. Rule 12's pairing.
    #[default]
    Forward,
    /// Branch daemon, previous-release TUI and CLI.
    Reverse,
}

impl Direction {
    pub fn name(self) -> &'static str {
        match self {
            Direction::Forward => "forward",
            Direction::Reverse => "reverse",
        }
    }
}

/// The real host's spelling of `/run/user/<uid>` — which, inside the run's
/// mount namespace, is [`Sandbox::run_user`].
pub fn runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

/// The two addresses a pre-#1121 build hardcodes when `XDG_RUNTIME_DIR` is
/// unset. Literal `/tmp`, which `TMPDIR` does not move — published v0.41.0 and
/// the branch's compatibility read both spell it this way. Inside the run's
/// namespace that `/tmp` is [`Sandbox::fallback_tmp`].
pub fn flat_endpoints(uid: u32) -> [PathBuf; 2] {
    [
        PathBuf::from(format!("/tmp/dot-agent-deck-{uid}.sock")),
        PathBuf::from(format!("/tmp/dot-agent-deck-attach-{uid}.sock")),
    ]
}

/// Where a post-#1121 build puts its endpoint directory when `XDG_RUNTIME_DIR`
/// is unset: `<TMPDIR>/dot-agent-deck-{uid}`. The run pins `TMPDIR=/tmp`, so
/// this is the same private `/tmp` the flat endpoints are in — the layout a
/// real host has.
pub fn per_uid_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/dot-agent-deck-{uid}"))
}

pub fn per_uid_endpoints(uid: u32) -> [PathBuf; 2] {
    let dir = per_uid_dir(uid);
    [dir.join("hook.sock"), dir.join("attach.sock")]
}

/// Every build's endpoint when `XDG_RUNTIME_DIR` is set.
pub fn xdg_endpoints(uid: u32) -> [PathBuf; 2] {
    let dir = runtime_dir(uid);
    [
        dir.join("dot-agent-deck.sock"),
        dir.join("dot-agent-deck-attach.sock"),
    ]
}

/// Which endpoints the previous release's daemon must own, and which candidate
/// addresses must be ABSENT, before any client of a run connects.
///
/// The absent half is what makes the matrix meaningful for a resolution change:
/// in `resolved` mode with `XDG_RUNTIME_DIR` unset it is what proves the only
/// address a branch client can reach is the legacy flat one, i.e. that the run
/// is on the fallback arm #1121 changed rather than on one it did not touch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointMatrix {
    /// `[hook, attach]`.
    pub owned: [PathBuf; 2],
    pub absent: Vec<PathBuf>,
}

impl EndpointMatrix {
    pub fn attach(&self) -> &Path {
        &self.owned[1]
    }

    pub fn for_run(sb: &Sandbox, mode: EndpointMode, keep_xdg: bool, uid: u32) -> Self {
        let [flat_h, flat_a] = flat_endpoints(uid);
        let [uid_h, uid_a] = per_uid_endpoints(uid);
        let [xdg_h, xdg_a] = xdg_endpoints(uid);
        match (mode, keep_xdg) {
            (EndpointMode::SandboxSockets, _) => Self {
                owned: [sb.hook_socket(), sb.attach_socket()],
                absent: vec![flat_h, flat_a, uid_h, uid_a, xdg_h, xdg_a],
            },
            (EndpointMode::Resolved, false) => Self {
                owned: [flat_h, flat_a],
                absent: vec![uid_h, uid_a, xdg_h, xdg_a],
            },
            (EndpointMode::Resolved, true) => Self {
                owned: [xdg_h, xdg_a],
                absent: vec![flat_h, flat_a, uid_h, uid_a],
            },
        }
    }

    /// Every matrix the run's DAEMON may legitimately bind, one of which the
    /// inner half selects by reading the kernel's table once the daemon is up.
    ///
    /// One candidate, except in a reverse `resolved` run with `XDG_RUNTIME_DIR`
    /// unset. There the daemon is a branch build, and which fallback it binds is
    /// the branch's own behaviour rather than something this harness can know in
    /// advance: a post-#1121 build binds the per-uid pair, every build before it
    /// the flat one. So both are candidates, each with the OTHER pair and the XDG
    /// pair absent, and the selected one is held to exactly the same assertion a
    /// predicted one would be — one pair owned by the daemon, every other
    /// candidate absent. The run records which one it was.
    pub fn candidates(
        sb: &Sandbox,
        mode: EndpointMode,
        keep_xdg: bool,
        uid: u32,
        direction: Direction,
    ) -> Vec<Self> {
        let predicted = Self::for_run(sb, mode, keep_xdg, uid);
        if direction == Direction::Forward || mode != EndpointMode::Resolved || keep_xdg {
            return vec![predicted];
        }
        let [flat_h, flat_a] = flat_endpoints(uid);
        let [uid_h, uid_a] = per_uid_endpoints(uid);
        let [xdg_h, xdg_a] = xdg_endpoints(uid);
        vec![
            Self {
                owned: [uid_h.clone(), uid_a.clone()],
                absent: vec![flat_h.clone(), flat_a.clone(), xdg_h.clone(), xdg_a.clone()],
            },
            Self {
                owned: [flat_h, flat_a],
                absent: vec![uid_h, uid_a, xdg_h, xdg_a],
            },
        ]
    }

    /// Whether this matrix's owned pair is the post-#1121 per-uid directory.
    pub fn owns_per_uid(&self, uid: u32) -> bool {
        self.owned == per_uid_endpoints(uid)
    }
}

/// Variable names the run environment may carry. Anything else in a sandbox
/// process's environment is a failure of the allowlist.
pub const ALLOWED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TERM",
    "LC_ALL",
    "COLORTERM",
    "SHELL",
    "USER",
    "LOGNAME",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "DOT_AGENT_DECK_STATE_DIR",
    "DOT_AGENT_DECK_LOCK_DIR",
    "DOT_AGENT_DECK_LOG",
    "DOT_AGENT_DECK_CONFIG",
    "DOT_AGENT_DECK_SESSION",
    "DOT_AGENT_DECK_SCHEDULES",
    "DOT_AGENT_DECK_EXPERIMENTAL",
    "DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS",
    "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
    "DOT_AGENT_DECK_SOCKET",
    "DOT_AGENT_DECK_ATTACH_SOCKET",
    "GIT_CONFIG_NOSYSTEM",
    "DAD_XVER_SANDBOX",
];

/// The system directories the run's `PATH` may name besides the sandbox's own
/// `bin`. The operator's `~/.local/bin` — where the production deck lives on
/// this box — is deliberately not one of them.
const SYSTEM_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

/// The run's settings that decide its environment.
#[derive(Clone, Copy, Debug)]
pub struct EnvSpec {
    pub mode: EndpointMode,
    pub keep_xdg_runtime_dir: bool,
    pub experimental: bool,
    pub max_lifetime_secs: u64,
    pub uid: u32,
}

/// The environment every process the harness starts in a run gets — its own
/// inner half, the daemon, both TUIs and every CLI call — built from nothing.
/// (The panes the daemon starts inherit the daemon's, plus the deck's own pane
/// variables.)
///
/// Nothing is inherited from the caller, including `PATH`: the audit that
/// hardened this found the caller's credentials in the environment of every
/// process the first version started, and an allowlist is the only way to
/// state an absence. Every path-valued entry is under `$S`, except `TMPDIR` and
/// `XDG_RUNTIME_DIR`, which keep the host's spelling (`/tmp`,
/// `/run/user/<uid>`) because inside the run's mount namespace both of those
/// ARE sandbox directories — see [`check_env`].
///
/// `extra` is a reverse probe's own additions (a timer knob, or #1190's hostile
/// git location variables): names that are NOT on [`ALLOWED_ENV`], each with an
/// exact value, which [`check_env`] then admits by name AND value and nothing
/// else. It cannot replace a base entry.
pub fn run_env(
    sb: &Sandbox,
    spec: EnvSpec,
    user: &str,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut set = |k: &str, v: String| {
        env.insert(k.to_string(), v);
    };
    let path_of = |p: &Path| p.display().to_string();

    // A pane that shells a bare `dot-agent-deck` must reach the BRANCH build.
    // That is not a convenience: it models the upgrade this test is about, where
    // the binary on disk has already been replaced while the daemon in memory
    // has not.
    set(
        "PATH",
        std::iter::once(path_of(&sb.bin))
            .chain(SYSTEM_PATH.iter().map(|s| s.to_string()))
            .collect::<Vec<_>>()
            .join(":"),
    );
    set("HOME", path_of(&sb.home));
    set("TMPDIR", "/tmp".to_string());
    set("TERM", "xterm-256color".to_string());
    set("LC_ALL", "C.UTF-8".to_string());
    set("COLORTERM", "truecolor".to_string());
    set("SHELL", "/bin/sh".to_string());
    set("USER", user.to_string());
    set("LOGNAME", user.to_string());
    // `HOME` alone does not cover these: `schedules_path()` consults an
    // inherited `XDG_CONFIG_HOME` before `HOME`, so a sandbox daemon could read
    // the operator's real schedules and fire them.
    set("XDG_CONFIG_HOME", path_of(&sb.config));
    set("XDG_STATE_HOME", path_of(&sb.xdg_state));
    set("XDG_DATA_HOME", path_of(&sb.xdg_data));
    set("XDG_CACHE_HOME", path_of(&sb.xdg_cache));
    if spec.keep_xdg_runtime_dir {
        set("XDG_RUNTIME_DIR", path_of(&runtime_dir(spec.uid)));
    }
    set("DOT_AGENT_DECK_STATE_DIR", path_of(&sb.state));
    set("DOT_AGENT_DECK_LOCK_DIR", path_of(&sb.locks));
    set("DOT_AGENT_DECK_LOG", path_of(&sb.log));
    set("DOT_AGENT_DECK_CONFIG", path_of(&sb.config_file()));
    set("DOT_AGENT_DECK_SESSION", path_of(&sb.session_file()));
    // The daemon loads the global `schedules.toml` at startup and fires every
    // enabled entry, so without this a sandbox daemon runs the operator's real
    // scheduled tasks a second time. An absent file means no schedules.
    set("DOT_AGENT_DECK_SCHEDULES", path_of(&sb.schedules_file()));
    // Pinned rather than inherited. Project-config discovery walks up from CWD,
    // and the trusted fixture in `$S/project` is what stops that walk; this pins
    // the flag in case a build reads it from somewhere else.
    set(
        "DOT_AGENT_DECK_EXPERIMENTAL",
        if spec.experimental { "1" } else { "0" }.to_string(),
    );
    // `DEFAULT_IDLE_SHUTDOWN_SECS` is 30, so more than 30 s between
    // `daemon serve` and the first attach and the daemon exits, the TUI replaces
    // it, and the run is silently a same-version test. `0` is the documented
    // "always on" production value.
    set("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0".to_string());
    // Backstop for a stand-in agent that escapes its process group. The PID
    // namespace is the first backstop; this is the second.
    set(
        "DOT_AGENT_DECK_TEST_MAX_LIFETIME_SECS",
        spec.max_lifetime_secs.to_string(),
    );
    set("GIT_CONFIG_NOSYSTEM", "1".to_string());
    // The run's unique marker: part of every sandbox process's identity, and
    // what the post-run census looks for.
    set("DAD_XVER_SANDBOX", path_of(&sb.root));

    if spec.mode == EndpointMode::SandboxSockets {
        set("DOT_AGENT_DECK_SOCKET", path_of(&sb.hook_socket()));
        set("DOT_AGENT_DECK_ATTACH_SOCKET", path_of(&sb.attach_socket()));
    }
    for (k, v) in extra {
        // A base entry is never replaced: `check_env` refuses an extra name that
        // is on the allowlist, so a collision here is a harness bug, and the
        // base value wins rather than the probe's.
        env.entry(k.clone()).or_insert_with(|| v.clone());
    }

    env.into_iter().collect()
}

/// Whether a variable name looks like it carries a credential. A backstop
/// behind [`ALLOWED_ENV`], never a substitute for it.
pub fn credential_like(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "API_KEY",
        "_KEY",
        "AUTH",
        "CAPABILITY",
        "COOKIE",
        "SESSION_ID",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
        || upper.starts_with("AWS_")
        || upper.starts_with("SSH_")
        || upper.starts_with("GITHUB_")
        || upper.starts_with("GH_")
}

/// Check an environment a sandbox process is about to receive — or, inside the
/// namespace, the harness's own — against the allowlist policy. Fails closed:
/// an unknown key is a failure, not a warning.
///
/// `extra` is the run's probe additions (see [`run_env`]). Each is admitted
/// only with its exact value, only if it is NOT a base allowlist name, and never
/// if it looks like a credential — so a probe widens the allowlist by exactly
/// the entries it names and cannot loosen a base check.
pub fn check_env(
    env: &[(String, String)],
    sb: &Sandbox,
    spec: EnvSpec,
    extra: &[(String, String)],
) -> Result<(), String> {
    let map: BTreeMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if map.len() != env.len() {
        return Err("the environment names a variable twice".to_string());
    }
    for (k, v) in extra {
        if ALLOWED_ENV.contains(&k.as_str()) {
            return Err(format!(
                "the probe's `{k}` would replace a base allowlist entry"
            ));
        }
        if credential_like(k) {
            return Err(format!("the probe's `{k}` looks like a credential"));
        }
        match map.get(k.as_str()) {
            Some(got) if got == v => {}
            Some(_) => return Err(format!("the probe's `{k}` does not have its fixed value")),
            None => return Err(format!("the probe's `{k}` is missing")),
        }
    }
    for key in map.keys() {
        if !ALLOWED_ENV.contains(key) && !extra.iter().any(|(k, _)| k == key) {
            return Err(format!("`{key}` is not on the run environment's allowlist"));
        }
        if credential_like(key) {
            return Err(format!("`{key}` looks like a credential"));
        }
    }
    let require = |key: &str, want: String| -> Result<(), String> {
        match map.get(key) {
            Some(v) if *v == want => Ok(()),
            Some(v) => Err(format!("`{key}` is {v:?}, expected {want:?}")),
            None => Err(format!("`{key}` is missing")),
        }
    };
    let under_root = |key: &str| -> Result<(), String> {
        match map.get(key) {
            Some(v) if Path::new(v).starts_with(&sb.root) => Ok(()),
            Some(v) => Err(format!(
                "`{key}` is {v:?}, outside the sandbox {}",
                sb.root.display()
            )),
            None => Err(format!("`{key}` is missing")),
        }
    };
    for key in [
        "HOME",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "DOT_AGENT_DECK_STATE_DIR",
        "DOT_AGENT_DECK_LOCK_DIR",
        "DOT_AGENT_DECK_LOG",
        "DOT_AGENT_DECK_CONFIG",
        "DOT_AGENT_DECK_SESSION",
        "DOT_AGENT_DECK_SCHEDULES",
    ] {
        under_root(key)?;
    }
    require("DAD_XVER_SANDBOX", sb.root.display().to_string())?;
    require("TMPDIR", "/tmp".to_string())?;
    require("DOT_AGENT_DECK_IDLE_SHUTDOWN_SECS", "0".to_string())?;
    match map.get("PATH") {
        Some(path) => {
            for dir in path.split(':') {
                if !(Path::new(dir).starts_with(&sb.root) || SYSTEM_PATH.contains(&dir)) {
                    return Err(format!(
                        "`PATH` names {dir:?}, which is neither in the sandbox nor a system directory"
                    ));
                }
            }
        }
        None => return Err("`PATH` is missing".to_string()),
    }
    match (spec.keep_xdg_runtime_dir, map.get("XDG_RUNTIME_DIR")) {
        (false, Some(v)) => {
            return Err(format!(
                "`XDG_RUNTIME_DIR` is {v:?} but this run pins it ABSENT"
            ));
        }
        (true, _) => require(
            "XDG_RUNTIME_DIR",
            runtime_dir(spec.uid).display().to_string(),
        )?,
        (false, None) => {}
    }
    match spec.mode {
        EndpointMode::SandboxSockets => {
            require(
                "DOT_AGENT_DECK_SOCKET",
                sb.hook_socket().display().to_string(),
            )?;
            require(
                "DOT_AGENT_DECK_ATTACH_SOCKET",
                sb.attach_socket().display().to_string(),
            )?;
        }
        EndpointMode::Resolved => {
            for key in ["DOT_AGENT_DECK_SOCKET", "DOT_AGENT_DECK_ATTACH_SOCKET"] {
                if map.contains_key(key) {
                    return Err(format!(
                        "`{key}` is set, but a resolved-mode run pins both overrides ABSENT"
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Refuse a sandbox whose socket paths would not fit in `sun_path`.
pub fn check_socket_path_lengths(matrix: &EndpointMatrix) -> Result<(), String> {
    for p in matrix.owned.iter().chain(matrix.absent.iter()) {
        let len = p.as_os_str().len();
        if len + 1 > SUN_PATH_MAX {
            return Err(format!(
                "{} is {len} bytes, too long for a Unix socket address ({SUN_PATH_MAX} including \
                 the NUL). Pass a shorter --runs-root.",
                p.display()
            ));
        }
    }
    Ok(())
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

/// Put a binary into the sandbox: a hard link when source and sandbox share a
/// filesystem, a copy otherwise. Either way the run executes a path under `$S`,
/// which is what lets an executable path serve as an identity field — no
/// production deck has one there.
pub fn stage_binary(src: &Path, dst: &Path) -> Result<String, String> {
    use std::os::unix::fs::PermissionsExt;
    let how = match std::fs::hard_link(src, dst) {
        Ok(()) => "hard-linked",
        Err(_) => {
            std::fs::copy(src, dst)
                .map_err(|e| format!("stage {} -> {}: {e}", src.display(), dst.display()))?;
            std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", dst.display()))?;
            "copied"
        }
    };
    let md = std::fs::symlink_metadata(dst).map_err(|e| format!("lstat {}: {e}", dst.display()))?;
    if !md.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file after staging",
            dst.display()
        ));
    }
    Ok(format!("{how} {} -> {}", src.display(), dst.display()))
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

pub fn project_config(sb: &Sandbox) -> PathBuf {
    sb.project.join(".dot-agent-deck.toml")
}

/// Write the fixture and make the project directory a standalone git
/// repository, because some deck paths probe `.git`.
///
/// Standalone — `git init`, never a worktree of the operator's repository — so
/// nothing a run does to it can write Git metadata outside `$S`. The `git`
/// process gets an empty environment plus exactly what it needs, which rules out
/// issue #834's shape by construction: an ambient `GIT_DIR` outranks the
/// `current_dir` a command is given, and there is none to inherit.
///
/// `fixture` is [`FIXTURE_TOML`] plus whatever a reverse probe adds. `commit`
/// also records one commit of it — a dispatch needs a `HEAD` to branch a
/// worktree from — with an author identity given on the command line rather
/// than through any configuration file.
pub fn write_project(sb: &Sandbox, fixture: &str, commit: bool) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = project_config(sb);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(fixture.as_bytes())
        .map_err(|e| format!("write fixture: {e}"))?;
    drop(f);
    let mut steps: Vec<Vec<&str>> = vec![vec!["init", "--quiet"]];
    if commit {
        steps.push(vec!["add", ".dot-agent-deck.toml"]);
        steps.push(vec![
            "-c",
            "user.name=xver",
            "-c",
            "user.email=xver@invalid.example",
            "commit",
            "--quiet",
            "-m",
            "xver fixture",
        ]);
    }
    for args in steps {
        let status = sandbox_git(sb, &sb.project)
            .args(&args)
            .status()
            .map_err(|e| format!("git {args:?}: {e}"))?;
        if !status.success() {
            return Err(format!(
                "git {args:?} in the sandbox project failed: {status}"
            ));
        }
    }
    verify_project(sb, fixture)
}

/// A `git` command for a repository under `$S`: an empty environment plus
/// exactly what git needs, so no ambient location variable (issue #834's shape)
/// and no operator configuration can reach it, and discovery cannot walk above
/// the sandbox.
pub fn sandbox_git(sb: &Sandbox, dir: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("/usr/bin/git");
    c.current_dir(dir)
        .env_clear()
        .env("PATH", SYSTEM_PATH.join(":"))
        .env("HOME", &sb.home)
        .env("XDG_CONFIG_HOME", &sb.config)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", sb.root.join("no-such-gitconfig"))
        .env("GIT_CEILING_DIRECTORIES", &sb.root);
    c
}

/// Require the fixture to be what stops the deck's project-config walk.
///
/// `resolve_project_dir` walks up from CWD and stops at the first
/// `.dot-agent-deck.toml` that is a regular file owned by this uid. So the
/// fixture must be exactly that — a regular file, not a symlink, owned by this
/// uid — or the walk continues past `$S` towards the operator's own config. And
/// the project's `.git` must be a directory: a `.git` FILE would make it a
/// linked worktree whose metadata lives outside `$S`.
pub fn verify_project(sb: &Sandbox, fixture: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let path = project_config(sb);
    let md =
        std::fs::symlink_metadata(&path).map_err(|e| format!("lstat {}: {e}", path.display()))?;
    if !md.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file, so the deck's project-config walk would not stop at it",
            path.display()
        ));
    }
    if md.uid() != current_uid() {
        return Err(format!(
            "{} is owned by uid {}, so the deck would not trust it and would walk past it",
            path.display(),
            md.uid()
        ));
    }
    let body =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if body != fixture {
        return Err(format!(
            "{} is not the fixture this harness wrote",
            path.display()
        ));
    }
    let git = sb.project.join(".git");
    let gmd =
        std::fs::symlink_metadata(&git).map_err(|e| format!("lstat {}: {e}", git.display()))?;
    if !gmd.file_type().is_dir() {
        return Err(format!(
            "{} is not a directory — the project must be a standalone repository, not a linked worktree",
            git.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xver-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn spec(mode: EndpointMode, keep_xdg: bool) -> EnvSpec {
        EnvSpec {
            mode,
            keep_xdg_runtime_dir: keep_xdg,
            experimental: false,
            max_lifetime_secs: 1800,
            uid: 1000,
        }
    }

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
    fn a_sandbox_is_created_owner_only_and_never_over_an_existing_entry() {
        let runs = scratch("create");
        let sb = Sandbox::create(&runs, "run-1").expect("fresh sandbox");
        require_private_dir(&sb.root).expect("0700 root");
        require_private_dir(&sb.fallback_tmp).expect("0700 subdir");
        let err = Sandbox::create(&runs, "run-1").expect_err("an existing root is refused");
        assert!(err.contains("must not already exist"), "{err}");
        assert!(Sandbox::create(&runs, "../escape").is_err());
        let _ = std::fs::remove_dir_all(runs);
    }

    #[test]
    fn resolved_mode_pins_both_socket_overrides_and_the_runtime_dir_absent() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let s = spec(EndpointMode::Resolved, false);
        let env = run_env(&sb, s, "someone", &[]);
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert!(!keys.contains(&"DOT_AGENT_DECK_SOCKET"));
        assert!(!keys.contains(&"DOT_AGENT_DECK_ATTACH_SOCKET"));
        assert!(!keys.contains(&"XDG_RUNTIME_DIR"));
        check_env(&env, &sb, s, &[]).expect("the harness's own env passes its own policy");
    }

    #[test]
    fn sandbox_mode_sets_both_socket_overrides_inside_the_sandbox() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let s = spec(EndpointMode::SandboxSockets, true);
        let env = run_env(&sb, s, "someone", &[]);
        let map: BTreeMap<_, _> = env.clone().into_iter().collect();
        assert_eq!(
            map.get("DOT_AGENT_DECK_ATTACH_SOCKET"),
            Some(&sb.attach_socket().display().to_string())
        );
        assert_eq!(
            map.get("XDG_RUNTIME_DIR"),
            Some(&"/run/user/1000".to_string())
        );
        assert_eq!(map.get("TMPDIR"), Some(&"/tmp".to_string()));
        assert_eq!(
            map.get("DOT_AGENT_DECK_SCHEDULES"),
            Some(&sb.schedules_file().display().to_string())
        );
        assert_eq!(
            map.get("XDG_CONFIG_HOME"),
            Some(&sb.config.display().to_string())
        );
        check_env(&env, &sb, s, &[]).expect("the harness's own env passes its own policy");
    }

    #[test]
    fn the_env_policy_refuses_anything_off_the_allowlist() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let s = spec(EndpointMode::Resolved, false);
        let base = run_env(&sb, s, "someone", &[]);

        let mut extra = base.clone();
        extra.push(("ANTHROPIC_API_KEY".into(), "x".into()));
        assert!(
            check_env(&extra, &sb, s, &[])
                .unwrap_err()
                .contains("allowlist")
        );

        let mut xdg = base.clone();
        xdg.push(("XDG_RUNTIME_DIR".into(), "/run/user/1000".into()));
        assert!(check_env(&xdg, &sb, s, &[]).unwrap_err().contains("ABSENT"));

        let mut sock = base.clone();
        sock.push((
            "DOT_AGENT_DECK_SOCKET".into(),
            "/srv/runs/r1/hook.sock".into(),
        ));
        assert!(
            check_env(&sock, &sb, s, &[])
                .unwrap_err()
                .contains("ABSENT")
        );

        let escaped: Vec<_> = base
            .iter()
            .map(|(k, v)| {
                if k == "DOT_AGENT_DECK_SCHEDULES" {
                    (
                        k.clone(),
                        "/home/op/.config/dot-agent-deck/schedules.toml".to_string(),
                    )
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect();
        assert!(
            check_env(&escaped, &sb, s, &[])
                .unwrap_err()
                .contains("outside the sandbox")
        );

        let path: Vec<_> = base
            .iter()
            .map(|(k, v)| {
                if k == "PATH" {
                    (k.clone(), format!("/home/op/.local/bin:{v}"))
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect();
        assert!(check_env(&path, &sb, s, &[]).unwrap_err().contains("PATH"));
    }

    #[test]
    fn credential_shaped_names_are_recognised() {
        for name in [
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "SSH_AUTH_SOCK",
            "AWS_SECRET_ACCESS_KEY",
            "DOT_AGENT_DECK_PANE_CAPABILITY",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(credential_like(name), "{name}");
        }
        for name in ALLOWED_ENV {
            assert!(
                !credential_like(name),
                "allowlisted {name} must not trip the backstop"
            );
        }
    }

    #[test]
    fn the_endpoint_matrix_puts_resolved_unset_runs_on_the_flat_fallback() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let m = EndpointMatrix::for_run(&sb, EndpointMode::Resolved, false, 1000);
        assert_eq!(
            m.attach(),
            Path::new("/tmp/dot-agent-deck-attach-1000.sock")
        );
        assert!(
            m.absent
                .contains(&PathBuf::from("/tmp/dot-agent-deck-1000/attach.sock"))
        );
        assert!(
            m.absent
                .contains(&PathBuf::from("/run/user/1000/dot-agent-deck-attach.sock"))
        );

        let m = EndpointMatrix::for_run(&sb, EndpointMode::Resolved, true, 1000);
        assert_eq!(
            m.attach(),
            Path::new("/run/user/1000/dot-agent-deck-attach.sock")
        );
        assert!(
            m.absent
                .contains(&PathBuf::from("/tmp/dot-agent-deck-attach-1000.sock"))
        );

        let m = EndpointMatrix::for_run(&sb, EndpointMode::SandboxSockets, true, 1000);
        assert_eq!(m.attach(), sb.attach_socket());
        assert_eq!(m.absent.len(), 6);
        check_socket_path_lengths(&m).expect("short paths fit");
    }

    #[test]
    fn an_overlong_socket_path_is_refused() {
        let sb = Sandbox::at(PathBuf::from(format!("/srv/{}", "x".repeat(120))));
        let m = EndpointMatrix::for_run(&sb, EndpointMode::SandboxSockets, false, 1000);
        assert!(
            check_socket_path_lengths(&m)
                .unwrap_err()
                .contains("too long")
        );
    }

    #[test]
    fn the_fixture_is_verified_as_what_stops_the_config_walk() {
        let runs = scratch("project");
        let sb = Sandbox::create(&runs, "run-1").expect("sandbox");
        write_project(&sb, FIXTURE_TOML, false).expect("fixture");
        verify_project(&sb, FIXTURE_TOML).expect("verified");
        // A symlink in its place would not stop the walk at `$S`.
        let cfg = project_config(&sb);
        std::fs::remove_file(&cfg).expect("rm");
        std::os::unix::fs::symlink("/etc/hostname", &cfg).expect("symlink");
        assert!(
            verify_project(&sb, FIXTURE_TOML)
                .unwrap_err()
                .contains("not a regular file")
        );
        let _ = std::fs::remove_dir_all(runs);
    }
}

#[cfg(test)]
mod reverse_tests {
    use super::*;

    fn spec(mode: EndpointMode, keep_xdg: bool) -> EnvSpec {
        EnvSpec {
            mode,
            keep_xdg_runtime_dir: keep_xdg,
            experimental: false,
            max_lifetime_secs: 1800,
            uid: 1000,
        }
    }

    #[test]
    fn a_reverse_run_puts_the_previous_release_on_path_and_the_branch_beside_it() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        assert_eq!(sb.new_bin(Direction::Forward), sb.path_bin());
        assert_eq!(
            sb.new_bin(Direction::Reverse),
            Path::new("/srv/runs/r1/branch/dot-agent-deck")
        );
        assert_ne!(sb.new_bin(Direction::Reverse), sb.path_bin());
        assert_eq!(sb.stub_claude().file_name().unwrap(), "claude");
    }

    #[test]
    fn a_reverse_resolved_no_xdg_run_learns_which_fallback_the_branch_daemon_binds() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let fwd = EndpointMatrix::candidates(
            &sb,
            EndpointMode::Resolved,
            false,
            1000,
            Direction::Forward,
        );
        assert_eq!(
            fwd,
            vec![EndpointMatrix::for_run(
                &sb,
                EndpointMode::Resolved,
                false,
                1000
            )],
            "forward keeps its one predicted matrix"
        );
        let rev = EndpointMatrix::candidates(
            &sb,
            EndpointMode::Resolved,
            false,
            1000,
            Direction::Reverse,
        );
        assert_eq!(rev.len(), 2);
        assert!(rev[0].owns_per_uid(1000) && !rev[1].owns_per_uid(1000));
        for m in &rev {
            // Whichever pair the daemon binds, the other pair and XDG must be
            // absent — the same strength as a predicted matrix.
            let mut all: Vec<PathBuf> = m.owned.to_vec();
            all.extend(m.absent.iter().cloned());
            all.sort();
            let mut every: Vec<PathBuf> = flat_endpoints(1000)
                .into_iter()
                .chain(per_uid_endpoints(1000))
                .chain(xdg_endpoints(1000))
                .collect();
            every.sort();
            assert_eq!(all, every, "{m:?}");
        }
        for (mode, keep) in [
            (EndpointMode::SandboxSockets, false),
            (EndpointMode::Resolved, true),
        ] {
            assert_eq!(
                EndpointMatrix::candidates(&sb, mode, keep, 1000, Direction::Reverse).len(),
                1,
                "{mode:?} keep_xdg={keep}: every build resolves the same address"
            );
        }
    }

    #[test]
    fn probe_env_is_admitted_by_exact_name_and_value_only() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let s = spec(EndpointMode::SandboxSockets, true);
        let extra = vec![("RUST_LOG".to_string(), "dot_agent_deck=debug".to_string())];
        let env = run_env(&sb, s, "someone", &extra);
        check_env(&env, &sb, s, &extra).expect("the probe's own entry passes");
        assert!(
            check_env(&env, &sb, s, &[])
                .unwrap_err()
                .contains("allowlist"),
            "without the probe it is off the allowlist"
        );
        let wrong = vec![("RUST_LOG".to_string(), "trace".to_string())];
        assert!(
            check_env(&env, &sb, s, &wrong)
                .unwrap_err()
                .contains("fixed value")
        );
        let missing = vec![("RUST_BACKTRACE".to_string(), "1".to_string())];
        assert!(
            check_env(&env, &sb, s, &missing)
                .unwrap_err()
                .contains("missing")
        );
        let base = vec![("HOME".to_string(), "/elsewhere".to_string())];
        assert!(
            check_env(&env, &sb, s, &base)
                .unwrap_err()
                .contains("replace a base")
        );
        let secret = vec![("SOME_TOKEN".to_string(), "x".to_string())];
        let env = run_env(&sb, s, "someone", &secret);
        assert!(
            check_env(&env, &sb, s, &secret)
                .unwrap_err()
                .contains("credential")
        );
    }

    #[test]
    fn a_probe_entry_never_replaces_a_base_one() {
        let sb = Sandbox::at(PathBuf::from("/srv/runs/r1"));
        let s = spec(EndpointMode::SandboxSockets, true);
        let env = run_env(
            &sb,
            s,
            "someone",
            &[("HOME".to_string(), "/elsewhere".to_string())],
        );
        let map: BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(map.get("HOME"), Some(&sb.home.display().to_string()));
    }

    #[test]
    fn a_dispatch_fixture_is_committed_by_a_git_that_sees_no_ambient_location() {
        let dir = std::env::temp_dir().join(format!("xver-commit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let sb = Sandbox::create(&dir, "run-1").expect("sandbox");
        write_project(&sb, FIXTURE_TOML, true).expect("fixture + commit");
        let head = sandbox_git(&sb, &sb.project)
            .args(["log", "--format=%s", "-1"])
            .output()
            .expect("git log");
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "xver fixture");
        let _ = std::fs::remove_dir_all(dir);
    }
}
