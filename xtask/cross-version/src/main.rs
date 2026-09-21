//! `cargo xver` — CLAUDE.md rule 12's cross-version contract check, scripted.
//!
//! # What this is
//!
//! Rule 12 requires, for any change touching the daemon, the TUI↔daemon
//! protocol, orchestration or hooks, that you "build the branch, start a daemon
//! from the previous release with an agent under it, run the branch TUI against
//! that older daemon, and confirm the core flows still work end to end".
//!
//! **The requirement is the SCENARIO, not human fingers.** Rule 12 spells the
//! procedure out in keystrokes because a person was always going to run it, and
//! four of its paragraphs are about ways a person silently gets it wrong. A
//! driver that reproduces the same scenario and asserts the same tells
//! discharges the same obligation, and is checkable afterwards in a way a
//! recollection is not.
//!
//! # The scenario, and why each step is in it
//!
//! 1. Start the **previous release's** daemon in an isolated sandbox, capturing
//!    its identity.
//! 2. Attach the **previous release's** TUI and bring an orchestration up under
//!    it. *Without live agents the branch TUI silently SIGTERMs the old daemon
//!    and lazy-spawns its own — the build-version handshake behaving as designed
//!    — and every later check then passes against a daemon of the branch's own
//!    build.*
//! 3. Close that TUI with `Ctrl+D`, `Ctrl+C`, **Detach**. *`Ctrl+C` in
//!    `PaneInput` mode goes to the pane, not the deck, and kills a role;
//!    `Stop` shuts down the very daemon the test needs kept alive.*
//! 4. Attach the **branch** TUI. The build-version mismatch prompt naming the
//!    live agents is itself the proof the scenario was reached. Decline it.
//! 5. Exercise the flows: **delegate first, status hook last.** *Sending
//!    `agent-event --type running` before the delegate check makes the daemon
//!    classify that pane's agent type as `Pi`, which routes prompt delivery
//!    differently and makes a delivered delegate briefly look undelivered.*
//! 6. Tear down by **verified identity**, never by name.
//!
//! # Two halves
//!
//! This file is the OUTER half: it builds the inputs, creates the sandbox and
//! starts one `bwrap` namespace with this same binary inside it
//! (`--inner-plan`). The daemon, both TUIs and every deck CLI call are started
//! by the INNER half (`inner.rs`), inside that namespace, and each client only
//! after a pre-connect assertion whose host-side part this half answers over
//! `ctl.rs`. This half executes no deck binary: it checks the downloaded old
//! release against the release's published `checksums.txt`, and the inner half
//! runs its `--version`. `isolation.rs` says what the namespace masks and why.
//!
//! **What this half does run is outside the namespace, on the host, as the
//! operator**: `git` and `gh` to acquire the inputs, and `cargo build` — with
//! whatever build scripts, proc macros and cargo-config linker that executes.
//! The namespace contains the runtime scenario, not the command. `buildgate.rs`
//! refuses, before anything is built, a branch that changes build-time code
//! relative to its merge-base with `main`, unless `--allow-build-changes`.
//!
//! `docs/develop/cross-version-harness.md` is the operational page: how to run
//! it, what it isolates, and what it does not cover.

mod buildgate;
mod ctl;
mod inner;
mod isolation;
mod previous;
mod probe;
mod probes;
mod proc;
mod pty;
mod report;
mod sandbox;
mod stub;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use clap::Parser;
use probe::Probe;
use report::{Evidence, RunVerdict};
use sandbox::{Direction, EndpointMatrix, EndpointMode, EnvSpec, Sandbox};

/// The hidden flag the outer half passes to the copy of this binary it starts
/// inside the namespace.
const INNER_FLAG: &str = "--inner-plan";

/// The eight variables that tell git where a repository is. An ambient one
/// outranks the `current_dir` a command is given (issue #834), so every git
/// command this half runs has them removed.
const GIT_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
];

/// The trust-boundary warning `--help` and `-h` print after the options, for
/// whoever never opens the harness doc.
const TRUST_BOUNDARY: &str = "\
TRUST BOUNDARY - read this before running it on code you have not reviewed.
`cargo xver` contains its daemon/TUI/CLI runtime scenario in a private
bubblewrap namespace and masks the current resolver's standard endpoint roots.
It is NOT an untrusted-code sandbox: the branch build (`cargo build`, with the
build scripts, proc macros and cargo-config linker it executes) and input
acquisition (`git`, `gh`) run on this host, as you, with your files,
processes, sockets and network; and the runtime namespace can still read most
host files and connect to pathname sockets outside the masks. A branch that
changes build-time code relative to its merge-base with `main` is refused
before anything is built unless you pass --allow-build-changes after
reviewing those changes; the merge-base's own build-time code runs on every
build. Run unreviewed or external code only in a disposable VM or as a
dedicated user holding no credentials. See docs/develop/cross-version-harness.md.";

/// CLAUDE.md rule 12's cross-version contract check, as a scripted PTY driver.
#[derive(Parser, Debug)]
#[command(
    name = "xtask-cross-version",
    about = "Reproduce and verify CLAUDE.md rule 12's cross-version manual test for a branch",
    after_help = TRUST_BOUNDARY
)]
struct Opts {
    /// The branch under test — the "new" side. Fetched from `origin` into the
    /// standalone build clone and checked out DETACHED, so nothing there can
    /// commit, amend, rebase or push to it.
    #[arg(long)]
    branch: String,

    /// The previous release — the "old" side. Its published Linux binary is
    /// downloaded with `gh release download` and cached.
    ///
    /// When omitted it is resolved, never defaulted: the highest
    /// `vMAJOR.MINOR.PATCH` release of `--repo` that is neither a draft nor a
    /// prerelease, which must also be the one GitHub marks Latest. A run that
    /// cannot resolve it stops and asks for this flag (`previous.rs`), and the
    /// evidence file records which of the two a run used. Required with
    /// `--old-binary`.
    #[arg(long)]
    previous: Option<String>,

    /// `owner/repo` the release asset and the branch come from.
    #[arg(long, default_value = "vfarcic/dot-agent-deck")]
    repo: String,

    /// Use this binary as the old side instead of downloading a release.
    #[arg(long)]
    old_binary: Option<PathBuf>,

    /// The standalone clone the branch is built in — its own `.git`, never a
    /// linked worktree of the operator's repository. Created on first use and
    /// reused across branches. Defaults to `<repo parent>/dot-agent-deck-xver-src`.
    #[arg(long)]
    source_clone: Option<PathBuf>,

    /// `CARGO_TARGET_DIR` for that clone, reused across branches so the cargo
    /// cache survives. Defaults to `<repo parent>/dot-agent-deck-xver-target`.
    #[arg(long)]
    target_dir: Option<PathBuf>,

    /// Where per-run sandboxes are created. Defaults to
    /// `<repo parent>/dot-agent-deck-xver-runs`.
    #[arg(long)]
    runs_root: Option<PathBuf>,

    /// Where release binaries are cached. Defaults to
    /// `<repo parent>/dot-agent-deck-xver-releases`.
    #[arg(long)]
    releases_dir: Option<PathBuf>,

    /// Where to write the markdown evidence report, exactly as given. Defaults
    /// to `.dot-agent-deck/xver-evidence/<branch slug>.md` in this checkout
    /// (`<branch slug>-reverse.md` in a reverse run). Refused with
    /// `--direction both`, which writes two files.
    #[arg(long)]
    evidence: Option<PathBuf>,

    /// How the run addresses the daemon.
    ///
    /// `sandbox-sockets` (the default) pins both socket overrides inside the
    /// sandbox. `resolved` sets neither, so both builds resolve their endpoint
    /// the way they would on a real host — into the run's private `/tmp` and
    /// `/run/user/<uid>`. Required when the change under test IS endpoint
    /// resolution, because those overrides short-circuit it.
    #[arg(long, value_enum, default_value_t = ModeArg::SandboxSockets)]
    endpoint_mode: ModeArg,

    /// Unset `XDG_RUNTIME_DIR` for every process of the runtime scenario.
    ///
    /// This is a whole failure mode of its own for a change that moves the
    /// endpoint path in the FALLBACK case only (issue #1121): with
    /// `XDG_RUNTIME_DIR` set, both builds resolve byte-identical endpoints, and
    /// the run exercises the arm the change did not touch. When it is kept, it
    /// is pinned to the host's spelling `/run/user/<uid>`, which inside the
    /// namespace is a sandbox directory.
    #[arg(long)]
    unset_xdg_runtime_dir: bool,

    /// Turn the experimental feature flag ON for the run. Off by default and
    /// always pinned explicitly.
    #[arg(long)]
    experimental: bool,

    /// Build a branch that changes build-time code relative to its merge-base
    /// with `main` of `--repo`.
    ///
    /// Without it such a branch is refused before `cargo build` runs, naming
    /// the changed build-time files. `cargo build` runs on the host, outside
    /// the namespace, so the branch's build scripts, proc macros and cargo
    /// configuration would execute as you. Review those files first; the
    /// evidence file records that you opted in and which files changed.
    #[arg(long)]
    allow_build_changes: bool,

    /// Skip `cargo build` and use whatever is already at the target dir. For
    /// iterating on the harness itself. The evidence file then says the binary
    /// was not rebuilt, and which commit its own build id names, if any.
    #[arg(long)]
    skip_build: bool,

    /// Keep the sandbox directory after the run. It is kept automatically on
    /// anything but a clean pass.
    #[arg(long)]
    keep_sandbox: bool,

    /// Refuse to start when the runs root or the cargo target dir has less than
    /// this many GiB free (CLAUDE.md rule 14).
    #[arg(long, default_value_t = 100)]
    min_free_gib: u64,

    /// Cap every stand-in agent's lifetime. The namespace's exit is the first
    /// backstop; this is the second.
    #[arg(long, default_value_t = 1800)]
    max_agent_lifetime_secs: u64,

    /// Kill the whole namespace if the inner half has not finished by then.
    #[arg(long, default_value_t = 1200)]
    run_timeout_secs: u64,

    /// Which pairing to run.
    ///
    /// `forward` (the default, and rule 12's pairing): the previous release's
    /// daemon with the branch TUI and CLI. `reverse`: the BRANCH daemon with the
    /// previous release's TUI and CLI — the only pairing that executes a
    /// daemon-side change. `both`: forward, then reverse, each in its own
    /// sandbox and namespace, each with its own evidence file.
    #[arg(long, value_enum, default_value_t = DirectionArg::Forward)]
    direction: DirectionArg,

    /// The branch-specific stimulus a reverse run carries after the four tells.
    ///
    /// `auto` (the default) selects it from the branch's issue number and falls
    /// back to `generic`: no stimulus, the four tells plus a `role-set` tell,
    /// in any endpoint mode — which is how the #1179 negative control runs
    /// (`--branch main --direction reverse --probe generic --endpoint-mode
    /// resolved --unset-xdg-runtime-dir`). Probes run in the reverse direction
    /// only; asking for one explicitly with `--direction forward` is refused
    /// rather than silently ignored.
    #[arg(long, value_enum, default_value_t = ProbeArg::Auto)]
    probe: ProbeArg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ModeArg {
    SandboxSockets,
    Resolved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum DirectionArg {
    Forward,
    Reverse,
    Both,
}

impl DirectionArg {
    fn directions(self) -> Vec<Direction> {
        match self {
            DirectionArg::Forward => vec![Direction::Forward],
            DirectionArg::Reverse => vec![Direction::Reverse],
            DirectionArg::Both => vec![Direction::Forward, Direction::Reverse],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ProbeArg {
    Auto,
    Generic,
    TeardownInventory,
    LateSessionStart,
    LogEscaping,
    DiscoveryFallback,
    PasteEnvelope,
    CrossPaneSessionKey,
    SignalAck,
    GitEnv,
}

/// The probe a run in `direction` carries. Forward runs carry none — they are
/// rule 12's pairing, whose tells are the four — and an explicit probe there is
/// refused rather than dropped, so nobody reads a forward evidence file as
/// having measured one.
fn select_probe(arg: ProbeArg, branch: &str, direction: Direction) -> Result<Probe, String> {
    let explicit = match arg {
        ProbeArg::Auto => None,
        ProbeArg::Generic => Some(Probe::Generic),
        ProbeArg::TeardownInventory => Some(Probe::TeardownInventory),
        ProbeArg::LateSessionStart => Some(Probe::LateSessionStart),
        ProbeArg::LogEscaping => Some(Probe::LogEscaping),
        ProbeArg::DiscoveryFallback => Some(Probe::DiscoveryFallback),
        ProbeArg::PasteEnvelope => Some(Probe::PasteEnvelope),
        ProbeArg::CrossPaneSessionKey => Some(Probe::CrossPaneSessionKey),
        ProbeArg::SignalAck => Some(Probe::SignalAck),
        ProbeArg::GitEnv => Some(Probe::GitEnv),
    };
    match (direction, explicit) {
        (Direction::Forward, None | Some(Probe::Generic)) => Ok(Probe::Generic),
        (Direction::Forward, Some(p)) => Err(format!(
            "the {} probe runs in the reverse direction only — pass `--direction reverse` (or \
             `both`); a forward run is rule 12's pairing and asserts the four tells",
            p.name()
        )),
        (Direction::Reverse, Some(p)) => Ok(p),
        (Direction::Reverse, None) => Ok(Probe::for_branch(branch)),
    }
}

/// An explicit `--evidence` is written exactly where it says, in either
/// direction. Otherwise `.dot-agent-deck/xver-evidence/<slug>.md` for a forward
/// run — the path every earlier run wrote — and `<slug>-reverse.md` for a
/// reverse one, so the two directions of one branch never overwrite each other.
///
/// The explicit path used to get the same `-reverse` suffix, which left a
/// reverse run's evidence somewhere its operator had not named. `--direction
/// both`, the one invocation where a single path cannot name both files, is
/// refused up front by [`check_evidence_arg`] instead.
fn evidence_path(explicit: Option<&Path>, root: &Path, slug: &str, d: Direction) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    let name = match d {
        Direction::Forward => format!("{slug}.md"),
        Direction::Reverse => format!("{slug}-reverse.md"),
    };
    root.join(".dot-agent-deck")
        .join("xver-evidence")
        .join(name)
}

/// `--evidence` names one file and `--direction both` writes two, so the pair
/// is refused before anything is built rather than resolved by renaming one.
fn check_evidence_arg(explicit: Option<&Path>, direction: DirectionArg) -> Result<(), String> {
    match (explicit, direction) {
        (Some(path), DirectionArg::Both) => Err(format!(
            "`--evidence {}` names one file, and `--direction both` writes two (one per \
             direction) — run each direction with its own `--evidence`, or omit it for the \
             default `<branch slug>.md` and `<branch slug>-reverse.md`",
            path.display()
        )),
        _ => Ok(()),
    }
}

fn main() -> ExitCode {
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    // Hard-linked into a reverse run's sandbox as `claude`, this binary is the
    // synthetic Claude Code stand-in (`stub.rs`), never the harness.
    if args
        .first()
        .and_then(|a| Path::new(a).file_name())
        .is_some_and(|n| n == "claude")
    {
        return stub::main();
    }
    if args.get(1).is_some_and(|a| a == INNER_FLAG) {
        let Some(plan) = args.get(2) else {
            eprintln!("xver: {INNER_FLAG} needs a path");
            return ExitCode::FAILURE;
        };
        return inner::main(Path::new(plan));
    }
    // The `xver` alias already ends in `--`, so `cargo xver -- --branch x`
    // arrives here as `-- --branch x`, and clap reads everything after a `--`
    // as positional. That spelling was the documented one before this was
    // tolerated, so accept it rather than break every copy of it.
    if args.get(1).is_some_and(|a| a == "--") {
        args.remove(1);
    }
    let opts = Opts::parse_from(args);
    match run(&opts) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("\nxver: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Small process helpers
// ---------------------------------------------------------------------------

fn utc_now() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "<unknown>".to_string())
}

pub(crate) fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run a plain command and require success, returning stdout.
fn must_run(cmd: &mut Command, what: &str) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed ({}):\n{}{}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A `git` command rooted at `dir`, with every ambient location variable
/// removed and no interactive credential prompt.
fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(dir).env("GIT_TERMINAL_PROMPT", "0");
    for v in GIT_LOCATION_VARS {
        c.env_remove(v);
    }
    c.env_remove("GIT_CEILING_DIRECTORIES");
    c
}

// ---------------------------------------------------------------------------
// Build inputs
// ---------------------------------------------------------------------------

fn repo_root() -> Result<PathBuf, String> {
    let out = must_run(
        git(&std::env::current_dir().map_err(|e| format!("cwd: {e}"))?)
            .args(["rev-parse", "--show-toplevel"]),
        "git rev-parse --show-toplevel",
    )?;
    Ok(PathBuf::from(out.trim()))
}

/// The release asset the old side is fetched as.
const RELEASE_ASSET: &str = "dot-agent-deck-linux-amd64";

/// The SHA-256 a release's `checksums.txt` publishes for `asset`.
///
/// The file is `task checksums`' `shasum -a 256` output: one `<hex>  <name>`
/// line per asset (`*<name>` in binary mode). Exactly one line must name the
/// asset, with a 64-digit hex digest; anything else is refused rather than
/// guessed at, because this is what authorises executing the file.
fn published_sha256(checksums: &str, asset: &str) -> Result<String, String> {
    let mut found = Vec::new();
    for line in checksums.lines() {
        let Some((digest, name)) = line.trim_end().split_once(char::is_whitespace) else {
            continue;
        };
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        if name == asset {
            found.push(digest.to_ascii_lowercase());
        }
    }
    match found.as_slice() {
        [one] if one.len() == 64 && one.chars().all(|c| c.is_ascii_hexdigit()) => Ok(one.clone()),
        [one] => Err(format!(
            "the release's `checksums.txt` names `{asset}` with {one:?}, which is not a SHA-256"
        )),
        [] => Err(format!(
            "the release's `checksums.txt` has no entry for `{asset}`"
        )),
        _ => Err(format!(
            "the release's `checksums.txt` names `{asset}` {} times",
            found.len()
        )),
    }
}

/// A file's SHA-256, by `sha256sum` with an empty environment.
fn sha256_of(path: &Path) -> Result<String, String> {
    let sum = must_run(
        Command::new("sha256sum")
            .arg(path)
            .env_clear()
            .env("PATH", "/usr/bin:/bin"),
        "sha256sum",
    )?;
    let sha = sum
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "could not read a SHA-256 for {}: {sum:?}",
            path.display()
        ));
    }
    Ok(sha)
}

/// Fetch (or reuse) the previous release's published Linux binary and require
/// its SHA-256 to be the one the release's own `checksums.txt` publishes.
///
/// Nothing here executes it. Its `--version` is checked by the inner half,
/// inside the namespace, against [`isolation::Plan::old_version`] — the old
/// check ran it here, on the host, before anything had authenticated it.
///
/// `checksums.txt` is downloaded again on every run rather than cached beside
/// the binary, so a cached binary is always compared with what the release
/// publishes now, not with a copy that sits in the same writable directory.
/// What a match establishes is that the file is the asset the release in
/// `--repo` published; it says nothing about whether that repository is one to
/// trust. `--old-binary` has no published checksum at all and is recorded as
/// not checksum-verified.
fn old_binary(
    opts: &Opts,
    previous: &str,
    releases: &Path,
    ev: &mut Evidence,
) -> Result<PathBuf, String> {
    if let Some(explicit) = &opts.old_binary {
        let bin =
            std::fs::canonicalize(explicit).map_err(|e| format!("{}: {e}", explicit.display()))?;
        let sha = sha256_of(&bin)?;
        ev.preflight.push(format!(
            "old binary `{}` is caller-supplied (`--old-binary`): **NOT checksum-verified** — no \
             release publishes a checksum for it; SHA-256 `{sha}`, recorded rather than checked. \
             Its `--version` is recorded inside the namespace and not enforced",
            bin.display()
        ));
        return Ok(bin);
    }
    let dir = releases.join(previous);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let bin = dir.join(RELEASE_ASSET);
    let cached = bin.exists();
    if !cached {
        must_run(
            Command::new("gh").args([
                "release",
                "download",
                previous,
                "--repo",
                &opts.repo,
                "-p",
                RELEASE_ASSET,
                "-D",
                &dir.to_string_lossy(),
            ]),
            &format!("gh release download {previous} {RELEASE_ASSET}"),
        )?;
    }
    must_run(
        Command::new("gh").args([
            "release",
            "download",
            previous,
            "--repo",
            &opts.repo,
            "-p",
            "checksums.txt",
            "-D",
            &dir.to_string_lossy(),
            "--clobber",
        ]),
        &format!("gh release download {previous} checksums.txt"),
    )?;
    let checksums_file = dir.join("checksums.txt");
    let checksums = std::fs::read_to_string(&checksums_file)
        .map_err(|e| format!("read {}: {e}", checksums_file.display()))?;
    let published = published_sha256(&checksums, RELEASE_ASSET)
        .map_err(|e| format!("{previous} of {}: {e} — refusing to run it", opts.repo))?;
    let bin = std::fs::canonicalize(&bin).map_err(|e| format!("{}: {e}", bin.display()))?;
    let sha = sha256_of(&bin)?;
    if sha != published {
        return Err(format!(
            "`{}` has SHA-256 `{sha}`, but release {previous} of {} publishes `{published}` for \
             `{RELEASE_ASSET}` in its `checksums.txt` — refusing to run it. {}",
            bin.display(),
            opts.repo,
            if cached {
                "The cached copy is not the published asset; delete it and re-run to download it \
                 again, after finding out what changed it."
            } else {
                "The download is not the published asset."
            }
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", bin.display()))?;
    ev.preflight.push(format!(
        "old binary `{}` ({}) matched release {previous}'s published checksum: SHA-256 `{sha}` \
         is the `{RELEASE_ASSET}` entry of that release's `checksums.txt` in `{}`, downloaded \
         for this run. Not executed outside the namespace; its `--version` is checked inside",
        bin.display(),
        if cached {
            "cached from an earlier run"
        } else {
            "downloaded by this run"
        },
        opts.repo
    ));
    Ok(bin)
}

/// `cargo`, run in `dir` on the host with the caller's toolchain environment —
/// the devbox/nix compiler wrappers need dozens of variables — minus anything
/// credential-shaped, the deck's own pane variables and the git location
/// variables. A denylist, so narrower than the run's own allowlist, and not a
/// security boundary: whatever cargo executes runs as the operator.
fn host_cargo(dir: &Path) -> Command {
    let mut cmd = Command::new("cargo");
    cmd.current_dir(dir);
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        if sandbox::credential_like(&name)
            || name.starts_with("DOT_AGENT_DECK_")
            || GIT_LOCATION_VARS.contains(&name.as_ref())
        {
            cmd.env_remove(&k);
        }
    }
    cmd
}

/// Point the standalone build clone at `origin/<branch>` and build it.
///
/// A standalone clone rather than a linked worktree of the operator's
/// repository, which is what the first version of this harness used: a linked
/// worktree writes its registration, HEAD, index and reflogs into the source
/// repository's common `.git`, outside anything the run owns, where another
/// session's `git worktree prune` can reach it. This clone has its own `.git`,
/// so building a branch writes nothing into the operator's repository. It stays
/// at a fixed path outside the per-run sandbox so the cargo cache in the
/// target dir survives across branches; the run itself executes a staged copy
/// of the binary from inside `$S`.
///
/// Detached, always: the branches this sweeps are being verified, not changed,
/// and a detached HEAD cannot commit to one by accident.
///
/// Between the checkout and the build sits the build-time gate
/// (`buildgate.rs`): `main` of `--repo` is fetched too, and a branch whose diff
/// from their merge-base touches build-time code is refused here, before
/// `cargo build` runs, unless `--allow-build-changes`. A gate that could not be
/// evaluated refuses too. `scratch` is where the merge-base's tree is extracted
/// for `cargo metadata`, and removed again.
fn new_binary(
    opts: &Opts,
    clone: &Path,
    target_dir: &Path,
    scratch: &Path,
    ev: &mut Evidence,
) -> Result<(PathBuf, String), String> {
    let url = format!("https://github.com/{}.git", opts.repo);
    let fresh = std::fs::symlink_metadata(clone).is_err();
    if fresh {
        let parent = clone
            .parent()
            .ok_or_else(|| format!("{} has no parent", clone.display()))?;
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        must_run(
            git(parent).args([
                "clone",
                "--quiet",
                "--no-checkout",
                &url,
                &clone.to_string_lossy(),
            ]),
            "git clone (standalone build clone)",
        )?;
        ev.preflight.push(format!(
            "created the standalone build clone {} from {url}",
            clone.display()
        ));
    }
    let dotgit = clone.join(".git");
    let md = std::fs::symlink_metadata(&dotgit)
        .map_err(|e| format!("{}: {e} — not a git clone", dotgit.display()))?;
    if md.file_type().is_file() {
        return Err(format!(
            "{} is a `.git` FILE, so {} is a linked worktree whose metadata lives in another \
             repository. Refusing: pass --source-clone at a standalone clone, or remove this one \
             and let the harness create one",
            dotgit.display(),
            clone.display()
        ));
    }
    if !md.file_type().is_dir() {
        return Err(format!("{} is not a directory", dotgit.display()));
    }
    let canon_dotgit = std::fs::canonicalize(&dotgit).map_err(|e| format!("{e}"))?;
    for (flag, what) in [
        ("--absolute-git-dir", "git dir"),
        ("--git-common-dir", "common dir"),
    ] {
        let mut cmd = git(clone);
        cmd.args(["rev-parse", "--path-format=absolute", flag]);
        let got = must_run(&mut cmd, &format!("git rev-parse {flag}"))?;
        let got = std::fs::canonicalize(got.trim()).map_err(|e| format!("{e}"))?;
        if got != canon_dotgit {
            return Err(format!(
                "the build clone's {what} is {}, not its own {} — it is not standalone",
                got.display(),
                canon_dotgit.display()
            ));
        }
    }
    let dirty = |when: &str| -> Result<(), String> {
        let st = must_run(
            git(clone).args(["status", "--porcelain", "--untracked-files=no"]),
            "git status",
        )?;
        if !st.trim().is_empty() {
            return Err(format!(
                "the build clone {} has tracked modifications {when} — something other than this \
                 harness edited it; refusing to build a branch on top of them:\n{st}",
                clone.display()
            ));
        }
        Ok(())
    };
    if !fresh {
        dirty("before checkout")?;
    }
    must_run(
        git(clone).args(["fetch", "--quiet", &url, buildgate::BASE_BRANCH]),
        "git fetch main (the build-time gate's base)",
    )?;
    let base_sha = must_run(
        git(clone).args(["rev-parse", "FETCH_HEAD"]),
        "git rev-parse FETCH_HEAD (main)",
    )?
    .trim()
    .to_string();
    must_run(
        git(clone).args(["fetch", "--quiet", &url, &opts.branch]),
        "git fetch <branch>",
    )?;
    must_run(
        git(clone).args(["checkout", "--quiet", "--detach", "FETCH_HEAD"]),
        "git checkout --detach FETCH_HEAD",
    )?;
    let sha = must_run(git(clone).args(["rev-parse", "HEAD"]), "git rev-parse HEAD")?
        .trim()
        .to_string();
    let fetched = must_run(
        git(clone).args(["rev-parse", "FETCH_HEAD"]),
        "git rev-parse FETCH_HEAD",
    )?
    .trim()
    .to_string();
    if sha != fetched {
        return Err(format!(
            "HEAD {sha} is not FETCH_HEAD {fetched} after checkout"
        ));
    }
    dirty("after checkout")?;
    ev.preflight.push(format!(
        "branch source: standalone clone {} (its own `.git`, not a linked worktree), detached at \
         {sha}",
        clone.display()
    ));

    println!(
        "xver: build-time gate — comparing `{}` with its merge-base with `origin/{}`",
        opts.branch,
        buildgate::BASE_BRANCH
    );
    let (cmp, changes) = buildgate::evaluate(
        clone,
        &opts.repo,
        &base_sha,
        &sha,
        scratch,
        &git,
        &host_cargo,
    )?;
    let gate = buildgate::decide(
        &opts.branch,
        &changes,
        &cmp,
        opts.allow_build_changes,
        opts.skip_build,
    )?;
    println!("xver: {gate}");
    ev.build_time = gate.clone();
    ev.preflight.push(gate);

    if !opts.skip_build {
        // The branch's build scripts, proc macros and cargo-config linker run
        // here, outside the namespace, as the operator (`host_cargo`). The gate
        // above is what stands between a branch's own build-time code and this
        // line; mainline's runs regardless.
        let mut cmd = host_cargo(clone);
        cmd.env("CARGO_TARGET_DIR", target_dir).args([
            "build",
            "--locked",
            "--bin",
            "dot-agent-deck",
        ]);
        must_run(&mut cmd, "cargo build --locked --bin dot-agent-deck")?;
    }
    let bin = target_dir.join("debug").join("dot-agent-deck");
    if !bin.exists() {
        return Err(format!("no branch binary at {}", bin.display()));
    }
    if opts.skip_build {
        ev.preflight.push(format!(
            "`--skip-build`: `cargo build` was NOT run, so `{}` is whatever was last built there — \
             not necessarily a build of {sha}",
            bin.display()
        ));
    }
    Ok((bin, sha))
}

/// What `--skip-build` means for the evidence: the binary under test is
/// whatever was already in the target dir, and the checked-out HEAD says
/// nothing about it.
///
/// The one source for which commit it was built from is the binary's own
/// build id, `<version>-g<short-sha>[-dirty]` (`build.rs`), read from its
/// `daemon hello`. That is the binary's claim about itself — the branch's
/// `build.rs` stamped it from `git` when it last ran, unless the build
/// environment injected a `DAD_BUILD_ID` — so the note reports it as that
/// rather than as a measurement, and says so plainly when there is none.
fn skip_build_note(head_sha: &str, new_hello: &str) -> String {
    let build_id = serde_json::from_str::<serde_json::Value>(new_hello.trim())
        .ok()
        .and_then(|v| {
            v.get("build_version")
                .and_then(|b| b.as_str())
                .map(str::to_string)
        });
    let Some(build_id) = build_id else {
        return format!(
            "The branch binary's `daemon hello` was not recorded, so which commit it was built \
             from is NOT knowable from this run. Do not read this run as a test of the branch \
             HEAD `{head_sha}`."
        );
    };
    let (stem, dirty) = match build_id.strip_suffix("-dirty") {
        Some(stem) => (stem, true),
        None => (build_id.as_str(), false),
    };
    let short = stem
        .rsplit_once("-g")
        .map(|(_, sha)| sha)
        .filter(|sha| sha.len() >= 4 && sha.chars().all(|c| c.is_ascii_hexdigit()));
    let Some(short) = short else {
        return format!(
            "Its build id `{build_id}` names no commit (no `-g<sha>` component), so which commit \
             it was built from is NOT knowable. Do not read this run as a test of the branch \
             HEAD `{head_sha}`."
        );
    };
    let provenance = "The build id is the binary's own stamp — what the branch's `build.rs` read \
                      from `git` when it last ran, or an injected `DAD_BUILD_ID` — not an \
                      independent measurement.";
    if !head_sha
        .to_ascii_lowercase()
        .starts_with(&short.to_ascii_lowercase())
    {
        return format!(
            "**STALE:** its build id `{build_id}` names commit `{short}`, NOT the branch HEAD \
             `{head_sha}`. This run tested another build, and its tells say nothing about that \
             HEAD. {provenance}"
        );
    }
    if dirty {
        return format!(
            "Its build id `{build_id}` names the branch HEAD `{head_sha}` with `-dirty`: it was \
             built from that commit PLUS uncommitted changes in the build clone, which is not \
             the commit under test. {provenance}"
        );
    }
    format!(
        "Its build id `{build_id}` names commit `{short}`, the branch HEAD `{head_sha}`, so by \
         the binary's own stamp it is a build of the commit under test. {provenance}"
    )
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn run(opts: &Opts) -> Result<bool, String> {
    check_evidence_arg(opts.evidence.as_deref(), opts.direction)?;
    let directions = opts.direction.directions();
    // Refuse a probe/direction combination before anything is built. With
    // `both`, an explicit probe belongs to the reverse half and the forward half
    // stays rule 12's four tells.
    let mut probes = Vec::new();
    for d in &directions {
        let arg = if *d == Direction::Forward && opts.direction == DirectionArg::Both {
            ProbeArg::Auto
        } else {
            opts.probe
        };
        let probe = select_probe(arg, &opts.branch, *d)?;
        if *d == Direction::Reverse {
            probe.check_config(
                match opts.endpoint_mode {
                    ModeArg::SandboxSockets => EndpointMode::SandboxSockets,
                    ModeArg::Resolved => EndpointMode::Resolved,
                },
                !opts.unset_xdg_runtime_dir,
            )?;
        }
        probes.push(probe);
    }
    // Resolved once, before anything is built, so `both` runs its two halves
    // against the same release even if one is published in between.
    let previous = previous::resolve(
        opts.previous.as_deref(),
        opts.old_binary.is_some(),
        &opts.repo,
        utc_now,
    )?;
    println!(
        "xver: previous release `{}` — {}",
        previous.tag,
        previous.describe()
    );
    let mut all_passed = true;
    let mut first_err = None;
    for (d, probe) in directions.into_iter().zip(probes) {
        match run_one(opts, &previous, d, probe) {
            Ok(passed) => all_passed &= passed,
            Err(e) => {
                eprintln!("\nxver ({}): {e}", d.name());
                all_passed = false;
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(all_passed),
    }
}

/// One run in one direction: its own preflight, sandbox, namespace, evidence
/// file and postconditions.
fn run_one(
    opts: &Opts,
    previous: &previous::Previous,
    direction: Direction,
    probe: Probe,
) -> Result<bool, String> {
    let root = repo_root()?;
    let parent = root
        .parent()
        .ok_or_else(|| "the repository root has no parent".to_string())?
        .to_path_buf();
    let clone = opts
        .source_clone
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-src"));
    let target_dir = opts
        .target_dir
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-target"));
    let runs_root = opts
        .runs_root
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-runs"));
    let releases = opts
        .releases_dir
        .clone()
        .unwrap_or_else(|| parent.join("dot-agent-deck-xver-releases"));
    let uid = sandbox::current_uid();
    let (user, _) = sandbox::passwd_entry().ok_or("this uid has no password-database entry")?;
    let mode = match opts.endpoint_mode {
        ModeArg::SandboxSockets => EndpointMode::SandboxSockets,
        ModeArg::Resolved => EndpointMode::Resolved,
    };
    let spec = EnvSpec {
        mode,
        keep_xdg_runtime_dir: !opts.unset_xdg_runtime_dir,
        experimental: opts.experimental,
        max_lifetime_secs: opts.max_agent_lifetime_secs,
        uid,
    };

    let mut ev = Evidence {
        branch: opts.branch.clone(),
        previous: previous.tag.clone(),
        previous_source: previous.describe(),
        direction,
        probe: probe.describe(),
        started_at: utc_now(),
        mode: match mode {
            EndpointMode::SandboxSockets => {
                "sandbox-sockets (both socket overrides pinned inside the sandbox), inside a private namespace".into()
            }
            EndpointMode::Resolved => {
                "resolved (neither socket override set — both builds resolve their own endpoint, into the namespace's private `/tmp` and `/run/user/<uid>`)"
                    .into()
            }
        },
        namespace: "one private bubblewrap namespace for every deck process of the runtime \
                    scenario (the build and input acquisition ran on the host) — \
                    private mount, PID, network, IPC, UTS and user namespaces; BOTH endpoint \
                    roots masked (`/tmp` and `/run/user/<uid>` are sandbox directories), \
                    `/var/tmp` masked, the operator's home an empty tmpfs, the rest of `/` \
                    bound read-only; see the Isolation section for what was measured"
            .into(),
        xdg_runtime_dir: if opts.unset_xdg_runtime_dir {
            "UNSET for every process of the runtime scenario".into()
        } else {
            format!(
                "`{}` — the host's spelling, which inside the namespace is the sandbox's `run-user`",
                sandbox::runtime_dir(uid).display()
            )
        },
        ..Default::default()
    };

    println!("xver ({}): preflight", direction.name());
    ev.preflight.push(format!(
        "runs root: {}",
        sandbox::require_disk_backed("runs root", &runs_root, opts.min_free_gib)?
    ));
    ev.preflight.push(format!(
        "cargo target dir: {}",
        sandbox::require_disk_backed("cargo target dir", &target_dir, opts.min_free_gib)?
    ));
    let smoke = isolation::bwrap_smoke()?;
    let outer_mnt = proc::namespace("self", "mnt").ok_or("cannot read /proc/self/ns/mnt")?;
    let outer_net = proc::namespace("self", "net").ok_or("cannot read /proc/self/ns/net")?;
    let outer_pid = proc::namespace("self", "pid").ok_or("cannot read /proc/self/ns/pid")?;
    let host = isolation::capture_host(uid)?;
    if mode == EndpointMode::Resolved {
        // With the namespace these cannot be reached from inside the run anyway;
        // requiring them absent keeps the before/after comparison unambiguous.
        // The SOCKETS, not the per-uid directory: an empty directory a deck left
        // behind is common and harmless, and the snapshot still requires it
        // unchanged for the length of the run.
        for e in host.iter().filter(|e| {
            sandbox::flat_endpoints(uid).contains(&e.path)
                || sandbox::per_uid_endpoints(uid).contains(&e.path)
        }) {
            if e.file.is_some() {
                return Err(format!(
                    "host {} exists. Something on this host is running in the fallback case; \
                     find out what before starting a resolved-mode run (`ss -xlp` names the \
                     owner). Nothing was started and nothing was touched.",
                    e.path.display()
                ));
            }
        }
    }
    let deck_baseline = isolation::deck_census()?;
    let real_log = std::env::var_os("DOT_AGENT_DECK_LOG")
        .map(PathBuf::from)
        .or_else(|| {
            sandbox::passwd_entry().map(|(_, h)| h.join(".local/state/dot-agent-deck/deck.log"))
        });
    let log_mark = real_log.as_deref().and_then(isolation::mark_log);

    println!("xver ({}): inputs", direction.name());
    let old_src = old_binary(opts, &previous.tag, &releases, &mut ev)?;
    let (new_src, head_sha) = new_binary(opts, &clone, &target_dir, &runs_root, &mut ev)?;
    ev.head_sha = head_sha;

    let slug: String = opts
        .branch
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let sb_name = match direction {
        Direction::Forward => format!("{slug}-{}", epoch_secs()),
        Direction::Reverse => format!("{slug}-rev-{}", epoch_secs()),
    };
    let sb = Sandbox::create(&runs_root, &sb_name)?;
    let runs_root = std::fs::canonicalize(&runs_root).map_err(|e| format!("{e}"))?;
    ev.sandbox_root = sb.root.clone();

    // From here on the evidence file is written whatever happens.
    let evidence_path = evidence_path(opts.evidence.as_deref(), &root, &slug, direction);
    let mut inner_pid_ns: Option<String> = None;
    let outcome = (|| -> Result<(), String> {
        let fixture = probe::fixture(&sb, probe);
        sandbox::write_project(&sb, &fixture, probe.needs_commit())?;
        let harness_src = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        // Forward: the branch build is the one on `PATH` (the upgrade: new
        // binary on disk, old daemon in memory). Reverse: the previous release
        // is (the downgrade: old binary on disk, branch daemon in memory), and
        // the branch build is staged beside it.
        let mut staging: Vec<(&Path, PathBuf)> = vec![(old_src.as_path(), sb.old_bin())];
        match direction {
            Direction::Forward => staging.push((new_src.as_path(), sb.new_bin(direction))),
            Direction::Reverse => {
                staging.push((new_src.as_path(), sb.new_bin(direction)));
                staging.push((old_src.as_path(), sb.path_bin()));
            }
        }
        staging.push((harness_src.as_path(), sb.harness_bin()));
        if probe.needs_stub() {
            staging.push((harness_src.as_path(), sb.stub_claude()));
        }
        for (src, dst) in staging {
            ev.preflight.push(sandbox::stage_binary(src, &dst)?);
        }
        ev.old_binary = sb.old_bin();
        ev.new_binary = sb.new_bin(direction);
        for note in probes::prepare_outer(&sb, probe)? {
            ev.preflight.push(note);
        }

        let extra_env = probe.extra_env(&sb);
        let env = sandbox::run_env(&sb, spec, &user, &extra_env);
        sandbox::check_env(&env, &sb, spec, &extra_env)?;
        let matrices =
            EndpointMatrix::candidates(&sb, mode, spec.keep_xdg_runtime_dir, uid, direction);
        for m in &matrices {
            sandbox::check_socket_path_lengths(m)?;
        }
        let plan = isolation::Plan {
            root: sb.root.clone(),
            uid,
            user: user.clone(),
            mode,
            keep_xdg_runtime_dir: spec.keep_xdg_runtime_dir,
            experimental: opts.experimental,
            max_lifetime_secs: opts.max_agent_lifetime_secs,
            previous: previous.tag.clone(),
            direction,
            probe,
            fixture,
            extra_env,
            env,
            matrices,
            old_version: opts
                .old_binary
                .is_none()
                .then(|| format!("dot-agent-deck {}", previous.tag.trim_start_matches('v'))),
            masks: isolation::masks_for(&sb, uid)?,
            masked_home: isolation::home_to_mask(),
            outer_mnt_ns: outer_mnt.clone(),
        };
        write_private(
            &sb.plan_file(),
            &serde_json::to_string_pretty(&plan).map_err(|e| format!("{e}"))?,
        )?;

        ev.isolated(format!("namespace: {smoke}"));
        ev.isolated(format!(
            "outer half: mount namespace {outer_mnt}, network namespace {outer_net}, PID namespace {outer_pid}"
        ));
        for line in isolation::describe_host(&host) {
            ev.isolated(format!("baseline, {line}"));
        }
        ev.isolated(format!(
            "baseline: {} deck process(es) on the host, recorded by pid, start time and exe",
            deck_baseline.len()
        ));

        let sup = supervise(opts, &sb, &plan, &host, &outer_mnt, &outer_net, &outer_pid)?;
        inner_pid_ns = sup.inner_pid_ns.clone();
        merge_inner(&mut ev, &sb)?;
        for line in sup.notes {
            ev.isolated(line);
        }
        if let Some(f) = sup.failure {
            ev.isolation_failed(f);
        }
        Ok(())
    })();
    if let Err(e) = &outcome {
        record_outer_abort(&mut ev, e);
    }
    if opts.skip_build {
        let note = skip_build_note(&ev.head_sha, &ev.new_hello);
        println!("xver ({}): --skip-build: {note}", direction.name());
        ev.skip_build = Some(note);
    }

    println!("xver ({}): postconditions", direction.name());
    let clean = postconditions(
        &mut ev,
        &sb,
        &host,
        &deck_baseline,
        log_mark.as_ref(),
        inner_pid_ns.as_deref(),
    );

    let verdict = ev.verdict();
    let passed = run_passed(&verdict, outcome.is_ok(), clean);
    let disposal = if passed && !opts.keep_sandbox {
        match remove_sandbox(&sb, &runs_root) {
            Ok(()) => format!(
                "the sandbox `{}` was removed after the clean pass",
                sb.root.display()
            ),
            Err(e) => format!("the sandbox `{}` was kept: {e}", sb.root.display()),
        }
    } else {
        format!(
            "the sandbox `{}` was kept (not a clean pass, or --keep-sandbox)",
            sb.root.display()
        )
    };
    println!("xver ({}): {disposal}", direction.name());
    ev.postconditions.push(disposal);
    ev.write_to(&evidence_path)?;
    println!(
        "xver ({}): evidence written to {}",
        direction.name(),
        evidence_path.display()
    );
    println!("xver ({}): {}", direction.name(), verdict.label());
    Ok(passed)
}

/// Record an error the outer half hit before or around the namespace. With no
/// tell measured yet it becomes a failing `aborted` tell, so the run cannot
/// read as INCOMPLETE-for-want-of-tells when it actually broke down.
fn record_outer_abort(ev: &mut Evidence, e: &str) {
    ev.step(format!("RUN ABORTED outside the namespace: {e}"));
    if ev.tells.is_empty() {
        ev.tell(
            "aborted",
            "the run did not complete",
            report::Verdict::Fail,
            e.to_string(),
        );
    }
}

/// Whether a run counts as a clean pass: the exit status, and whether the
/// sandbox may be removed. Only a PASS verdict qualifies — FAIL, INCOMPLETE
/// and a measured non-discovery all exit non-zero — and only when the outer
/// half itself completed and every postcondition held.
fn run_passed(verdict: &RunVerdict, outer_completed: bool, postconditions_clean: bool) -> bool {
    *verdict == RunVerdict::Pass && outer_completed && postconditions_clean
}

/// Write a file only its owner can read.
fn write_private(path: &Path, body: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("create {}: {e}", path.display()))?;
    f.write_all(body.as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// What the outer half learned while the namespace ran.
struct Supervision {
    inner_pid_ns: Option<String>,
    notes: Vec<String>,
    failure: Option<String>,
}

/// Start the namespace and answer the inner half until it exits.
fn supervise(
    opts: &Opts,
    sb: &Sandbox,
    plan: &isolation::Plan,
    host: &[isolation::HostEndpoint],
    outer_mnt: &str,
    outer_net: &str,
    outer_pid: &str,
) -> Result<Supervision, String> {
    println!("xver: entering the namespace");
    let mut child = Command::new("bwrap")
        .args(isolation::bwrap_args(plan))
        .arg("--")
        .arg(sb.harness_bin())
        .arg(INNER_FLAG)
        .arg(sb.plan_file())
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("start bwrap: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or("no stdout pipe to the namespace")?;
    let mut stdin = child.stdin.take().ok_or("no stdin pipe to the namespace")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut sup = Supervision {
        inner_pid_ns: None,
        notes: Vec::new(),
        failure: None,
    };
    let mut preconnects = 0usize;
    let deadline = Instant::now() + Duration::from_secs(opts.run_timeout_secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Ok(line)) => {
                let Some(req) = line.strip_prefix(ctl::PREFIX) else {
                    println!("{line}");
                    continue;
                };
                let reply = match answer(req, &sup, sb, host, outer_mnt, outer_net, outer_pid) {
                    Ok(Some(ns)) => {
                        sup.notes.push(format!(
                            "the inner half reported mount/PID/network namespaces {ns}; each \
                             differs from the outer half's"
                        ));
                        sup.inner_pid_ns = ns.split_whitespace().nth(1).map(str::to_string);
                        "ok".to_string()
                    }
                    Ok(None) => {
                        preconnects += 1;
                        "ok".to_string()
                    }
                    Err(e) => {
                        if sup.failure.is_none() {
                            sup.failure = Some(format!("outer half: {e}"));
                        }
                        ctl::fail_reply(&e)
                    }
                };
                if writeln!(stdin, "{reply}")
                    .and_then(|_| stdin.flush())
                    .is_err()
                {
                    break;
                }
            }
            Ok(Err(e)) => {
                eprintln!("xver: reading the namespace's output: {e}");
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The child handle is not reaped, so this reaches bwrap and no
                // one else; `--die-with-parent` and the PID namespace take every
                // process inside down with it.
                let _ = child.kill();
                sup.failure = Some(format!(
                    "the inner half did not finish within {}s; the namespace was killed as a whole",
                    opts.run_timeout_secs
                ));
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(stdin);
    let status = child.wait().map_err(|e| format!("wait for bwrap: {e}"))?;
    sup.notes.push(format!(
        "outer half: answered {preconnects} pre-connect host check(s), every one of them against \
         the baseline; the namespace exited ({status})"
    ));
    Ok(sup)
}

/// Answer one control request. `Ok(Some(ns))` for the namespace report,
/// `Ok(None)` for a passed pre-connect check.
fn answer(
    req: &str,
    sup: &Supervision,
    sb: &Sandbox,
    host: &[isolation::HostEndpoint],
    outer_mnt: &str,
    outer_net: &str,
    outer_pid: &str,
) -> Result<Option<String>, String> {
    if let Some(rest) = req.strip_prefix("ns ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let [mnt, pid, net] = parts[..] else {
            return Err(format!("malformed namespace report {rest:?}"));
        };
        for (inner, outer, what) in [
            (mnt, outer_mnt, "mount"),
            (pid, outer_pid, "PID"),
            (net, outer_net, "network"),
        ] {
            if inner == outer {
                return Err(format!(
                    "the inner half shares the outer half's {what} namespace ({inner})"
                ));
            }
        }
        return Ok(Some(rest.to_string()));
    }
    if let Some(label) = req.strip_prefix("preconnect ") {
        if sup.inner_pid_ns.is_none() {
            return Err(format!("{label}: the namespace was never reported"));
        }
        isolation::verify_host(host).map_err(|e| format!("{label}: {e}"))?;
        let under =
            isolation::host_listeners_under(&sb.root).map_err(|e| format!("{label}: {e}"))?;
        if !under.is_empty() {
            return Err(format!(
                "{label}: the HOST network namespace has listeners under the sandbox: {under:?}"
            ));
        }
        return Ok(None);
    }
    Err(format!("unknown control request {req:?}"))
}

/// Fold the inner half's evidence into this half's.
fn merge_inner(ev: &mut Evidence, sb: &Sandbox) -> Result<(), String> {
    let raw = std::fs::read_to_string(sb.inner_evidence()).map_err(|e| {
        format!(
            "the inner half left no evidence at {}: {e}",
            sb.inner_evidence().display()
        )
    })?;
    let inner: Evidence =
        serde_json::from_str(&raw).map_err(|e| format!("parse the inner half's evidence: {e}"))?;
    ev.old_hello = inner.old_hello;
    ev.new_hello = inner.new_hello;
    ev.daemon_pid = inner.daemon_pid;
    ev.daemon_endpoint = inner.daemon_endpoint;
    ev.discovery = inner.discovery;
    ev.preflight.extend(inner.preflight);
    ev.steps.extend(inner.steps);
    ev.tells.extend(inner.tells);
    ev.excerpts.extend(inner.excerpts);
    ev.isolation.extend(inner.isolation);
    // Already printed by the inner half; keep the first cause, silently.
    if ev.isolation_failure.is_none() {
        ev.isolation_failure = inner.isolation_failure;
    }
    Ok(())
}

/// The outer half's checks after the namespace has exited. Returns whether the
/// sandbox may be removed.
fn postconditions(
    ev: &mut Evidence,
    sb: &Sandbox,
    host: &[isolation::HostEndpoint],
    deck_baseline: &[isolation::DeckProcess],
    log_mark: Option<&isolation::LogMark>,
    inner_pid_ns: Option<&str>,
) -> bool {
    let checks = PostChecks::gather(sb, host, log_mark, inner_pid_ns);
    judge_postconditions(ev, checks, &sb.root, host, deck_baseline, inner_pid_ns)
}

/// What the operator's real log gained during the run: its byte count, and
/// the lines mentioning the sandbox.
type LogGrowth = Result<(u64, Vec<String>), String>;

/// What each postcondition check returned, gathered before any of them is
/// judged, so the judging — which decides whether the run was clean — is a
/// pure function a unit test can drive. `Err` is a check that could not run.
struct PostChecks {
    /// Processes that still refer to the sandbox.
    leaks: Result<Vec<String>, String>,
    /// The host's endpoint candidates against the baseline.
    host: Result<(), String>,
    /// Host listeners under the sandbox.
    host_listeners: Result<Vec<proc::UnixListener>, String>,
    /// The host's deck processes now.
    decks: Result<Vec<isolation::DeckProcess>, String>,
    /// The operator's real log and what it gained during the run; `None` when
    /// it did not exist at baseline.
    log: Option<(PathBuf, LogGrowth)>,
}

impl PostChecks {
    /// Run every check, in the order they are judged.
    fn gather(
        sb: &Sandbox,
        host: &[isolation::HostEndpoint],
        log_mark: Option<&isolation::LogMark>,
        inner_pid_ns: Option<&str>,
    ) -> Self {
        PostChecks {
            leaks: isolation::processes_touching(&sb.root, inner_pid_ns),
            host: isolation::verify_host(host),
            host_listeners: isolation::host_listeners_under(&sb.root),
            decks: isolation::deck_census(),
            log: log_mark.map(|mark| {
                (
                    mark.path.clone(),
                    isolation::log_mentions_since(mark, &sb.root.display().to_string()),
                )
            }),
        }
    }
}

/// Record every postcondition in the evidence and return whether all of them
/// held. A check that FAILED and a check that COULD NOT RUN are treated alike:
/// each makes the run unclean and is recorded as an isolation failure, which
/// makes the verdict INCOMPLETE — an unmeasured postcondition is not a met one.
fn judge_postconditions(
    ev: &mut Evidence,
    checks: PostChecks,
    sandbox_root: &Path,
    host: &[isolation::HostEndpoint],
    deck_baseline: &[isolation::DeckProcess],
    inner_pid_ns: Option<&str>,
) -> bool {
    let mut clean = true;
    let mut fail = |ev: &mut Evidence, msg: String| {
        clean = false;
        ev.postconditions.push(format!("**FAILED:** {msg}"));
        ev.isolation_failed(msg);
    };
    match checks.leaks {
        Ok(hits) if hits.is_empty() => ev.postconditions.push(format!(
            "no process is left in the run's PID namespace ({}), and none refers to `{}` by cwd, \
             root, exe, open file or the run's marker",
            inner_pid_ns.unwrap_or("never reported"),
            sandbox_root.display()
        )),
        Ok(hits) => fail(
            ev,
            format!(
                "processes still refer to the sandbox after the namespace exited (NOT signalled — \
                 the outer half signals nothing but its own bwrap child): {}",
                hits.join("; ")
            ),
        ),
        Err(e) => fail(ev, format!("the leak census could not run: {e}")),
    }
    match checks.host {
        Ok(()) => {
            for line in isolation::describe_host(host) {
                ev.postconditions
                    .push(format!("unchanged since baseline, {line}"));
            }
        }
        Err(e) => fail(ev, e),
    }
    match checks.host_listeners {
        Ok(v) if v.is_empty() => ev
            .postconditions
            .push("the host's Unix socket table has no listener under the sandbox".to_string()),
        Ok(v) => fail(ev, format!("host listeners under the sandbox: {v:?}")),
        Err(e) => fail(ev, e),
    }
    match checks.decks {
        Ok(now) => {
            let gone: Vec<&isolation::DeckProcess> = deck_baseline
                .iter()
                .filter(|b| {
                    !now.iter()
                        .any(|n| n.pid == b.pid && n.start_time == b.start_time)
                })
                .collect();
            ev.postconditions.push(format!(
                "{} of {} baseline deck process(es) are still the same process (pid and start \
                 time){}",
                deck_baseline.len() - gone.len(),
                deck_baseline.len(),
                if gone.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; exited during the run, from outside it — this harness signals no host \
                         process: {}",
                        gone.iter()
                            .map(|g| format!("pid {} `{}`", g.pid, g.exe.display()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            ));
        }
        Err(e) => fail(ev, format!("the deck census could not run: {e}")),
    }
    match checks.log {
        Some((path, result)) => match result {
            Ok((n, hits)) if hits.is_empty() => ev.postconditions.push(format!(
                "the operator's real log `{}` gained {n} byte(s) from other writers during the \
                 run; none of them mention the sandbox",
                path.display()
            )),
            Ok((_, hits)) => fail(
                ev,
                format!(
                    "the operator's real log `{}` mentions the sandbox: {hits:?}",
                    path.display()
                ),
            ),
            Err(e) => fail(ev, e),
        },
        None => ev.postconditions.push(
            "the operator's real deck log does not exist, so there was nothing to escape into"
                .to_string(),
        ),
    }
    clean
}

/// Remove a finished sandbox — only the canonical `$S` of this run, only after
/// the leak census found nothing, and only if it is still the private directory
/// this run created directly under the runs root.
fn remove_sandbox(sb: &Sandbox, runs_root: &Path) -> Result<(), String> {
    let canon = std::fs::canonicalize(&sb.root).map_err(|e| format!("{e}"))?;
    if canon != sb.root || canon.parent() != Some(runs_root) {
        return Err(format!(
            "{} is not directly under {}; not removing it",
            canon.display(),
            runs_root.display()
        ));
    }
    sandbox::require_private_dir(&canon)?;
    std::fs::remove_dir_all(&canon).map_err(|e| format!("remove {}: {e}", canon.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_is_the_default_and_carries_no_probe() {
        let opts = Opts::parse_from(["xver", "--branch", "agent/dispatch-issue-1109"]);
        assert_eq!(opts.direction, DirectionArg::Forward);
        assert_eq!(opts.direction.directions(), vec![Direction::Forward]);
        assert_eq!(
            select_probe(opts.probe, &opts.branch, Direction::Forward),
            Ok(Probe::Generic),
            "an existing invocation keeps its meaning: rule 12's four tells"
        );
    }

    #[test]
    fn previous_has_no_hardcoded_default_and_an_explicit_one_is_kept_verbatim() {
        let opts = Opts::parse_from(["xver", "--branch", "b"]);
        assert_eq!(
            opts.previous, None,
            "an omitted --previous is resolved (previous.rs), never defaulted to a tag"
        );
        let opts = Opts::parse_from(["xver", "--branch", "b", "--previous", "v0.40.2"]);
        assert_eq!(opts.previous.as_deref(), Some("v0.40.2"));
    }

    #[test]
    fn a_reverse_run_selects_its_probe_from_the_branch() {
        assert_eq!(
            select_probe(
                ProbeArg::Auto,
                "agent/dispatch-issue-1109",
                Direction::Reverse
            ),
            Ok(Probe::TeardownInventory)
        );
        assert_eq!(
            select_probe(ProbeArg::Auto, "some/other-branch", Direction::Reverse),
            Ok(Probe::Generic)
        );
        assert_eq!(
            select_probe(
                ProbeArg::Generic,
                "agent/dispatch-issue-1109",
                Direction::Reverse
            ),
            Ok(Probe::Generic),
            "an explicit `generic` downgrades on purpose, never silently"
        );
    }

    #[test]
    fn an_explicit_probe_is_refused_in_the_forward_direction_rather_than_dropped() {
        let err = select_probe(ProbeArg::LogEscaping, "x", Direction::Forward)
            .expect_err("a forward run carries no probe");
        assert!(err.contains("reverse direction only"), "{err}");
    }

    #[test]
    fn both_runs_forward_then_reverse() {
        assert_eq!(
            DirectionArg::Both.directions(),
            vec![Direction::Forward, Direction::Reverse]
        );
    }

    #[test]
    fn a_reverse_run_never_overwrites_the_forward_evidence_file() {
        let root = Path::new("/repo");
        let fwd = evidence_path(None, root, "agent-x", Direction::Forward);
        let rev = evidence_path(None, root, "agent-x", Direction::Reverse);
        assert_eq!(
            fwd,
            Path::new("/repo/.dot-agent-deck/xver-evidence/agent-x.md")
        );
        assert_eq!(
            rev,
            Path::new("/repo/.dot-agent-deck/xver-evidence/agent-x-reverse.md")
        );
    }

    #[test]
    fn an_explicit_evidence_path_is_written_verbatim_in_either_direction() {
        let root = Path::new("/repo");
        for d in [Direction::Forward, Direction::Reverse] {
            for given in [
                "/out/e.md",
                "/out/control-main-1121-generic-reverse.md",
                "/out/e",
            ] {
                assert_eq!(
                    evidence_path(Some(Path::new(given)), root, "agent-x", d),
                    Path::new(given),
                    "{d:?}"
                );
            }
        }
    }

    const CHECKSUMS: &str = "\
6e0d67bea338fac639a66ce4c1cf2e31ef458c55bfc568d51a76a6aa3aaa307f  dot-agent-deck-darwin-amd64
86ea39485067afc2b58bf0c53bb19fdae9ccf43288bac8c6cc3ef5707c6def87  dot-agent-deck-linux-amd64
5ad0d21594739a56f7040dd55ddad30a994f7a0f7c29df6754608bc6656c5829  dot-agent-deck-linux-arm64
";

    #[test]
    fn the_published_checksum_is_the_one_line_naming_the_asset_exactly() {
        assert_eq!(
            published_sha256(CHECKSUMS, RELEASE_ASSET).as_deref(),
            Ok("86ea39485067afc2b58bf0c53bb19fdae9ccf43288bac8c6cc3ef5707c6def87"),
            "not the arm64 line beside it"
        );
        let binary_mode = "86EA39485067AFC2B58BF0C53BB19FDAE9CCF43288BAC8C6CC3EF5707C6DEF87 *dot-agent-deck-linux-amd64\n";
        assert_eq!(
            published_sha256(binary_mode, RELEASE_ASSET).as_deref(),
            Ok("86ea39485067afc2b58bf0c53bb19fdae9ccf43288bac8c6cc3ef5707c6def87"),
            "`shasum -b`'s `*name` and an upper-case digest"
        );
    }

    #[test]
    fn a_missing_duplicated_or_malformed_checksum_entry_is_refused() {
        let missing = "6e0d67bea338fac639a66ce4c1cf2e31ef458c55bfc568d51a76a6aa3aaa307f  dot-agent-deck-linux-amd64.sig\n";
        let err = published_sha256(missing, RELEASE_ASSET).expect_err("no exact entry");
        assert!(err.contains("no entry"), "{err}");
        let twice = format!("{CHECKSUMS}{CHECKSUMS}");
        let err = published_sha256(&twice, RELEASE_ASSET).expect_err("ambiguous");
        assert!(err.contains("2 times"), "{err}");
        let short = "86ea3948  dot-agent-deck-linux-amd64\n";
        let err = published_sha256(short, RELEASE_ASSET).expect_err("not a digest");
        assert!(err.contains("not a SHA-256"), "{err}");
        assert!(published_sha256("", RELEASE_ASSET).is_err());
    }

    #[test]
    fn build_changes_are_refused_by_default_and_the_help_carries_the_trust_boundary() {
        let opts = Opts::parse_from(["xver", "--branch", "b"]);
        assert!(!opts.allow_build_changes, "the opt-in is off unless given");
        let opts = Opts::parse_from(["xver", "--branch", "b", "--allow-build-changes"]);
        assert!(opts.allow_build_changes);
        use clap::CommandFactory;
        let help = Opts::command().render_help().to_string();
        for needle in [
            "TRUST BOUNDARY",
            "NOT an untrusted-code sandbox",
            "--allow-build-changes",
            "disposable VM",
        ] {
            assert!(
                help.contains(needle),
                "{needle:?} missing from --help:\n{help}"
            );
        }
    }

    #[test]
    fn an_explicit_evidence_path_is_refused_only_with_both_directions() {
        let given = Some(Path::new("/out/e.md"));
        assert!(check_evidence_arg(given, DirectionArg::Forward).is_ok());
        assert!(check_evidence_arg(given, DirectionArg::Reverse).is_ok());
        let refused = check_evidence_arg(given, DirectionArg::Both).expect_err("both");
        assert!(
            refused.contains("`--evidence /out/e.md` names one file"),
            "{refused}"
        );
        // The defaults already give each direction its own file.
        assert!(check_evidence_arg(None, DirectionArg::Both).is_ok());
    }
}

/// The outer half's verdict glue: `run_passed`, `record_outer_abort` and the
/// postcondition judging decide the exit status and whether the run was clean,
/// so they are covered here rather than trusted.
#[cfg(test)]
mod verdict_tests {
    use super::*;
    use report::Verdict;

    fn with_tells(verdicts: &[Verdict]) -> Evidence {
        let mut ev = Evidence::default();
        for (i, v) in verdicts.iter().enumerate() {
            ev.tell(&format!("tell-{}", i + 1), "t", *v, "measured");
        }
        ev
    }

    fn all_held() -> PostChecks {
        PostChecks {
            leaks: Ok(Vec::new()),
            host: Ok(()),
            host_listeners: Ok(Vec::new()),
            decks: Ok(Vec::new()),
            log: None,
        }
    }

    fn judge(ev: &mut Evidence, checks: PostChecks, baseline: &[isolation::DeckProcess]) -> bool {
        judge_postconditions(
            ev,
            checks,
            Path::new("/runs/agent-x-1"),
            &[],
            baseline,
            Some("pid:[4026532000]"),
        )
    }

    #[test]
    fn only_a_pass_with_the_outer_half_complete_and_every_postcondition_met_is_clean() {
        let v = with_tells(&[Verdict::Pass; 4]).verdict();
        assert_eq!(v, RunVerdict::Pass);
        assert!(run_passed(&v, true, true));
        assert!(!run_passed(&v, false, true), "the outer half aborted");
        assert!(!run_passed(&v, true, false), "a postcondition did not hold");
    }

    #[test]
    fn an_isolation_failure_dominates_even_a_failing_tell_and_is_incomplete() {
        let mut ev = with_tells(&[Verdict::Pass, Verdict::Fail]);
        ev.isolation_failed("host endpoint changed");
        let v = ev.verdict();
        assert!(
            matches!(v, RunVerdict::Incomplete(ref why) if why.contains("host endpoint changed")),
            "{v:?}"
        );
        assert!(!run_passed(&v, true, true));
    }

    #[test]
    fn any_failing_tell_is_a_fail_even_beside_an_unmeasured_one() {
        let v = with_tells(&[Verdict::Pass, Verdict::NotChecked, Verdict::Fail]).verdict();
        assert_eq!(v, RunVerdict::Fail);
        assert!(!run_passed(&v, true, true));
    }

    #[test]
    fn an_unmeasured_tell_is_incomplete_and_never_a_clean_pass() {
        let v = with_tells(&[Verdict::Pass, Verdict::Pass, Verdict::NotChecked]).verdict();
        assert!(matches!(v, RunVerdict::Incomplete(_)), "{v:?}");
        assert!(!run_passed(&v, true, true));
    }

    #[test]
    fn a_measured_non_discovery_exits_non_zero() {
        let mut ev = with_tells(&[Verdict::Pass; 3]);
        ev.discovery = Some("the old TUI lazy-spawned its own daemon".into());
        let v = ev.verdict();
        assert!(matches!(v, RunVerdict::OldClientCannotDiscover(_)), "{v:?}");
        assert!(!run_passed(&v, true, true));
    }

    #[test]
    fn an_outer_abort_before_any_tell_is_a_fail_not_an_incomplete() {
        let mut ev = Evidence::default();
        record_outer_abort(&mut ev, "start bwrap: No such file or directory");
        assert_eq!(ev.verdict(), RunVerdict::Fail);
        assert_eq!(ev.tells.len(), 1);
        assert_eq!(ev.tells[0].id, "aborted");
        assert!(!run_passed(&ev.verdict(), false, true));
    }

    #[test]
    fn an_outer_abort_after_passing_tells_adds_none_but_still_is_not_clean() {
        let mut ev = with_tells(&[Verdict::Pass; 4]);
        record_outer_abort(&mut ev, "the inner half left no evidence");
        assert_eq!(
            ev.tells.len(),
            4,
            "no tell is invented on top of measured ones"
        );
        let v = ev.verdict();
        assert_eq!(v, RunVerdict::Pass);
        assert!(
            !run_passed(&v, false, true),
            "the abort alone keeps it from being a clean pass"
        );
    }

    #[test]
    fn every_postcondition_held_is_clean_and_leaves_the_verdict_alone() {
        let mut ev = with_tells(&[Verdict::Pass; 4]);
        assert!(judge(&mut ev, all_held(), &[]));
        assert!(ev.isolation_failure.is_none());
        assert_eq!(ev.verdict(), RunVerdict::Pass);
        assert!(
            !ev.postconditions.iter().any(|p| p.contains("FAILED")),
            "{:?}",
            ev.postconditions
        );
        assert!(
            ev.postconditions
                .iter()
                .any(|p| p.contains("does not exist, so there was nothing to escape into")),
            "an absent real log is reported, not failed: {:?}",
            ev.postconditions
        );
    }

    #[test]
    fn a_postcondition_that_could_not_run_is_unclean_and_voids_the_run() {
        let cases: Vec<(&str, PostChecks)> = vec![
            (
                "leak census",
                PostChecks {
                    leaks: Err("cannot list /proc".into()),
                    ..all_held()
                },
            ),
            (
                "host endpoints",
                PostChecks {
                    host: Err("cannot stat /tmp/dot-agent-deck.sock".into()),
                    ..all_held()
                },
            ),
            (
                "host listeners",
                PostChecks {
                    host_listeners: Err("cannot read /proc/net/unix".into()),
                    ..all_held()
                },
            ),
            (
                "deck census",
                PostChecks {
                    decks: Err("cannot list /proc".into()),
                    ..all_held()
                },
            ),
            (
                "real log",
                PostChecks {
                    log: Some((
                        PathBuf::from("/home/op/.local/state/dot-agent-deck/deck.log"),
                        Err("stat: permission denied".into()),
                    )),
                    ..all_held()
                },
            ),
        ];
        for (name, checks) in cases {
            let mut ev = with_tells(&[Verdict::Pass; 4]);
            let clean = judge(&mut ev, checks, &[]);
            assert!(
                !clean,
                "{name}: a check that could not run is not a met one"
            );
            assert!(
                matches!(ev.verdict(), RunVerdict::Incomplete(_)),
                "{name}: {:?}",
                ev.verdict()
            );
            assert!(!run_passed(&ev.verdict(), true, clean), "{name}");
            assert!(
                ev.postconditions
                    .iter()
                    .any(|p| p.starts_with("**FAILED:**")),
                "{name}: {:?}",
                ev.postconditions
            );
        }
    }

    #[test]
    fn a_postcondition_that_failed_is_unclean_and_voids_the_run() {
        let cases: Vec<(&str, PostChecks)> = vec![
            (
                "a leaked process",
                PostChecks {
                    leaks: Ok(vec!["pid 7 (cwd /runs/agent-x-1/project)".into()]),
                    ..all_held()
                },
            ),
            (
                "a host listener under the sandbox",
                PostChecks {
                    host_listeners: Ok(vec![proc::UnixListener {
                        inode: 9,
                        path: "/runs/agent-x-1/attach.sock".into(),
                    }]),
                    ..all_held()
                },
            ),
            (
                "the real log mentions the sandbox",
                PostChecks {
                    log: Some((
                        PathBuf::from("/home/op/deck.log"),
                        Ok((120, vec!["… /runs/agent-x-1 …".into()])),
                    )),
                    ..all_held()
                },
            ),
        ];
        for (name, checks) in cases {
            let mut ev = with_tells(&[Verdict::Pass; 4]);
            assert!(!judge(&mut ev, checks, &[]), "{name}");
            assert!(
                matches!(ev.verdict(), RunVerdict::Incomplete(_)),
                "{name}: {:?}",
                ev.verdict()
            );
        }
    }

    #[test]
    fn a_baseline_deck_process_that_exited_during_the_run_is_reported_not_failed() {
        let baseline = vec![
            isolation::DeckProcess {
                pid: 42,
                start_time: 7,
                exe: PathBuf::from("/usr/bin/dot-agent-deck"),
            },
            isolation::DeckProcess {
                pid: 43,
                start_time: 8,
                exe: PathBuf::from("/usr/bin/dot-agent-deck"),
            },
        ];
        // pid 43 is still there but with another start time: a different
        // process under a reused pid, so it counts as gone too.
        let now = vec![isolation::DeckProcess {
            pid: 43,
            start_time: 99,
            exe: PathBuf::from("/usr/bin/dot-agent-deck"),
        }];
        let mut ev = with_tells(&[Verdict::Pass; 4]);
        let clean = judge(
            &mut ev,
            PostChecks {
                decks: Ok(now),
                ..all_held()
            },
            &baseline,
        );
        assert!(
            clean,
            "the harness signals no host process; this is a report"
        );
        let line = ev
            .postconditions
            .iter()
            .find(|p| p.contains("baseline deck process"))
            .expect("the census line");
        assert!(line.starts_with("0 of 2"), "{line}");
        assert!(line.contains("pid 42") && line.contains("pid 43"), "{line}");
    }
}

#[cfg(test)]
mod skip_build_tests {
    use super::*;

    const HEAD: &str = "e2bbb2050f00ba5eba11c0ffee00000000000000";

    fn hello(build_version: &str) -> String {
        format!(r#"{{"ok":true,"server_version":10,"build_version":"{build_version}"}}"#)
    }

    #[test]
    fn a_build_id_naming_the_head_is_reported_as_the_binarys_own_claim() {
        let note = skip_build_note(HEAD, &hello("0.41.0-ge2bbb205"));
        assert!(note.contains("build of the commit under test"), "{note}");
        assert!(note.contains("not an independent measurement"), "{note}");
        assert!(!note.contains("STALE"), "{note}");
    }

    #[test]
    fn a_build_id_naming_another_commit_is_called_stale() {
        let note = skip_build_note(HEAD, &hello("0.41.0-g66995314"));
        assert!(note.starts_with("**STALE:**"), "{note}");
        assert!(note.contains("`66995314`") && note.contains(HEAD), "{note}");
    }

    #[test]
    fn a_dirty_build_of_the_head_is_not_the_commit_under_test() {
        let note = skip_build_note(HEAD, &hello("0.41.0-ge2bbb205-dirty"));
        assert!(note.contains("-dirty"), "{note}");
        assert!(note.contains("not the commit under test"), "{note}");
    }

    #[test]
    fn a_build_id_with_no_commit_says_the_commit_is_not_knowable() {
        for id in ["0.41.0-unknown", "0.25.0-gamma.1-unknown", "custom"] {
            let note = skip_build_note(HEAD, &hello(id));
            assert!(note.contains("NOT knowable"), "{id}: {note}");
            assert!(note.contains(HEAD), "{id}: {note}");
        }
    }

    #[test]
    fn a_prerelease_version_does_not_hide_the_commit() {
        let note = skip_build_note(HEAD, &hello("0.25.0-gamma.1-ge2bbb205"));
        assert!(note.contains("build of the commit under test"), "{note}");
    }

    #[test]
    fn a_missing_hello_says_the_commit_is_not_knowable() {
        for raw in ["", "not json", r#"{"ok":true}"#] {
            let note = skip_build_note(HEAD, raw);
            assert!(note.contains("NOT knowable"), "{raw:?}: {note}");
        }
    }
}
