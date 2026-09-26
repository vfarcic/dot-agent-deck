//! The build namespace (issue #1212): where the branch's `cargo build`, and the
//! build-time gate's `cargo metadata --no-deps`, execute.
//!
//! # Why a second namespace
//!
//! The runtime namespace (`isolation.rs`) exists before the deck processes it
//! contains, but the branch build ran before it, on the host, as the operator:
//! every build script, proc macro and cargo-configured linker it executed had
//! the operator's files, processes, sockets, credentials and network. The
//! build-time gate (`buildgate.rs`) detects a branch that SUPPLIES build-time
//! code; it contains nothing, and mainline's own build-time code and every
//! registry crate's ran on every build regardless.
//!
//! So Cargo now runs in a bubblewrap namespace of its own, shaped like the
//! runtime one:
//!
//! * the rest of `/` read-only, the operator's home an empty tmpfs, and `/tmp`,
//!   `/var/tmp` and `/run` private tmpfs mounts — `/run` whole, not just
//!   `/run/user/<uid>`, because the build needs nothing there and it holds the
//!   system's sockets (the Docker socket among them on the box this was written
//!   on); the operator's Cargo home is masked too when it lives outside the home;
//! * bound back at their own absolute paths: the build clone **read-only** (so
//!   `build.rs`'s `git describe` and cargo's fingerprints keep working, and build
//!   code cannot plant a hook or a config in it), the trust domain's target dir
//!   **read-write**, and the Rust toolchain's sysroot read-only when it lives
//!   under the home;
//! * a fresh Cargo home in the private `/tmp`, with the fetch phase's `registry`
//!   bound read-only beneath it — never the operator's Cargo home, whose
//!   `config.toml`, `credentials.toml` and `bin/` stay outside;
//! * every pathname socket listening in the host's network namespace when the
//!   plan is made, and still visible through the binds, covered by a read-only
//!   bind of `/dev/null`, so `connect(2)` on it is refused (a read-only mount does
//!   not stop `connect(2)` on a Unix socket; on the box this was written on the
//!   one such socket was the nix daemon's, which can fetch over the network on a
//!   client's behalf);
//! * private PID, network, IPC, UTS and user namespaces, nested user namespaces
//!   disabled, `--die-with-parent`, `--new-session`;
//! * an environment built from an allowlist ([`env_for`]), never the caller's.
//!
//! # How each run is measured
//!
//! Bubblewrap does not start Cargo directly. It starts a read-only copy of this
//! binary with [`PROBE_FLAG`], which measures the namespace it is in — the
//! checks in [`probe`] — writes the result as the FIRST line of its stderr, and
//! only then `exec`s Cargo, in the same namespace and with the same environment.
//! A failed check exits before Cargo exists. The report cannot be forged by the
//! build: nothing the build prints can precede the line the probe wrote before
//! Cargo started, and the outer half reads the first marker line only.
//!
//! What the probe measures is the namespace as a process bubblewrap started sees
//! it. A build script sees that namespace plus the variables Cargo itself sets
//! for it (`OUT_DIR`, `CARGO_*` and the like); the hostile-`build.rs` proof in
//! `docs/develop/cross-version-harness.md` is the measurement from inside one.
//!
//! # The fetch phase
//!
//! The build is offline (`--offline`, and no network in the namespace), so the
//! crates it needs are downloaded first, on the host, by `cargo fetch --locked`
//! ([`fetch`]). That runs no build script or proc macro, but Cargo configuration
//! can name executables (a credential provider, `net.git-fetch-with-cli`) and
//! redirect sources, so it runs where no configuration the branch or the
//! operator supplies is read: from `/` with `--manifest-path` (Cargo reads
//! configuration from its working directory's hierarchy, not the manifest's —
//! measured on Cargo 1.98.1), with a Cargo home the harness owns and an
//! environment of `PATH`, that Cargo home, and git switched off from the
//! operator's global and system configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{isolation, proc, sandbox};

/// The hidden flag bubblewrap starts this binary with inside the namespace.
pub const PROBE_FLAG: &str = "--build-probe";

/// Prefix of the probe's one report line on stderr.
const REPORT_MARKER: &str = "XVER-BUILD-PROBE ";

/// Where the read-only copy of this binary is bound inside, under the masked
/// `/run`.
pub const HARNESS_IN_NS: &str = "/run/xver/harness";

/// The build's Cargo home: a directory in the namespace's private `/tmp`, fresh
/// on every run, so nothing written to it — a `config.toml` a build script left
/// behind included — outlives the build that wrote it.
pub const CARGO_HOME_IN_NS: &str = "/tmp/xver-cargo-home";

/// Size caps for the private tmpfs mounts. They are RAM-backed (CLAUDE.md rule
/// 14), so each is bounded; `/tmp` holds the Cargo home's bookkeeping and the
/// compiler's and linker's temporary files, the others nothing the build needs.
const TMP_BYTES: u64 = 1 << 30;
const SMALL_BYTES: u64 = 64 << 20;

/// Everything bubblewrap is told, minus the command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shape {
    /// The operator's home, replaced by an empty tmpfs; `None` on a host whose
    /// password-database home is `/` or not a directory.
    pub home: Option<PathBuf>,
    /// Other paths replaced by an empty tmpfs, with a size cap.
    pub tmpfs: Vec<(PathBuf, u64)>,
    /// Bound read-only at their own path, after the masks.
    pub ro: Vec<PathBuf>,
    /// Bound read-write at their own path, after the masks.
    pub rw: Vec<PathBuf>,
    /// `(source, destination)`, bound read-only after the same-path binds.
    pub ro_at: Vec<(PathBuf, PathBuf)>,
    /// Host listening sockets still visible through the binds, each covered by
    /// a read-only bind of `/dev/null`, last.
    pub nulled_sockets: Vec<PathBuf>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

impl Shape {
    /// Every path replaced by something private: the home, the tmpfs masks,
    /// and the fresh `/proc` and `/dev`.
    fn masks(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = self.home.iter().map(PathBuf::as_path).collect();
        v.extend(self.tmpfs.iter().map(|(p, _)| p.as_path()));
        v.push(Path::new("/proc"));
        v.push(Path::new("/dev"));
        v
    }

    /// Where a host path is visible inside the namespace — at its own path when
    /// a same-path bind covers it or no mask does, and under a remapped bind's
    /// destination when that bind's source covers it. Empty when masked.
    pub fn visible_at(&self, host: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let bound = self.ro.iter().chain(&self.rw).any(|b| host.starts_with(b));
        let masked = self.masks().iter().any(|m| host.starts_with(m));
        if bound || !masked {
            out.push(host.to_path_buf());
        }
        for (src, dst) in &self.ro_at {
            if let Ok(rel) = host.strip_prefix(src) {
                out.push(dst.join(rel));
            }
        }
        out
    }

    /// Every bind destination, which is what may exist inside the masked home.
    fn destinations(&self) -> Vec<&Path> {
        let mut v: Vec<&Path> = self
            .ro
            .iter()
            .chain(&self.rw)
            .map(PathBuf::as_path)
            .collect();
        v.extend(self.ro_at.iter().map(|(_, d)| d.as_path()));
        v
    }
}

/// The bubblewrap invocation for `shape`, with `harness` bound read-only at
/// [`HARNESS_IN_NS`], minus the command.
///
/// Order is load-bearing, as in `isolation::bwrap_args`: a later mount lands on
/// top of an earlier one, so the read-only `/` comes first, the masks next, the
/// binds that re-expose paths under the masks after them, and the `/dev/null`
/// socket covers last of all, on top of whatever bind made the socket visible.
pub fn bwrap_args(shape: &Shape, harness: &Path) -> Vec<String> {
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
    let s = |p: &Path| p.display().to_string();
    if let Some(home) = &shape.home {
        a.extend([
            "--size".into(),
            SMALL_BYTES.to_string(),
            "--tmpfs".into(),
            s(home),
        ]);
    }
    for (p, size) in &shape.tmpfs {
        a.extend(["--size".into(), size.to_string(), "--tmpfs".into(), s(p)]);
    }
    for p in &shape.ro {
        a.extend(["--ro-bind".into(), s(p), s(p)]);
    }
    for p in &shape.rw {
        a.extend(["--bind".into(), s(p), s(p)]);
    }
    for (src, dst) in &shape.ro_at {
        a.extend(["--ro-bind".into(), s(src), s(dst)]);
    }
    for p in &shape.nulled_sockets {
        a.extend(["--ro-bind".into(), "/dev/null".into(), s(p)]);
    }
    a.extend([
        "--ro-bind".into(),
        s(harness),
        HARNESS_IN_NS.into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--chdir".into(),
        s(&shape.cwd),
        "--clearenv".into(),
    ]);
    for (k, v) in &shape.env {
        a.extend(["--setenv".into(), k.clone(), v.clone()]);
    }
    a
}

// ---------------------------------------------------------------------------
// The host facts a plan is made from
// ---------------------------------------------------------------------------

/// The Rust toolchain the build uses, resolved on the host.
#[derive(Clone, Debug)]
pub struct Toolchain {
    pub sysroot: PathBuf,
    pub bin: PathBuf,
    pub cargo: PathBuf,
    pub version: String,
}

impl Toolchain {
    pub fn describe(&self) -> String {
        format!(
            "toolchain: sysroot `{}` (`{}`), resolved on the host by `rustc --print sysroot` run \
             from `/`; the build namespace runs its `bin/cargo` and `bin/rustc` directly, not \
             through a rustup proxy, so a toolchain file in the branch selects nothing",
            self.sysroot.display(),
            self.version
        )
    }
}

/// Resolve the toolchain the caller's `rustc` names.
///
/// `rustc --print sysroot` runs from `/`, so no project directory's toolchain
/// file applies, with the caller's environment minus credential-shaped names
/// and the deck's own variables — the rustup proxy reads `RUSTUP_HOME`,
/// `RUSTUP_TOOLCHAIN` and its settings from it. It executes the proxy and the
/// compiler it selects, and nothing of the branch's. A sysroot carrying no
/// `bin/cargo` — a nix or devbox `rustc` package, whose cargo is a separate
/// store path — is refused rather than widened into: the build namespace's
/// `PATH` is that one directory plus the system's.
pub fn resolve_toolchain() -> Result<Toolchain, String> {
    let mut cmd = Command::new("rustc");
    cmd.args(["--print", "sysroot"])
        .current_dir("/")
        .stdin(Stdio::null());
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        if sandbox::credential_like(&name) || name.starts_with("DOT_AGENT_DECK_") {
            cmd.env_remove(&k);
        }
    }
    let out = cmd
        .output()
        .map_err(|e| format!("rustc --print sysroot: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "rustc --print sysroot failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let sysroot = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let sysroot = std::fs::canonicalize(&sysroot)
        .map_err(|e| format!("canonicalize the sysroot {}: {e}", sysroot.display()))?;
    let bin = sysroot.join("bin");
    let cargo = bin.join("cargo");
    let rustc = bin.join("rustc");
    for tool in [&cargo, &rustc] {
        if !tool.is_file() {
            return Err(format!(
                "the toolchain at {} carries no {}: the build namespace runs a toolchain whose \
                 sysroot holds both `cargo` and `rustc` (a rustup toolchain), and refuses rather \
                 than widening its `PATH`. Run from a shell whose `rustc` resolves to one.",
                sysroot.display(),
                tool.display()
            ));
        }
    }
    let v = Command::new(&rustc)
        .arg("-V")
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("{} -V: {e}", rustc.display()))?;
    let version = String::from_utf8_lossy(&v.stdout).trim().to_string();
    Ok(Toolchain {
        sysroot,
        bin,
        cargo,
        version,
    })
}

/// What the outer half knows about the host when it plans a namespace.
#[derive(Clone, Debug)]
pub struct Host {
    pub uid: u32,
    /// The operator's home, canonical; masked.
    pub home: Option<PathBuf>,
    /// The operator's Cargo home — `$CARGO_HOME`, or `~/.cargo` — canonical
    /// when it exists.
    pub cargo_home: Option<PathBuf>,
    pub outer_mnt: String,
    pub outer_pid: String,
    pub outer_net: String,
    /// This binary, bound read-only into every build namespace as the probe.
    pub harness: PathBuf,
}

impl Host {
    pub fn capture(uid: u32) -> Result<Self, String> {
        let home = isolation::home_to_mask().and_then(|h| std::fs::canonicalize(h).ok());
        let cargo_home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".cargo")))
            .and_then(|p| std::fs::canonicalize(p).ok());
        Ok(Host {
            uid,
            home,
            cargo_home,
            outer_mnt: proc::namespace("self", "mnt").ok_or("cannot read /proc/self/ns/mnt")?,
            outer_pid: proc::namespace("self", "pid").ok_or("cannot read /proc/self/ns/pid")?,
            outer_net: proc::namespace("self", "net").ok_or("cannot read /proc/self/ns/net")?,
            harness: std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?,
        })
    }
}

/// Every pathname socket listening in the host's network namespace now, by
/// canonical path, that still exists as a socket. A path whose parent resolves
/// through a symlink (`/var/run` → `/run`) is recorded where it really is, which
/// is what decides whether a mask covers it. Sockets in other network
/// namespaces, and a socket's other hard-linked names, are not in the table.
pub fn host_sockets() -> Result<Vec<PathBuf>, String> {
    let mut out = BTreeSet::new();
    for l in proc::unix_listeners()? {
        let p = Path::new(&l.path);
        let (Some(parent), Some(name)) = (p.parent(), p.file_name()) else {
            continue;
        };
        let Ok(parent) = std::fs::canonicalize(parent) else {
            continue;
        };
        let p = parent.join(name);
        if std::fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_socket()) {
            out.insert(p);
        }
    }
    Ok(out.into_iter().collect())
}

/// The environment of every process in a build namespace: an allowlist, not
/// the caller's environment. No `RUSTC`, `RUSTC_WRAPPER`,
/// `RUSTC_WORKSPACE_WRAPPER`, `RUSTFLAGS` or `CARGO_*` configuration variable
/// is admitted, and no `DAD_*` build seam — `DAD_BUILD_GATE_DIR` among them, so
/// the link gate's pool is the namespace's own (see [`LINK_POOL_NOTE`]).
pub fn env_for(
    tc: &Toolchain,
    home: Option<&Path>,
    target: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env = vec![
        (
            "PATH".to_string(),
            format!("{}:/usr/local/bin:/usr/bin:/bin", tc.bin.display()),
        ),
        (
            "HOME".to_string(),
            home.map_or_else(|| "/tmp".to_string(), |h| h.display().to_string()),
        ),
        ("CARGO_HOME".to_string(), CARGO_HOME_IN_NS.to_string()),
        ("TMPDIR".to_string(), "/tmp".to_string()),
        ("LC_ALL".to_string(), "C.UTF-8".to_string()),
        ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
    ];
    if let Some(t) = target {
        env.push(("CARGO_TARGET_DIR".to_string(), t.display().to_string()));
    }
    env
}

/// What the evidence file says about the linker pool, whichever domain built.
pub const LINK_POOL_NOTE: &str = "linker pool: the link gate (`scripts/link-gate.sh`, which execs \
     `scripts/build-gate.sh`) ran inside the namespace against its default pool under the private \
     `/tmp` — `DAD_BUILD_GATE_DIR` is not admitted — so it bounded this build's own concurrent \
     links and nothing else. The build was **not** gated against other builds on the host, and \
     the machine-wide pool was never bound into it";

fn base_shape(host: &Host, tc: &Toolchain, cwd: &Path, target: Option<&Path>) -> Shape {
    let mut tmpfs = vec![
        (PathBuf::from("/tmp"), TMP_BYTES),
        (PathBuf::from("/var/tmp"), SMALL_BYTES),
        (PathBuf::from("/run"), SMALL_BYTES),
    ];
    let mut ro = Vec::new();
    let mut shape_masks: Vec<PathBuf> = host.home.iter().cloned().collect();
    shape_masks.extend(tmpfs.iter().map(|(p, _)| p.clone()));
    if let Some(ch) = &host.cargo_home
        && ch.is_dir()
        && !shape_masks.iter().any(|m| ch.starts_with(m))
    {
        tmpfs.push((ch.clone(), SMALL_BYTES));
    }
    if host
        .home
        .as_ref()
        .is_some_and(|h| tc.sysroot.starts_with(h))
    {
        ro.push(tc.sysroot.clone());
    }
    Shape {
        home: host.home.clone(),
        tmpfs,
        ro,
        rw: Vec::new(),
        ro_at: Vec::new(),
        nulled_sockets: Vec::new(),
        cwd: cwd.to_path_buf(),
        env: env_for(tc, host.home.as_deref(), target),
    }
}

/// Cover every host socket the shape leaves visible.
fn cover_sockets(shape: &mut Shape, sockets: &[PathBuf]) {
    let mut covered = BTreeSet::new();
    for s in sockets {
        for p in shape.visible_at(s) {
            covered.insert(p);
        }
    }
    shape.nulled_sockets = covered.into_iter().collect();
}

fn probe_plan(
    what: &str,
    host: &Host,
    shape: Shape,
    read_only: Vec<PathBuf>,
    sockets: Vec<PathBuf>,
) -> ProbePlan {
    let mut absent = Vec::new();
    for dir in host
        .cargo_home
        .iter()
        .cloned()
        .chain([PathBuf::from(CARGO_HOME_IN_NS)])
    {
        for f in ["config.toml", "config", "credentials.toml", "credentials"] {
            absent.push(dir.join(f));
        }
    }
    ProbePlan {
        what: what.to_string(),
        shape,
        outer_mnt: host.outer_mnt.clone(),
        outer_pid: host.outer_pid.clone(),
        outer_net: host.outer_net.clone(),
        read_only,
        absent,
        production: isolation::host_candidates(host.uid),
        host_sockets: sockets,
    }
}

/// The namespace the branch build runs in.
///
/// The clone, the target dir and the fetch-phase Cargo home must not nest: a
/// read-write target dir inside either of the others would make that part of
/// it writable — the Cargo home's `registry` is bound read-only only at its
/// remapped path — and either of them inside the target dir would be reachable
/// read-write through the target dir's bind.
pub fn build_plan(
    host: &Host,
    tc: &Toolchain,
    clone: &Path,
    target: &Path,
    fetch_home: &Path,
    sockets: Vec<PathBuf>,
) -> Result<ProbePlan, String> {
    let paths = [
        ("the build clone", clone),
        ("the target dir", target),
        ("the fetch-phase Cargo home", fetch_home),
    ];
    for (i, (a, pa)) in paths.iter().enumerate() {
        for (b, pb) in &paths[i + 1..] {
            if pa.starts_with(pb) || pb.starts_with(pa) {
                return Err(format!(
                    "{a} {} and {b} {} overlap; the build namespace binds the target dir \
                     read-write and the other two read-only, so all three must be disjoint",
                    pa.display(),
                    pb.display()
                ));
            }
        }
    }
    let mut shape = base_shape(host, tc, clone, Some(target));
    shape.ro.push(clone.to_path_buf());
    shape.rw.push(target.to_path_buf());
    for sub in ["registry", "git"] {
        let src = fetch_home.join(sub);
        if src.is_dir() {
            shape
                .ro_at
                .push((src, Path::new(CARGO_HOME_IN_NS).join(sub)));
        }
    }
    cover_sockets(&mut shape, &sockets);
    Ok(probe_plan(
        "the branch build",
        host,
        shape,
        vec![clone.to_path_buf(), clone.join(".git")],
        sockets,
    ))
}

/// The namespace the build-time gate's `cargo metadata --no-deps` runs in: the
/// merge-base's extracted tree read-only as the working directory, no target
/// dir, and no registry — `--no-deps --offline` reads neither.
pub fn metadata_plan(host: &Host, tc: &Toolchain, tree: &Path, sockets: Vec<PathBuf>) -> ProbePlan {
    let mut shape = base_shape(host, tc, tree, None);
    shape.ro.push(tree.to_path_buf());
    cover_sockets(&mut shape, &sockets);
    probe_plan(
        "the build-time gate's `cargo metadata`",
        host,
        shape,
        vec![tree.to_path_buf()],
        sockets,
    )
}

// ---------------------------------------------------------------------------
// The probe: inside the namespace
// ---------------------------------------------------------------------------

/// What the probe is told, as JSON in its argv.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbePlan {
    pub what: String,
    pub shape: Shape,
    pub outer_mnt: String,
    pub outer_pid: String,
    pub outer_net: String,
    /// Must refuse a write.
    pub read_only: Vec<PathBuf>,
    /// Must not exist: the operator's Cargo configuration and credentials, and
    /// the private Cargo home's.
    pub absent: Vec<PathBuf>,
    /// The host's standard deck endpoint candidates: must not exist inside.
    pub production: Vec<PathBuf>,
    /// Every pathname socket listening on the host at plan time: none may
    /// accept a connection from inside.
    pub host_sockets: Vec<PathBuf>,
}

/// What the probe measured.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ProbeReport {
    pub pid_ns: String,
    pub notes: Vec<String>,
    pub failures: Vec<String>,
}

/// `--build-probe <plan json> -- <command…>`: measure, report, then exec.
pub fn probe_main(args: &[OsString]) -> ExitCode {
    let (Some(json), Some(sep)) = (args.first(), args.get(1)) else {
        eprintln!("xver: {PROBE_FLAG} needs a plan and a command");
        return ExitCode::FAILURE;
    };
    let cmd = &args[2..];
    if sep != "--" || cmd.is_empty() {
        eprintln!("xver: {PROBE_FLAG} <plan> -- <command…>");
        return ExitCode::FAILURE;
    }
    let report = match serde_json::from_str::<ProbePlan>(&json.to_string_lossy()) {
        Ok(plan) => probe(&plan),
        Err(e) => ProbeReport {
            failures: vec![format!("the probe could not read its plan: {e}")],
            ..Default::default()
        },
    };
    let line = serde_json::to_string(&report).unwrap_or_else(|e| {
        format!(r#"{{"pid_ns":"","notes":[],"failures":["serialise the report: {e}"]}}"#)
    });
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{REPORT_MARKER}{line}");
    let _ = err.flush();
    drop(err);
    if !report.failures.is_empty() {
        return ExitCode::from(3);
    }
    let e = Command::new(&cmd[0])
        .args(&cmd[1..])
        .env_remove("PWD")
        .exec();
    eprintln!("xver: exec {}: {e}", cmd[0].to_string_lossy());
    ExitCode::FAILURE
}

fn errno_name(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(libc::EROFS) => "EROFS".into(),
        Some(libc::EACCES) => "EACCES".into(),
        Some(libc::EPERM) => "EPERM".into(),
        Some(libc::ENOENT) => "ENOENT".into(),
        Some(libc::ECONNREFUSED) => "ECONNREFUSED".into(),
        Some(libc::ENETUNREACH) => "ENETUNREACH".into(),
        _ => e.to_string(),
    }
}

fn fail(r: &mut ProbeReport, m: String) {
    r.failures.push(m);
}

/// Every check, each recorded as a note when it holds and a failure when it
/// does not or cannot be evaluated.
pub fn probe(plan: &ProbePlan) -> ProbeReport {
    let mut r = ProbeReport::default();

    // Namespaces.
    for (kind, outer) in [
        ("mnt", &plan.outer_mnt),
        ("pid", &plan.outer_pid),
        ("net", &plan.outer_net),
    ] {
        match proc::namespace("self", kind) {
            Some(id) if &id == outer => fail(
                &mut r,
                format!("the {kind} namespace is the outer half's ({id}): not isolated"),
            ),
            Some(id) => {
                if kind == "pid" {
                    r.pid_ns = id.clone();
                }
                r.notes.push(format!(
                    "{kind} namespace {id}, differing from the outer half's {outer}"
                ));
            }
            None => fail(&mut r, format!("cannot read /proc/self/ns/{kind}")),
        }
    }
    for kind in ["user", "ipc", "uts"] {
        if let Some(id) = proc::namespace("self", kind) {
            r.notes.push(format!("{kind} namespace {id}"));
        }
    }

    // Environment: exactly the plan's.
    // bwrap adds one entry of its own after `--chdir`, `PWD`, set to the chdir
    // target. It is accepted only with that value, and `probe_main` removes it
    // before the exec, so the command's environment is exactly the allowlist.
    let want: BTreeMap<String, String> = plan.shape.env.iter().cloned().collect();
    let mut have: BTreeMap<String, String> = std::env::vars_os()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let cwd = plan.shape.cwd.display().to_string();
    match have.remove("PWD") {
        Some(pwd) if pwd != cwd && !want.contains_key("PWD") => fail(
            &mut r,
            format!("bwrap's `PWD` is {pwd:?}, not its --chdir target {cwd:?}"),
        ),
        _ => {}
    }
    if have == want {
        r.notes.push(format!(
            "environment exactly the allowlist ({} variables, once bwrap's own `PWD={cwd}` is \
             removed before the exec): {}",
            want.len(),
            want.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    } else {
        let extra: Vec<_> = have.keys().filter(|k| !want.contains_key(*k)).collect();
        let missing: Vec<_> = want.keys().filter(|k| !have.contains_key(*k)).collect();
        let differ: Vec<_> = want
            .iter()
            .filter(|(k, v)| have.get(*k).is_some_and(|h| h != *v))
            .map(|(k, _)| k)
            .collect();
        fail(
            &mut r,
            format!(
                "the environment is not the allowlist: extra {extra:?}, missing {missing:?}, \
                 different {differ:?}"
            ),
        );
    }

    // Working directory, and Cargo configuration in its visible ancestors.
    match std::env::current_dir() {
        Ok(cwd) if cwd == plan.shape.cwd => {}
        Ok(cwd) => fail(
            &mut r,
            format!(
                "the working directory is {}, not {}",
                cwd.display(),
                plan.shape.cwd.display()
            ),
        ),
        Err(e) => fail(&mut r, format!("cannot read the working directory: {e}")),
    }
    let mut found = Vec::new();
    for a in plan.shape.cwd.ancestors().skip(1) {
        for n in ["config", "config.toml"] {
            let p = a.join(".cargo").join(n);
            if std::fs::symlink_metadata(&p).is_ok() {
                found.push(p.display().to_string());
            }
        }
    }
    if found.is_empty() {
        r.notes.push(format!(
            "no `.cargo/config` or `.cargo/config.toml` in any ancestor of the working directory \
             `{}` visible here, so Cargo's hierarchical configuration is the working directory's \
             own and the private Cargo home's (empty)",
            plan.shape.cwd.display()
        ));
    } else {
        fail(
            &mut r,
            format!(
                "Cargo configuration in an ancestor of the working directory is visible inside: {}",
                found.join(", ")
            ),
        );
    }

    // Network.
    match std::fs::read_to_string("/proc/net/dev") {
        Ok(dev) => {
            let ifaces: Vec<String> = dev
                .lines()
                .skip(2)
                .filter_map(|l| l.split(':').next().map(|n| n.trim().to_string()))
                .collect();
            if ifaces.iter().all(|i| i == "lo") {
                r.notes
                    .push(format!("network interfaces: {ifaces:?} (loopback only)"));
            } else {
                fail(
                    &mut r,
                    format!("network interfaces beyond loopback: {ifaces:?}"),
                );
            }
        }
        Err(e) => fail(&mut r, format!("cannot read /proc/net/dev: {e}")),
    }
    for addr in ["1.1.1.1:443", "[2606:4700:4700::1111]:443"] {
        let sa: std::net::SocketAddr = addr.parse().expect("literal address");
        match std::net::TcpStream::connect_timeout(&sa, Duration::from_secs(3)) {
            Ok(_) => fail(
                &mut r,
                format!("connect({addr}) SUCCEEDED: the network is reachable"),
            ),
            Err(e) => r
                .notes
                .push(format!("connect({addr}) failed: {}", errno_name(&e))),
        }
    }

    // The masked home holds only the bind destinations under it.
    match &plan.shape.home {
        Some(home) => {
            let dests: Vec<&Path> = plan
                .shape
                .destinations()
                .into_iter()
                .filter(|d| d.starts_with(home))
                .collect();
            let mut stray = Vec::new();
            walk_masked(home, &dests, &mut stray);
            if stray.is_empty() {
                r.notes.push(format!(
                    "the operator's home `{}` is an empty tmpfs apart from the bind destinations \
                     under it: {}",
                    home.display(),
                    if dests.is_empty() {
                        "none".to_string()
                    } else {
                        dests
                            .iter()
                            .map(|d| format!("`{}`", d.display()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ));
            } else {
                fail(
                    &mut r,
                    format!(
                        "the operator's home `{}` is not masked: {} visible inside",
                        home.display(),
                        stray.join(", ")
                    ),
                );
            }
        }
        None => r.notes.push(
            "**no home was masked**: the password database names no home that is a directory \
             other than `/`"
                .to_string(),
        ),
    }

    // Operator Cargo configuration and credentials.
    let present: Vec<String> = plan
        .absent
        .iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .map(|p| p.display().to_string())
        .collect();
    if present.is_empty() {
        r.notes.push(format!(
            "absent inside: {}",
            plan.absent
                .iter()
                .map(|p| format!("`{}`", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        fail(
            &mut r,
            format!("present inside, and must not be: {}", present.join(", ")),
        );
    }

    // Read-only paths refuse a write.
    for dir in &plan.read_only {
        let p = dir.join(format!(".xver-build-probe-{}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&p);
                fail(
                    &mut r,
                    format!("`{}` is WRITABLE inside the build namespace", dir.display()),
                );
            }
            Err(e) => r.notes.push(format!(
                "`{}` refused a file creation: {}",
                dir.display(),
                errno_name(&e)
            )),
        }
    }

    // The production deck's endpoints, and every host socket.
    let visible: Vec<String> = plan
        .production
        .iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .map(|p| p.display().to_string())
        .collect();
    if visible.is_empty() {
        r.notes.push(format!(
            "the host's {} standard deck endpoint candidates are absent inside",
            plan.production.len()
        ));
    } else {
        fail(
            &mut r,
            format!(
                "host deck endpoint candidates visible inside: {}",
                visible.join(", ")
            ),
        );
    }
    let mut connected = Vec::new();
    let mut refused = 0usize;
    for s in plan.host_sockets.iter().chain(&plan.production) {
        match std::os::unix::net::UnixStream::connect(s) {
            Ok(_) => connected.push(s.display().to_string()),
            Err(_) => refused += 1,
        }
    }
    if connected.is_empty() {
        r.notes.push(format!(
            "connect(2) refused for all {refused} paths tried: the {} pathname sockets listening \
             on the host at plan time and the deck endpoint candidates ({} visible through the \
             binds and covered by `/dev/null`)",
            plan.host_sockets.len(),
            plan.shape.nulled_sockets.len()
        ));
    } else {
        fail(
            &mut r,
            format!(
                "host sockets accepted a connection from inside: {}",
                connected.join(", ")
            ),
        );
    }

    // No host process is visible.
    match proc::pids() {
        Ok(pids) => r.notes.push(format!(
            "{} process(es) visible in `/proc` ({pids:?}), all in this PID namespace — no host \
             pid is visible",
            pids.len()
        )),
        Err(e) => fail(&mut r, format!("cannot list /proc: {e}")),
    }

    // What is writable. A residual `rw` submount the read-only root did not
    // remount is a FAILURE here, not the exposure note the runtime namespace
    // records: it would let build code write a host filesystem outside the
    // target dir. So is mountinfo that cannot be read, since then no one knows.
    match std::fs::read_to_string("/proc/self/mountinfo") {
        Ok(info) => {
            let own = |mp: &Path| {
                plan.shape.rw.iter().any(|p| mp == p)
                    || plan.shape.masks().contains(&mp)
                    || mp.starts_with("/proc")
                    || mp.starts_with("/dev")
            };
            let residual = isolation::residual_rw_mounts_where(&info, own);
            if residual.is_empty() {
                r.notes.push(format!(
                    "writable inside: {}; the private tmpfs mounts {}; no residual `rw` submount",
                    if plan.shape.rw.is_empty() {
                        "no host path".to_string()
                    } else {
                        plan.shape
                            .rw
                            .iter()
                            .map(|p| format!("`{}` (read-write bind)", p.display()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    plan.shape
                        .home
                        .iter()
                        .chain(plan.shape.tmpfs.iter().map(|(p, _)| p))
                        .map(|p| format!("`{}`", p.display()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            } else {
                fail(
                    &mut r,
                    format!(
                        "residual `rw` submounts the read-only root did not remount, writable by \
                         build code outside the target dir: {}",
                        residual
                            .iter()
                            .map(|m| format!("`{m}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
        }
        Err(e) => fail(
            &mut r,
            format!(
                "cannot read /proc/self/mountinfo, so residual `rw` submounts are unknown: {e}"
            ),
        ),
    }
    r
}

/// Collect every entry under the masked `dir` that is neither a bind
/// destination nor a directory on the way to one.
fn walk_masked(dir: &Path, dests: &[&Path], stray: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        stray.push(format!("`{}` (unreadable)", dir.display()));
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if dests.iter().any(|d| *d == p) {
            continue;
        }
        let on_the_way = dests.iter().any(|d| d.starts_with(&p));
        if on_the_way && e.file_type().is_ok_and(|t| t.is_dir()) {
            walk_masked(&p, dests, stray);
        } else {
            stray.push(format!("`{}`", p.display()));
        }
    }
}

// ---------------------------------------------------------------------------
// The outer half's side
// ---------------------------------------------------------------------------

/// A finished run in a build namespace.
pub struct Run {
    pub report: ProbeReport,
    pub stdout: Vec<u8>,
}

/// The last `lines` lines of a namespaced command's output, for the
/// operator's terminal, with every control character but tab escaped: the
/// output is the build's, and an escape sequence in it would otherwise drive
/// that terminal.
fn tail(s: &str, lines: usize) -> String {
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..]
        .iter()
        .map(|l| {
            l.chars()
                .map(|c| {
                    if c.is_control() && c != '\t' {
                        c.escape_default().to_string()
                    } else {
                        c.to_string()
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The probe's report: the first line of `stderr` carrying the marker. Lines
/// before it can only be bubblewrap's own; anything the command printed comes
/// after it, because the probe wrote it before the command existed.
pub fn parse_report(stderr: &str) -> Option<(ProbeReport, String)> {
    let mut rest = Vec::new();
    let mut report = None;
    for line in stderr.lines() {
        if report.is_none()
            && let Some(json) = line.strip_prefix(REPORT_MARKER)
        {
            report = Some(serde_json::from_str::<ProbeReport>(json).ok()?);
            continue;
        }
        rest.push(line);
    }
    report.map(|r| (r, rest.join("\n")))
}

/// Run `command` in the namespace `plan` describes, behind the probe. Refuses
/// when the probe's report is missing or names a failure, when the command
/// fails, and when any process of the namespace outlives it.
pub fn run(plan: &ProbePlan, harness: &Path, command: &[String]) -> Result<Run, String> {
    let json = serde_json::to_string(plan).map_err(|e| format!("{e}"))?;
    let mut args = bwrap_args(&plan.shape, harness);
    args.extend([
        "--".to_string(),
        HARNESS_IN_NS.to_string(),
        PROBE_FLAG.to_string(),
        json,
        "--".to_string(),
    ]);
    args.extend(command.iter().cloned());
    let out = Command::new("bwrap")
        .args(&args)
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("bwrap ({}): {e}", plan.what))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let Some((report, rest)) = parse_report(&stderr) else {
        return Err(format!(
            "{}: the build namespace produced no probe report, so nothing ran in it ({}):\n{}",
            plan.what,
            out.status,
            tail(&stderr, 20)
        ));
    };
    if !report.failures.is_empty() {
        return Err(format!(
            "refusing {}: the build namespace did not hold, and nothing was run in it: {}",
            plan.what,
            report.failures.join("; ")
        ));
    }
    let survivors = survivors(&report.pid_ns)?;
    if !survivors.is_empty() {
        return Err(format!(
            "{}: {} process(es) outlived the build namespace's PID namespace {}: {}",
            plan.what,
            survivors.len(),
            report.pid_ns,
            survivors.join("; ")
        ));
    }
    if !out.status.success() {
        return Err(format!(
            "{} failed in the build namespace ({}):\n{}",
            plan.what,
            out.status,
            tail(&rest, 40)
        ));
    }
    Ok(Run {
        report,
        stdout: out.stdout,
    })
}

/// Processes still in PID namespace `ns`. When bubblewrap's init exits the
/// kernel kills the rest, but reaping is not instantaneous, so this waits up
/// to two seconds for them to go before reporting any.
pub fn survivors(ns: &str) -> Result<Vec<String>, String> {
    if ns.is_empty() {
        return Err("the probe reported no PID namespace to check for survivors".into());
    }
    let me = std::process::id() as i32;
    for attempt in 0..=10 {
        let left: Vec<String> = proc::pids()?
            .into_iter()
            .filter(|p| *p != me)
            .filter(|p| proc::namespace(&p.to_string(), "pid").as_deref() == Some(ns))
            .map(|p| {
                format!(
                    "pid {p} (`{}`)",
                    proc::cmdline(p).unwrap_or_default().join(" ")
                )
            })
            .collect();
        if left.is_empty() || attempt == 10 {
            return Ok(left);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    unreachable!()
}

/// The fetch phase: `cargo fetch --locked` for `manifest`, on the host, into
/// `fetch_home`, where no Cargo configuration but the harness's is read.
///
/// * The working directory is `/`, whose `.cargo/config*` must be absent — it
///   is the only directory in that hierarchy — and `--manifest-path` names the
///   branch, so the build clone's `.cargo/config.toml` is not read.
/// * `CARGO_HOME` is `fetch_home`, which must hold no `config*` or
///   `credentials*`: the harness writes none, so one there was put there by
///   something else.
/// * The environment is exactly `PATH`, `CARGO_HOME` (and `HOME` and
///   `XDG_CONFIG_HOME` set to it), `GIT_CONFIG_GLOBAL=/dev/null` and
///   `GIT_CONFIG_NOSYSTEM=1`: no `CARGO_*` configuration variable, no
///   `RUSTC*`, no proxy, no credential, and none of the operator's git
///   configuration for a git dependency.
///
/// `cargo fetch` downloads and checksum-verifies crates against `Cargo.lock`
/// and unpacks them; it runs no build script and no proc macro.
pub fn fetch(tc: &Toolchain, fetch_home: &Path, manifest: &Path) -> Result<Vec<String>, String> {
    for n in ["config", "config.toml"] {
        let p = Path::new("/.cargo").join(n);
        if std::fs::symlink_metadata(&p).is_ok() {
            return Err(format!(
                "refusing the fetch phase: {} exists, and Cargo would read it from the fetch's \
                 working directory `/`",
                p.display()
            ));
        }
    }
    for n in [
        "config",
        "config.toml",
        "credentials",
        "credentials.toml",
        ".gitconfig",
        "git/config",
    ] {
        let p = fetch_home.join(n);
        if std::fs::symlink_metadata(&p).is_ok() {
            return Err(format!(
                "refusing the fetch phase: {} exists in the harness's own Cargo home, which the \
                 harness never writes a configuration or credential into",
                p.display()
            ));
        }
    }
    let path = format!("{}:/usr/bin:/bin", tc.bin.display());
    let started = std::time::Instant::now();
    let out = Command::new(&tc.cargo)
        .args(["fetch", "--locked", "--manifest-path"])
        .arg(manifest)
        .current_dir("/")
        .env_clear()
        .env("PATH", &path)
        .env("CARGO_HOME", fetch_home)
        // Git configuration, for a git dependency: none of the operator's.
        // `HOME` and `XDG_CONFIG_HOME` point at the harness's Cargo home (no
        // `.gitconfig` or `git/config`, checked above), and git's own
        // switches for the global and system files are set too.
        .env("HOME", fetch_home)
        .env("XDG_CONFIG_HOME", fetch_home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cargo fetch: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo fetch --locked failed ({}):\n{}",
            out.status,
            tail(&String::from_utf8_lossy(&out.stderr), 30)
        ));
    }
    Ok(vec![format!(
        "fetch phase: `cargo fetch --locked --manifest-path {}` on the host, from the working \
         directory `/` (no `/.cargo/config*`), with an environment of exactly `PATH={path}`, \
         `CARGO_HOME`, `HOME` and `XDG_CONFIG_HOME` all `{}` (the harness's own Cargo home, \
         holding no `config*`, `credentials*`, `.gitconfig` or `git/config`), \
         `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1`, in {:.1}s. It downloads, \
         checksum-verifies and unpacks crates and runs no build script or proc macro",
        manifest.display(),
        fetch_home.display(),
        started.elapsed().as_secs_f64()
    )])
}

/// Describe a shape for the evidence file.
pub fn describe(shape: &Shape) -> String {
    let list = |v: &mut dyn Iterator<Item = String>| {
        let v: Vec<String> = v.collect();
        if v.is_empty() {
            "none".to_string()
        } else {
            v.join(", ")
        }
    };
    format!(
        "bubblewrap: `--unshare-all --unshare-user --disable-userns --die-with-parent \
         --new-session`, `/` read-only; masked (empty tmpfs): {}; read-only at their own path: \
         {}; read-write at their own path: {}; read-only elsewhere: {}; host sockets covered by \
         `/dev/null`: {}; working directory `{}`; `--clearenv` then {}",
        list(
            &mut shape
                .home
                .iter()
                .chain(shape.tmpfs.iter().map(|(p, _)| p))
                .map(|p| format!("`{}`", p.display()))
        ),
        list(&mut shape.ro.iter().map(|p| format!("`{}`", p.display()))),
        list(&mut shape.rw.iter().map(|p| format!("`{}`", p.display()))),
        list(&mut shape.ro_at.iter().map(|(s, d)| format!(
            "`{}` at `{}`",
            s.display(),
            d.display()
        ))),
        list(
            &mut shape
                .nulled_sockets
                .iter()
                .map(|p| format!("`{}`", p.display()))
        ),
        shape.cwd.display(),
        list(&mut shape.env.iter().map(|(k, v)| format!("`{k}={v}`")))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc() -> Toolchain {
        Toolchain {
            sysroot: "/home/op/.rustup/toolchains/stable".into(),
            bin: "/home/op/.rustup/toolchains/stable/bin".into(),
            cargo: "/home/op/.rustup/toolchains/stable/bin/cargo".into(),
            version: "rustc 1.98.1".into(),
        }
    }

    fn host() -> Host {
        Host {
            uid: 1000,
            home: Some("/home/op".into()),
            // Any existing directory outside every mask stands in for an
            // operator Cargo home set elsewhere with `CARGO_HOME`: the planner
            // masks only one that exists, since bwrap cannot mount a tmpfs on
            // a missing path under the read-only root.
            cargo_home: Some("/usr/share".into()),
            outer_mnt: "mnt:[1]".into(),
            outer_pid: "pid:[2]".into(),
            outer_net: "net:[3]".into(),
            harness: "/home/op/code/dad/target/debug/xtask-cross-version".into(),
        }
    }

    fn plan() -> ProbePlan {
        build_plan(
            &host(),
            &tc(),
            Path::new("/home/op/code/xver-src"),
            Path::new("/home/op/code/xver-target"),
            Path::new("/nonexistent-fetch-home"),
            vec![
                "/nix/var/nix/daemon-socket/socket".into(),
                "/run/docker.sock".into(),
                "/home/op/code/xver-target/weird.sock".into(),
                "/home/op/.ssh/agent.sock".into(),
            ],
        )
        .expect("plan")
    }

    fn pos(args: &[String], needle: &[&str]) -> usize {
        args.windows(needle.len())
            .position(|w| w.iter().zip(needle).all(|(a, b)| a == b))
            .unwrap_or_else(|| panic!("{needle:?} not in {args:?}"))
    }

    #[test]
    fn the_build_namespace_is_private_offline_and_over_a_read_only_root() {
        let args = bwrap_args(&plan().shape, &host().harness);
        for flag in [
            "--unshare-all",
            "--unshare-user",
            "--disable-userns",
            "--assert-userns-disabled",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing: {args:?}");
        }
        // `--unshare-all` is what takes the network; nothing re-shares it.
        assert!(!args.iter().any(|a| a == "--share-net"), "{args:?}");
        assert_eq!(pos(&args, &["--ro-bind", "/", "/"]), 6);
    }

    #[test]
    fn home_tmp_var_tmp_run_and_an_outside_cargo_home_are_masked_before_any_bind() {
        let args = bwrap_args(&plan().shape, &host().harness);
        let clone = pos(
            &args,
            &[
                "--ro-bind",
                "/home/op/code/xver-src",
                "/home/op/code/xver-src",
            ],
        );
        for mask in ["/home/op", "/tmp", "/var/tmp", "/run", "/usr/share"] {
            let at = pos(&args, &["--tmpfs", mask]);
            assert!(
                at < clone,
                "{mask} must be masked before the binds land on it"
            );
            assert_eq!(
                args[at - 2],
                "--size",
                "{mask} is RAM-backed and must be capped"
            );
        }
    }

    #[test]
    fn the_clone_is_read_only_and_the_target_dir_read_write_at_their_own_paths() {
        let args = bwrap_args(&plan().shape, &host().harness);
        pos(
            &args,
            &[
                "--ro-bind",
                "/home/op/code/xver-src",
                "/home/op/code/xver-src",
            ],
        );
        pos(
            &args,
            &[
                "--bind",
                "/home/op/code/xver-target",
                "/home/op/code/xver-target",
            ],
        );
        assert!(
            !args
                .windows(2)
                .any(|w| w[0] == "--bind" && w[1] == "/home/op/code/xver-src"),
            "the clone must never be bound read-write: {args:?}"
        );
        // The sysroot is under the home, so it is bound back read-only.
        pos(
            &args,
            &[
                "--ro-bind",
                "/home/op/.rustup/toolchains/stable",
                "/home/op/.rustup/toolchains/stable",
            ],
        );
        // The operator's Cargo home is never bound; it is masked.
        assert!(
            !args
                .windows(2)
                .any(|w| w[0].ends_with("bind") && w[1] == "/usr/share")
        );
    }

    #[test]
    fn the_fetched_registry_is_read_only_under_a_private_cargo_home() {
        let dir = std::env::temp_dir().join(format!("xver-fetch-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("registry")).expect("dir");
        let p = build_plan(
            &host(),
            &tc(),
            Path::new("/home/op/code/xver-src"),
            Path::new("/home/op/code/xver-target"),
            &dir,
            vec![],
        )
        .expect("plan");
        let args = bwrap_args(&p.shape, &host().harness);
        let reg = dir.join("registry").display().to_string();
        let at = pos(&args, &["--ro-bind", &reg, "/tmp/xver-cargo-home/registry"]);
        assert!(pos(&args, &["--tmpfs", "/tmp"]) < at);
        assert!(
            p.shape
                .env
                .contains(&("CARGO_HOME".into(), CARGO_HOME_IN_NS.into()))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn visible_host_sockets_are_covered_last_and_masked_ones_are_left_alone() {
        let p = plan();
        assert_eq!(
            p.shape.nulled_sockets,
            vec![
                PathBuf::from("/home/op/code/xver-target/weird.sock"),
                PathBuf::from("/nix/var/nix/daemon-socket/socket"),
            ],
            "`/run` and the home are masked; a socket under the read-write target dir and one \
             outside every mask stay visible and are covered"
        );
        let args = bwrap_args(&p.shape, &host().harness);
        let cover = pos(
            &args,
            &[
                "--ro-bind",
                "/dev/null",
                "/nix/var/nix/daemon-socket/socket",
            ],
        );
        let target = pos(
            &args,
            &[
                "--bind",
                "/home/op/code/xver-target",
                "/home/op/code/xver-target",
            ],
        );
        assert!(
            target < cover,
            "a cover must land on top of the bind exposing it"
        );
        assert_eq!(
            p.host_sockets.len(),
            4,
            "the probe still tries every host socket"
        );
    }

    #[test]
    fn the_environment_is_an_allowlist_with_nothing_credential_shaped_or_configuring_cargo() {
        let p = plan();
        let names: Vec<&str> = p.shape.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "PATH",
                "HOME",
                "CARGO_HOME",
                "TMPDIR",
                "LC_ALL",
                "GIT_CONFIG_NOSYSTEM",
                "CARGO_TARGET_DIR"
            ]
        );
        for (k, _) in &p.shape.env {
            assert!(!sandbox::credential_like(k), "{k}");
            assert!(!k.starts_with("RUSTC") && !k.starts_with("DAD_"), "{k}");
        }
        let args = bwrap_args(&p.shape, &host().harness);
        let clear = pos(&args, &["--clearenv"]);
        let setenvs: Vec<(String, String)> = args[clear + 1..]
            .chunks(3)
            .map(|c| {
                assert_eq!(c[0], "--setenv");
                (c[1].clone(), c[2].clone())
            })
            .collect();
        assert_eq!(setenvs, p.shape.env);
    }

    #[test]
    fn the_probe_is_told_to_check_the_clone_is_read_only_and_the_cargo_credentials_absent() {
        let p = plan();
        assert_eq!(
            p.read_only,
            vec![
                PathBuf::from("/home/op/code/xver-src"),
                PathBuf::from("/home/op/code/xver-src/.git")
            ]
        );
        for f in ["/usr/share/config.toml", "/usr/share/credentials.toml"] {
            assert!(p.absent.contains(&PathBuf::from(f)), "{f}");
        }
        assert!(
            p.production
                .contains(&PathBuf::from("/run/user/1000/dot-agent-deck.sock"))
        );
    }

    #[test]
    fn a_target_dir_overlapping_the_clone_is_refused() {
        for (clone, target) in [
            ("/home/op/code/src", "/home/op/code/src/target"),
            ("/home/op/code/t/src", "/home/op/code/t"),
        ] {
            let e = build_plan(
                &host(),
                &tc(),
                Path::new(clone),
                Path::new(target),
                Path::new("/x"),
                vec![],
            )
            .unwrap_err();
            assert!(e.contains("overlap"), "{e}");
        }
        for cache in [
            "/home/op/code/xver-target",
            "/home/op/code/xver-target/cargo",
            "/home/op/code",
            "/home/op/code/xver-src/.cache",
        ] {
            let e = build_plan(
                &host(),
                &tc(),
                Path::new("/home/op/code/xver-src"),
                Path::new("/home/op/code/xver-target"),
                Path::new(cache),
                vec![],
            )
            .unwrap_err();
            assert!(e.contains("fetch-phase Cargo home"), "{cache}: {e}");
        }
    }

    #[test]
    fn the_metadata_namespace_binds_the_extraction_read_only_and_no_target_dir() {
        let p = metadata_plan(
            &host(),
            &tc(),
            Path::new("/home/op/code/runs/.xver-merge-base-x"),
            vec![],
        );
        assert!(p.shape.rw.is_empty());
        assert!(p.shape.ro_at.is_empty());
        assert!(
            p.shape
                .ro
                .contains(&PathBuf::from("/home/op/code/runs/.xver-merge-base-x"))
        );
        assert!(!p.shape.env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"));
        assert_eq!(
            p.shape.cwd,
            PathBuf::from("/home/op/code/runs/.xver-merge-base-x")
        );
    }

    #[test]
    fn only_the_first_report_line_counts_so_a_build_cannot_forge_one() {
        let good = ProbeReport {
            pid_ns: "pid:[9]".into(),
            notes: vec!["held".into()],
            failures: vec![],
        };
        let forged = ProbeReport {
            pid_ns: "pid:[9]".into(),
            notes: vec![],
            failures: vec![],
        };
        let stderr = format!(
            "{REPORT_MARKER}{}\n   Compiling x\n{REPORT_MARKER}{}\nwarning: y\n",
            serde_json::to_string(&good).unwrap(),
            serde_json::to_string(&forged).unwrap()
        );
        let (r, rest) = parse_report(&stderr).expect("report");
        assert_eq!(r.notes, vec!["held".to_string()]);
        assert!(rest.contains("Compiling x") && rest.contains(REPORT_MARKER));
        assert!(parse_report("bwrap: Can't mount tmpfs\n").is_none());
    }

    #[test]
    fn a_failed_builds_output_reaches_the_terminal_with_its_control_characters_escaped() {
        let out = tail("one\ntwo \u{1b}]52;c;AAAA\u{7} x\tthree\nfour", 2);
        assert_eq!(out, "two \\u{1b}]52;c;AAAA\\u{7} x\tthree\nfour");
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'));
    }

    #[test]
    fn the_home_walk_allows_only_bind_destinations_and_the_directories_leading_to_them() {
        let root = std::env::temp_dir().join(format!("xver-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dest = root.join("code/xver-src");
        std::fs::create_dir_all(&dest).expect("dir");
        std::fs::write(dest.join("inside-a-bind"), b"").expect("file");
        let mut stray = Vec::new();
        walk_masked(&root, &[dest.as_path()], &mut stray);
        assert!(stray.is_empty(), "{stray:?}");
        std::fs::write(root.join(".gitconfig"), b"").expect("file");
        std::fs::create_dir_all(root.join("code/other")).expect("dir");
        walk_masked(&root, &[dest.as_path()], &mut stray);
        assert_eq!(stray.len(), 2, "{stray:?}");
        let _ = std::fs::remove_dir_all(root);
    }
}
