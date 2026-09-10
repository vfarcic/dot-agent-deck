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

/// Upper bound on a projected **project display name** — the directory
/// basename enumeration offers beside a path.
///
/// **512 bytes.** A basename is untrusted text out of a directory entry;
/// `NAME_MAX` is 255 bytes on the filesystems this actually runs on (ext4, XFS,
/// btrfs, APFS, NTFS), and a directory name that long is already unreadable in
/// a picker. 512 leaves headroom over that figure without letting a single name
/// carry a payload. A candidate past it is **dropped from the listing rather
/// than truncated** — the same disposition every other bound here takes, for
/// the same reason: a silently shortened name is a wrong answer that looks like
/// a right one. It is a drop rather than a refusal because the path is still a
/// perfectly good project; only its label is unusable, and the enumeration's
/// contract is already "include only what resolves".
///
/// **This bound applies only to the display-only half**, and
/// [`MAX_PROJECTED_LAUNCH_NAME_BYTES`] applies to the two projected names that
/// are protocol identities. The split is the PRD #819 audit's second P2 finding:
/// a project's basename is rendered and never sent back, so its ceiling is a
/// readability question; an orchestration's or a role's name is sent back and
/// has to satisfy the spawn's own validator, so its ceiling is a
/// *compatibility* question and is not ours to choose.
pub const MAX_PROJECTED_NAME_BYTES: usize = 512;

/// Upper bound on a projected **orchestration or role name** — the two
/// projected names a client sends back.
///
/// **Derived from [`crate::agent_pty::DISPLAY_NAME_MAX_LEN`] rather than
/// chosen**, because the consumer's limit is the only limit that can be right
/// here. PRD #819 audit fix (P2, finding 1): this used to be
/// [`MAX_PROJECTED_NAME_BYTES`] (512) while the value's *destination* accepts
/// 128 — an orchestration name becomes `TabMembership::Orchestration.name` and
/// a role name becomes both `TabMembership::Orchestration.role_name` and the
/// pane's `display_name`, and `agent_pty::validate_tab_membership` runs every
/// one of those through `is_valid_display_name`, whose length rule is
/// `DISPLAY_NAME_MAX_LEN`. An over-long membership does not degrade: it
/// **fails the spawn** with `AgentPtyError::Validation`.
///
/// So a name of 129..=512 bytes was offered by the picker and then refused at
/// launch. Two limits on one value, in two crates, is how that class of bug
/// returns, so there is now one — and the direction is deliberate:
///
/// * **Tightening the projection** is what shipped. It is strictly stronger
///   against a hostile config (the response-size argument
///   [`MAX_PROJECTED_NAME_BYTES`] and [`MAX_PROJECT_ORCHESTRATIONS`] make is
///   unaffected by a *smaller* cap), and it makes the offered set equal to the
///   launchable set, which is the property the picker's whole existence rests
///   on.
/// * **Loosening the consumer** was the alternative and does not work: the
///   desktop's own `validate_workflow_shape` is not the binding limit — the
///   daemon's is. Raising the desktop's check to 512 would have moved the same
///   refusal one hop later, into the spawn, where it arrives as a failed launch
///   with roles already started instead of as a project that cannot be offered.
///
/// A name past the cap is **refused, not truncated**, for the reason
/// [`MAX_PROJECTED_NAME_BYTES`] gives: a shortened name is a name a spawn would
/// not resolve.
pub const MAX_PROJECTED_LAUNCH_NAME_BYTES: usize = crate::agent_pty::DISPLAY_NAME_MAX_LEN;

// The split above only makes sense in one direction: the bound on a name that
// has to survive the spawn is the tighter of the two. A compile-time assertion
// rather than a test, because it is a relationship between two constants and a
// build error is a better place to learn about an inverted one than a test run.
const _: () = assert!(MAX_PROJECTED_NAME_BYTES > MAX_PROJECTED_LAUNCH_NAME_BYTES);

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
    /// An orchestration's or a role's projected name exceeds
    /// [`MAX_PROJECTED_LAUNCH_NAME_BYTES`].
    ///
    /// Only those two, because only those two are names a client sends back. An
    /// over-long project *display* basename is dropped from an enumeration
    /// rather than turned into an error — see [`MAX_PROJECTED_NAME_BYTES`].
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
                "a declared name is {n} bytes; at most {MAX_PROJECTED_LAUNCH_NAME_BYTES} can be \
                 offered for a name a client sends back"
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

/// An opaque revision identifier for one config file's exact bytes.
///
/// **Derived from the content as read**, and deliberately not from mtime or
/// size: those two collide (any edit that preserves length, on a filesystem
/// whose timestamp granularity the edit fits inside) and, in the other
/// direction, a `git checkout` or a `cp -p` perturbs them without a real change
/// — so a metadata revision both misses changes and invents them.
///
/// FNV-1a, 128-bit, over the bytes [`read_config_file`] returned, prefixed with
/// the scheme so a later derivation can be told apart from this one rather than
/// silently compared against it. FNV-1a rather than
/// [`std::hash::DefaultHasher`] for the reason
/// [`crate::platform::lock`]'s own hash records: SipHash's keys are not stable
/// across Rust versions, and this value has to mean the same thing to a client
/// that got it from one daemon and a daemon that re-derives it later.
///
/// **What it detects, precisely:** that the config bytes changed between two
/// reads. It is not collision-resistant against a *deliberately* crafted
/// second config — FNV-1a is not a cryptographic hash, and 128 bits of it is
/// still not one. That buys nothing here: anyone who can write
/// `.dot-agent-deck.toml` already controls the `command` strings this daemon
/// executes, which is strictly more than defeating a staleness check.
pub fn config_revision(contents: &str) -> String {
    format!("fnv1a128-{}", fnv1a128_hex(contents.as_bytes()))
}

/// An opaque digest of the coordinator-context bytes one preparation published.
///
/// The launch-verb counterpart of [`config_revision`], carried on
/// [`crate::prep_token::PrepBinding::context_digest`] so a spawn can prove it is
/// launching against the artifact its own preparation wrote rather than against
/// whatever later landed at the same fixed path. Same construction, and a
/// **different prefix** for the reason `config_revision`'s own doc gives: two
/// derivations that mean different things must not be silently comparable.
///
/// **What it detects, precisely:** that the published bytes changed. Like
/// [`config_revision`] it is FNV-1a and is a change *hint*, not a commitment —
/// it does not withstand a deliberately crafted collision, and it is not
/// authorization for anything (see [`crate::prep_token`]'s module doc). On Unix
/// it is also not the only check: the published file's inode identity is
/// compared alongside it, and `rename(2)` installs a fresh inode on every
/// publish, so a republish is caught structurally whatever its bytes hash to.
/// The digest is what covers the case the inode misses — an in-place rewrite of
/// the same inode, which is what a shell `>` redirect or another tool's
/// `fs::write` does.
pub fn context_digest(content: &str) -> String {
    format!("ctx-fnv1a128-{}", fnv1a128_hex(content.as_bytes()))
}

/// FNV-1a, 128-bit, rendered as 32 lowercase hex digits.
///
/// One implementation shared by [`config_revision`] and [`context_digest`], so
/// the two cannot drift into hashing the same bytes differently. The choice of
/// FNV-1a over [`std::hash::DefaultHasher`] is argued at `config_revision`;
/// callers add their own scheme prefix.
fn fnv1a128_hex(bytes: &[u8]) -> String {
    let mut hash: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    for byte in bytes {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000_0000_0100_0000_0000_0000_0000_013b);
    }
    format!("{hash:032x}")
}

/// Read and parse `<dir>/.dot-agent-deck.toml` under every bound this module
/// defines.
///
/// `dir` is expected to be canonical already ([`canonicalize_project_dir`]),
/// because the orchestration-name fallback is derived from its basename.
pub fn read_project_config(dir: &Path) -> Result<ProjectConfig, ProjectResolveError> {
    read_project_config_with_revision(dir).map(|(config, _)| config)
}

/// [`read_project_config`] plus the [`config_revision`] of the bytes it read.
///
/// One read, two answers: the revision has to be derived from **the same bytes
/// that produced this `ProjectConfig`**, or it would identify a snapshot nobody
/// resolved against. That is why it is computed here rather than by a second
/// caller re-reading the file.
pub fn read_project_config_with_revision(
    dir: &Path,
) -> Result<(ProjectConfig, String), ProjectResolveError> {
    let path = dir.join(CONFIG_FILE_NAME);
    let contents = read_config_file(&path)?;
    let revision = config_revision(&contents);
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
    Ok((config, revision))
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
        let name = crate::project_config::resolve_orchestration_name(&orch.name, dir);
        bound_launch_name(&name)?;
        out.push(ProjectOrchestration {
            name,
            default: default_index == Some(index),
            roles: project_roles_onto_wire(orch)?,
        });
    }
    Ok(out)
}

/// Project one orchestration's roles under [`MAX_PROJECT_ROLES`] and
/// [`MAX_PROJECTED_LAUNCH_NAME_BYTES`].
///
/// Split out so the launch verb applies the **same** caps to the roles it
/// answers with as the resolve verb applies to the ones it offers. Two
/// projections of one shape is how a picker and a spawn come to disagree, which
/// is the class of bug this whole PRD is about.
pub fn project_roles_onto_wire(
    orch: &crate::project_config::OrchestrationConfig,
) -> Result<Vec<ProjectRole>, ProjectResolveError> {
    if orch.roles.len() > MAX_PROJECT_ROLES {
        return Err(ProjectResolveError::TooManyRoles(orch.roles.len()));
    }
    let mut roles = Vec::with_capacity(orch.roles.len());
    for role in &orch.roles {
        bound_launch_name(&role.name)?;
        roles.push(ProjectRole {
            name: role.name.clone(),
            start: role.start,
        });
    }
    Ok(roles)
}

/// Bound one projected name that a client **sends back** — an orchestration's
/// or a role's.
///
/// The cap is the consumer's own ([`MAX_PROJECTED_LAUNCH_NAME_BYTES`]), so what
/// this function admits is exactly what `agent_pty::validate_tab_membership`
/// and `spawn_agent` will accept later. Read that constant's doc for why the
/// alignment goes in this direction; the short version is that the alternative
/// moves the refusal into the spawn, after roles have already started.
fn bound_launch_name(name: &str) -> Result<(), ProjectResolveError> {
    if name.len() > MAX_PROJECTED_LAUNCH_NAME_BYTES {
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
    let (config, revision) = read_project_config_with_revision(dir)?;
    let orchestrations = project_config_onto_wire(dir, &config)?;
    Ok(ResolvedProject {
        // `canonicalize_project_dir` has already refused a non-UTF-8 canonical
        // form, so this cannot lose bytes.
        path: dir.to_string_lossy().into_owned(),
        orchestrations,
        config_revision: Some(revision),
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
        // The DISPLAY-name bound, not the launch one: this basename is rendered
        // beside the path and is never sent back, so its ceiling is the
        // readability figure rather than the spawn validator's. Tightening it to
        // `MAX_PROJECTED_LAUNCH_NAME_BYTES` would drop a perfectly launchable
        // project from the listing over a long directory name.
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

// ---------------------------------------------------------------------------
// PRD #819 M4: the launch verb
// ---------------------------------------------------------------------------

/// Resolve, compose and **publish** — the whole of
/// [`crate::daemon_protocol::AttachRequest::PrepareWorkflow`] behind one
/// blocking call.
///
/// The ordering is the point, and it is what makes "a failed preparation starts
/// no roles" true rather than aspirational: every step that can fail runs
/// before anything observable is created, the publish is the last step with a
/// side effect outside this process, and this function starts nothing. Roles are
/// started by a later `StartAgent` sequence that never runs if this returns
/// `Err`. (The token minted after the publish is an in-memory record, so it
/// cannot exist for a preparation that failed.)
///
/// **What the returned token binds, and why that is not a detail.** PRD #819's
/// original design had this issue a token recording only its issuance time, and
/// the audit of the finished branch showed that binds nothing usable: the
/// coordinator context is published at a path fixed per project, so a second
/// preparation in the same project replaces the first's artifact while the
/// first's token is still inside its TTL. The record now carries the canonical
/// directory and its inode identity, the config revision, the orchestration and
/// the published bytes' digest and inode ([`crate::prep_token::PrepBinding`]),
/// and every spawn presenting the token re-checks all of it
/// ([`revalidate_preparation`]). The **second** preparation is the one whose
/// artifact is on disk and whose token validates; the first is refused at its
/// spawn rather than launched against the wrong brief.
///
/// **One canonical string, carried end to end.** `path` is canonicalised once,
/// here, and the same `PathBuf` is what the config is read from, what the
/// orchestration name is resolved against, and what the context is published
/// under. Re-deriving or re-spelling it between those steps is PRD #220's bug
/// verbatim: an unnamed orchestration takes its name from the directory
/// basename (`crate::project_config::resolve_orchestration_name`), and
/// canonicalising a symlinked path CHANGES that basename — so a listing built
/// from one spelling and a launch built from another disagree about the name.
///
/// **Blocking** — the caller goes through [`run_bounded`]. Returns either the
/// prepared workflow or the exact `error` string the refusal carries; the
/// detail is logged daemon-locally on every failure, whichever refusal goes
/// back.
pub fn prepare_workflow_for_wire(
    path: &str,
    orchestration: &str,
    task: &str,
    expected_revision: Option<&str>,
    seeds: &[ProjectCandidate],
) -> Result<crate::event::PreparedWorkflow, String> {
    // --- resolve. Failures here take the disclosure split, exactly as
    // `resolve_for_wire`'s do: a path the daemon already knows gets the detail,
    // and every other path gets one fixed sentence.
    let mut canonical: Option<PathBuf> = None;
    let outcome = (|| {
        let dir = canonicalize_project_dir(Path::new(path))?;
        canonical = Some(dir.clone());
        let (config, revision) = read_project_config_with_revision(&dir)?;
        if config.orchestrations.len() > MAX_PROJECT_ORCHESTRATIONS {
            return Err(ProjectResolveError::TooManyOrchestrations(
                config.orchestrations.len(),
            ));
        }
        Ok((dir, config, revision))
    })();
    let split = |err: &ProjectResolveError, canonical: Option<&Path>| {
        if is_known_seed(path, canonical, seeds) {
            known_path_refusal(err)
        } else {
            generic_refusal()
        }
    };
    let (dir, config, revision) = match outcome {
        Ok(resolved) => resolved,
        Err(err) => {
            warn!(reason = %err, "prepare-workflow refused: the project did not resolve");
            return Err(split(&err, canonical.as_deref()));
        }
    };

    // --- the revision gate, before any work and long before any write. A
    // client that resolved against different bytes is refused rather than
    // silently launched against the ones on disk now.
    if let Some(expected) = expected_revision
        && expected != revision
    {
        warn!("prepare-workflow refused: the client's config revision is stale");
        return Err(stale_revision_refusal());
    }

    // --- pick the orchestration.
    //
    // A refusal from here on names a fact the caller could already have got:
    // `resolve_for_wire` answers ANY path that resolves with the full
    // projection, so "this directory is a project" is not what the uniform
    // refusal is protecting — the uniform refusal covers resolve FAILURES, and
    // a path that reaches this line has not had one. That is why the two
    // refusals below carry their own codes and their own sentences rather than
    // the one generic one.
    //
    // Same selection rule the spawn uses (`crate::spawn::decide_target`):
    // roleless entries are skipped, because two entries can resolve to the SAME
    // name and matching the empty one would refuse a target the listing
    // legitimately offered.
    let Some(orch) = config
        .orchestrations
        .iter()
        .filter(|o| !o.roles.is_empty())
        .find(|o| {
            crate::project_config::resolve_orchestration_name(&o.name, &dir) == orchestration
        })
    else {
        warn!(
            "prepare-workflow refused: the project defines no orchestration under the requested name"
        );
        return Err(no_such_orchestration_refusal());
    };
    // The projection caps ARE a resolve failure — `resolve_for_wire` refuses the
    // same config for the same reason — so this one takes the disclosure split
    // rather than its own code. Answering it differently here would hand an
    // arbitrary caller a role cardinality that the resolve verb withholds.
    let roles = project_roles_onto_wire(orch).map_err(|err| {
        warn!(reason = %err, "prepare-workflow refused: the orchestration exceeds a projection bound");
        split(&err, Some(dir.as_path()))
    })?;

    // The project directory's own identity, read BEFORE the publish so it
    // describes the directory this preparation actually resolved and wrote into.
    // A `None` here is not a soft failure: it means the platform has no inode
    // identity, and the verb that reaches this code is refused on such a
    // platform (`crate::daemon_protocol::PROJECT_ERR_UNSUPPORTED_PLATFORM`).
    let project_identity = std::fs::symlink_metadata(&dir)
        .ok()
        .as_ref()
        .and_then(crate::prep_token::inode_identity);

    // --- compose and publish. Last, and the only step with a side effect.
    let prepared = crate::orchestrator_context::prepare_orchestrator_context(
        orch,
        &dir,
        Some(task),
    )
    .map_err(|err| {
        warn!(reason = %err, "prepare-workflow refused: the coordinator context was not published");
        publish_refusal(&err)
    })?;

    // --- bind the record to what was just approved.
    //
    // PRD #819's audit finding: a token that records only its issuance time
    // binds nothing, so a launch can present a live token and spawn against an
    // artifact some *other* preparation published at the same fixed path. The
    // record therefore carries the canonical directory and its inode identity,
    // the config revision resolved against, the orchestration, and the exact
    // published bytes' digest and inode — and every spawn presenting the token
    // re-validates all of it (`revalidate_preparation`). It remains a staleness
    // and integrity mechanism and is not an authorization token; see
    // `crate::prep_token`'s module doc.
    let token = crate::prep_token::issue(crate::prep_token::PrepBinding {
        project_dir: dir.clone(),
        project_identity,
        config_revision: revision,
        orchestration: crate::project_config::resolve_orchestration_name(&orch.name, &dir),
        context_path: prepared.context_path.clone(),
        context_identity: prepared.context_identity,
        context_digest: context_digest(&prepared.content),
    });

    Ok(crate::event::PreparedWorkflow {
        context_path: prepared.context_path.to_string_lossy().into_owned(),
        // The canonical directory this preparation actually resolved to, so the
        // spawn does not have to trust the caller's spelling — see
        // `PreparedWorkflow::path`.
        path: dir.to_string_lossy().into_owned(),
        token,
        roles,
        // The composer already built the pointer line and until PRD #819 M6 it
        // was dropped on the floor here. The client spawning the roles is the
        // party that delivers it, and it may not compose its own copy — see
        // `PreparedWorkflow::prompt`.
        prompt: prepared.prompt,
    })
}

/// Re-validate a preparation at spawn time: is the artifact this token was
/// issued for still the artifact a launch would run against?
///
/// **This is one half of the spawn-side gate, and callers want the other one.**
/// [`verify_prepared_start`] is what the daemon's `start-prepared-agent` arm
/// calls: it runs these checks *and* matches the submitted request against the
/// binding, which is the half Greptile's P1(a) found missing. This function
/// stays public and exercised directly by `tests/prep_binding.rs`, which isolates
/// each staleness cause; nothing in `src/` calls it alone, and a new caller that
/// wants it alone is almost certainly re-opening P1(a).
///
/// **These checks are PRD #819's audit fix, and without them the token means
/// nothing at all.** The original design recorded only
/// `(token, issued_at)`, so nothing here was possible: two clients preparing in
/// the same project overwrite one fixed file, both tokens stay inside the TTL,
/// and the earlier launch spawns a coordinator pointed at a path now holding the
/// later client's brief. The record now carries what it approved
/// ([`crate::prep_token::PrepBinding`]) and this function re-checks every part
/// of it:
///
/// 1. the canonical project directory still resolves, and still canonicalises to
///    the same string — a project replaced by a symlink elsewhere is caught;
/// 2. that directory is the same **inode** — a delete-and-recreate under the same
///    name is caught, which a path comparison alone cannot see;
/// 3. the config still reads and its [`config_revision`] is unchanged;
/// 4. the orchestration is still defined, with roles, under the prepared name;
/// 5. the published coordinator context is the same **inode** and the same
///    **bytes** — which is what catches the interleaving above, since a second
///    publish `rename(2)`s a fresh inode over the destination.
///
/// **It is the conjunction that carries the claim, not any single check**, and
/// two of them are individually defeasible: an inode number is reusable
/// ([`crate::prep_token::InodeIdentity`]) and [`config_revision`] is an FNV-1a
/// change hint rather than a commitment. What survives is narrow and sufficient:
/// for all five to agree, the config bytes, the published bytes and both inode
/// identities have to coincide, which means the artifact on disk is
/// byte-identical to the one this preparation approved. No stronger property is
/// claimed here, and none is needed — a peer able to reach this socket can spawn
/// with no token at all.
///
/// **Every failure is one refusal with one sentence on the wire**
/// ([`stale_preparation_refusal`] — a different sentence from the one a
/// [`PreparationMismatch`] gets), and the specific cause stays daemon-local.
/// That is the same disposition the resolve verb's refusals take, and for a
/// sharper reason here: the caller can already learn all five facts by resolving
/// the project again, so the uniform sentence costs it nothing, while
/// enumerating which check failed would turn a staleness answer into a
/// description of another party's launch. The split is a typed
/// [`PreparationStale`] rather than a pre-rendered string, exactly as
/// [`crate::orchestrator_context::ContextPublishError`] splits its two
/// renderings — so the log and the tests can name the cause without the wire
/// gaining the ability to.
///
/// **It is not an authorization check and does not become one.** A peer that
/// wants to start a process calls `StartAgent` with no token at all; refusing a
/// stale one stops a *mistake* — a launch consuming state it did not publish —
/// and nothing more. Read [`crate::prep_token`]'s module doc before treating a
/// refusal here as a denied privilege.
///
/// **Blocking** — the caller goes through [`run_bounded`].
pub fn revalidate_preparation(
    binding: &crate::prep_token::PrepBinding,
) -> Result<(), PreparationStale> {
    revalidate_approved_roles(binding).map(|_| ())
}

/// [`revalidate_preparation`], plus the role identities the approved
/// orchestration declares — the list [`verify_prepared_start`] checks a
/// submitted role against.
///
/// It is one function rather than two because the role list has to come from the
/// **same read** the staleness checks passed: re-reading the config to answer
/// "does this orchestration declare that role" would answer it about a config
/// that could already have moved, which is the shape of defect this whole seam
/// exists to close.
fn revalidate_approved_roles(
    binding: &crate::prep_token::PrepBinding,
) -> Result<Vec<ApprovedRole>, PreparationStale> {
    let dir = canonicalize_project_dir(&binding.project_dir)
        .map_err(|_| PreparationStale::ProjectUnresolved)?;
    if dir != binding.project_dir {
        return Err(PreparationStale::ProjectMoved);
    }
    let identity = std::fs::symlink_metadata(&dir)
        .ok()
        .as_ref()
        .and_then(crate::prep_token::inode_identity);
    if identity != binding.project_identity {
        return Err(PreparationStale::ProjectReplaced);
    }

    let (config, revision) =
        read_project_config_with_revision(&dir).map_err(|_| PreparationStale::ConfigUnreadable)?;
    if revision != binding.config_revision {
        return Err(PreparationStale::ConfigChanged);
    }
    // Cheap, and not a tautology given the revision matched: `config_revision`
    // is a change hint rather than a commitment, so this asks the config itself
    // the question the hint only stands in for.
    let Some(orch) = config
        .orchestrations
        .iter()
        .filter(|o| !o.roles.is_empty())
        .find(|o| {
            crate::project_config::resolve_orchestration_name(&o.name, &dir)
                == binding.orchestration
        })
    else {
        return Err(PreparationStale::OrchestrationGone);
    };
    // Captured before the context checks so the returned list is the one the
    // orchestration lookup just matched, rather than a second lookup's.
    let approved_roles: Vec<ApprovedRole> = orch
        .roles
        .iter()
        .map(|role| ApprovedRole {
            name: role.name.clone(),
            start: role.start,
        })
        .collect();

    let (content, context_identity) = read_published_context(&binding.context_path)?;
    if context_identity != binding.context_identity {
        return Err(PreparationStale::ContextReplaced);
    }
    if context_digest(&content) != binding.context_digest {
        return Err(PreparationStale::ContextRewritten);
    }
    Ok(approved_roles)
}

/// One role of the orchestration a preparation approved, as the config declares
/// it: the exact name a spawn must send back, and whether that role is the one
/// the orchestration starts.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ApprovedRole {
    name: String,
    start: bool,
}

/// Why a preparation no longer describes what it approved.
///
/// Two renderings, for [`crate::orchestrator_context::ContextPublishError`]'s
/// reason: [`Self::detail`] is the daemon-local diagnostic and names the
/// specific check, and the wire gets [`stale_preparation_refusal`]'s one
/// sentence for **every** variant. Nothing here reaches a client, and the enum
/// exists so the log and the tests can tell the five checks apart — a test that
/// can only observe "refused" cannot show that the check it is exercising is the
/// one that fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparationStale {
    /// The canonical project directory no longer resolves at all.
    ProjectUnresolved,
    /// It resolves, but now canonicalises somewhere else — the path became a
    /// symlink, or a parent component did.
    ProjectMoved,
    /// Same path, different inode: deleted and recreated under the same name.
    ProjectReplaced,
    /// The project config no longer reads.
    ConfigUnreadable,
    /// The config's [`config_revision`] moved after the preparation.
    ConfigChanged,
    /// The prepared orchestration is no longer defined with roles.
    OrchestrationGone,
    /// The published coordinator context could not be read back.
    ContextUnreadable,
    /// Something other than a regular file now sits at the published path.
    ContextNotRegularFile,
    /// What sits there is larger than this daemon publishes.
    ContextTooLarge,
    /// Same path, different inode — which is what a second publish's
    /// `rename(2)` produces, and therefore the interleaving case PRD #819's
    /// audit found.
    ContextReplaced,
    /// Same inode, different bytes — an in-place rewrite, which a `>` redirect
    /// or another tool's `fs::write` performs.
    ContextRewritten,
}

impl PreparationStale {
    /// The **daemon-local** diagnostic. Safe to log; reaches no client.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::ProjectUnresolved => "the prepared project directory no longer resolves",
            Self::ProjectMoved => "the prepared project directory now canonicalises elsewhere",
            Self::ProjectReplaced => "the prepared project directory has been replaced",
            Self::ConfigUnreadable => "the prepared project's config no longer reads",
            Self::ConfigChanged => "the project config changed after the preparation",
            Self::OrchestrationGone => "the prepared orchestration is no longer defined with roles",
            Self::ContextUnreadable => "the published coordinator context could not be read back",
            Self::ContextNotRegularFile => {
                "the published coordinator context is no longer a regular file"
            }
            Self::ContextTooLarge => {
                "the published coordinator context is larger than this daemon publishes"
            }
            Self::ContextReplaced => "the published coordinator context has been replaced",
            Self::ContextRewritten => "the published coordinator context has been rewritten",
        }
    }
}

impl std::fmt::Display for PreparationStale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

/// What a prepared start is actually asking for, in the terms
/// [`crate::prep_token::PrepBinding`] records.
///
/// The daemon builds one of these from the request it is about to spawn, so the
/// comparison in [`verify_prepared_start`] runs on the submitted fields rather
/// than on anything re-derived from the token.
///
/// **Owned**, deliberately: the daemon runs the comparison on the same bounded
/// blocking pool the rest of the project verbs use, so the request has to cross
/// a `'static` boundary. Borrowing here would push a lifetime through that
/// closure for no benefit — this is built once per prepared start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreparedStartRequest {
    /// `StartAgent.cwd` exactly as submitted, and `None` when the request sent
    /// none — which is not a neutral value here but a request to spawn in the
    /// daemon's own working directory, and therefore not the prepared project.
    pub cwd: Option<String>,
    /// The orchestration membership the request declares, and `None` for a
    /// request that declares none at all (a dashboard pane, or a
    /// [`crate::agent_pty::TabMembership::Mode`] tab).
    pub membership: Option<PreparedStartMembership>,
}

/// The orchestration identity a prepared start submits, lifted out of
/// [`crate::agent_pty::TabMembership::Orchestration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStartMembership {
    /// `TabMembership::Orchestration::name`.
    pub orchestration: String,
    /// `TabMembership::Orchestration::orchestration_cwd`, when the membership
    /// declared one. It is a **second** spelling of the project identity inside
    /// the same request, so it is checked when present and skipped when absent —
    /// absence claims nothing.
    pub orchestration_cwd: Option<String>,
    /// `TabMembership::Orchestration::role_name`.
    pub role: String,
    /// `TabMembership::Orchestration::is_start_role`, which is what puts the
    /// pane in `orchestrator_pane_ids` and therefore decides where delegations
    /// come from.
    pub is_start_role: bool,
}

/// Why a prepared start is not the launch its token approved.
///
/// Same two-rendering split [`PreparationStale`] takes, and for the same reason:
/// [`Self::detail`] is the daemon-local diagnostic that names the specific
/// check, and every variant answers the wire with
/// [`preparation_mismatch_refusal`]'s one sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparationMismatch {
    /// The submitted `cwd` is absent, or is not the directory the preparation
    /// resolved. This is the finding's own case: a token prepared for project X
    /// presented with spawn fields naming project Y.
    ProjectDiffers,
    /// The request declares no orchestration membership. A preparation approves
    /// an orchestration launch, so a start that claims to be one of its roles
    /// and does not say which orchestration is not that launch.
    NoOrchestration,
    /// The membership names an orchestration other than the prepared one.
    OrchestrationDiffers,
    /// The membership's own copy of the project directory disagrees with the
    /// prepared one.
    OrchestrationCwdDiffers,
    /// The submitted role is not one the approved orchestration declares.
    RoleNotDeclared,
    /// The role is declared, but the request's start marker is not the one the
    /// config gives it — so the launch would register the wrong pane as the
    /// orchestration's start role.
    StartMarkerDiffers,
}

impl PreparationMismatch {
    /// The **daemon-local** diagnostic. Safe to log; reaches no client.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::ProjectDiffers => {
                "the submitted working directory is not the project this preparation approved"
            }
            Self::NoOrchestration => {
                "the submitted request declares no orchestration membership to match against"
            }
            Self::OrchestrationDiffers => {
                "the submitted orchestration is not the one this preparation approved"
            }
            Self::OrchestrationCwdDiffers => {
                "the submitted orchestration directory is not the project this preparation approved"
            }
            Self::RoleNotDeclared => {
                "the submitted role is not declared by the approved orchestration"
            }
            Self::StartMarkerDiffers => {
                "the submitted start marker is not the one the approved orchestration declares"
            }
        }
    }
}

impl std::fmt::Display for PreparationMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

/// Why a prepared start was refused: the world moved, or the request is not the
/// one that was approved.
///
/// Two categories rather than one code, because they send an operator in
/// different directions — see [`crate::daemon_protocol::PROJECT_ERR_PREPARATION_MISMATCH`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreparedStartRefusal {
    /// The preparation no longer describes what it approved
    /// ([`revalidate_preparation`]).
    Stale(PreparationStale),
    /// The preparation is intact and the request is a different one
    /// ([`verify_prepared_start`]).
    Mismatch(PreparationMismatch),
}

impl PreparedStartRefusal {
    /// The **daemon-local** diagnostic, naming the specific check that fired.
    pub fn detail(&self) -> &'static str {
        match self {
            Self::Stale(stale) => stale.detail(),
            Self::Mismatch(mismatch) => mismatch.detail(),
        }
    }

    /// The sentence that goes back on the wire — one per category, naming
    /// neither the check that fired nor any value from the binding.
    pub fn wire_refusal(&self) -> String {
        match self {
            Self::Stale(_) => stale_preparation_refusal(),
            Self::Mismatch(_) => preparation_mismatch_refusal(),
        }
    }
}

impl std::fmt::Display for PreparedStartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.detail())
    }
}

/// The whole spawn-side gate on a prepared start: the preparation still
/// describes what it approved, **and** the request being made is that launch.
///
/// **The second half is what Greptile's P1(a) found missing, and the shape of
/// the miss is worth keeping.** The audit fix made
/// [`crate::prep_token::PrepBinding`] record what a preparation approved and
/// made [`revalidate_preparation`] re-check that record against the filesystem.
/// Both halves were correct and neither compared the record to the *submitted*
/// spawn fields — so a caller could present a token prepared for project X while
/// submitting the `cwd`, the orchestration and the role of project Y, and the
/// daemon validated X and started Y. The first fix made the code **look**
/// validated, which is precisely what made the second easy to miss.
///
/// # What is bound
///
/// * the submitted `cwd` is the daemon's own canonical spelling of the prepared
///   project directory. Compared as the string this daemon minted and handed
///   back on [`crate::event::PreparedWorkflow::path`] — **not** re-canonicalised,
///   because re-resolving here would ask the filesystem the question again and
///   accept any other spelling that happens to land on the same directory, which
///   is the class of drift PRD #220 is about;
/// * the membership's orchestration is the prepared one, and its
///   `orchestration_cwd` — a second copy of the project identity in the same
///   request — agrees with the prepared directory when it is present;
/// * the role is one the approved orchestration actually declares, with the
///   start marker that orchestration gives it. The role list comes from the
///   config read the staleness checks just passed, so it describes the config
///   this preparation approved rather than whatever is on disk by the time a
///   second read would run.
///
/// # What is deliberately NOT bound
///
/// **The `command`.** Per-launch command override is an existing, documented
/// feature — `docs/develop/desktop-gui.md`: "The submitted command overrides the
/// matching project role for that launch only" — and it is how the desktop's
/// agent profiles reach a launch at all. Requiring the submitted command to
/// match the config's role command would break every profile, so what is bound
/// here is **identity** (project, orchestration, role) and not **content**.
/// Binding the command would also buy nothing this socket protects: a peer that
/// wants to run an arbitrary command calls `StartAgent` and presents no token.
///
/// Nor are `rows`, `cols`, `display_name`, `env`, `agent_type`, `seed`,
/// `role_index`, `display_title` or `orchestration_id`: each is either
/// presentation, per-launch shape, or an identity the client mints for itself,
/// and none of them names the project or the workflow this token approved.
///
/// **This is still not an authorization check.** It stops a request from
/// consuming a preparation that was made for something else; it stops nothing a
/// peer able to call `StartAgent` could not do with no token at all. Read
/// [`crate::prep_token`]'s module doc before reading a refusal here as a denied
/// privilege.
///
/// **Ordering.** The identity comparisons run first and touch no filesystem, so
/// a request that does not match its own token is refused without a single
/// syscall — and, more importantly, is refused with the code that is *true* of
/// it rather than with whatever the staleness checks happened to say.
///
/// **Blocking** — the caller goes through [`run_bounded`].
pub fn verify_prepared_start(
    binding: &crate::prep_token::PrepBinding,
    request: &PreparedStartRequest,
) -> Result<(), PreparedStartRefusal> {
    use PreparedStartRefusal::{Mismatch, Stale};

    // The prepared directory as the daemon spelled it. `canonicalize_project_dir`
    // refuses a non-UTF-8 path, so a binding minted by `prepare_workflow_for_wire`
    // always has one; a `None` here could only come from a hand-built binding and
    // is treated as "nothing the caller could have matched".
    let approved_dir = binding.project_dir.to_str();
    if approved_dir.is_none() || request.cwd.as_deref() != approved_dir {
        return Err(Mismatch(PreparationMismatch::ProjectDiffers));
    }
    let Some(membership) = request.membership.as_ref() else {
        return Err(Mismatch(PreparationMismatch::NoOrchestration));
    };
    if membership.orchestration != binding.orchestration {
        return Err(Mismatch(PreparationMismatch::OrchestrationDiffers));
    }
    if let Some(orchestration_cwd) = membership.orchestration_cwd.as_deref()
        && Some(orchestration_cwd) != approved_dir
    {
        return Err(Mismatch(PreparationMismatch::OrchestrationCwdDiffers));
    }

    let approved_roles = revalidate_approved_roles(binding).map_err(Stale)?;
    let Some(role) = approved_roles
        .iter()
        .find(|role| role.name == membership.role)
    else {
        return Err(Mismatch(PreparationMismatch::RoleNotDeclared));
    };
    if role.start != membership.is_start_role {
        return Err(Mismatch(PreparationMismatch::StartMarkerDiffers));
    }
    Ok(())
}

/// Read the coordinator context back for [`revalidate_preparation`], under the
/// same bounds and the same symlink discipline [`read_config_file`] applies.
///
/// `O_NOFOLLOW` because the destination is a name another party may have turned
/// into a link, and the regular-file check because a FIFO substituted for it
/// would otherwise stall this blocking thread. The cap is
/// [`crate::orchestrator_context::MAX_CONTEXT_BYTES`] — the same bound the
/// publish enforces, so a file this daemon wrote always fits and anything larger
/// is by definition not one.
fn read_published_context(
    path: &Path,
) -> Result<(String, Option<crate::prep_token::InodeIdentity>), PreparationStale> {
    let max = crate::orchestrator_context::MAX_CONTEXT_BYTES as u64;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| PreparationStale::ContextUnreadable)?;
    let metadata = file
        .metadata()
        .map_err(|_| PreparationStale::ContextUnreadable)?;
    if !metadata.is_file() {
        return Err(PreparationStale::ContextNotRegularFile);
    }
    if metadata.len() > max {
        return Err(PreparationStale::ContextTooLarge);
    }
    let identity = crate::prep_token::inode_identity(&metadata);
    let content =
        crate::bounded_read::read_capped(file, max, crate::orchestrator_context::CONTEXT_FILE_NAME)
            .map_err(|_| PreparationStale::ContextUnreadable)?;
    Ok((content, identity))
}

/// The refusal a `StartAgent` whose preparation no longer holds gets.
///
/// One fixed sentence for all five checks, for the reason
/// [`revalidate_preparation`]'s doc gives, and the remedy the caller needs is
/// the same in every case: prepare again.
pub fn stale_preparation_refusal() -> String {
    format!(
        "{}: that preparation no longer matches the project it was made against; prepare the \
         workflow again",
        crate::daemon_protocol::PROJECT_ERR_STALE_PREPARATION
    )
}

/// The refusal a prepared start whose request does not match its preparation
/// gets.
///
/// One fixed sentence for all six checks, for the reason
/// [`verify_prepared_start`]'s doc gives — and it names no value from the
/// binding, because "the project you prepared for is actually /x/y" is a fact
/// about another party's launch rather than about this request. The caller
/// already holds everything it needs: the preparation handed it the canonical
/// path, the orchestration and the role list.
///
/// **The remedy is deliberately not "prepare again".** That is
/// [`stale_preparation_refusal`]'s remedy and it is the wrong one here: nothing
/// about the world moved, so a fresh preparation presented the same way is
/// refused the same way.
pub fn preparation_mismatch_refusal() -> String {
    format!(
        "{}: that request is not the launch this preparation approved; send the project, \
         orchestration and role the preparation returned",
        crate::daemon_protocol::PROJECT_ERR_PREPARATION_MISMATCH
    )
}

/// The refusal a stale [`config_revision`] gets.
///
/// One fixed sentence naming neither revision: the client already holds the one
/// it sent, and the one on disk is a property of the file rather than of the
/// request.
pub fn stale_revision_refusal() -> String {
    format!(
        "{}: the project config changed since it was resolved; resolve it again and retry",
        crate::daemon_protocol::PROJECT_ERR_STALE_REVISION
    )
}

/// The refusal a `PrepareWorkflow` naming an orchestration the project does not
/// define gets.
///
/// It deliberately lists nothing. The available names are config *content* for
/// a path the caller may merely have pasted, and the caller that legitimately
/// got here from `ResolveProject` already has the list.
pub fn no_such_orchestration_refusal() -> String {
    format!(
        "{}: that project defines no orchestration with roles under that name",
        crate::daemon_protocol::PROJECT_ERR_NO_ORCHESTRATION
    )
}

/// The refusal a failed publish gets: the stable code plus the publish error's
/// own client-safe sentence.
///
/// [`crate::orchestrator_context::ContextPublishError::client_sentence`] names
/// no path and no raw OS error; see its doc for why it is allowed to be more
/// specific than [`generic_refusal`] is.
pub fn publish_refusal(err: &crate::orchestrator_context::ContextPublishError) -> String {
    format!(
        "{}: {}",
        crate::daemon_protocol::PROJECT_ERR_PUBLISH_FAILED,
        err.client_sentence()
    )
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
        let long = "n".repeat(MAX_PROJECTED_LAUNCH_NAME_BYTES + 1);
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
            matches!(
                err,
                ProjectResolveError::NameTooLong(n) if n > MAX_PROJECTED_LAUNCH_NAME_BYTES
            ),
            "expected NameTooLong, got {err:?}"
        );
    }

    /// PRD #819 audit fix (P2, finding 1): **the offered set is the launchable
    /// set**, asserted from both sides of the seam at the exact boundary.
    ///
    /// The projection used to admit up to 512 bytes while the value's
    /// destination — `TabMembership::Orchestration.name` / `.role_name`, and
    /// the pane's `display_name` — accepts `DISPLAY_NAME_MAX_LEN`, so a name of
    /// 129..=512 bytes was offered by the picker and then **failed the spawn**.
    /// So the property is not "long names are refused somewhere": it is that
    /// the last name the projection admits is one `is_valid_display_name`
    /// accepts, and the first it refuses is one that validator would have
    /// refused too. Widen either number alone and one of these two halves goes
    /// red.
    #[test]
    fn the_longest_projected_launch_name_is_one_the_spawn_will_accept() {
        use crate::agent_pty::is_valid_display_name;

        let at_ceiling = "n".repeat(MAX_PROJECTED_LAUNCH_NAME_BYTES);
        let over_ceiling = "n".repeat(MAX_PROJECTED_LAUNCH_NAME_BYTES + 1);
        // The consumer's half: what the projection admits, the spawn accepts —
        // and what it refuses, the spawn would have refused.
        assert!(
            is_valid_display_name(&at_ceiling),
            "a name at the projection ceiling must survive the spawn's own validator"
        );
        assert!(
            !is_valid_display_name(&over_ceiling),
            "one byte past the ceiling must be a name the spawn refuses, or the caps are not aligned"
        );

        // The projection's half, for both names it sends back: an orchestration's
        // and a role's.
        let (_guard, root) = scratch();
        write_project(
            &root,
            &format!(
                "[[orchestrations]]\nname = \"{at_ceiling}\"\n\n[[orchestrations.roles]]\n\
                 name = \"{at_ceiling}\"\ncommand = \"cat\"\n"
            ),
        );
        let config = read_project_config(&root).expect("read");
        let projected = project_config_onto_wire(&root, &config)
            .expect("a name at the ceiling must be offered, not refused");
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].name, at_ceiling);
        assert_eq!(projected[0].roles[0].name, at_ceiling);
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
    ///
    /// **Linux-only because the FIXTURE is, not because the property is.**
    /// `canonicalize_project_dir`'s `to_str().is_none()` arm is compiled on
    /// every platform, so this test's coverage of it is Linux's alone. It was
    /// `#[cfg(unix)]` and `build-macos` failed it at `mkdir`: APFS and HFS+
    /// require a valid UTF-8 name and refuse `proj-\xff` with `EILSEQ`, so the
    /// setup died before the code under test was reached. Linux treats a
    /// filename as opaque bytes and is where the input can be constructed.
    ///
    /// The narrow claim, and no wider one: what macOS cannot do is *build this
    /// fixture on its own filesystems*. Whether a non-UTF-8 name could still
    /// reach the check there by some other route — an NFS or SMB mount handing
    /// over arbitrary bytes — is not something this test settles either way, so
    /// read the gate as "unverified on macOS", not as "unreachable there".
    ///
    /// A probe-and-skip was rejected: a skip that nextest counts as a pass is
    /// exactly the trap CLAUDE.md rule 5 names, whereas a `cfg`-gated test is
    /// absent from the run and cannot be mistaken for a green one.
    #[cfg(target_os = "linux")]
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

    /// The revision identifies the config CONTENT, and identifies nothing else.
    ///
    /// The three claims that make it usable as a staleness check: identical
    /// bytes give the same value, a one-byte edit gives a different one, and a
    /// rewrite that changes only the file's metadata gives the same one — which
    /// is the half that rules out mtime and size, since a `git checkout` or a
    /// `cp -p` perturbs those without a real change.
    #[test]
    fn the_revision_tracks_the_bytes_and_not_the_file() {
        let (_guard, root) = scratch();
        let project = root.join("revision-project");
        std::fs::create_dir_all(&project).expect("create the project dir");
        write_project(&project, SMALL_PROJECT);

        let first = read_project_config_with_revision(&project)
            .expect("read the config")
            .1;
        assert!(
            first.starts_with("fnv1a128-"),
            "the scheme is part of the value so a later derivation cannot be silently compared \
             against this one: {first}"
        );

        // Rewritten byte-for-byte: a new inode timestamp, the same content.
        write_project(&project, SMALL_PROJECT);
        assert_eq!(
            read_project_config_with_revision(&project)
                .expect("re-read the config")
                .1,
            first,
            "rewriting the same bytes must not change the revision"
        );

        // One byte different, and the length is deliberately unchanged so the
        // test would fail against a size-based derivation.
        write_project(&project, &SMALL_PROJECT.replace("builder", "buildeR"));
        assert_ne!(
            read_project_config_with_revision(&project)
                .expect("read the edited config")
                .1,
            first,
            "a same-length edit must change the revision"
        );
    }

    /// A resolve carries the revision a later `PrepareWorkflow` echoes back.
    /// Without it the launch verb has nothing to compare against and the
    /// staleness check is unreachable from a client.
    #[test]
    fn a_resolve_carries_the_revision_of_the_config_it_read() {
        let (_guard, root) = scratch();
        let project = root.join("resolved-project");
        std::fs::create_dir_all(&project).expect("create the project dir");
        write_project(&project, SMALL_PROJECT);

        let resolved = resolve_project(&project).expect("resolve");
        assert_eq!(
            resolved.config_revision.as_deref(),
            Some(
                read_project_config_with_revision(&project)
                    .expect("read the config")
                    .1
                    .as_str()
            ),
            "the revision on the wire must be the one derived from the bytes that were resolved"
        );
    }

    /// The launch verb refuses a stale revision, and refuses it BEFORE it
    /// publishes anything — which is the whole point of the field. A check that
    /// ran after the write would report an error having already replaced the
    /// coordinator context.
    #[test]
    fn prepare_refuses_a_stale_revision_without_publishing() {
        let (_guard, root) = scratch();
        let project = root.join("staleness-project");
        std::fs::create_dir_all(&project).expect("create the project dir");
        write_project(&project, SMALL_PROJECT);
        let context = project
            .join(crate::orchestrator_context::CONTEXT_DIR_NAME)
            .join(crate::orchestrator_context::CONTEXT_FILE_NAME);

        let stale = config_revision("something else entirely");
        let refusal = prepare_workflow_for_wire(
            project.to_str().expect("utf-8 scratch path"),
            "loop",
            "a task",
            Some(&stale),
            &[],
        )
        .expect_err("a stale revision must be refused");
        assert!(
            refusal.starts_with(crate::daemon_protocol::PROJECT_ERR_STALE_REVISION),
            "expected the stable stale-revision code, got {refusal}"
        );
        assert!(
            !context.exists(),
            "the revision check must run before the publish, but {} was written",
            context.display()
        );

        // The matching revision goes through, so the refusal above is about
        // staleness rather than about the verb being broken.
        let current = read_project_config_with_revision(&project)
            .expect("read the config")
            .1;
        let prepared = prepare_workflow_for_wire(
            project.to_str().expect("utf-8 scratch path"),
            "loop",
            "a task",
            Some(&current),
            &[],
        )
        .expect("a matching revision must be accepted");
        assert_eq!(Path::new(&prepared.context_path), context);
        assert!(context.is_file(), "and the context is published");
        assert!(
            !prepared.token.is_empty(),
            "a preparation carries the token the later spawn presents"
        );
    }

    /// An orchestration the project does not define is refused with its own
    /// code, and the refusal enumerates nothing — the available names are config
    /// content for a path the caller may merely have pasted.
    #[test]
    fn prepare_refuses_an_unknown_orchestration_without_naming_the_known_ones() {
        let (_guard, root) = scratch();
        let project = root.join("named-project");
        std::fs::create_dir_all(&project).expect("create the project dir");
        write_project(&project, SMALL_PROJECT);

        let refusal = prepare_workflow_for_wire(
            project.to_str().expect("utf-8 scratch path"),
            "no-such-orchestration",
            "a task",
            None,
            &[],
        )
        .expect_err("an unknown orchestration must be refused");
        assert!(
            refusal.starts_with(crate::daemon_protocol::PROJECT_ERR_NO_ORCHESTRATION),
            "expected the stable no-such-orchestration code, got {refusal}"
        );
        assert!(
            !refusal.contains("loop"),
            "the refusal must not enumerate what the config declares: {refusal}"
        );
    }

    /// The canonical spelling carries from the resolve through to the publish.
    ///
    /// This is PRD #220's bug in its PRD #819 form: an unnamed orchestration is
    /// named after the directory basename, canonicalising a symlinked path
    /// CHANGES that basename, and a launch prepared through the alias must still
    /// land under the canonical directory and answer to the canonical name.
    #[cfg(unix)]
    #[test]
    fn a_launch_through_a_symlink_publishes_under_the_canonical_directory() {
        let (_guard, root) = scratch();
        let code = root.join("code");
        let project = code.join("canonical-project");
        std::fs::create_dir_all(&project).expect("create the project dir");
        write_project(
            &project,
            "[[orchestrations]]\n\n[[orchestrations.roles]]\nname = \"planner\"\n\
             command = \"cat\"\nstart = true\n",
        );
        let alias = root.join("current");
        std::os::unix::fs::symlink(&project, &alias).expect("symlink");

        let prepared = prepare_workflow_for_wire(
            alias.to_str().expect("utf-8 scratch path"),
            // The CANONICAL basename, because that is the name the resolve
            // offers; `current` is the spelling that must never name anything.
            "canonical-project",
            "a task",
            None,
            &[],
        )
        .expect("a launch through the alias must resolve and publish");

        assert_eq!(
            Path::new(&prepared.context_path),
            project
                .join(crate::orchestrator_context::CONTEXT_DIR_NAME)
                .join(crate::orchestrator_context::CONTEXT_FILE_NAME),
            "the context must land under the canonical directory, not under the alias"
        );
        assert!(
            prepare_workflow_for_wire(
                alias.to_str().expect("utf-8 scratch path"),
                "current",
                "a task",
                None,
                &[],
            )
            .is_err(),
            "the symlink's basename must never name an orchestration"
        );
    }

    /// The launch verb's *resolve* refusals discloses exactly what the resolve
    /// verb's do — no more.
    ///
    /// Prepare-workflow does filesystem work on a caller-supplied path just as
    /// resolve-project does, so a refusal from that half must be
    /// indistinguishable in the same way: one code, one sentence, no parser
    /// source line, no raw OS error, no echo of the caller's path. A second
    /// verb reaching the same reader through a different refusal is how a
    /// disclosure bound gets quietly reopened.
    #[test]
    fn a_prepare_refusal_on_an_arbitrary_path_discloses_no_more_than_a_resolve_refusal() {
        let (_guard, root) = scratch();
        let project = root.join("broken-project");
        std::fs::create_dir(&project).expect("mkdir");
        write_project(&project, MALFORMED);

        let refusal = prepare_workflow_for_wire(
            project.to_str().expect("utf-8 scratch path"),
            "loop",
            "a task",
            None,
            &[],
        )
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
            !refusal.contains("broken-project")
                && !refusal.contains(root.to_str().expect("utf-8 scratch path")),
            "the caller's own path must not be echoed: {refusal}"
        );
        assert!(
            !refusal.contains("os error"),
            "no raw OS error may escape: {refusal}"
        );

        let missing = prepare_workflow_for_wire(
            &format!("{}/no-such-dir", root.display()),
            "loop",
            "a task",
            None,
            &[],
        )
        .expect_err("a missing directory must be refused");
        assert_eq!(
            missing, refusal,
            "the response must not directly distinguish a missing directory from a broken config"
        );
    }

    /// A projection cap is a RESOLVE failure, so the launch verb refuses it the
    /// way the resolve verb does — through the disclosure split — rather than
    /// handing an arbitrary caller a role cardinality that `resolve-project`
    /// withholds.
    #[test]
    fn a_prepare_that_trips_a_projection_cap_takes_the_disclosure_split() {
        let (_guard, root) = scratch();
        let project = root.join("crowded-project");
        std::fs::create_dir(&project).expect("mkdir");
        let mut toml = String::from("[[orchestrations]]\nname = \"loop\"\n");
        for i in 0..=MAX_PROJECT_ROLES {
            toml.push_str(&format!(
                "\n[[orchestrations.roles]]\nname = \"r{i}\"\ncommand = \"cat\"\n"
            ));
        }
        write_project(&project, &toml);
        let path = project.to_str().expect("utf-8 scratch path");

        let prepare_refusal = prepare_workflow_for_wire(path, "loop", "a task", None, &[])
            .expect_err("too many roles must be refused");
        let resolve_refusal =
            resolve_for_wire(path, &[]).expect_err("and the resolve refuses it too");
        assert_eq!(
            prepare_refusal, resolve_refusal,
            "both verbs must refuse the same config with the same words"
        );
        assert!(
            !prepare_refusal.contains(&MAX_PROJECT_ROLES.to_string()),
            "the generic refusal must not carry the cardinality: {prepare_refusal}"
        );
        assert!(
            !project
                .join(crate::orchestrator_context::CONTEXT_DIR_NAME)
                .exists(),
            "a refused preparation publishes nothing"
        );
    }
}
