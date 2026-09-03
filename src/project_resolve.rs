//! Bounded, symlink-safe project resolution and project enumeration for the
//! daemon's project verbs (PRD #819 M3).
//!
//! Two things live here, and the split is the same one [`crate::bounded_read`]
//! makes: a **reader** carrying no policy beyond its bounds, and an
//! **enumeration** built on it.
//!
//! # Why the existing loader is not reachable from here
//!
//! [`crate::project_config::load_project_config`] uses `std::fs::read_to_string`
//! with no type check and no size check. That is fine for a file this process
//! wrote; it is not fine for a path a caller selected over the attach socket,
//! and `crate::bounded_read`'s own module doc (issue #328) already names the
//! three shapes that bite — a file that is enormous or growing exhausts memory,
//! a FIFO with no writer blocks forever inside `open(2)`, and a character
//! device never ends. The unbounded helper is pre-existing; **reaching it from a
//! caller-selected path is what PRD #819 adds**, so the bounded reader is this
//! PRD's problem rather than a pre-existing one.
//!
//! Canonicalising the project *directory* does not protect the config, either:
//! `.dot-agent-deck.toml` is resolved separately and may itself be a symlink
//! pointing outside the project or at a special file. [`read_config_file`] opens
//! the final component with `O_NOFOLLOW` for exactly that reason.
//!
//! # What is deliberately NOT claimed
//!
//! **No timing property.** Canonicalisation, directory traversal, `open(2)` and
//! TOML parsing do observably different amounts of work, and a concurrency bound
//! protects availability rather than constant time. The one property delivered
//! is narrower and is about the *response*: every refusal produced by
//! *resolving* an arbitrary caller-supplied path carries
//! [`crate::daemon_protocol::PROJECT_ERR_UNRESOLVED`] and one fixed sentence, so
//! the wire response does not directly distinguish "no such directory" from "no
//! config there". (A path that fails the wire-boundary *string* check earlier —
//! relative, empty, control-bearing, over-long — is a different refusal with its
//! own code, and it never reaches a filesystem at all.) See
//! [`generic_refusal`].
//!
//! # The disclosure split
//!
//! [`crate::project_config::ProjectConfigError`]'s `Display` renders the
//! offending TOML source line verbatim — its own doc comment says so, and notes
//! that escaping control bytes is not redaction. Returning it for a pasted path
//! would disclose file *content*, not merely existence. So detail is reserved
//! for a path the daemon **already knows** (it has something live there), which
//! is where "your config is broken, not empty" earns its keep; every other path
//! gets [`generic_refusal`] and the detail is logged daemon-locally.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;
use tracing::{debug, warn};

use crate::agent_pty::{AgentRecord, TabMembership, is_valid_orchestration_cwd};
use crate::event::{
    KnownProject, ProjectListing, ProjectOrchestration, ProjectRole, ResolvedProject,
};
use crate::project_config::{CONFIG_FILE_NAME, ProjectConfig, ProjectConfigError};

// ---------------------------------------------------------------------------
// The bounds. Every one is documented where it is defined, with the reason for
// that number rather than another.
// ---------------------------------------------------------------------------

/// Upper bound on a `.dot-agent-deck.toml` this daemon will read on behalf of a
/// caller-selected path.
///
/// **1 MiB**, the same figure and the same shape of argument as
/// [`crate::bounded_read::MAX_TASK_BYTES`], reused rather than re-derived: a
/// project config is hand-written declarative TOML, and this repository's own —
/// the largest in the tree, carrying a 300-line manual-test guide in comments —
/// is **32 KiB**, so 1 MiB clears the largest real one by 32x. Anything past it
/// is pathological rather than large, and gets a refusal instead of an
/// allocation.
///
/// It is checked twice, and the second check is the one that matters: once
/// against the recorded length from the open handle's `fstat` (cheap, and
/// refuses a huge file without reading a byte of it) and again through
/// [`crate::bounded_read::read_capped`], which is what catches a file that
/// *grows* past the limit between the two.
pub const MAX_PROJECT_CONFIG_BYTES: u64 = 1024 * 1024;

/// Upper bound on the orchestrations one resolve projects onto the wire.
///
/// **128.** The config byte cap alone does not bound the *response*: 1 MiB of
/// bare `[[orchestrations]]` headers declares roughly 55 000 blocks, each of
/// which projects a name up to a filesystem's 255-byte basename limit — about
/// 14 MB, the same order of magnitude as the frame ceiling itself
/// (`MAX_FRAME_LEN`, 16 MiB). So a small request could generate response work
/// approaching that ceiling, and this is what stops it. 128 is far above any
/// real config (this repository declares 4) and far below the cardinality needed
/// to build a large frame.
///
/// A config past the cap is **refused, not truncated** — the same disposition
/// [`crate::bounded_read::read_capped`] takes, and for the same reason: a
/// silently shortened list is a wrong answer that looks like a right one.
pub const MAX_PROJECT_ORCHESTRATIONS: usize = 128;

/// Upper bound on the roles of a single orchestration.
///
/// **64.** An orchestration's roles are panes a human watches, so the real
/// ceiling is screen real estate rather than memory — the largest in this
/// repository has 4. 64 is chosen on the same basis as
/// [`MAX_PROJECT_ORCHESTRATIONS`]: comfortably past anything anyone would write,
/// comfortably short of the cardinality needed to build a large frame. Together
/// the two cap one response at 128 x 64 role entries.
pub const MAX_PROJECT_ROLES: usize = 64;

/// Upper bound on each projected name (an orchestration's, a role's, a
/// project's display name).
///
/// **512 bytes.** Names are untrusted text out of a config file or a directory
/// basename; `NAME_MAX` is 255 bytes on the filesystems this actually runs on
/// (ext4, XFS, btrfs, APFS, NTFS), and a config-declared name that long is
/// already unreadable in a picker. 512 leaves headroom over that figure without
/// letting a single name carry a payload. Refused rather than truncated, so a
/// picker never shows a name that no longer matches the one a spawn would
/// resolve.
pub const MAX_PROJECTED_NAME_BYTES: usize = 512;

/// Upper bound on the candidate directories one enumeration will do filesystem
/// work for.
///
/// **32.** Enumeration revalidates every candidate, so the cost of a listing is
/// N canonicalisations plus N bounded reads — which is why the cap is applied
/// **before** any of that work rather than to the result. 32 is above the number
/// of distinct project directories a single daemon plausibly has live agents,
/// orchestrations and schedules in (each contributes at most one entry after
/// string-level deduplication), and low enough that the worst case is a few
/// dozen `stat`s rather than an unbounded sweep driven by however many agents a
/// caller can start.
pub const MAX_ENUMERATION_CANDIDATES: usize = 32;

/// How many project-verb filesystem operations may be in flight across the whole
/// daemon at once.
///
/// **4.** The bound exists because the handlers live in `handle_connection`,
/// which is async and per-connection: `serve_attach_with_counter` spawns each
/// accepted connection independently, so N concurrent resolves would otherwise
/// occupy N blocking threads. A timeout wrapped around an uncancellable blocking
/// read is not a substitute — the read still holds its thread — so the
/// protection is the bound, not a deadline.
///
/// 4 rather than 1 because a single permit would serialise unrelated clients
/// behind one slow directory, and rather than a large number because the point
/// is that project resolution never occupies more than a small fixed slice of
/// tokio's blocking pool, which the daemon also uses for PTY teardown and
/// `git status` probes. Both callers in this module take a bounded *quantity* of
/// work under a permit — at most [`MAX_ENUMERATION_CANDIDATES`] reads of at most
/// [`MAX_PROJECT_CONFIG_BYTES`] each. That is not the same as a bounded
/// duration, and no such claim is made: a `stat` on an unresponsive network
/// mount takes as long as it takes, which is the availability limit this bound
/// contains rather than removes.
pub const MAX_CONCURRENT_PROJECT_READS: usize = 4;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why a project did not resolve.
///
/// The variants exist for the **daemon-local** diagnostic and for the known-path
/// half of the disclosure split. They are deliberately *not* projected onto the
/// wire for an arbitrary caller-supplied path: every one of them renders as the
/// single [`generic_refusal`] there, so the reply does not directly distinguish
/// a missing directory from a directory with no config in it.
#[derive(Debug)]
pub enum ProjectResolveError {
    /// The path does not exist, is not reachable, or is not a directory.
    NotADirectory,
    /// The canonical form of the path is not UTF-8, so it cannot be the wire
    /// identity. Skipped explicitly rather than lossily converted — a lossy
    /// spelling is a path the caller could not send back.
    NotUtf8,
    /// The directory holds no `.dot-agent-deck.toml`, so it is a directory but
    /// not a project.
    NoConfig,
    /// The final `.dot-agent-deck.toml` component is a symlink. Refused; see
    /// [`read_config_file`].
    ConfigIsSymlink,
    /// The config path opened but is not a regular file — a FIFO, a device, a
    /// socket, a directory.
    ConfigNotRegularFile(&'static str),
    /// The config is larger than [`MAX_PROJECT_CONFIG_BYTES`], either by its
    /// recorded length or by growing past the cap during the read.
    ConfigTooLarge,
    /// The config could not be read.
    ConfigUnreadable(std::io::Error),
    /// The config did not parse. Carries the loader's own error type, whose
    /// `Display` renders the offending source line — which is precisely why it
    /// never reaches an arbitrary caller.
    ConfigInvalid(ProjectConfigError),
    /// The config declares more orchestrations than
    /// [`MAX_PROJECT_ORCHESTRATIONS`].
    TooManyOrchestrations(usize),
    /// One orchestration declares more roles than [`MAX_PROJECT_ROLES`].
    TooManyRoles(usize),
    /// A projected name exceeds [`MAX_PROJECTED_NAME_BYTES`].
    NameTooLong(usize),
    /// The blocking task carrying this work did not complete (runtime shutdown,
    /// or a panic inside it). Never a statement about the caller's path.
    Internal,
}

impl ProjectResolveError {
    /// The **daemon-local** diagnostic: the detailed sentence, safe to log and
    /// safe to return for a path the daemon already knows.
    ///
    /// Everything interpolated here goes through
    /// [`crate::config_validation::escape_multiline_for_terminal`], the same
    /// seam `ProjectConfigError`'s own `Display` uses, so a config carrying real
    /// `0x1B` bytes cannot emit terminal control sequences through a log line or
    /// a client that prints the message.
    pub fn detail(&self) -> String {
        let raw = match self {
            Self::NotADirectory => "no such directory, or the path is not a directory".to_string(),
            Self::NotUtf8 => {
                "the canonical form of that path is not UTF-8, so it cannot be a project identity"
                    .to_string()
            }
            Self::NoConfig => format!("the directory holds no {CONFIG_FILE_NAME}"),
            Self::ConfigIsSymlink => format!(
                "{CONFIG_FILE_NAME} is a symlink; a project config must be a regular file in the \
                 project directory itself"
            ),
            Self::ConfigNotRegularFile(kind) => {
                format!("{CONFIG_FILE_NAME} is {kind}, not a regular file")
            }
            Self::ConfigTooLarge => {
                format!("{CONFIG_FILE_NAME} exceeds the {MAX_PROJECT_CONFIG_BYTES}-byte limit")
            }
            Self::ConfigUnreadable(e) => format!("failed to read {CONFIG_FILE_NAME}: {e}"),
            // `ProjectConfigError`'s own `Display` already escapes; escaping the
            // composed string again is idempotent for the sequences that matter
            // and keeps one rule here rather than a per-variant exception.
            Self::ConfigInvalid(e) => format!("{e}"),
            Self::TooManyOrchestrations(n) => format!(
                "the config declares {n} orchestrations; at most {MAX_PROJECT_ORCHESTRATIONS} can \
                 be offered"
            ),
            Self::TooManyRoles(n) => format!(
                "an orchestration declares {n} roles; at most {MAX_PROJECT_ROLES} can be offered"
            ),
            Self::NameTooLong(n) => format!(
                "a declared name is {n} bytes; at most {MAX_PROJECTED_NAME_BYTES} can be offered"
            ),
            Self::Internal => "the daemon could not complete the request".to_string(),
        };
        crate::config_validation::escape_multiline_for_terminal(&raw)
    }
}

impl std::fmt::Display for ProjectResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail())
    }
}

/// The bounded refusal an **arbitrary** caller-supplied path gets.
///
/// One code and one sentence for every [`ProjectResolveError`], so the wire
/// response does not directly distinguish "no such directory" from "no config
/// there" — and, critically, carries no parser source line, no raw OS error and
/// no echo of the caller's own path. This is a claim about the *response* alone;
/// see the module doc for what is deliberately not claimed alongside it.
pub fn generic_refusal() -> String {
    format!(
        "{}: that path did not resolve to a project on this daemon",
        crate::daemon_protocol::PROJECT_ERR_UNRESOLVED
    )
}

/// The refusal a path the daemon **already knows** gets: the same stable code,
/// followed by the detail. This is where "your config is broken, not empty"
/// earns its keep, and it is reachable only for a directory the daemon has
/// something live in.
pub fn known_path_refusal(err: &ProjectResolveError) -> String {
    format!(
        "{}: {}",
        crate::daemon_protocol::PROJECT_ERR_UNRESOLVED,
        err.detail()
    )
}

// ---------------------------------------------------------------------------
// The bounded reader
// ---------------------------------------------------------------------------

/// Canonicalise `path` and confirm it is a directory whose canonical spelling
/// can cross the wire.
///
/// **Canonicalisation happens once, here, at the boundary**, and the canonical
/// form is what everything downstream uses — the config read, the projected
/// identity, the orchestration-name fallback. Getting it only partway through
/// the flow is PRD #220's bug verbatim: canonicalising a symlinked path changes
/// its *basename*, and an unnamed orchestration is named after that basename, so
/// a listing built from one spelling and a spawn built from another disagree
/// about the name.
pub fn canonicalize_project_dir(path: &Path) -> Result<PathBuf, ProjectResolveError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| ProjectResolveError::NotADirectory)?;
    if !std::fs::metadata(&canonical).is_ok_and(|m| m.is_dir()) {
        return Err(ProjectResolveError::NotADirectory);
    }
    if canonical.to_str().is_none() {
        return Err(ProjectResolveError::NotUtf8);
    }
    Ok(canonical)
}

/// Read one project-config file under every bound this module defines, and hand
/// back its bytes as a `String`.
///
/// `path` is the final `.dot-agent-deck.toml`, not the directory — split out
/// from [`read_project_config`] so the hostile-input tests can point it at a
/// real character device, which is the one shape a test cannot construct inside
/// a scratch directory without privileges.
///
/// Four properties, each of which the plain loader lacks:
///
/// * **Opened once, judged from the open handle.** The type check reads
///   `File::metadata` — an `fstat` on the descriptor already held — not a second
///   `std::fs::metadata` on the path, so there is no window in which the thing
///   checked and the thing read can differ.
/// * **A symlinked config is refused.** On Unix the open carries `O_NOFOLLOW`,
///   so the refusal is a property of the open itself rather than of a
///   check-then-open pair. The alternative — resolving the link and proving the
///   target stays beneath the canonical project root — was rejected: it buys a
///   use case nobody has asked for (the loader every other caller uses has never
///   promised link support), and "beneath the root" is a containment argument
///   that would have to hold against every future mount, bind mount and hardlink
///   rather than against the tree as it looks today. Refusing is a smaller claim
///   that is actually true.
/// * **The open cannot hang.** `O_NONBLOCK` too, because a plain `open(2)` of a
///   FIFO with no writer blocks *inside the open*, before any check could run —
///   the same reason [`crate::bounded_read`] sets it. The flag is ignored for
///   regular files, the only kind that survives the check.
/// * **Bounded twice.** The recorded length is checked before reading, and
///   [`crate::bounded_read::read_capped`] applies the cap again, which is what
///   catches a file growing between the two.
///
/// On a platform with no `O_NOFOLLOW` on `OpenOptions` the symlink check is a
/// separate `symlink_metadata` lookup from the open, which is a **narrower**
/// guarantee than the Unix path's — stated here rather than papered over.
pub fn read_config_file(path: &Path) -> Result<String, ProjectResolveError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    {
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ProjectResolveError::ConfigIsSymlink);
        }
    }

    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProjectResolveError::NoConfig);
        }
        Err(e) => {
            // Consulting the path here cannot reintroduce the TOCTOU the open
            // handle exists to avoid: the open has already failed, so nothing is
            // read on this branch either way, and the only thing a race can
            // change is the wording of an error returned regardless. Same
            // recovery `bounded_read::read_task_file` performs, for the same
            // reason — `O_NOFOLLOW` reports `ELOOP` on Linux and macOS but the
            // spelling is not portable, and a directory is refused outright by
            // `open` on Windows.
            return Err(match std::fs::symlink_metadata(path) {
                Ok(m) if m.file_type().is_symlink() => ProjectResolveError::ConfigIsSymlink,
                Ok(m) if m.is_dir() => ProjectResolveError::ConfigNotRegularFile("a directory"),
                _ => ProjectResolveError::ConfigUnreadable(e),
            });
        }
    };

    let metadata = file
        .metadata()
        .map_err(ProjectResolveError::ConfigUnreadable)?;
    if !metadata.is_file() {
        return Err(ProjectResolveError::ConfigNotRegularFile(
            describe_file_type(&metadata.file_type()),
        ));
    }
    if metadata.len() > MAX_PROJECT_CONFIG_BYTES {
        return Err(ProjectResolveError::ConfigTooLarge);
    }

    // The source noun never reaches an arbitrary caller — `read_capped`'s
    // message is folded into `ProjectResolveError` and only the daemon-local
    // detail renders it — so it names the file rather than the caller's path.
    crate::bounded_read::read_capped(file, MAX_PROJECT_CONFIG_BYTES, CONFIG_FILE_NAME).map_err(
        |message| {
            if message.contains("-byte limit") {
                ProjectResolveError::ConfigTooLarge
            } else {
                ProjectResolveError::ConfigUnreadable(std::io::Error::other(message))
            }
        },
    )
}

/// Read and parse `<dir>/.dot-agent-deck.toml` under every bound this module
/// defines.
///
/// `dir` is expected to be canonical already ([`canonicalize_project_dir`]),
/// because the orchestration-name fallback is derived from its basename.
pub fn read_project_config(dir: &Path) -> Result<ProjectConfig, ProjectResolveError> {
    let path = dir.join(CONFIG_FILE_NAME);
    let contents = read_config_file(&path)?;
    let mut config: ProjectConfig = toml::from_str(&contents).map_err(|source| {
        ProjectResolveError::ConfigInvalid(ProjectConfigError::Parse {
            path: path.display().to_string(),
            source,
        })
    })?;
    // The same normalisation `load_project_config` performs, and it has to
    // happen against the CANONICAL `dir`: an empty orchestration name resolves
    // to the directory basename, which is exactly what canonicalising a
    // symlinked spelling changes.
    for orch in &mut config.orchestrations {
        if orch.name.is_empty() {
            orch.name = crate::project_config::resolve_orchestration_name(&orch.name, dir);
        }
    }
    Ok(config)
}

/// Name a rejected file type. Mirrors [`crate::bounded_read`]'s helper; kept
/// separate because that one is private to its own policy and words its
/// fallback for `--task-file`.
fn describe_file_type(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        return "a directory";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if file_type.is_fifo() {
            return "a FIFO";
        }
        if file_type.is_socket() {
            return "a socket";
        }
        if file_type.is_char_device() {
            return "a character device";
        }
        if file_type.is_block_device() {
            return "a block device";
        }
    }
    "of an unrecognised type"
}

/// Project a loaded config onto the wire shape, under the cardinality and length
/// caps.
///
/// Roleless `[[orchestrations]]` are filtered out for the reason
/// [`crate::dispatch::available_orchestrations`] filters them: the spawn skips
/// them, so offering one would offer a target that cannot start.
pub fn project_config_onto_wire(
    dir: &Path,
    config: &ProjectConfig,
) -> Result<Vec<ProjectOrchestration>, ProjectResolveError> {
    if config.orchestrations.len() > MAX_PROJECT_ORCHESTRATIONS {
        return Err(ProjectResolveError::TooManyOrchestrations(
            config.orchestrations.len(),
        ));
    }
    // Matched by INDEX rather than by name, for the reason
    // `available_orchestrations` records: duplicate names are only a validation
    // warning, so comparing names would mark every namesake as the default.
    let default_index = crate::project_config::default_orchestration(config, dir).map(|d| d.index);

    let mut out = Vec::new();
    for (index, orch) in config.orchestrations.iter().enumerate() {
        if orch.roles.is_empty() {
            continue;
        }
        if orch.roles.len() > MAX_PROJECT_ROLES {
            return Err(ProjectResolveError::TooManyRoles(orch.roles.len()));
        }
        let name = crate::project_config::resolve_orchestration_name(&orch.name, dir);
        bound_name(&name)?;
        let mut roles = Vec::with_capacity(orch.roles.len());
        for role in &orch.roles {
            bound_name(&role.name)?;
            roles.push(ProjectRole {
                name: role.name.clone(),
                start: role.start,
            });
        }
        out.push(ProjectOrchestration {
            name,
            default: default_index == Some(index),
            roles,
        });
    }
    Ok(out)
}

fn bound_name(name: &str) -> Result<(), ProjectResolveError> {
    if name.len() > MAX_PROJECTED_NAME_BYTES {
        return Err(ProjectResolveError::NameTooLong(name.len()));
    }
    Ok(())
}

/// Resolve one project directory end to end: canonicalise, read under bounds,
/// project. **Blocking** — every caller on the async side goes through
/// [`run_bounded`].
pub fn resolve_project(path: &Path) -> Result<ResolvedProject, ProjectResolveError> {
    let canonical = canonicalize_project_dir(path)?;
    resolve_canonical_project(&canonical)
}

/// The half of [`resolve_project`] that runs against an already-canonical
/// directory, so enumeration does not canonicalise twice.
fn resolve_canonical_project(dir: &Path) -> Result<ResolvedProject, ProjectResolveError> {
    let config = read_project_config(dir)?;
    let orchestrations = project_config_onto_wire(dir, &config)?;
    Ok(ResolvedProject {
        // `canonicalize_project_dir` has already refused a non-UTF-8 canonical
        // form, so this cannot lose bytes.
        path: dir.to_string_lossy().into_owned(),
        orchestrations,
    })
}

// ---------------------------------------------------------------------------
// Keeping the blocking work off the runtime
// ---------------------------------------------------------------------------

fn project_fs_limit() -> &'static Arc<Semaphore> {
    static LIMIT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    LIMIT.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_PROJECT_READS)))
}

/// Run one unit of project filesystem work on a blocking thread, behind the
/// daemon-wide [`MAX_CONCURRENT_PROJECT_READS`] bound.
///
/// The permit is acquired **before** the blocking task is spawned and moved into
/// it, so it is held for exactly as long as a thread is occupied and released
/// when the closure returns. It lives in the closure's own frame, so an
/// unwinding panic drops it too rather than leaking a slot.
///
/// One call is one permit, so an enumeration's whole candidate sweep runs under
/// a single permit rather than acquiring one per candidate. That is deliberate:
/// per-candidate acquisition from inside a task that already holds one is the
/// shape that deadlocks, and the sweep is itself bounded by
/// [`MAX_ENUMERATION_CANDIDATES`].
pub async fn run_bounded<T, F>(f: F) -> Result<T, ProjectResolveError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let permit = project_fs_limit()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ProjectResolveError::Internal)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|_| ProjectResolveError::Internal)
}

// ---------------------------------------------------------------------------
// The daemon's own startup cwd
// ---------------------------------------------------------------------------

static DAEMON_STARTUP_CWD: OnceLock<Option<String>> = OnceLock::new();

/// Capture the daemon's own working directory, **once, at startup**.
///
/// A process's current directory is not guaranteed stable, so reading it lazily
/// at request time would make the answer depend on when it was asked. Called
/// from [`crate::daemon::run_daemon_with`]; a harness that serves the attach
/// protocol without going through it leaves this unset, and enumeration then
/// simply contributes no daemon-cwd candidate.
pub fn capture_daemon_startup_cwd() {
    let _ = DAEMON_STARTUP_CWD.set(
        std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(str::to_string)),
    );
}

/// The captured startup cwd, or `None` when it was never captured or is not
/// UTF-8.
pub fn daemon_startup_cwd() -> Option<String> {
    DAEMON_STARTUP_CWD.get().cloned().flatten()
}

// ---------------------------------------------------------------------------
// Enumeration
// ---------------------------------------------------------------------------

/// One directory the daemon has a reason to believe might be a project.
///
/// **A candidate, not a project.** An ordinary agent cwd or a scheduled task's
/// `working_dir` need not hold a `.dot-agent-deck.toml` at all, so treating the
/// seed's origin as proof would return directories that are not projects. Every
/// candidate is revalidated through the bounded reader before it is offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCandidate {
    /// The seed spelling, as the daemon holds it. Never what is returned — the
    /// canonical form is the identity.
    pub path: String,
    /// Epoch milliseconds of the most recent real activity the daemon recorded
    /// for whatever produced this candidate, or `None` for a seed with no clock
    /// (a scheduled task's working directory, the daemon's own startup cwd).
    ///
    /// This is a **fact the daemon already records**, not a heuristic:
    /// [`crate::state::SessionSnapshot::last_activity_ms`] when a live session
    /// joined, else [`AgentRecord::spawned_at_ms`].
    pub activity_ms: Option<i64>,
}

/// Gather every candidate directory from what the daemon already holds, apply
/// the string-form check, deduplicate, order deterministically and **cap** — all
/// without touching a filesystem.
///
/// The cap is applied here, before [`resolve_candidates`] does any filesystem
/// work, because the cap exists to bound that work: applying it to the result
/// would protect nothing.
///
/// **Ordering is total.** Candidates sort by activity descending (a candidate
/// with a timestamp before one without), then by path ascending — and paths are
/// unique by then, so two candidates with equal timestamps cannot order
/// arbitrarily. That matters twice over: it decides which candidates survive the
/// cap, and it is the order [`resolve_candidates`] nominates a primary from.
pub fn collect_candidates(
    startup_cwd: Option<&str>,
    agents: &[AgentRecord],
    schedule_dirs: &[String],
) -> Vec<ProjectCandidate> {
    // Keyed by the seed spelling, holding the best activity seen for it. Two
    // spellings of one directory are NOT collapsed here — that needs
    // canonicalisation, which is filesystem work, and happens in
    // `resolve_candidates` after the cap.
    let mut seen: BTreeMap<String, Option<i64>> = BTreeMap::new();
    {
        let mut add = |path: &str, activity: Option<i64>| {
            if !is_valid_orchestration_cwd(path) {
                return;
            }
            let slot = seen.entry(path.to_string()).or_insert(None);
            if activity > *slot {
                *slot = activity;
            }
        };

        for record in agents {
            let activity = record
                .live
                .as_ref()
                .and_then(|live| live.last_activity_ms)
                .or(record.spawned_at_ms);
            if let Some(cwd) = record.cwd.as_deref() {
                add(cwd, activity);
            }
            // The strongest of the seeds: an orchestration cwd is a project by
            // construction. It is still revalidated like every other candidate,
            // because "by construction" describes how it was created and not
            // what is on disk now.
            if let Some(TabMembership::Orchestration {
                orchestration_cwd: Some(cwd),
                ..
            }) = record.tab_membership.as_ref()
            {
                add(cwd, activity);
            }
        }
        for dir in schedule_dirs {
            add(dir, None);
        }
        if let Some(cwd) = startup_cwd {
            add(cwd, None);
        }
    }

    let mut candidates: Vec<ProjectCandidate> = seen
        .into_iter()
        .map(|(path, activity_ms)| ProjectCandidate { path, activity_ms })
        .collect();
    candidates.sort_by(|a, b| {
        b.activity_ms
            .cmp(&a.activity_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    candidates.truncate(MAX_ENUMERATION_CANDIDATES);
    candidates
}

/// Canonicalise, deduplicate and revalidate every candidate, and answer with the
/// projects that **currently resolve**. **Blocking** — the caller goes through
/// [`run_bounded`].
///
/// Deduplication happens **after** canonicalisation, not before: two spellings of
/// one project must collapse to one entry, and only the canonical form can see
/// that they are the same directory.
///
/// A candidate that fails to resolve contributes **nothing** — not an entry, not
/// its raw path, not an error string. The reason is logged daemon-locally.
pub fn resolve_candidates(candidates: &[ProjectCandidate]) -> ProjectListing {
    // Canonical path -> best activity seen for it. `BTreeMap` so the resulting
    // listing is ordered by path with no separate sort, and so equal-timestamp
    // candidates cannot order by hash.
    let mut by_canonical: BTreeMap<String, Option<i64>> = BTreeMap::new();
    for candidate in candidates {
        let canonical = match canonicalize_project_dir(Path::new(&candidate.path)) {
            Ok(p) => p,
            Err(e) => {
                // `debug!`, not `warn!`: a seed that is no longer a directory is
                // the ordinary outcome of enumerating live state, and one line
                // per candidate per call at warn level would bury the refusals
                // that do mean something.
                debug!(
                    reason = %e,
                    "project enumeration skipped a candidate that no longer canonicalises"
                );
                continue;
            }
        };
        let key = canonical.to_string_lossy().into_owned();
        let slot = by_canonical.entry(key).or_insert(None);
        if candidate.activity_ms > *slot {
            *slot = candidate.activity_ms;
        }
    }

    let mut projects = Vec::new();
    let mut primary: Option<(i64, String)> = None;
    for (path, activity) in by_canonical {
        let dir = PathBuf::from(&path);
        if let Err(e) = resolve_canonical_project(&dir) {
            // Daemon-local, never on the wire: a candidate that does not resolve
            // is simply absent from the listing. `debug!` for the reason above —
            // an agent cwd that is not a project is expected, not an anomaly.
            debug!(reason = %e, "project enumeration skipped a candidate that did not resolve");
            continue;
        }
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.clone());
        if name.len() > MAX_PROJECTED_NAME_BYTES {
            debug!("project enumeration skipped a candidate whose basename exceeds the name bound");
            continue;
        }
        // The primary is the survivor with the most recent recorded activity.
        // Ties break on the path, which is unique here, so the nomination is
        // deterministic rather than dependent on iteration luck. A survivor with
        // no timestamp is never nominated: `primary` is a fact about live state,
        // and `None` — "this daemon knows nothing live" — is the honest answer
        // rather than a failure.
        if let Some(ms) = activity {
            let better = match &primary {
                None => true,
                Some((best_ms, best_path)) => {
                    ms > *best_ms || (ms == *best_ms && path < *best_path)
                }
            };
            if better {
                primary = Some((ms, path.clone()));
            }
        }
        projects.push(KnownProject { path, name });
    }

    ProjectListing {
        projects,
        primary: primary.map(|(_, path)| path),
    }
}

/// Whether `raw` (or its canonical form) is a directory the daemon **already
/// knows** — the condition that unlocks the detailed diagnostic.
///
/// The raw comparison comes first and costs nothing. The canonical comparison
/// runs only when a canonical form exists, and it is what makes a seed spelled
/// through a symlink and a caller's canonical spelling the same directory.
pub fn is_known_seed(raw: &str, canonical: Option<&Path>, seeds: &[ProjectCandidate]) -> bool {
    if seeds.iter().any(|c| c.path == raw) {
        return true;
    }
    let Some(canonical) = canonical else {
        return false;
    };
    seeds
        .iter()
        .any(|c| std::fs::canonicalize(&c.path).is_ok_and(|p| p == canonical))
}

/// Resolve `path` for the wire, choosing the disclosure by trust.
///
/// **Blocking** — the caller goes through [`run_bounded`]. Returns either the
/// projection or the exact `error` string the refusal carries; the detail is
/// logged daemon-locally on every failure, whichever refusal goes back.
pub fn resolve_for_wire(path: &str, seeds: &[ProjectCandidate]) -> Result<ResolvedProject, String> {
    let mut canonical: Option<PathBuf> = None;
    let outcome = (|| {
        let dir = canonicalize_project_dir(Path::new(path))?;
        canonical = Some(dir.clone());
        resolve_canonical_project(&dir)
    })();
    let err = match outcome {
        Ok(resolved) => return Ok(resolved),
        Err(e) => e,
    };
    warn!(reason = %err, "resolve-project refused");
    if is_known_seed(path, canonical.as_deref(), seeds) {
        Err(known_path_refusal(&err))
    } else {
        Err(generic_refusal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL_PROJECT: &str = r#"
[[orchestrations]]
name = "loop"

[[orchestrations.roles]]
name = "planner"
command = "cat"
start = true

[[orchestrations.roles]]
name = "builder"
command = "cat"
"#;

    /// A config that fails to parse, with a real ESC byte **on the offending
    /// source line** — so `toml`'s gutter rendering quotes it verbatim and the
    /// disclosure tests are asserting against a payload that genuinely reaches
    /// the error, not one the renderer happens to omit.
    const MALFORMED: &str = "bogus = \u{1b}[31mPWNED\u{1b}[0m\n";

    fn write_project(dir: &Path, toml: &str) {
        std::fs::write(dir.join(CONFIG_FILE_NAME), toml).expect("seed project config");
    }

    /// A canonical scratch root. Canonicalised because the harness temp base can
    /// itself sit behind a symlink, and every assertion here compares canonical
    /// spellings.
    fn scratch() -> (tempfile::TempDir, PathBuf) {
        let dir = crate::test_temp::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonicalize scratch root");
        (dir, root)
    }

    /// Run `f` on a scratch thread and fail — rather than hang the tier — if it
    /// has not returned within five seconds. Half of what the hostile-input
    /// tests assert is that the call returns at all.
    fn within_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the bounded read must return promptly, not block");
        handle.join().expect("worker thread panicked");
        got
    }

    #[cfg(unix)]
    fn mkfifo_at(path: &Path) {
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).expect("cstring");
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `mkfifo` only reads through it.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
    }

    fn agent_with(cwd: &str, spawned_at_ms: Option<i64>) -> AgentRecord {
        AgentRecord {
            id: cwd.to_string(),
            pane_id_env: None,
            display_name: None,
            cwd: Some(cwd.to_string()),
            tab_membership: None,
            agent_type: None,
            rows: 0,
            cols: 0,
            live: None,
            spawned_at_ms,
        }
    }

    // --- the reader's hostile inputs --------------------------------------

    #[test]
    fn a_regular_config_under_the_cap_reads() {
        let (_guard, root) = scratch();
        write_project(&root, SMALL_PROJECT);
        let config = read_project_config(&root).expect("a small regular config must read");
        assert_eq!(config.orchestrations.len(), 1);
    }

    #[test]
    fn an_oversized_config_is_refused_by_its_recorded_length() {
        let (_guard, root) = scratch();
        // One byte over the cap, so the `fstat` check is what refuses it and
        // nothing is read.
        let mut body = String::from("# ");
        body.push_str(&"x".repeat(MAX_PROJECT_CONFIG_BYTES as usize));
        write_project(&root, &body);
        let err = read_project_config(&root).expect_err("an oversized config must be refused");
        assert!(
            matches!(err, ProjectResolveError::ConfigTooLarge),
            "expected ConfigTooLarge, got {err:?}"
        );
    }

    /// The growth case: a source whose recorded length is under the cap when the
    /// metadata is taken and over it by the time the bytes are read. Exercised
    /// at the primitive that catches it, because racing a real writer against an
    /// `fstat` is not something a test can make deterministic — and the second
    /// cap is the whole reason the first one is not sufficient.
    #[test]
    fn a_config_growing_past_the_cap_during_the_read_is_still_refused() {
        let err = within_timeout(|| {
            crate::bounded_read::read_capped(
                std::io::repeat(b'#'),
                MAX_PROJECT_CONFIG_BYTES,
                CONFIG_FILE_NAME,
            )
            .expect_err("a source past the cap must be refused")
        });
        assert!(
            err.contains(&format!(
                "exceeds the {MAX_PROJECT_CONFIG_BYTES}-byte limit"
            )),
            "the second cap must refuse rather than truncate: {err}"
        );
    }

    /// The hang case. With no writer attached a plain `open(2)` of this FIFO
    /// never returns, so this asserts both that the read is refused and that it
    /// is refused *promptly*.
    #[cfg(unix)]
    #[test]
    fn a_fifo_config_is_refused_without_blocking() {
        let (_guard, root) = scratch();
        mkfifo_at(&root.join(CONFIG_FILE_NAME));
        let path = root.clone();
        let err = within_timeout(move || {
            read_project_config(&path).expect_err("a FIFO config must be refused")
        });
        assert!(
            matches!(err, ProjectResolveError::ConfigNotRegularFile("a FIFO")),
            "expected a FIFO refusal, got {err:?}"
        );
    }

    /// The endless-device case: `/dev/zero` never stops producing bytes, and is
    /// refused on its type before one is read. A device node cannot be created
    /// inside a scratch directory without privileges, so the reader is pointed
    /// at the real one — which is why [`read_config_file`] takes the file path
    /// rather than only the directory.
    #[cfg(unix)]
    #[test]
    fn a_device_config_is_refused_without_reading_it() {
        if !Path::new("/dev/zero").exists() {
            eprintln!("SKIP: /dev/zero is absent on this host");
            return;
        }
        let err = within_timeout(|| {
            read_config_file(Path::new("/dev/zero")).expect_err("a device must be refused")
        });
        assert!(
            matches!(
                err,
                ProjectResolveError::ConfigNotRegularFile("a character device")
            ),
            "expected a character-device refusal, got {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_config_is_refused_even_when_its_target_is_a_valid_config() {
        let (_guard, root) = scratch();
        let elsewhere = root.join("elsewhere.toml");
        std::fs::write(&elsewhere, SMALL_PROJECT).expect("write link target");
        let project = root.join("project");
        std::fs::create_dir(&project).expect("mkdir");
        std::os::unix::fs::symlink(&elsewhere, project.join(CONFIG_FILE_NAME)).expect("symlink");

        let err = read_project_config(&project)
            .expect_err("a symlinked project config must be refused by default");
        assert!(
            matches!(err, ProjectResolveError::ConfigIsSymlink),
            "expected ConfigIsSymlink, got {err:?}"
        );
    }

    #[test]
    fn a_control_bearing_path_never_reaches_a_filesystem() {
        // The wire boundary refuses it, so enumeration drops it before any
        // filesystem work — the same predicate `validate_project_path` applies
        // to a caller-supplied path.
        let hostile = "/tmp/\u{1b}[31mpwned";
        assert!(
            !is_valid_orchestration_cwd(hostile),
            "a control-bearing path must fail the string-form check"
        );
        let candidates = collect_candidates(Some(hostile), &[agent_with(hostile, Some(1))], &[]);
        assert!(
            candidates.is_empty(),
            "a control-bearing candidate must be dropped before canonicalisation: {candidates:?}"
        );
    }

    #[test]
    fn excessive_orchestration_cardinality_is_refused_rather_than_truncated() {
        let (_guard, root) = scratch();
        let mut body = String::new();
        for i in 0..=MAX_PROJECT_ORCHESTRATIONS {
            body.push_str(&format!(
                "[[orchestrations]]\nname = \"o{i}\"\n\n[[orchestrations.roles]]\n\
                 name = \"r\"\ncommand = \"cat\"\n\n"
            ));
        }
        write_project(&root, &body);
        let config = read_project_config(&root).expect("the config itself is small enough to read");
        let err = project_config_onto_wire(&root, &config)
            .expect_err("too many orchestrations must be refused");
        assert!(
            matches!(err, ProjectResolveError::TooManyOrchestrations(n) if n > MAX_PROJECT_ORCHESTRATIONS),
            "expected TooManyOrchestrations, got {err:?}"
        );
    }

    #[test]
    fn excessive_role_cardinality_is_refused_rather_than_truncated() {
        let (_guard, root) = scratch();
        let mut body = String::from("[[orchestrations]]\nname = \"big\"\n\n");
        for i in 0..=MAX_PROJECT_ROLES {
            body.push_str(&format!(
                "[[orchestrations.roles]]\nname = \"r{i}\"\ncommand = \"cat\"\n\n"
            ));
        }
        write_project(&root, &body);
        let config = read_project_config(&root).expect("the config is small enough to read");
        let err =
            project_config_onto_wire(&root, &config).expect_err("too many roles must be refused");
        assert!(
            matches!(err, ProjectResolveError::TooManyRoles(n) if n > MAX_PROJECT_ROLES),
            "expected TooManyRoles, got {err:?}"
        );
    }

    #[test]
    fn an_over_long_name_is_refused_rather_than_truncated() {
        let (_guard, root) = scratch();
        let long = "n".repeat(MAX_PROJECTED_NAME_BYTES + 1);
        write_project(
            &root,
            &format!(
                "[[orchestrations]]\nname = \"{long}\"\n\n[[orchestrations.roles]]\n\
                 name = \"r\"\ncommand = \"cat\"\n"
            ),
        );
        let config = read_project_config(&root).expect("read");
        let err = project_config_onto_wire(&root, &config)
            .expect_err("an over-long name must be refused");
        assert!(
            matches!(err, ProjectResolveError::NameTooLong(n) if n > MAX_PROJECTED_NAME_BYTES),
            "expected NameTooLong, got {err:?}"
        );
    }

    /// Concurrent slow resolves: four times as many callers as permits, every
    /// one of them returning. The property is that the bound queues rather than
    /// starves — a caller that never gets a permit is the failure this asserts
    /// against.
    #[test]
    fn concurrent_resolves_all_complete_under_the_concurrency_bound() {
        let (_guard, root) = scratch();
        write_project(&root, SMALL_PROJECT);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let n = MAX_CONCURRENT_PROJECT_READS * 4;
        let results: Vec<bool> = runtime.block_on(async move {
            let mut handles = Vec::new();
            for _ in 0..n {
                let path = root.clone();
                handles.push(tokio::spawn(async move {
                    run_bounded(move || resolve_project(&path).is_ok())
                        .await
                        .expect("the bounded call must complete rather than be starved")
                }));
            }
            let mut out = Vec::new();
            for handle in handles {
                out.push(handle.await.expect("join"));
            }
            out
        });
        assert_eq!(results.len(), n, "every caller must be answered");
        assert!(
            results.iter().all(|ok| *ok),
            "every resolve of a valid project must succeed: {results:?}"
        );
    }

    // --- enumeration -------------------------------------------------------

    #[test]
    fn the_candidate_cap_applies_before_any_filesystem_work() {
        let agents: Vec<AgentRecord> = (0..MAX_ENUMERATION_CANDIDATES * 3)
            .map(|i| agent_with(&format!("/nonexistent/project-{i:04}"), Some(i as i64)))
            .collect();
        let candidates = collect_candidates(None, &agents, &[]);
        assert_eq!(
            candidates.len(),
            MAX_ENUMERATION_CANDIDATES,
            "the candidate set must be capped before canonicalisation"
        );
        // None of these paths exists, so a cap applied only after the filesystem
        // work would have had to `stat` every one of them to find that out.
        assert!(
            candidates.iter().all(|c| !Path::new(&c.path).exists()),
            "the cap must not depend on the candidates existing"
        );
    }

    #[test]
    fn equal_timestamps_order_deterministically_rather_than_arbitrarily() {
        let agents = vec![
            agent_with("/tmp/zeta", Some(500)),
            agent_with("/tmp/alpha", Some(500)),
            agent_with("/tmp/mid", Some(500)),
        ];
        let first = collect_candidates(None, &agents, &[]);
        let mut reversed = agents.clone();
        reversed.reverse();
        let second = collect_candidates(None, &reversed, &[]);
        assert_eq!(
            first, second,
            "candidates tying on a timestamp must order by path, not by input order"
        );
        let paths: Vec<&str> = first.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["/tmp/alpha", "/tmp/mid", "/tmp/zeta"]);
    }

    #[test]
    fn a_candidate_with_a_timestamp_outranks_one_without() {
        let agents = vec![
            agent_with("/tmp/aaa", None),
            agent_with("/tmp/zzz", Some(1)),
        ];
        let candidates = collect_candidates(None, &agents, &[]);
        assert_eq!(
            candidates[0].path, "/tmp/zzz",
            "activity beats alphabetical order: {candidates:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_project_collapse_to_one_entry_after_canonicalisation() {
        let (_guard, root) = scratch();
        let project = root.join("real-project");
        std::fs::create_dir(&project).expect("mkdir");
        write_project(&project, SMALL_PROJECT);
        let alias = root.join("current");
        std::os::unix::fs::symlink(&project, &alias).expect("symlink");

        let listing = resolve_candidates(&[
            ProjectCandidate {
                path: project.to_string_lossy().into_owned(),
                activity_ms: Some(10),
            },
            ProjectCandidate {
                path: alias.to_string_lossy().into_owned(),
                activity_ms: Some(20),
            },
        ]);
        assert_eq!(
            listing.projects.len(),
            1,
            "two spellings of one project must collapse to one entry: {:?}",
            listing.projects
        );
        assert_eq!(
            Path::new(&listing.projects[0].path),
            project,
            "the surviving entry must be the canonical spelling"
        );
        assert_eq!(
            listing.primary.as_deref(),
            Some(project.to_string_lossy().as_ref()),
            "the merged entry must keep the better of the two timestamps"
        );
    }

    #[test]
    fn a_candidate_that_is_not_a_project_is_excluded() {
        let (_guard, root) = scratch();
        let bare = root.join("bare-agent-cwd");
        std::fs::create_dir(&bare).expect("mkdir");
        let project = root.join("real-project");
        std::fs::create_dir(&project).expect("mkdir");
        write_project(&project, SMALL_PROJECT);

        let listing = resolve_candidates(&[
            ProjectCandidate {
                path: bare.to_string_lossy().into_owned(),
                activity_ms: Some(99),
            },
            ProjectCandidate {
                path: project.to_string_lossy().into_owned(),
                activity_ms: Some(1),
            },
        ]);
        let offered: Vec<&str> = listing.projects.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(
            offered,
            vec![project.to_string_lossy().as_ref()],
            "a directory holding no {CONFIG_FILE_NAME} is a candidate, not a project"
        );
        assert_eq!(
            listing.primary.as_deref(),
            Some(project.to_string_lossy().as_ref()),
            "the primary must be nominated from the SURVIVORS, not from the seeds"
        );
    }

    #[test]
    fn nothing_live_means_no_primary_rather_than_a_failure() {
        let (_guard, root) = scratch();
        write_project(&root, SMALL_PROJECT);
        let listing = resolve_candidates(&[ProjectCandidate {
            path: root.to_string_lossy().into_owned(),
            activity_ms: None,
        }]);
        assert_eq!(listing.projects.len(), 1);
        assert!(
            listing.primary.is_none(),
            "a survivor with no recorded activity must not be nominated: {:?}",
            listing.primary
        );
    }

    /// Non-UTF-8 is **skipped explicitly**, not lossily converted. The seed side
    /// is structural — `AgentRecord.cwd` is a `String` — so the case that can
    /// actually arise is a UTF-8 spelling whose canonical form is not, which is
    /// what a symlink into a non-UTF-8 directory produces.
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_canonical_form_is_skipped_rather_than_lossily_converted() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt as _;
        let (_guard, root) = scratch();
        let bad = root.join(OsStr::from_bytes(b"proj-\xff"));
        std::fs::create_dir(&bad).expect("mkdir");
        write_project(&bad, SMALL_PROJECT);
        let alias = root.join("utf8-alias");
        std::os::unix::fs::symlink(&bad, &alias).expect("symlink");

        let err = canonicalize_project_dir(&alias)
            .expect_err("a non-UTF-8 canonical form must be refused");
        assert!(
            matches!(err, ProjectResolveError::NotUtf8),
            "expected NotUtf8, got {err:?}"
        );
        let listing = resolve_candidates(&[ProjectCandidate {
            path: alias.to_string_lossy().into_owned(),
            activity_ms: Some(1),
        }]);
        assert!(
            listing.projects.is_empty(),
            "a candidate whose canonical form is not UTF-8 must be skipped: {:?}",
            listing.projects
        );
    }

    // --- the disclosure split ---------------------------------------------

    #[test]
    fn an_arbitrary_path_refusal_carries_no_parser_line_no_os_error_and_no_path() {
        let (_guard, root) = scratch();
        let project = root.join("broken-project");
        std::fs::create_dir(&project).expect("mkdir");
        write_project(&project, MALFORMED);

        let refusal = resolve_for_wire(project.to_str().unwrap(), &[])
            .expect_err("a malformed config must be refused");
        assert_eq!(
            refusal,
            generic_refusal(),
            "an arbitrary path must get the one bounded refusal and nothing else"
        );
        assert!(
            !refusal.contains("PWNED") && !refusal.contains("TOML"),
            "no parser source line may escape: {refusal}"
        );
        assert!(
            !refusal.contains("broken-project") && !refusal.contains(root.to_str().unwrap()),
            "the caller's own path must not be echoed: {refusal}"
        );
        assert!(
            !refusal.contains("os error"),
            "no raw OS error may escape: {refusal}"
        );

        // The same shape for a path that does not exist at all, and for a
        // directory that is simply not a project — all three indistinguishable
        // in the response.
        let missing = resolve_for_wire(&format!("{}/no-such-dir", root.display()), &[])
            .expect_err("a missing directory must be refused");
        let bare = root.join("bare");
        std::fs::create_dir(&bare).expect("mkdir");
        let no_config = resolve_for_wire(bare.to_str().unwrap(), &[])
            .expect_err("a bare directory is not a project");
        assert_eq!(
            missing, refusal,
            "the response must not directly distinguish a missing directory from a broken config"
        );
        assert_eq!(
            no_config, refusal,
            "the response must not directly distinguish a bare directory from a broken config"
        );
    }

    #[test]
    fn a_known_path_refusal_carries_the_detail() {
        let (_guard, root) = scratch();
        let project = root.join("broken-project");
        std::fs::create_dir(&project).expect("mkdir");
        write_project(&project, MALFORMED);

        let seeds = vec![ProjectCandidate {
            path: project.to_string_lossy().into_owned(),
            activity_ms: Some(1),
        }];
        let refusal = resolve_for_wire(project.to_str().unwrap(), &seeds)
            .expect_err("a malformed config must be refused");
        assert_ne!(
            refusal,
            generic_refusal(),
            "a path the daemon already knows must get the detailed diagnostic"
        );
        assert!(
            refusal.contains("Failed to parse"),
            "the detail must say the config is broken rather than empty: {refusal}"
        );
        assert!(
            !refusal.contains('\u{1b}'),
            "the detail must still be escaped before it reaches a terminal: {refusal:?}"
        );
    }

    #[test]
    fn a_valid_project_resolves_to_its_canonical_spelling() {
        let (_guard, root) = scratch();
        write_project(&root, SMALL_PROJECT);
        let resolved = resolve_for_wire(root.to_str().unwrap(), &[]).expect("a valid project");
        assert_eq!(Path::new(&resolved.path), root);
        assert_eq!(resolved.orchestrations.len(), 1);
        assert_eq!(resolved.orchestrations[0].name, "loop");
        let roles: Vec<(&str, bool)> = resolved.orchestrations[0]
            .roles
            .iter()
            .map(|r| (r.name.as_str(), r.start))
            .collect();
        assert_eq!(roles, vec![("planner", true), ("builder", false)]);
    }

    #[cfg(unix)]
    #[test]
    fn an_unnamed_orchestration_is_named_after_the_canonical_basename() {
        let (_guard, root) = scratch();
        let project = root.join("canonical-project");
        std::fs::create_dir(&project).expect("mkdir");
        write_project(
            &project,
            "[[orchestrations]]\n\n[[orchestrations.roles]]\nname = \"planner\"\n\
             command = \"cat\"\nstart = true\n",
        );
        let alias = root.join("current");
        std::os::unix::fs::symlink(&project, &alias).expect("symlink");

        let resolved = resolve_for_wire(alias.to_str().unwrap(), &[]).expect("resolve via symlink");
        assert_eq!(
            Path::new(&resolved.path),
            project,
            "the canonical path is the identity, not the spelling that was sent"
        );
        assert_eq!(
            resolved.orchestrations[0].name, "canonical-project",
            "canonicalisation changes the basename, and the name follows it"
        );
    }
}
