//! The private namespace a run executes inside, and the checks that prove it.
//!
//! # Why a namespace, and why `TMPDIR` was not enough
//!
//! The first version of this harness isolated a run with environment variables
//! alone. In `--endpoint-mode resolved` that bound REAL host addresses: a run's
//! published-v0.41.0 daemon, with `TMPDIR` pointing inside its sandbox, owned the
//! host's `/tmp/dot-agent-deck-1000.sock` and `/tmp/dot-agent-deck-attach-1000.sock`,
//! so for the length of that run any production client resolving the flat
//! fallback would have reached a SANDBOX daemon. `TMPDIR` moves only a
//! post-#1121 build's new primary fallback. The branch's compatibility root and
//! every published build before it spell a literal `/tmp`, which no variable
//! relocates.
//!
//! So a run's processes execute inside one `bwrap` namespace in which:
//!
//! * `/tmp` is `$S/fallback-tmp` and `/run/user/<uid>` is `$S/run-user` — BOTH
//!   endpoint roots. Masking `/tmp` alone is not enough: under a read-only view
//!   of `/` the production XDG sockets stay connectable, because a read-only
//!   mount does not stop `connect(2)` on a Unix socket;
//! * `/var/tmp` is `$S/var-tmp`, so other sandboxes' sockets parked there are
//!   not reachable either (no deck resolution rule names `/var/tmp`; this is
//!   defence in depth);
//! * the operator's real home is an empty tmpfs with only `$S` bound back
//!   into it, so the production binary in `~/.local/bin`, the operator's
//!   `~/.config/dot-agent-deck` and `~/.dot-agent-deck.toml`, and every
//!   neighbouring sandbox under `~/code` are not there to be read or reached;
//! * everything else is the host's `/`, bound read-only — except submounts
//!   bwrap cannot remount, which stay `rw` (on the box this was written on:
//!   Docker's per-container mounts and `binfmt_misc`, all root-owned);
//! * the PID, network, IPC, UTS and user namespaces are private, and nested
//!   user namespaces are disabled.
//!
//! The PID namespace is what makes teardown structural rather than a matter of
//! care: inside it no host process has a pid, so no signal the run sends can
//! reach one; and when the run's first process exits the kernel kills every
//! other process in the namespace, `setsid`'d or not.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::probe::Probe;
use crate::proc;
use crate::sandbox::{self, Direction, EndpointMatrix, EndpointMode, EnvSpec, Sandbox};

/// `(st_dev, st_ino)` — what makes two paths the same directory, which is how a
/// bind mount is proven from the inside.
pub type DevIno = (u64, u64);

pub fn dev_ino(path: &Path) -> Result<DevIno, String> {
    let md = std::fs::metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    Ok((md.dev(), md.ino()))
}

/// One path the namespace replaces with a sandbox directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mask {
    /// What the run's processes see, e.g. `/tmp`.
    pub target: PathBuf,
    /// The sandbox directory bound there.
    pub source: PathBuf,
    /// `source`'s identity, recorded outside before launch.
    pub dev_ino: DevIno,
}

/// Everything the half of the harness that runs INSIDE the namespace needs,
/// written to `$S/inner-plan.json` by the half that runs outside.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub root: PathBuf,
    pub uid: u32,
    pub user: String,
    pub mode: EndpointMode,
    pub keep_xdg_runtime_dir: bool,
    pub experimental: bool,
    pub max_lifetime_secs: u64,
    pub previous: String,
    /// The exact line the old binary's `--version` must print, checked by the
    /// inner half before any daemon starts — inside the namespace, where the
    /// old check's host-side run of an unauthenticated download no longer
    /// happens. `None` for `--old-binary`, whose version is recorded and not
    /// enforced.
    pub old_version: Option<String>,
    /// Which build serves the daemon.
    pub direction: Direction,
    /// The reverse probe the run carries ([`Probe::Generic`] in a forward run).
    pub probe: Probe,
    /// The exact `.dot-agent-deck.toml` the run wrote, re-verified inside.
    pub fixture: String,
    /// The probe's additions to the environment, each admitted by name and
    /// exact value (see `sandbox::check_env`).
    pub extra_env: Vec<(String, String)>,
    /// The exact environment every process of the runtime scenario gets — not
    /// the outer half's `git`, `gh` and `cargo fetch`, which run on the host,
    /// nor the build namespace's, which `buildns::env_for` builds.
    pub env: Vec<(String, String)>,
    /// Every endpoint matrix the daemon may legitimately bind; the inner half
    /// selects one from the kernel's table once the daemon is up (see
    /// [`EndpointMatrix::candidates`]). Exactly one in a forward run.
    pub matrices: Vec<EndpointMatrix>,
    pub masks: Vec<Mask>,
    /// The operator's real home, masked by an empty tmpfs; `None` when it does
    /// not exist on this host.
    pub masked_home: Option<PathBuf>,
    /// Host paths replaced by a small empty tmpfs ([`host_only_dirs_to_mask`]).
    #[serde(default)]
    pub tmpfs: Vec<PathBuf>,
    /// The outer half's mount namespace, which the inner half must differ from.
    pub outer_mnt_ns: String,
}

impl Plan {
    pub fn sandbox(&self) -> Sandbox {
        Sandbox::at(self.root.clone())
    }

    pub fn env_spec(&self) -> EnvSpec {
        EnvSpec {
            mode: self.mode,
            keep_xdg_runtime_dir: self.keep_xdg_runtime_dir,
            experimental: self.experimental,
            max_lifetime_secs: self.max_lifetime_secs,
            uid: self.uid,
        }
    }
}

/// The masks a run mounts, with their sources' identities.
pub fn masks_for(sb: &Sandbox, uid: u32) -> Result<Vec<Mask>, String> {
    let mut out = Vec::new();
    for (target, source) in [
        (PathBuf::from("/tmp"), sb.fallback_tmp.clone()),
        (sandbox::runtime_dir(uid), sb.run_user.clone()),
        (PathBuf::from("/var/tmp"), sb.var_tmp.clone()),
    ] {
        // bwrap cannot create a mount point under the read-only root, and a
        // missing target is also a host whose layout this was not written for.
        if !target.is_dir() {
            return Err(format!(
                "{} does not exist on this host, so the run cannot mask it — refusing rather than \
                 running with an endpoint root unmasked",
                target.display()
            ));
        }
        out.push(Mask {
            dev_ino: dev_ino(&source)?,
            target,
            source,
        });
    }
    Ok(out)
}

/// The operator's home, if it is a directory `bwrap` can safely replace. `/`
/// never is.
pub fn home_to_mask() -> Option<PathBuf> {
    let (_, home) = sandbox::passwd_entry()?;
    (home.is_dir() && home != Path::new("/")).then_some(home)
}

/// The bubblewrap invocation for a run, minus the command.
///
/// Order is load-bearing: a later mount lands on top of an earlier one, so the
/// tmpfs over the home must come before `$S` is bound back into it, and the
/// read-only `/` before everything.
pub fn bwrap_args(plan: &Plan) -> Vec<String> {
    let mut a: Vec<String> = [
        "--unshare-all",
        "--unshare-user",
        "--disable-userns",
        "--assert-userns-disabled",
        "--die-with-parent",
        "--new-session",
        "--ro-bind",
        "/",
        "/",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(home) = &plan.masked_home {
        a.extend([
            "--size".into(),
            "1048576".into(),
            "--tmpfs".into(),
            home.display().to_string(),
        ]);
    }
    for p in &plan.tmpfs {
        a.extend([
            "--size".into(),
            "1048576".into(),
            "--tmpfs".into(),
            p.display().to_string(),
        ]);
    }
    let root = plan.root.display().to_string();
    a.extend(["--bind".into(), root.clone(), root.clone()]);
    for m in &plan.masks {
        a.extend([
            "--bind".into(),
            m.source.display().to_string(),
            m.target.display().to_string(),
        ]);
    }
    a.extend([
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--chdir".into(),
        root,
        "--clearenv".into(),
    ]);
    for (k, v) in &plan.env {
        a.extend(["--setenv".into(), k.clone(), v.clone()]);
    }
    a
}

/// A trivial run of the same kind of namespace, before anything expensive
/// happens. It proves `bwrap` exists and unprivileged user namespaces work on
/// this host; the inner half's own checks are what prove a given run's mounts.
pub fn bwrap_smoke() -> Result<String, String> {
    let version = std::process::Command::new("bwrap")
        .arg("--version")
        .env_clear()
        .output()
        .map_err(|e| format!("bwrap is required and could not be run: {e}"))?;
    let out = std::process::Command::new("bwrap")
        .args([
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--clearenv",
            "--",
            "/bin/true",
        ])
        .env_clear()
        .output()
        .map_err(|e| format!("bwrap smoke: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "an unprivileged bubblewrap namespace does not work on this host ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(format!(
        "{} — an unprivileged `--unshare-all --disable-userns` namespace starts on this host",
        String::from_utf8_lossy(&version.stdout).trim()
    ))
}

/// Everything the inner half proves about its own namespace before it starts
/// any deck process. Any failure — including a check that cannot be evaluated —
/// is an `Err`.
pub fn check_namespace(plan: &Plan, recorded_mnt: &str) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    let now = proc::namespace("self", "mnt").ok_or("cannot read /proc/self/ns/mnt")?;
    if now != recorded_mnt {
        return Err(format!(
            "mount namespace is now {now}, recorded {recorded_mnt} at startup"
        ));
    }
    if now == plan.outer_mnt_ns {
        return Err(format!(
            "the inner half is in the OUTER mount namespace ({now}) — the run is not isolated"
        ));
    }
    notes.push(format!(
        "mount namespace {now}, differing from the outer half's {}",
        plan.outer_mnt_ns
    ));
    for m in &plan.masks {
        let inside = dev_ino(&m.target)?;
        if inside != m.dev_ino {
            return Err(format!(
                "{} is dev:ino {}:{} inside the namespace, but {} is {}:{} — the mask is not in place",
                m.target.display(),
                inside.0,
                inside.1,
                m.source.display(),
                m.dev_ino.0,
                m.dev_ino.1
            ));
        }
        notes.push(format!(
            "`{}` is `{}` (dev:ino {}:{} on both sides of the mount)",
            m.target.display(),
            m.source.display(),
            inside.0,
            inside.1
        ));
    }
    if let Some(home) = &plan.masked_home {
        // Only the path down to `$S` may exist inside the masked home.
        let allowed = plan
            .root
            .strip_prefix(home)
            .ok()
            .and_then(|rel| rel.components().next())
            .map(|c| c.as_os_str().to_os_string());
        let entries: Vec<_> = std::fs::read_dir(home)
            .map_err(|e| format!("read masked home {}: {e}", home.display()))?
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        for name in &entries {
            if Some(name) != allowed.as_ref() {
                return Err(format!(
                    "the operator's home {} is not masked: {:?} is visible inside the namespace",
                    home.display(),
                    name
                ));
            }
        }
        notes.push(format!(
            "the operator's home `{}` is an empty tmpfs inside the namespace ({} visible entr{}: \
             the path down to the sandbox)",
            home.display(),
            entries.len(),
            if entries.len() == 1 { "y" } else { "ies" }
        ));
    }
    if plan.masked_home.is_none() {
        notes.push(
            "**no home was masked**: the password database names no home that is a directory \
             other than `/` (`home_to_mask`), so nothing under a home is hidden inside the \
             namespace"
                .to_string(),
        );
    }
    for p in &plan.tmpfs {
        let n = std::fs::read_dir(p)
            .map_err(|e| format!("read masked {}: {e}", p.display()))?
            .count();
        if n != 0 {
            return Err(format!(
                "{} is not masked: {n} entr{} visible inside the namespace",
                p.display(),
                if n == 1 { "y" } else { "ies" }
            ));
        }
        notes.push(format!(
            "`{}` is an empty tmpfs inside the namespace",
            p.display()
        ));
    }
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(info) => {
            let RwMounts { residual, covered } = residual_rw_mounts(&info, plan);
            notes.extend(describe_covered(&covered));
            notes.push(if residual.is_empty() {
                "no residual `rw` submount: every mount point outside the run's own is read-only"
                    .to_string()
            } else {
                format!(
                    "**exposure, not a failure**: {} residual `rw` submount(s) the read-only root \
                     did not remount, counted as writable because root ownership of a mount \
                     point does not prove otherwise (mode bits, ACLs, group access and FUSE all \
                     decide beneath it): {}",
                    residual.len(),
                    residual
                        .iter()
                        .map(|m| format!("`{m}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        }
        Err(e) => notes.push(format!(
            "**residual `rw` submounts unknown**: /proc/self/mountinfo could not be read ({e})"
        )),
    }
    for (kind, label) in [("pid", "PID"), ("net", "network"), ("user", "user")] {
        let id = proc::namespace("self", kind).ok_or_else(|| format!("cannot read ns/{kind}"))?;
        notes.push(format!("{label} namespace {id}"));
    }
    Ok(notes)
}

/// A `/proc/self/mountinfo` path field with its octal escapes decoded.
fn unescape_mount_path(field: &str) -> String {
    let mut out = String::new();
    let mut rest = field;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let esc = rest
            .get(i + 1..i + 4)
            .and_then(|o| u8::from_str_radix(o, 8).ok());
        match esc {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The runtime namespace's `rw` mounts that are not its own: `$S` and what is
/// under it, the masks, the masked home's tmpfs, the tmpfs masks, the private
/// `/proc` and the `/dev` bwrap builds. What remains is what the read-only `/`
/// did not reach — the residual ones reported, not proven unwritable.
pub fn residual_rw_mounts(mountinfo: &str, plan: &Plan) -> RwMounts {
    let mut masks: Vec<&Path> = plan.masks.iter().map(|m| m.target.as_path()).collect();
    masks.extend(plan.masked_home.as_deref());
    masks.extend(plan.tmpfs.iter().map(PathBuf::as_path));
    masks.extend([Path::new("/proc"), Path::new("/dev")]);
    classify_rw_mounts(
        mountinfo,
        |mp: &Path| mp.starts_with(&plan.root) || masks.contains(&mp) || mp.starts_with("/dev"),
        &masks,
    )
}

/// The `rw` mounts in a mountinfo table that are not a namespace's own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RwMounts {
    /// Reachable: nothing the namespace mounted lies over them.
    pub residual: Vec<String>,
    /// `(mount point, mask)`: still listed in mountinfo, but beneath a mask the
    /// namespace mounted over them, so no path inside reaches them.
    pub covered: Vec<(String, String)>,
}

/// One `/proc/self/mountinfo` line, as far as reachability needs it.
struct MountEntry {
    id: u64,
    parent: u64,
    mount_point: PathBuf,
    rw: bool,
}

fn parse_mountinfo(mountinfo: &str) -> Vec<MountEntry> {
    mountinfo
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            Some(MountEntry {
                id: f.first()?.parse().ok()?,
                parent: f.get(1)?.parse().ok()?,
                mount_point: PathBuf::from(unescape_mount_path(f.get(4)?)),
                rw: f.get(5)?.split(',').any(|o| o == "rw"),
            })
        })
        .collect()
}

/// The mask in `masks` that hides `entry` from every path inside the
/// namespace, if one does.
///
/// A mount is hidden when some mount on its way to the root — itself or an
/// ancestor, `below` here — sits in a parent mount `Q` beneath the mount point
/// of a mask that is ALSO a child of `Q`: path resolution crossing that point
/// in `Q` enters the mask, never `Q`, so nothing mounted in `Q` under it is
/// reached. That is what bwrap's own layering produces: `--ro-bind / /` copies
/// the host's submounts, and a `--tmpfs /run` mounted afterwards becomes a
/// child of the copied `/run`, alongside the copied `/run/docker/netns/*`.
///
/// Decided from the parent ids, not from the order of the mount ids: those are
/// the lowest free number, reused after an unmount, so a mask can carry a
/// lower id than a mount it was layered over (measured: a `/run` mask at 499
/// over a copied netns mount at 528). The mask's mount point must be a STRICT
/// prefix of `below`'s — two children of one parent at the same point is not a
/// layering this reads, so it counts as reachable. A mount made AFTER the mask
/// on a path beneath it is a child of the mask (or of something on it), not of
/// `Q`, so its chain meets no such sibling and it stays reachable; so does
/// anything a later bind copies back out from under a mask.
fn covering_mask(entries: &[MountEntry], entry: &MountEntry, masks: &[&Path]) -> Option<PathBuf> {
    let mut below = entry;
    let mut seen = std::collections::BTreeSet::new();
    while seen.insert(below.id) {
        let q = entries
            .iter()
            .find(|m| m.id == below.parent && m.id != below.id)?;
        if let Some(mask) = entries.iter().find(|d| {
            d.parent == q.id
                && d.id != below.id
                && masks.contains(&d.mount_point.as_path())
                && below.mount_point.starts_with(&d.mount_point)
                && below.mount_point != d.mount_point
        }) {
            return Some(mask.mount_point.clone());
        }
        below = q;
    }
    None
}

/// Every `rw` mount in `mountinfo` for which `own` is false, split into the
/// residual — reachable from inside — and those a mask in `masks`, mounted by
/// the namespace itself, hides ([`covering_mask`]). Shared by the runtime
/// namespace ([`residual_rw_mounts`]) and the build namespace (`buildns.rs`),
/// each with its own idea of which mounts are its own and which are masks.
pub fn classify_rw_mounts(
    mountinfo: &str,
    own: impl Fn(&Path) -> bool,
    masks: &[&Path],
) -> RwMounts {
    let entries = parse_mountinfo(mountinfo);
    let mut out = RwMounts::default();
    for e in entries.iter().filter(|e| e.rw && !own(&e.mount_point)) {
        let mp = e.mount_point.display().to_string();
        match covering_mask(&entries, e, masks) {
            Some(mask) => out.covered.push((mp, mask.display().to_string())),
            None => out.residual.push(mp),
        }
    }
    out.residual.sort();
    out.residual.dedup();
    out.covered.sort();
    out.covered.dedup();
    out
}

/// A note naming the `rw` mounts a mask hides, for the evidence file.
pub fn describe_covered(covered: &[(String, String)]) -> Option<String> {
    (!covered.is_empty()).then(|| {
        format!(
            "{} `rw` mount(s) the read-only root did not remount, each hidden beneath a mask this \
             namespace mounted over it (so no path inside reaches it): {}",
            covered.len(),
            covered
                .iter()
                .map(|(m, by)| format!("`{m}` under `{by}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// Host paths the build and runtime namespaces replace with a small empty
/// tmpfs when they exist, because nothing a run does needs them and a running
/// container leaves `rw` mounts there that a read-only root cannot remount
/// (issue #1413). bwrap cannot create a missing mount point under the
/// read-only root, so a host without one is simply not masked there.
pub const HOST_ONLY_DIRS: &[&str] = &["/var/lib/docker"];

/// The [`HOST_ONLY_DIRS`] present on this host, resolved — the kernel mounts on
/// the resolved path, and that is what mountinfo lists — and without any that
/// lie under one of `already`, which masks them anyway, or that contain one of
/// `keep`, which the namespace needs (a runs root under `/var/lib/docker` would
/// otherwise be buried beneath the mask).
pub fn host_only_dirs_to_mask(already: &[&Path], keep: &[&Path]) -> Vec<PathBuf> {
    host_only_dirs_from(HOST_ONLY_DIRS, already, keep)
}

fn host_only_dirs_from(candidates: &[&str], already: &[&Path], keep: &[&Path]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = candidates
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .filter(|p| p.is_dir() && p != Path::new("/"))
        .filter(|p| !already.iter().any(|m| p.starts_with(m)))
        .filter(|p| !keep.iter().any(|k| k.starts_with(p)))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The kernel-level answer to "who is listening at `path`": the listening
/// socket inodes bound to that pathname in the caller's network namespace.
pub fn listening_inodes(listeners: &[proc::UnixListener], path: &Path) -> Vec<u64> {
    let want = path.to_string_lossy();
    listeners
        .iter()
        .filter(|l| l.path == want)
        .map(|l| l.inode)
        .collect()
}

// ---------------------------------------------------------------------------
// The outer half's view: the host
// ---------------------------------------------------------------------------

/// A file's identity at one moment, or its absence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileState {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub mtime_ns: i128,
}

fn file_state(path: &Path) -> Result<Option<FileState>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(md) => Ok(Some(FileState {
            dev: md.dev(),
            ino: md.ino(),
            mode: md.mode(),
            mtime_ns: md.mtime() as i128 * 1_000_000_000 + md.mtime_nsec() as i128,
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("lstat {}: {e}", path.display())),
    }
}

/// One host endpoint candidate: its file, the listening sockets bound to it in
/// the host network namespace, and who held them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostEndpoint {
    pub path: PathBuf,
    pub file: Option<FileState>,
    pub inodes: Vec<u64>,
    /// `(pid, start time)` of every process that held one of `inodes` at
    /// baseline.
    pub owners: Vec<(i32, u64)>,
}

/// Every host path a deck on this uid could resolve an endpoint to.
pub fn host_candidates(uid: u32) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    v.extend(sandbox::xdg_endpoints(uid));
    v.extend(sandbox::flat_endpoints(uid));
    v.push(sandbox::per_uid_dir(uid));
    v.extend(sandbox::per_uid_endpoints(uid));
    v
}

/// Snapshot the host's endpoint candidates, including who owns each listener.
/// The owner scan walks every readable `/proc/<pid>/fd`, so it runs once, at
/// baseline; [`verify_host`] then re-checks just those owners.
pub fn capture_host(uid: u32) -> Result<Vec<HostEndpoint>, String> {
    let listeners = proc::unix_listeners()?;
    let mut out = Vec::new();
    for path in host_candidates(uid) {
        let inodes = listening_inodes(&listeners, &path);
        let mut owners = Vec::new();
        for inode in &inodes {
            for pid in proc::owners_of(*inode)? {
                if let Some(start) = proc::start_time(pid) {
                    owners.push((pid, start));
                }
            }
        }
        owners.sort_unstable();
        owners.dedup();
        out.push(HostEndpoint {
            file: file_state(&path)?,
            path,
            inodes,
            owners,
        });
    }
    Ok(out)
}

/// Require every host endpoint candidate to be exactly as it was at baseline:
/// same file identity (or still absent), same listening inodes, and every
/// baseline owner still the same process and still holding its socket.
pub fn verify_host(baseline: &[HostEndpoint]) -> Result<(), String> {
    let listeners = proc::unix_listeners()?;
    for b in baseline {
        let file = file_state(&b.path)?;
        if file != b.file {
            return Err(format!(
                "host {} changed: {:?} at baseline, {:?} now",
                b.path.display(),
                b.file,
                file
            ));
        }
        let inodes = listening_inodes(&listeners, &b.path);
        if inodes != b.inodes {
            return Err(format!(
                "host listeners at {} changed: inodes {:?} at baseline, {:?} now",
                b.path.display(),
                b.inodes,
                inodes
            ));
        }
        for (pid, start) in &b.owners {
            if proc::start_time(*pid) != Some(*start) {
                return Err(format!(
                    "the owner of host {} (pid {pid}, start time {start}) is gone or replaced",
                    b.path.display()
                ));
            }
            let held = proc::socket_inodes(*pid)
                .map_err(|e| format!("re-read the owner of host {}: {e}", b.path.display()))?;
            if !b.inodes.iter().any(|i| held.contains(i)) {
                return Err(format!(
                    "pid {pid} no longer holds the listener at host {}",
                    b.path.display()
                ));
            }
        }
    }
    Ok(())
}

pub fn describe_host(snapshot: &[HostEndpoint]) -> Vec<String> {
    snapshot
        .iter()
        .map(|e| match &e.file {
            None => format!("host `{}`: absent", e.path.display()),
            Some(f) => format!(
                "host `{}`: dev:ino {}:{}, mode {:o}, mtime_ns {}, listening inodes {:?}, owners \
                 (pid, start time) {:?}",
                e.path.display(),
                f.dev,
                f.ino,
                f.mode & 0o7777,
                f.mtime_ns,
                e.inodes,
                e.owners
            ),
        })
        .collect()
}

/// Host listeners whose path lies under `root`. A run's listeners live in its
/// private network namespace, so the host's table should never show one.
pub fn host_listeners_under(root: &Path) -> Result<Vec<proc::UnixListener>, String> {
    Ok(proc::unix_listeners()?
        .into_iter()
        .filter(|l| Path::new(&l.path).starts_with(root))
        .collect())
}

/// A deck process seen on the host: enough to tell, later, whether it is still
/// the same process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckProcess {
    pub pid: i32,
    pub start_time: u64,
    pub exe: PathBuf,
}

/// Every process whose executable is named `dot-agent-deck` or
/// `dot-agent-deck-*`. By `exe`, not `comm`: Linux truncates `comm` to 15
/// bytes, which is how a name search once missed `dot-agent-deck-linux-amd64`.
/// Read-only — this is a census, and nothing in this harness signals what it
/// finds.
pub fn deck_census() -> Result<Vec<DeckProcess>, String> {
    let mut out = Vec::new();
    for pid in proc::pids()? {
        let Some(exe) = proc::exe_path(pid) else {
            continue;
        };
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = name.trim_end_matches(" (deleted)");
        if (name == "dot-agent-deck" || name.starts_with("dot-agent-deck-"))
            && let Some(start_time) = proc::start_time(pid)
        {
            out.push(DeckProcess {
                pid,
                start_time,
                exe,
            });
        }
    }
    Ok(out)
}

/// Every host process that is still inside the run's PID namespace, or that
/// refers to `root` by working directory, root directory, executable, open file
/// or the run's `DAD_XVER_SANDBOX` marker. After the namespace has exited this
/// must be empty; a non-empty answer is a leak.
pub fn processes_touching(root: &Path, pid_ns: Option<&str>) -> Result<Vec<String>, String> {
    let root_s = root.display().to_string();
    let mut out = Vec::new();
    let me = std::process::id() as i32;
    for pid in proc::pids()? {
        if pid == me {
            continue;
        }
        let mut why = Vec::new();
        if let Some(ns) = pid_ns
            && proc::namespace(&pid.to_string(), "pid").as_deref() == Some(ns)
        {
            why.push(format!("in the run's PID namespace {ns}"));
        }
        for link in ["cwd", "root", "exe"] {
            if let Ok(p) = std::fs::read_link(format!("/proc/{pid}/{link}"))
                && p.starts_with(root)
            {
                why.push(format!("{link} -> {}", p.display()));
            }
        }
        if let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) {
            for fd in fds.flatten() {
                if let Ok(p) = std::fs::read_link(fd.path())
                    && p.starts_with(root)
                {
                    why.push(format!("fd -> {}", p.display()));
                    break;
                }
            }
        }
        if proc::environ(pid).is_some_and(|e| e.get("DAD_XVER_SANDBOX") == Some(&root_s)) {
            why.push("carries this run's DAD_XVER_SANDBOX marker".to_string());
        }
        if !why.is_empty() {
            let cmd = proc::cmdline(pid).unwrap_or_default().join(" ");
            out.push(format!("pid {pid} (`{cmd}`): {}", why.join("; ")));
        }
    }
    Ok(out)
}

/// The operator's real deck log — the file an escaped sandbox process would
/// most likely append to — and how far it reached at baseline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogMark {
    pub path: PathBuf,
    pub ino: u64,
    pub size: u64,
}

pub fn mark_log(path: &Path) -> Option<LogMark> {
    let md = std::fs::metadata(path).ok()?;
    Some(LogMark {
        path: path.to_path_buf(),
        ino: md.ino(),
        size: md.len(),
    })
}

/// Search what was appended to the real log since `mark` for `needle` (the
/// sandbox path). Production appends to this file all the time, so an unchanged
/// mtime is not a usable postcondition; the absence of the sandbox's own path
/// in the new bytes is.
pub fn log_mentions_since(mark: &LogMark, needle: &str) -> Result<(u64, Vec<String>), String> {
    use std::io::{Read, Seek, SeekFrom};
    let md =
        std::fs::metadata(&mark.path).map_err(|e| format!("stat {}: {e}", mark.path.display()))?;
    let from = if md.ino() == mark.ino && md.len() >= mark.size {
        mark.size
    } else {
        0
    };
    let mut f = std::fs::File::open(&mark.path)
        .map_err(|e| format!("open {}: {e}", mark.path.display()))?;
    f.seek(SeekFrom::Start(from))
        .map_err(|e| format!("seek {}: {e}", mark.path.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|e| format!("read {}: {e}", mark.path.display()))?;
    let text = String::from_utf8_lossy(&buf);
    let hits = text
        .lines()
        .filter(|l| l.contains(needle))
        .map(|l| l.chars().take(300).collect())
        .collect();
    Ok((buf.len() as u64, hits))
}

/// Render an environment for the evidence file. Every value is a sandbox path
/// or a fixed setting — the allowlist admits nothing else — but values whose
/// NAME looks like a credential are redacted anyway, so the evidence file can
/// never become the leak.
pub fn render_env(env: &BTreeMap<String, String>) -> String {
    env.iter()
        .map(|(k, v)| {
            if sandbox::credential_like(k) {
                format!("{k}=<redacted>")
            } else {
                format!("{k}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> Plan {
        let sb = Sandbox::at(PathBuf::from("/home/op/code/runs/r1"));
        let spec = EnvSpec {
            mode: EndpointMode::Resolved,
            keep_xdg_runtime_dir: false,
            experimental: false,
            max_lifetime_secs: 1800,
            uid: 1000,
        };
        Plan {
            root: sb.root.clone(),
            uid: 1000,
            user: "op".into(),
            mode: spec.mode,
            keep_xdg_runtime_dir: false,
            experimental: false,
            max_lifetime_secs: 1800,
            previous: "v0.41.0".into(),
            old_version: Some("dot-agent-deck 0.41.0".into()),
            direction: Direction::Forward,
            probe: Probe::Generic,
            fixture: sandbox::FIXTURE_TOML.to_string(),
            extra_env: Vec::new(),
            env: sandbox::run_env(&sb, spec, "op", &[]),
            matrices: vec![EndpointMatrix::for_run(&sb, spec.mode, false, 1000)],
            masks: vec![
                Mask {
                    target: "/tmp".into(),
                    source: sb.fallback_tmp.clone(),
                    dev_ino: (1, 2),
                },
                Mask {
                    target: "/run/user/1000".into(),
                    source: sb.run_user.clone(),
                    dev_ino: (1, 3),
                },
            ],
            masked_home: Some("/home/op".into()),
            tmpfs: vec!["/var/lib/docker".into()],
            outer_mnt_ns: "mnt:[1]".into(),
        }
    }

    #[test]
    fn residual_rw_mounts_are_what_the_read_only_root_did_not_reach() {
        // Laid out the way bwrap lays it out: `--ro-bind / /` copies the host's
        // submounts (ids 14-17 under the copied root 1), and each mask is then
        // mounted as a child of whatever copy it lands on.
        let info = "\
1 0 252:0 / / ro,relatime - ext4 /dev/root rw
2 1 0:30 / /home/op rw,nosuid,nodev - tmpfs tmpfs rw
3 2 252:0 /home/op/code/runs/r1 /home/op/code/runs/r1 rw,relatime - ext4 /dev/root rw
4 1 252:0 /home/op/code/runs/r1/fallback-tmp /tmp rw,relatime - ext4 /dev/root rw
5 16 252:0 /home/op/code/runs/r1/run-user /run/user/1000 rw,relatime - ext4 /dev/root rw
14 1 0:22 / /proc ro,nosuid,nodev,noexec - proc proc rw
6 14 0:5 / /proc rw,nosuid,nodev,noexec - proc proc rw
7 1 0:6 / /dev rw,nosuid - tmpfs tmpfs rw
8 7 0:7 / /dev/pts rw,nosuid,noexec - devpts devpts rw
16 1 0:28 / /run ro,nosuid,nodev - tmpfs tmpfs rw
9 16 0:40 / /run/docker/netns/abc rw - nsfs nsfs rw
10 14 0:41 / /proc/sys/fs/binfmt_misc rw,relatime - autofs systemd-1 rw
11 1 0:42 / /var/lib/docker/rootfs/overlayfs/x rw,relatime - overlay overlay rw
15 1 0:45 / /var/lib/docker rw,nosuid,nodev - tmpfs tmpfs rw
12 1 0:43 / /mnt/with\\040space rw - ext4 /dev/sdb rw
13 1 0:44 / /srv/ro ro,relatime - ext4 /dev/sdc rw
";
        let got = residual_rw_mounts(info, &plan());
        assert_eq!(
            got.residual,
            vec!["/mnt/with space", "/run/docker/netns/abc"],
            "the runtime namespace does not mask `/run`, so Docker's netns mounts stay exposure"
        );
        assert_eq!(
            got.covered,
            vec![
                ("/proc/sys/fs/binfmt_misc".into(), "/proc".into()),
                (
                    "/var/lib/docker/rootfs/overlayfs/x".into(),
                    "/var/lib/docker".into()
                ),
            ]
        );
    }

    #[test]
    fn a_host_only_dir_holding_the_sandbox_or_under_a_mask_is_not_masked() {
        let dir = std::env::temp_dir().join(format!("xver-host-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let docker = dir.join("docker");
        std::fs::create_dir_all(docker.join("runs/r1")).expect("dir");
        let docker = std::fs::canonicalize(&docker).expect("canonical");
        let cand = docker.display().to_string();
        let missing = dir.join("absent").display().to_string();
        let sandbox = docker.join("runs/r1");
        let elsewhere = Path::new("/home/op/code/runs/r1");
        assert_eq!(
            host_only_dirs_from(&[&cand, &missing], &[], &[elsewhere]),
            vec![docker.clone()],
            "an existing dir is masked; a missing one is not"
        );
        assert!(host_only_dirs_from(&[&cand], &[], &[&sandbox]).is_empty());
        assert!(host_only_dirs_from(&[&cand], &[&dir], &[elsewhere]).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_runtime_namespace_masks_var_lib_docker_before_the_sandbox_is_bound() {
        let args = bwrap_args(&plan());
        let docker = pos(&args, &["--tmpfs", "/var/lib/docker"]);
        assert_eq!(args[docker - 2], "--size");
        let back = pos(
            &args,
            &["--bind", "/home/op/code/runs/r1", "/home/op/code/runs/r1"],
        );
        assert!(pos(&args, &["--ro-bind", "/", "/"]) < docker && docker < back);
    }

    /// `/proc/self/mountinfo` from a real `bwrap --ro-bind / / --tmpfs /run
    /// --tmpfs /var/lib/docker` on a host running one container (issue #1413),
    /// trimmed to the lines that matter. The `/run` mask (499) carries a LOWER
    /// id than the netns mount it covers (528): ids are reused, not ordered.
    const MEASURED: &str = "\
503 232 253:1 / / ro,nosuid,nodev,relatime master:1 - ext4 /dev/vda1 rw
521 503 0:28 / /run ro,nosuid,nodev master:12 - tmpfs tmpfs rw
528 521 0:5 net:[4026532620] /run/docker/netns/2b3b370f1677 rw master:181 - nsfs nsfs rw
533 503 0:50 / /var/lib/docker/rootfs/overlayfs/814c7cd18fc9 rw,relatime master:172 - overlay overlay rw
499 521 0:59 / /run rw,nosuid,nodev,relatime - tmpfs tmpfs rw,size=980k
534 503 0:62 / /var/lib/docker rw,nosuid,nodev,relatime - tmpfs tmpfs rw,size=980k
";

    const NETNS: &str = "/run/docker/netns/2b3b370f1677";
    const OVERLAY: &str = "/var/lib/docker/rootfs/overlayfs/814c7cd18fc9";

    fn classify(info: &str, masks: &[&str]) -> RwMounts {
        let masks: Vec<&Path> = masks.iter().map(Path::new).collect();
        classify_rw_mounts(info, |mp| masks.contains(&mp), &masks)
    }

    fn without(info: &str, id: &str) -> String {
        info.lines()
            .filter(|l| !l.starts_with(&format!("{id} ")))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    #[test]
    fn a_rw_mount_beneath_a_mask_mounted_over_it_is_covered_not_residual() {
        let got = classify(MEASURED, &["/run", "/var/lib/docker"]);
        assert_eq!(got.residual, Vec::<String>::new());
        assert_eq!(
            got.covered,
            vec![
                (NETNS.into(), "/run".into()),
                (OVERLAY.into(), "/var/lib/docker".into()),
            ]
        );
    }

    #[test]
    fn with_no_covering_mask_the_same_mounts_are_residual() {
        // The mask absent from mountinfo, and a mask the namespace did not
        // mount (the path is not one of its masks) both leave them reachable.
        let got = classify(&without(MEASURED, "534"), &["/run", "/var/lib/docker"]);
        assert_eq!(got.residual, vec![OVERLAY]);
        let got = classify(MEASURED, &["/run"]);
        assert_eq!(got.residual, vec!["/var/lib/docker", OVERLAY]);
        assert_eq!(got.covered, vec![(NETNS.into(), "/run".into())]);
    }

    #[test]
    fn a_rw_mount_layered_above_the_mask_is_residual() {
        // 540 was mounted on the mask itself; 541 is what a later bind of a
        // host directory into the mask copies along with it.
        let info = format!(
            "{MEASURED}\
540 499 0:70 / /run/later rw - tmpfs tmpfs rw
539 499 253:1 /srv/data /run/data ro - ext4 /dev/vda1 rw
541 539 0:71 / /run/data/sub rw - ext4 /dev/vdb rw
"
        );
        let got = classify(&info, &["/run", "/var/lib/docker"]);
        assert_eq!(got.residual, vec!["/run/data/sub", "/run/later"]);
        assert_eq!(got.covered.len(), 2, "{got:?}");
    }

    #[test]
    fn a_mount_nested_under_a_covered_one_is_covered_too() {
        let info = format!(
            "{MEASURED}\
550 528 0:72 / {NETNS}/inner rw - tmpfs tmpfs rw
"
        );
        let got = classify(&info, &["/run", "/var/lib/docker"]);
        assert!(got.residual.is_empty(), "{got:?}");
        assert!(
            got.covered
                .contains(&(format!("{NETNS}/inner"), "/run".into()))
        );
    }

    #[test]
    fn a_mask_that_is_not_a_strict_prefix_does_not_cover() {
        // A sibling mask elsewhere in the same parent, and a sibling at the
        // very same mount point, which is not a layering this reads.
        let info = "\
1 0 253:1 / / ro - ext4 /dev/vda1 rw
2 1 0:28 / /run ro - tmpfs tmpfs rw
3 2 0:5 / /run/docker/netns/a rw - nsfs nsfs rw
4 2 0:59 / /run/user rw - tmpfs tmpfs rw
5 2 0:60 / /run/docker/netns/a rw - tmpfs tmpfs rw
";
        let got = classify(info, &["/run/user", "/run/docker/netns/a"]);
        assert_eq!(got.residual, Vec::<String>::new());
        let got = classify_rw_mounts(
            info,
            |mp| mp == Path::new("/run/user"),
            &[Path::new("/run/user"), Path::new("/run/docker/netns/a")],
        );
        assert_eq!(got.residual, vec!["/run/docker/netns/a"]);
        assert!(got.covered.is_empty(), "{got:?}");
    }

    #[test]
    fn a_parent_cycle_or_a_parent_outside_the_table_is_not_covered() {
        let info = "\
7 8 0:5 / /run/a rw - nsfs nsfs rw
8 7 0:6 / /run rw - tmpfs tmpfs rw
9 7 0:7 / /run rw - tmpfs tmpfs rw
10 99 0:8 / /x/y rw - ext4 /dev/vdb rw
";
        let got = classify_rw_mounts(info, |_| false, &[Path::new("/x")]);
        assert!(got.residual.contains(&"/x/y".to_string()), "{got:?}");
        assert!(got.residual.contains(&"/run/a".to_string()), "{got:?}");
    }

    fn pos(args: &[String], needle: &[&str]) -> usize {
        args.windows(needle.len())
            .position(|w| w.iter().zip(needle).all(|(a, b)| a == b))
            .unwrap_or_else(|| panic!("{needle:?} not in {args:?}"))
    }

    #[test]
    fn the_namespace_masks_both_endpoint_roots_over_a_read_only_root() {
        let args = bwrap_args(&plan());
        let ro = pos(&args, &["--ro-bind", "/", "/"]);
        let tmp = pos(
            &args,
            &["--bind", "/home/op/code/runs/r1/fallback-tmp", "/tmp"],
        );
        let run = pos(
            &args,
            &["--bind", "/home/op/code/runs/r1/run-user", "/run/user/1000"],
        );
        assert!(
            ro < tmp && ro < run,
            "the masks must land on top of the read-only root"
        );
        for flag in [
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ] {
            assert!(
                args.iter().any(|a| a == flag),
                "{flag} missing from {args:?}"
            );
        }
    }

    #[test]
    fn the_home_is_masked_before_the_sandbox_is_bound_back_into_it() {
        let args = bwrap_args(&plan());
        let home = pos(&args, &["--tmpfs", "/home/op"]);
        let back = pos(
            &args,
            &["--bind", "/home/op/code/runs/r1", "/home/op/code/runs/r1"],
        );
        assert!(
            home < back,
            "binding $S first would bury it under the tmpfs"
        );
    }

    #[test]
    fn the_namespace_environment_is_exactly_the_plan_env() {
        let p = plan();
        let args = bwrap_args(&p);
        let clear = pos(&args, &["--clearenv"]);
        let setenvs: Vec<(String, String)> = args[clear + 1..]
            .chunks(3)
            .map(|c| {
                assert_eq!(c[0], "--setenv");
                (c[1].clone(), c[2].clone())
            })
            .collect();
        assert_eq!(setenvs, p.env);
    }

    #[test]
    fn the_host_candidates_cover_every_resolution_rule() {
        let c = host_candidates(1000);
        for want in [
            "/run/user/1000/dot-agent-deck.sock",
            "/run/user/1000/dot-agent-deck-attach.sock",
            "/tmp/dot-agent-deck-1000.sock",
            "/tmp/dot-agent-deck-attach-1000.sock",
            "/tmp/dot-agent-deck-1000",
            "/tmp/dot-agent-deck-1000/hook.sock",
            "/tmp/dot-agent-deck-1000/attach.sock",
        ] {
            assert!(c.contains(&PathBuf::from(want)), "{want}");
        }
    }

    #[test]
    fn a_host_snapshot_detects_a_new_file() {
        let dir = std::env::temp_dir().join(format!("xver-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("endpoint.sock");
        let baseline = vec![HostEndpoint {
            file: file_state(&path).expect("stat"),
            path: path.clone(),
            inodes: vec![],
            owners: vec![],
        }];
        verify_host(&baseline).expect("unchanged");
        std::fs::write(&path, b"").expect("create");
        assert!(verify_host(&baseline).unwrap_err().contains("changed"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn this_test_process_touches_nothing_under_an_unrelated_root() {
        let hits =
            processes_touching(Path::new("/definitely/not/a/sandbox/9f3a"), None).expect("scan");
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn rendered_environments_redact_credential_shaped_names() {
        let env: BTreeMap<String, String> = [
            ("HOME".to_string(), "/s/home".to_string()),
            ("SOME_TOKEN".to_string(), "hunter2".to_string()),
        ]
        .into();
        let out = render_env(&env);
        assert!(out.contains("HOME=/s/home"));
        assert!(out.contains("SOME_TOKEN=<redacted>") && !out.contains("hunter2"));
    }
}
