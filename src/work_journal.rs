//! PRD #1889: the work journal — an append-only record of the deck's units of
//! work (a pane, an orchestration, a delegation, a dispatch, a scheduled fire)
//! and of the attempts, reports and ends that belong to them.
//!
//! This module is self-contained: ids, the version-1 line codec, the redaction
//! guard, the per-run writer, the reader, startup recovery and compaction. The
//! daemon owns one [`WorkJournal`] per run. The maintainer page is
//! `docs/develop/work-journal.md`.
//!
//! # Files
//!
//! Everything lives in `<state_dir>/work-journal/`, an owner-only directory:
//!
//! - `run-<run>.jsonl`, then `run-<run>.<n>.jsonl` (`n` = 1, 2, …): the
//!   segments of one daemon run, written only by that run, which holds
//!   `run-<run>.lock` for its whole life (the retained store's run-lock
//!   pattern: `.pending-<run>.lock` is created, locked and renamed). The run
//!   starts a new segment when the current one would pass
//!   [`SEGMENT_MAX_BYTES`]; only its highest-numbered segment is still
//!   written, the lower ones are **sealed**. A run whose lock is absent or not
//!   held is **dead**, and so are all its files.
//! - `compacted-<run>.jsonl`, then `compacted-<run>.<n>.jsonl`: the chunks
//!   run `<run>`'s compaction wrote, each at most about
//!   [`COMPACTED_CHUNK_BYTES`]. Chunks are written whole under the
//!   maintenance lock and never appended to, so any run's compaction may fold
//!   any chunk again.
//! - `maintenance.lock`: recovery and compaction hold it exclusively, and every
//!   read of the directory holds it shared, so a reader never sees a
//!   compaction half done and two daemons sharing one state directory never
//!   maintain it at once.
//!
//! # Lines
//!
//! One ASCII-escaped JSON object per line, at most [`MAX_LINE_BYTES`] bytes
//! before its newline:
//!
//! ```text
//! {"v":1,"seq":1,"ts_ms":…,"run":"<run>","kind":"opened","work":"w-<deck>-<16 hex>",…}
//! ```
//!
//! A reader skips a line whose `v` is newer than [`SCHEMA_VERSION`], a line
//! whose `kind` it does not know and a complete line it cannot parse, drops a
//! final line with no newline (a torn write), and ignores unknown fields.
//!
//! # Redaction
//!
//! Only [`Workspace`] holds a filesystem path, with the home prefix written as
//! `~` whichever spelling of the path or of the home it arrives in. Every other
//! string field is refused when it looks like a path, and every string field
//! is refused when it holds a control character or is longer than
//! [`MAX_STRING_BYTES`]. A refused event is dropped with a `warn!`.
//!
//! # Calling it
//!
//! [`WorkJournal::record`] hands the event to the run's writer thread through
//! a bounded queue and returns at once: it never blocks and never does file
//! I/O, so it may be called under a bookkeeping lock. A full queue drops the
//! event (counted, and logged at most once a minute). The writer assigns
//! `seq`, so the file is in the order events were handed over. It never
//! panics and never fails the caller: a failure is logged and the event
//! dropped.
//!
//! # Budgets
//!
//! Every file is streamed: nothing of it is held beyond the line being decoded,
//! except what each pass keeps by design (a read's events, recovery's attempt
//! index, compaction's per-work index and its bounded de-duplication sets).
//! Reading, recovery and compaction each have a [`Budget`] of files, bytes and
//! entries that limits what one call takes on; it bounds memory and input,
//! not wall-clock time. Over budget, a read answers what it read
//! (`ReadStats::truncated`), recovery writes nothing that attempt and is
//! retried, and compaction folds what fits and leaves the rest for a later
//! round (see [`WorkJournal::compact_with`]).

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::deck_identity::{DeckId, is_valid_deck_id};
use crate::platform::trusted_dir::{TrustedDir, valid_component};

/// The directory under the state directory.
pub const DIR_NAME: &str = "work-journal";

/// The line schema this build writes and reads.
pub const SCHEMA_VERSION: u64 = 1;

/// The longest line, in bytes before its newline. A longer event is refused.
pub const MAX_LINE_BYTES: usize = 4096;

/// The longest string field, in bytes.
pub const MAX_STRING_BYTES: usize = 1024;

/// A finished work whose last event is older than this is dropped by
/// compaction (unless it is an ancestor of a kept work).
pub const RETENTION_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Compaction keeps at most this many of the most recent finished works
/// (again keeping ancestors).
pub const RETENTION_FINISHED_WORKS: usize = 10_000;

/// The environment variable a spawned agent receives its work id in.
pub const ENV_WORK_ID: &str = "DOT_AGENT_DECK_WORK_ID";

/// Events the writer queue holds; a full queue drops the event.
pub const QUEUE_CAPACITY: usize = 4096;

/// A run starts a new segment when the current one would pass this size.
pub const SEGMENT_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// How long a graceful daemon shutdown waits for the writer to drain.
pub const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);

/// How often a deferred startup recovery tries the maintenance lock again,
/// and the first wait before an incomplete or failed one is retried.
pub const RECOVERY_RETRY: Duration = Duration::from_millis(500);

/// The longest wait between retries of an incomplete or failed startup
/// recovery (the wait doubles from [`RECOVERY_RETRY`] up to this).
pub const RECOVERY_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);

/// Compaction starts a new output chunk when the current one would pass this
/// size.
pub const COMPACTED_CHUNK_BYTES: u64 = SEGMENT_MAX_BYTES;

/// A full queue's drops are logged at most this often.
const DROP_WARN_EVERY: Duration = Duration::from_secs(60);

/// A compaction index over its entry budget is logged at most this often:
/// once saturated it stays so round after round (see
/// [`CompactReport::index_saturated`]).
const SATURATION_WARN_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// The most journal file names one listing keeps.
const MAX_LISTED: usize = 65_536;

const MAINTENANCE_LOCK: &str = "maintenance.lock";

const KNOWN_KINDS: [&str; 6] = [
    "opened",
    "attempt_started",
    "attempt_ended",
    "linked",
    "reported",
    "closed",
];

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

/// A work id: `w-<deck id>-<16 lowercase hex>`, the hex from OS randomness.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkId(String);

const WORK_ID_HEX: usize = 16;

impl WorkId {
    /// A fresh work id under `deck`.
    pub fn mint(deck: &DeckId) -> io::Result<Self> {
        let mut bytes = [0u8; WORK_ID_HEX / 2];
        getrandom::fill(&mut bytes).map_err(io::Error::other)?;
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        Ok(WorkId(format!("w-{}-{hex}", deck.as_str())))
    }

    /// `Some` when `text` is exactly a work id.
    pub fn parse(text: &str) -> Option<Self> {
        let rest = text.strip_prefix("w-")?;
        let (deck, hex) = rest.split_once('-')?;
        let ok = is_valid_deck_id(deck)
            && hex.len() == WORK_ID_HEX
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        ok.then(|| WorkId(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for WorkId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for WorkId {
    type Error = String;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        WorkId::parse(&text).ok_or_else(|| format!("{text:?} is not a work id"))
    }
}

impl From<WorkId> for String {
    fn from(id: WorkId) -> String {
        id.0
    }
}

/// An attempt id: `<run>/<agent id>`, one agent generation under one daemon
/// run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AttemptId(String);

impl AttemptId {
    /// `None` when `run` is not a plain run id.
    pub fn new(run: &str, agent: u64) -> Option<Self> {
        valid_component(run).then(|| AttemptId(format!("{run}/{agent}")))
    }

    pub fn parse(text: &str) -> Option<Self> {
        let (run, agent) = text.rsplit_once('/')?;
        let agent_ok = !agent.is_empty()
            && agent.bytes().all(|b| b.is_ascii_digit())
            && agent.parse::<u64>().is_ok_and(|n| n.to_string() == agent);
        (valid_component(run) && agent_ok).then(|| AttemptId(text.to_string()))
    }

    /// The run this attempt ran under.
    pub fn run(&self) -> &str {
        self.0.rsplit_once('/').map_or("", |(run, _)| run)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttemptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AttemptId {
    type Error = String;
    fn try_from(text: String) -> Result<Self, Self::Error> {
        AttemptId::parse(&text).ok_or_else(|| format!("{text:?} is not an attempt id"))
    }
}

impl From<AttemptId> for String {
    fn from(id: AttemptId) -> String {
        id.0
    }
}

/// The one path a line may hold: a workspace, with the home directory prefix
/// written as `~`. Built only from a [`Path`], so no other field can carry one
/// by construction; reading a line accepts whatever string it holds.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Workspace(String);

impl Workspace {
    /// `path` with the current user's home directory written as `~`; `None`
    /// when the path is not UTF-8 or no privacy-safe spelling of it can be
    /// established.
    pub fn from_path(path: &Path) -> Option<Self> {
        Self::from_path_with_home(path, &crate::platform::paths::home_dir())
    }

    /// [`Self::from_path`] against an explicit home directory.
    ///
    /// The path and the home are compared after `.` and `..` are resolved
    /// lexically, and again in their canonical (symlink-resolved) forms where
    /// those resolve, so `/home/../home/u/x` and a symlinked home's other
    /// spelling are both written `~/x`. The prefix is matched component by
    /// component, so `/home/al` is not a prefix of `/home/alice`. A path that
    /// is under no spelling of the home is written in its lexical form; one
    /// whose canonical form cannot be read for a reason other than its absence
    /// is refused (`None`), since it might be the home under another name.
    pub fn from_path_with_home(path: &Path, home: &Path) -> Option<Self> {
        Self::resolve(path, &HomeSpellings::of(home))
    }

    fn resolve(path: &Path, homes: &HomeSpellings) -> Option<Self> {
        let lexical_path = lexical(path);
        let canonical = match std::fs::canonicalize(path) {
            Ok(canonical) => Some(canonical),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                None
            }
            // No home to protect: nothing to establish.
            Err(_) if homes.spellings.is_empty() => None,
            Err(_) => return None,
        };
        for base in &homes.spellings {
            for spelling in std::iter::once(&lexical_path).chain(canonical.as_ref()) {
                if let Ok(rest) = spelling.strip_prefix(base) {
                    let rest = rest.to_str()?;
                    if rest.is_empty() {
                        return Some(Workspace("~".to_string()));
                    }
                    let rest = rest.replace(std::path::MAIN_SEPARATOR, "/");
                    return Some(Workspace(format!("~/{rest}")));
                }
            }
        }
        if lexical_path.as_os_str().is_empty() {
            return Some(Workspace(".".to_string()));
        }
        Some(Workspace(lexical_path.to_str()?.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// `path` with `.` dropped and `..` taken back over the component before it,
/// without touching the filesystem. `..` at the root stays at the root; a
/// relative path keeps its leading `..`.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The spellings of a home directory a path is redacted against: its lexical
/// form and, when it resolves to something else, its canonical form. Empty
/// when the home has no normal component (`/`, the fallback when `HOME` is
/// unset), so such a home redacts nothing.
#[derive(Debug, Clone, Default)]
struct HomeSpellings {
    spellings: Vec<PathBuf>,
}

impl HomeSpellings {
    fn of(home: &Path) -> Self {
        let mut spellings = Vec::new();
        let lexical_home = lexical(home);
        if home_is_meaningful(&lexical_home) {
            spellings.push(lexical_home);
        }
        if let Ok(canonical) = std::fs::canonicalize(home)
            && home_is_meaningful(&canonical)
            && !spellings.contains(&canonical)
        {
            spellings.push(canonical);
        }
        HomeSpellings { spellings }
    }

    /// Each spelling as text, for the "does a plain field hold the home"
    /// check: only ones longer than `/`.
    fn texts(&self) -> impl Iterator<Item = &str> {
        self.spellings
            .iter()
            .filter_map(|home| home.to_str())
            .filter(|home| home.len() > 1)
    }
}

/// A home directory worth redacting: one with a normal component, so `/` (the
/// fallback when `HOME` is unset) never turns every path into `~/…`.
fn home_is_meaningful(home: &Path) -> bool {
    home.components().any(|c| matches!(c, Component::Normal(_)))
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// How a work was launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Launch {
    Pane,
    Orchestration,
    Delegation,
    Dispatch,
    ScheduleFire,
    IssueDispatch,
}

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndHow {
    Exited,
    Closed,
    Replaced,
    ReplacementFailed,
    Interrupted,
    Unknown,
}

/// Why an attempt was interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    /// The daemon shut down with the attempt live.
    DaemonShutdown,
    /// The daemon went away without recording an end; written by the next
    /// run's startup recovery.
    DaemonGone,
}

/// Where a report went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Inbox,
    Recovery,
}

/// How a work closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseHow {
    PaneClosed,
    UnitEnded,
    TaskDone,
    FireDone,
    OrchestrationEnded,
}

/// External references of a work. Every field is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u64>,
    /// `owner/name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

impl Refs {
    fn strings(&self) -> [(&'static str, Option<&str>); 9] {
        [
            ("refs.repo", self.repo.as_deref()),
            ("refs.branch", self.branch.as_deref()),
            ("refs.task_id", self.task_id.as_deref()),
            ("refs.unit_id", self.unit_id.as_deref()),
            ("refs.orchestration_id", self.orchestration_id.as_deref()),
            ("refs.orchestration", self.orchestration.as_deref()),
            ("refs.role", self.role.as_deref()),
            ("refs.schedule", self.schedule.as_deref()),
            ("refs.pane", self.pane.as_deref()),
        ]
    }
}

/// One journal event, without the header the writer adds (`v`, `seq`,
/// `ts_ms`, `run`). `work` is the work the event belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Opened {
        work: WorkId,
        launch: Launch,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<WorkId>,
        #[serde(default)]
        refs: Box<Refs>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<Workspace>,
    },
    AttemptStarted {
        work: WorkId,
        attempt: AttemptId,
        pane: String,
        agent: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    AttemptEnded {
        work: WorkId,
        attempt: AttemptId,
        how: EndHow,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<EndCause>,
    },
    Linked {
        work: WorkId,
        #[serde(default)]
        refs: Box<Refs>,
    },
    Reported {
        work: WorkId,
        report_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        unit_id: Option<String>,
        destination: Destination,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<WorkId>,
    },
    Closed {
        work: WorkId,
        how: CloseHow,
    },
}

impl Event {
    /// The work this event belongs to.
    pub fn work(&self) -> &WorkId {
        match self {
            Event::Opened { work, .. }
            | Event::AttemptStarted { work, .. }
            | Event::AttemptEnded { work, .. }
            | Event::Linked { work, .. }
            | Event::Reported { work, .. }
            | Event::Closed { work, .. } => work,
        }
    }

    /// The line's `kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Opened { .. } => "opened",
            Event::AttemptStarted { .. } => "attempt_started",
            Event::AttemptEnded { .. } => "attempt_ended",
            Event::Linked { .. } => "linked",
            Event::Reported { .. } => "reported",
            Event::Closed { .. } => "closed",
        }
    }
}

/// One line: the header and its event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub v: u64,
    pub seq: u64,
    pub ts_ms: u64,
    pub run: String,
    #[serde(flatten)]
    pub event: Event,
}

impl Line {
    /// The order lines are merged in across segments.
    fn order_key(&self) -> (u64, &str, u64) {
        (self.ts_ms, self.run.as_str(), self.seq)
    }
}

// ---------------------------------------------------------------------------
// The redaction guard and the codec
// ---------------------------------------------------------------------------

/// Why an event was not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A string field holds a control character.
    ControlCharacter { field: &'static str },
    /// A string field is longer than [`MAX_STRING_BYTES`].
    StringTooLong { field: &'static str, bytes: usize },
    /// A field other than `workspace` holds what looks like a path.
    PathOutsideWorkspace { field: &'static str },
    /// The encoded line is longer than [`MAX_LINE_BYTES`].
    LineTooLong { bytes: usize },
    /// The run id is not a plain path component.
    InvalidRun,
    /// A field that only goes with one `how` was set with another.
    Inconsistent { field: &'static str },
    /// Serialisation failed.
    Encode(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::ControlCharacter { field } => {
                write!(f, "field {field} holds a control character")
            }
            Refusal::StringTooLong { field, bytes } => write!(
                f,
                "field {field} is {bytes} bytes, over the {MAX_STRING_BYTES}-byte limit"
            ),
            Refusal::PathOutsideWorkspace { field } => {
                write!(f, "field {field} holds a path; only workspace may")
            }
            Refusal::LineTooLong { bytes } => write!(
                f,
                "the line is {bytes} bytes, over the {MAX_LINE_BYTES}-byte limit"
            ),
            Refusal::InvalidRun => f.write_str("the run id is not a plain path component"),
            Refusal::Inconsistent { field } => {
                write!(f, "field {field} does not go with this event's how")
            }
            Refusal::Encode(why) => write!(f, "the line could not be encoded: {why}"),
        }
    }
}

fn check_string(field: &'static str, text: &str) -> Result<(), Refusal> {
    if text.len() > MAX_STRING_BYTES {
        return Err(Refusal::StringTooLong {
            field,
            bytes: text.len(),
        });
    }
    if text.chars().any(char::is_control) {
        return Err(Refusal::ControlCharacter { field });
    }
    Ok(())
}

/// `text` reads as a filesystem path: absolute, home-relative, a Windows drive
/// path, or one that contains a spelling of the home directory.
fn looks_like_path(text: &str, homes: &HomeSpellings) -> bool {
    let bytes = text.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    text.starts_with('/')
        || text.starts_with('\\')
        || text.starts_with('~')
        || drive
        || homes.texts().any(|home| text.contains(home))
}

fn check_plain(field: &'static str, text: &str, homes: &HomeSpellings) -> Result<(), Refusal> {
    check_string(field, text)?;
    if looks_like_path(text, homes) {
        return Err(Refusal::PathOutsideWorkspace { field });
    }
    Ok(())
}

/// Apply the redaction rule to a line. `homes` are the spellings of the home
/// directory a path must not carry outside `workspace`.
fn validate(line: &Line, homes: &HomeSpellings) -> Result<(), Refusal> {
    if !valid_component(&line.run) {
        return Err(Refusal::InvalidRun);
    }
    let plain = |field, text: &str| check_plain(field, text, homes);
    let refs = |refs: &Refs| -> Result<(), Refusal> {
        for (field, text) in refs.strings() {
            if let Some(text) = text {
                plain(field, text)?;
            }
        }
        Ok(())
    };
    match &line.event {
        Event::Opened {
            refs: r, workspace, ..
        } => {
            refs(r)?;
            if let Some(workspace) = workspace {
                check_string("workspace", workspace.as_str())?;
                if homes
                    .spellings
                    .iter()
                    .any(|home| lexical(Path::new(workspace.as_str())).starts_with(home))
                {
                    return Err(Refusal::PathOutsideWorkspace {
                        field: "workspace (home not redacted)",
                    });
                }
            }
        }
        Event::AttemptStarted { pane, role, .. } => {
            plain("pane", pane)?;
            if let Some(role) = role {
                plain("role", role)?;
            }
        }
        Event::AttemptEnded {
            how, code, cause, ..
        } => {
            if code.is_some() && *how != EndHow::Exited {
                return Err(Refusal::Inconsistent { field: "code" });
            }
            if cause.is_some() && *how != EndHow::Interrupted {
                return Err(Refusal::Inconsistent { field: "cause" });
            }
        }
        Event::Linked { refs: r, .. } => refs(r)?,
        Event::Reported {
            report_id,
            task_id,
            unit_id,
            ..
        } => {
            plain("report_id", report_id)?;
            if let Some(task_id) = task_id {
                plain("task_id", task_id)?;
            }
            if let Some(unit_id) = unit_id {
                plain("unit_id", unit_id)?;
            }
        }
        Event::Closed { .. } => {}
    }
    Ok(())
}

/// Encode `line` as one ASCII line, without its newline, refusing it when the
/// redaction rule or the size limit forbids it. Paths are judged against the
/// current user's home directory.
pub fn encode_line(line: &Line) -> Result<Vec<u8>, Refusal> {
    encode_line_with_home(line, &crate::platform::paths::home_dir())
}

/// [`encode_line`] against an explicit home directory.
pub fn encode_line_with_home(line: &Line, home: &Path) -> Result<Vec<u8>, Refusal> {
    encode_line_for(line, &HomeSpellings::of(home))
}

fn encode_line_for(line: &Line, homes: &HomeSpellings) -> Result<Vec<u8>, Refusal> {
    validate(line, homes)?;
    let mut out = Vec::with_capacity(256);
    let mut ser =
        serde_json::Serializer::with_formatter(&mut out, crate::retained_store::AsciiFormatter);
    line.serialize(&mut ser)
        .map_err(|e| Refusal::Encode(e.to_string()))?;
    if out.len() > MAX_LINE_BYTES {
        return Err(Refusal::LineTooLong { bytes: out.len() });
    }
    Ok(out)
}

/// What one complete line decoded to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    Event(Line),
    /// `v` is newer than this build reads.
    NewerVersion,
    /// A `kind` this build does not know.
    UnknownKind,
    /// Not a version-1 line this build can parse.
    Unparsable,
}

/// Decode one complete line (without its newline).
pub fn decode_line(bytes: &[u8]) -> Decoded {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return Decoded::Unparsable;
    };
    let Some(v) = value.get("v").and_then(Value::as_u64) else {
        return Decoded::Unparsable;
    };
    if v > SCHEMA_VERSION {
        return Decoded::NewerVersion;
    }
    if v != SCHEMA_VERSION {
        return Decoded::Unparsable;
    }
    let Some(kind) = value.get("kind").and_then(Value::as_str) else {
        return Decoded::Unparsable;
    };
    if !KNOWN_KINDS.contains(&kind) {
        return Decoded::UnknownKind;
    }
    match serde_json::from_value::<Line>(value) {
        Ok(line) if valid_component(&line.run) => Decoded::Event(line),
        _ => Decoded::Unparsable,
    }
}

// ---------------------------------------------------------------------------
// Budgets
// ---------------------------------------------------------------------------

/// How much one read, recovery or compaction round may take on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Journal files opened.
    pub files: usize,
    /// Bytes read (each file counts at its length when it is opened).
    pub bytes: u64,
    /// Entries held: lines for a read, attempts for recovery; for compaction,
    /// the entries of its per-work index (works, attempts, and each work's
    /// files) and, separately, the `(run, seq)` keys one round de-duplicates.
    pub entries: usize,
}

/// The journal's budgets. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budgets {
    pub read: Budget,
    pub recovery: Budget,
    pub compaction: Budget,
    /// Unreadable lines one compaction round carries into its output; more
    /// are discarded (and counted) as they are read, never held.
    pub carried_unreadable: usize,
}

const MIB: u64 = 1024 * 1024;

impl Default for Budgets {
    fn default() -> Self {
        Budgets {
            read: Budget {
                files: 4096,
                bytes: 256 * MIB,
                entries: 500_000,
            },
            recovery: Budget {
                files: 4096,
                bytes: 512 * MIB,
                entries: 500_000,
            },
            compaction: Budget {
                files: 1024,
                bytes: 128 * MIB,
                entries: 500_000,
            },
            carried_unreadable: 10_000,
        }
    }
}

/// What a round has spent of its [`Budget`]'s files and bytes.
struct Spend {
    budget: Budget,
    files: usize,
    bytes: u64,
}

impl Spend {
    fn new(budget: Budget) -> Self {
        Spend {
            budget,
            files: 0,
            bytes: 0,
        }
    }

    /// Take room for one more file of `len` bytes; `false` when it does not
    /// fit, and nothing is taken.
    fn take(&mut self, len: u64) -> bool {
        self.take_many(1, len)
    }

    /// Take room for `files` more files of `bytes` bytes in all; `false` when
    /// they do not fit, and nothing is taken.
    fn take_many(&mut self, files: usize, bytes: u64) -> bool {
        let total = self.bytes.saturating_add(bytes);
        if self.files.saturating_add(files) > self.budget.files || total > self.budget.bytes {
            return false;
        }
        self.files += files;
        self.bytes = total;
        true
    }

    /// Take room for `files` files of `bytes` bytes whether or not they fit.
    fn force(&mut self, files: usize, bytes: u64) {
        self.files = self.files.saturating_add(files);
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

// ---------------------------------------------------------------------------
// The reader
// ---------------------------------------------------------------------------

/// What reading skipped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReadStats {
    pub files: usize,
    pub events: usize,
    pub newer_version: usize,
    pub unknown_kind: usize,
    pub unparsable: usize,
    pub too_long: usize,
    pub torn_tails: usize,
    pub duplicates: usize,
    /// The read stopped at its budget: some files or lines were not read.
    pub truncated: bool,
}

/// Every event in the journal, merged across files in `(ts_ms, run, seq)`
/// order, with duplicates (the same `run` and `seq`) removed.
#[derive(Debug, Clone, Default)]
pub struct JournalRead {
    pub lines: Vec<Line>,
    pub stats: ReadStats,
}

/// Why a complete line was not an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    NewerVersion,
    UnknownKind,
    Unparsable,
}

/// One item of a file, borrowing the line's bytes.
enum Item<'a> {
    Event(Line, &'a [u8]),
    /// A complete line this build cannot read, as written.
    Skipped(Skip, &'a [u8]),
    /// A line over [`MAX_LINE_BYTES`], not buffered.
    TooLong,
}

fn classify(bytes: &[u8]) -> Item<'_> {
    match decode_line(bytes) {
        Decoded::Event(line) => Item::Event(line, bytes),
        Decoded::NewerVersion => Item::Skipped(Skip::NewerVersion, bytes),
        Decoded::UnknownKind => Item::Skipped(Skip::UnknownKind, bytes),
        Decoded::Unparsable => Item::Skipped(Skip::Unparsable, bytes),
    }
}

fn tally(stats: &mut ReadStats, item: &Item<'_>) {
    match item {
        Item::Event(..) => stats.events += 1,
        Item::TooLong => stats.too_long += 1,
        Item::Skipped(Skip::NewerVersion, _) => stats.newer_version += 1,
        Item::Skipped(Skip::UnknownKind, _) => stats.unknown_kind += 1,
        Item::Skipped(Skip::Unparsable, _) => stats.unparsable += 1,
    }
}

enum Piece<'a> {
    Line(&'a [u8]),
    TooLong,
    Torn,
}

/// Split `reader` into newline-terminated lines of at most `max` bytes. A
/// longer line is reported as [`Piece::TooLong`] without being buffered, and
/// bytes after the last newline as [`Piece::Torn`].
fn scan_lines(reader: impl Read, max: usize, mut on: impl FnMut(Piece<'_>)) -> io::Result<()> {
    let mut reader = BufReader::with_capacity(64 * 1024, reader);
    let mut line: Vec<u8> = Vec::new();
    let mut overflow = false;
    let mut partial = false;
    loop {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf.is_empty() {
            if partial {
                on(Piece::Torn);
            }
            return Ok(());
        }
        let (take, newline) = match buf.iter().position(|&b| b == b'\n') {
            Some(at) => (at, true),
            None => (buf.len(), false),
        };
        if !overflow {
            if line.len().saturating_add(take) > max {
                overflow = true;
                line.clear();
            } else {
                line.extend_from_slice(&buf[..take]);
            }
        }
        partial = true;
        let consumed = if newline { take + 1 } else { take };
        if newline {
            if overflow {
                on(Piece::TooLong);
            } else {
                on(Piece::Line(&line));
            }
            line.clear();
            overflow = false;
            partial = false;
        }
        reader.consume(consumed);
    }
}

/// A journal file opened for one pass, with its length and modification time
/// at the open.
struct OpenFile {
    file: File,
    len: u64,
    modified: SystemTime,
}

fn open_file(dir: &TrustedDir, name: &str) -> io::Result<OpenFile> {
    let file = dir.open_regular(name)?;
    let meta = file.metadata()?;
    Ok(OpenFile {
        file,
        len: meta.len(),
        modified: meta.modified().unwrap_or(UNIX_EPOCH),
    })
}

/// Stream `file` up to its length at the open, one item at a time; nothing of
/// the file is held beyond the line being handed over. Answers whether its
/// tail was torn.
fn pass(file: OpenFile, mut on: impl FnMut(Item<'_>)) -> io::Result<bool> {
    let mut torn = false;
    scan_lines(
        file.file.take(file.len),
        MAX_LINE_BYTES,
        |piece| match piece {
            // A blank line carries nothing, not even an unreadable event.
            Piece::Line([]) => {}
            Piece::Line(bytes) => on(classify(bytes)),
            Piece::TooLong => on(Item::TooLong),
            Piece::Torn => torn = true,
        },
    )?;
    Ok(torn)
}

/// What a journal file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FileKind {
    /// A compacted chunk, by its number (0 for `compacted-<run>.jsonl`).
    Compacted(u64),
    /// A run's segment, by its number (0 for `run-<run>.jsonl`).
    Segment(u64),
}

#[derive(Debug, Clone)]
struct Listed {
    name: String,
    run: String,
    kind: FileKind,
}

/// The journal files and run locks in the directory.
#[derive(Debug, Default)]
struct Listing {
    /// Every segment and compacted chunk, chunks first, then by run
    /// and segment number.
    files: Vec<Listed>,
    /// Runs with a lock file.
    locks: Vec<String>,
    /// More than [`MAX_LISTED`] names: the rest were not listed. A truncated
    /// listing never feeds retention or the removal of a lock: what it omits
    /// could pin a work or show a run live.
    truncated: bool,
}

impl Listing {
    /// The highest segment number each run has.
    fn top_segments(&self) -> HashMap<&str, u64> {
        let mut top: HashMap<&str, u64> = HashMap::new();
        for file in &self.files {
            if let FileKind::Segment(n) = file.kind {
                let entry = top.entry(file.run.as_str()).or_insert(n);
                *entry = (*entry).max(n);
            }
        }
        top
    }
}

/// `run-<run>.jsonl` for segment 0, `run-<run>.<n>.jsonl` after it.
fn segment_name_n(run: &str, n: u64) -> String {
    if n == 0 {
        format!("run-{run}.jsonl")
    } else {
        format!("run-{run}.{n}.jsonl")
    }
}

#[cfg(test)]
fn segment_name(run: &str) -> String {
    segment_name_n(run, 0)
}

/// `compacted-<run>.jsonl` for chunk 0, `compacted-<run>.<n>.jsonl` after it.
fn compacted_name_n(run: &str, n: u64) -> String {
    if n == 0 {
        format!("compacted-{run}.jsonl")
    } else {
        format!("compacted-{run}.{n}.jsonl")
    }
}

#[cfg(test)]
fn compacted_name(run: &str) -> String {
    compacted_name_n(run, 0)
}

fn lock_name(run: &str) -> String {
    format!("run-{run}.lock")
}

/// A segment's run and number, from its name. A run id holds no `.`, so the
/// first `.` starts the number.
fn parse_segment(name: &str) -> Option<(&str, u64)> {
    parse_numbered(name, "run-")
}

/// A compacted chunk's run and number, from its name.
fn parse_compacted(name: &str) -> Option<(&str, u64)> {
    parse_numbered(name, "compacted-")
}

/// `<prefix><run>.jsonl` (number 0) or `<prefix><run>.<n>.jsonl`.
fn parse_numbered<'a>(name: &'a str, prefix: &str) -> Option<(&'a str, u64)> {
    let rest = name.strip_prefix(prefix)?.strip_suffix(".jsonl")?;
    let (run, n) = match rest.split_once('.') {
        Some((run, n)) => {
            let canonical =
                !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && !n.starts_with('0');
            (
                run,
                if canonical {
                    n.parse().ok()?
                } else {
                    return None;
                },
            )
        }
        None => (rest, 0),
    };
    valid_component(run).then_some((run, n))
}

fn run_of<'a>(name: &'a str, prefix: &str, suffix: &str) -> Option<&'a str> {
    let run = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    valid_component(run).then_some(run)
}

/// A predicate on a file name.
type NameFilter = dyn Fn(&str) -> bool + Send + Sync;

/// Names a listing treats as beyond its cap (a test seam; `None` in the
/// daemon).
type Omit<'a> = Option<&'a NameFilter>;

fn list(dir: &TrustedDir) -> io::Result<Listing> {
    list_with(dir, None)
}

fn list_with(dir: &TrustedDir, omit: Omit<'_>) -> io::Result<Listing> {
    let mut listing = Listing::default();
    let mut stream = dir.stream()?;
    while let Some(name) = stream.next_name() {
        let name = name?;
        let Some(name) = name.to_str() else {
            continue;
        };
        let entry = if let Some((run, n)) = parse_segment(name) {
            Some((run, FileKind::Segment(n)))
        } else {
            parse_compacted(name).map(|(run, n)| (run, FileKind::Compacted(n)))
        };
        let lock = run_of(name, "run-", ".lock");
        if entry.is_none() && lock.is_none() {
            continue;
        }
        if omit.is_some_and(|omit| omit(name)) {
            listing.truncated = true;
            continue;
        }
        if listing.files.len() + listing.locks.len() >= MAX_LISTED {
            listing.truncated = true;
            break;
        }
        if let Some((run, kind)) = entry {
            listing.files.push(Listed {
                name: name.to_string(),
                run: run.to_string(),
                kind,
            });
        } else if let Some(run) = lock {
            listing.locks.push(run.to_string());
        }
    }
    if listing.truncated {
        warn!(
            limit = MAX_LISTED,
            "work journal: the directory holds more files than one listing keeps; the rest are left for a later round"
        );
    }
    let segment = |kind: FileKind| matches!(kind, FileKind::Segment(_));
    listing.files.sort_by(|a, b| {
        segment(a.kind)
            .cmp(&segment(b.kind))
            .then_with(|| a.run.cmp(&b.run))
            .then_with(|| a.kind.cmp(&b.kind))
    });
    listing.locks.sort();
    Ok(listing)
}

/// The maintenance lock's file, created owner-only when it is missing.
fn maintenance_file(dir: &TrustedDir) -> io::Result<File> {
    match dir.create_new_owner_only(MAINTENANCE_LOCK) {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => dir.open_regular(MAINTENANCE_LOCK),
        Err(e) => Err(e),
    }
}

/// The maintenance lock, held exclusively, or `None` when anyone else holds
/// it (a reader, another daemon's recovery or compaction). Never waits.
fn maintenance_exclusive(dir: &TrustedDir) -> io::Result<Option<File>> {
    let file = maintenance_file(dir)?;
    Ok(crate::platform::lock::try_lock_file(&file)?.then_some(file))
}

#[cfg(test)]
thread_local! {
    /// TEST seam: a reader thread that sets this is told, just before it
    /// starts waiting, that the maintenance lock was held and it is about to
    /// wait.
    pub(crate) static ON_SHARED_WAIT: std::cell::RefCell<Option<mpsc::Sender<()>>> =
        const { std::cell::RefCell::new(None) };
}

/// The maintenance lock, held shared: waits while a recovery or compaction
/// holds it, so a read sees the directory before or after one, never during.
fn maintenance_shared(dir: &TrustedDir) -> io::Result<File> {
    let file = maintenance_file(dir)?;
    match file.try_lock_shared() {
        Ok(()) => return Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {}
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    #[cfg(test)]
    ON_SHARED_WAIT.with(|waiting| {
        if let Some(tx) = waiting.borrow().as_ref() {
            let _ = tx.send(());
        }
    });
    file.lock_shared()?;
    Ok(file)
}

/// Read every segment and compacted chunk in `dir`, within `budget`.
fn read_dir_all(dir: &TrustedDir, budget: Budget) -> io::Result<JournalRead> {
    let _shared = maintenance_shared(dir)?;
    let listing = list(dir)?;
    let mut out = JournalRead::default();
    let mut seen: HashSet<(String, u64)> = HashSet::new();
    let mut spend = Spend::new(budget);
    let mut full = false;
    for file in &listing.files {
        let opened = match open_file(dir, &file.name) {
            Ok(opened) => opened,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        if !spend.take(opened.len) {
            full = true;
            break;
        }
        let stats = &mut out.stats;
        let lines = &mut out.lines;
        let torn = pass(opened, |item| {
            tally(stats, &item);
            if let Item::Event(line, _) = item {
                if lines.len() >= budget.entries {
                    full = true;
                } else if seen.insert((line.run.clone(), line.seq)) {
                    lines.push(line);
                } else {
                    stats.duplicates += 1;
                }
            }
        })?;
        out.stats.files += 1;
        if torn {
            out.stats.torn_tails += 1;
        }
        if full {
            break;
        }
    }
    if full || listing.truncated {
        out.stats.truncated = true;
        warn!(
            ?budget,
            "work journal: a read stopped at its budget; it answers part of the journal"
        );
    }
    out.lines.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
    if out.stats != ReadStats::default() {
        debug!(stats = ?out.stats, "work journal: read");
    }
    Ok(out)
}

/// Read the journal under `state_dir`, within the default read budget. A
/// state directory with no journal reads as empty. Waits while a recovery or
/// compaction holds the directory.
pub fn read_journal(state_dir: &Path) -> io::Result<JournalRead> {
    read_journal_with(state_dir, Budgets::default().read)
}

/// [`read_journal`] within `budget`.
pub fn read_journal_with(state_dir: &Path, budget: Budget) -> io::Result<JournalRead> {
    let anchor = TrustedDir::anchor(state_dir)?;
    let dir = match anchor.child(DIR_NAME, false) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(JournalRead::default()),
        Err(e) => return Err(e),
    };
    read_dir_all(&dir, budget)
}

// ---------------------------------------------------------------------------
// The handle
// ---------------------------------------------------------------------------

/// Retention limits for compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub max_age: Duration,
    pub max_finished_works: usize,
}

impl Default for Retention {
    fn default() -> Self {
        Retention {
            max_age: RETENTION_AGE,
            max_finished_works: RETENTION_FINISHED_WORKS,
        }
    }
}

/// Why an event was not recorded.
#[derive(Debug)]
pub enum RecordError {
    /// The journal could not be opened at startup.
    Disabled,
    /// An earlier write failed and could not be rolled back, so the segment
    /// takes no further lines.
    Broken,
    /// The writer thread is gone.
    Stopped,
    Refused(Refusal),
    Io(io::Error),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Disabled => f.write_str("the journal is disabled"),
            RecordError::Broken => f.write_str("an earlier write failed and could not be undone"),
            RecordError::Stopped => f.write_str("the journal's writer has stopped"),
            RecordError::Refused(why) => write!(f, "refused: {why}"),
            RecordError::Io(e) => write!(f, "{e}"),
        }
    }
}

/// What startup recovery did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `attempt_ended … interrupted` lines written.
    pub interrupted: usize,
    /// Interrupted attempts whose line could not be written.
    pub failed: usize,
    /// Other runs found live (their segments were read, not recovered).
    pub live_runs: usize,
    /// The maintenance lock was held by someone else: nothing was read or
    /// written, and recovery has to run again.
    pub busy: bool,
    /// Recovery stopped at its budget: it wrote nothing, since an end it did
    /// not read could make an interruption it wrote false.
    pub incomplete: bool,
    /// Recovery failed with this error (reading the journal, or the run's
    /// writer); what it had not written is written by a later attempt.
    pub error: Option<String>,
    pub stats: ReadStats,
}

impl RecoveryReport {
    /// Recovery read the whole journal and wrote every interruption it owed.
    /// Anything else (busy, over budget, an error, a line it could not write)
    /// has to run again.
    pub fn completed(&self) -> bool {
        !self.busy && !self.incomplete && self.error.is_none() && self.failed == 0
    }
}

/// How [`WorkJournal::start_recovery`] went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryStart {
    /// It ran now, to completion.
    Done(RecoveryReport),
    /// It ran but did not complete (over its budget, or an error): it is
    /// retried in the background with a growing wait, and after every
    /// compaction round that folds files. Compaction does not wait for it.
    Retrying(RecoveryReport),
    /// The maintenance lock was busy: it runs in the background once the lock
    /// is free, and compaction waits for it.
    Deferred,
    /// The journal is disabled.
    Disabled,
}

/// What a compaction did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport {
    /// Another daemon was compacting or recovering, or a reader was reading:
    /// nothing was done.
    pub skipped_busy: bool,
    /// This run's startup recovery has not run yet: nothing was done.
    pub skipped_recovery_pending: bool,
    /// The round's input budget left files for a later round.
    pub over_budget: bool,
    /// The round could not index the whole journal (its index budget, or a
    /// truncated listing), so it dropped no work and removed no lock.
    pub retention_skipped: bool,
    /// The journal holds more distinct works, attempts and file references
    /// than the compaction budget's `entries`: the index is incomplete, so
    /// retention is skipped (`retention_skipped`). An accepted limitation:
    /// folding files does not reduce the number of distinct works, so this
    /// lasts until the budget is raised (about 125,000 finished works at the
    /// defaults).
    pub index_saturated: bool,
    /// Files folded into new compacted chunks and removed.
    pub consumed_files: usize,
    /// Compacted chunks written.
    pub written_files: usize,
    pub kept_lines: usize,
    pub dropped_lines: usize,
    pub dropped_works: usize,
    /// Unreadable lines carried over unchanged.
    pub carried_unreadable: usize,
    /// Lines over the limit, torn tails, and unreadable lines over the carry
    /// budget, which are not carried.
    pub discarded: usize,
    pub removed_files: usize,
    /// The most de-duplication entries (event keys and unreadable lines) the
    /// round held at once, and their bytes.
    pub held_peak: usize,
    pub held_peak_bytes: u64,
    /// The round folded files while startup recovery was still incomplete,
    /// so recovery was tried again after it.
    pub recovery_retried: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryPhase {
    /// Nobody asked for recovery (a journal a test opened): compaction may
    /// run.
    NotRequested,
    /// Startup recovery has not run yet (the lock was busy): compaction waits.
    Pending,
    /// Startup recovery ran and did not complete (over budget, or an error):
    /// it is retried, and compaction runs meanwhile, since folding files is
    /// what can bring the journal back under recovery's budget.
    Retry,
    Done,
}

/// A message to the writer thread.
enum Cmd {
    Record {
        event: Event,
        ts_ms: u64,
        reply: Option<mpsc::Sender<Result<u64, RecordError>>>,
    },
    /// Answered once everything queued before it is written.
    Flush(mpsc::Sender<()>),
}

/// The writer thread's state: the run's current segment and the run lock,
/// which it holds until it has written everything it was handed, so the
/// run's files stay live as long as anything can still be written to them.
struct Writer {
    dir: TrustedDir,
    run: String,
    homes: HomeSpellings,
    file: File,
    /// The current segment's number.
    segment: u64,
    /// Shared with the handle, for compaction: segments below it are sealed.
    current: Arc<AtomicU64>,
    segment_max: u64,
    next_seq: u64,
    len: u64,
    /// After a failed rotation, the current segment's length at which the
    /// next one is tried (zero: at once, when the segment is full).
    rotate_at: u64,
    /// How much further the segment grows before the next try after another
    /// failure: doubles per failure from a sixteenth of `segment_max` up to
    /// `segment_max`; zero while rotation has not failed.
    rotate_backoff: u64,
    /// Rotations failed in a row.
    rotate_failures: u64,
    broken: bool,
    #[cfg(test)]
    gate: Option<Arc<WriterGate>>,
    #[cfg(test)]
    rotate_sync_fault: Option<Arc<AtomicU64>>,
    /// Released when the writer ends, after everything is written.
    _lock: File,
    /// Dropped last, so the handle's last drop can wait for the lock's release.
    _done: mpsc::Sender<()>,
}

impl Writer {
    fn run(mut self, rx: mpsc::Receiver<Cmd>) {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Record {
                    event,
                    ts_ms,
                    reply,
                } => {
                    #[cfg(test)]
                    if let Some(gate) = &self.gate {
                        gate.pass();
                    }
                    let kind = event.kind();
                    let work = event.work().clone();
                    let written = self.write(event, ts_ms);
                    match reply {
                        Some(reply) => {
                            let _ = reply.send(written);
                        }
                        None => {
                            if let Err(error) = written {
                                warn!(
                                    kind,
                                    %work,
                                    %error,
                                    "work journal: an event was not recorded"
                                );
                            }
                        }
                    }
                }
                Cmd::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    }

    fn write(&mut self, event: Event, ts_ms: u64) -> Result<u64, RecordError> {
        if self.broken {
            return Err(RecordError::Broken);
        }
        let line = Line {
            v: SCHEMA_VERSION,
            seq: self.next_seq,
            ts_ms,
            run: self.run.clone(),
            event,
        };
        let mut bytes = encode_line_for(&line, &self.homes).map_err(RecordError::Refused)?;
        bytes.push(b'\n');
        if self.len > 0
            && self.len.saturating_add(bytes.len() as u64) > self.segment_max
            && self.len >= self.rotate_at
        {
            self.rotate();
        }
        let written = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_data());
        match written {
            Ok(()) => {
                self.next_seq += 1;
                self.len += bytes.len() as u64;
                Ok(line.seq)
            }
            Err(error) => {
                // Undo a partial line, so the next one does not glue onto it.
                let len = self.len;
                let undone = self
                    .file
                    .set_len(len)
                    .and_then(|()| self.file.seek(SeekFrom::Start(len)).map(|_| ()));
                if let Err(undo) = undone {
                    warn!(
                        %undo,
                        "work journal: a failed write could not be undone; this run records nothing more"
                    );
                    self.broken = true;
                }
                Err(RecordError::Io(error))
            }
        }
    }

    /// Seal the current segment and start the next. On failure the current
    /// one keeps taking lines: that includes a failed directory sync after
    /// the new segment was created, since lines synced into a file whose
    /// directory entry is not durable can vanish with it in a crash. The new,
    /// empty file is removed then, so it does not read as the run's highest
    /// segment. A failed rotation is not tried again on the next line, but
    /// once the segment has grown by [`Self::rotate_backoff`] more, so a
    /// lasting fault costs one attempt (and one warning) per stretch of
    /// lines, not one per line.
    ///
    /// Only this writer creates this run's segments (it holds the run lock),
    /// so a name that already exists is an empty segment an earlier failed
    /// rotation could not remove. It is skipped for the next free number:
    /// once a later segment exists it is sealed, and compaction folds it
    /// away.
    fn rotate(&mut self) {
        match self.start_next_segment() {
            Ok(next) => {
                if self.rotate_failures > 0 {
                    info!(
                        failures = self.rotate_failures,
                        "work journal: rotation works again"
                    );
                }
                self.rotate_at = 0;
                self.rotate_backoff = 0;
                self.rotate_failures = 0;
                debug!(segment = next, "work journal: started a new segment");
            }
            Err(error) => {
                self.rotate_failures += 1;
                self.rotate_backoff = self
                    .rotate_backoff
                    .saturating_mul(2)
                    .clamp((self.segment_max / 16).max(1), self.segment_max);
                self.rotate_at = self.len.saturating_add(self.rotate_backoff);
                warn!(
                    %error,
                    failures = self.rotate_failures,
                    retry_after_bytes = self.rotate_backoff,
                    "work journal: could not start a new segment; the current one keeps growing"
                );
            }
        }
    }

    /// Create, sync and switch to the next segment; its number.
    fn start_next_segment(&mut self) -> io::Result<u64> {
        let mut next = self.segment + 1;
        let mut skipped = 0;
        let file = loop {
            let name = segment_name_n(&self.run, next);
            match self.dir.create_new_owner_only(&name) {
                Ok(file) => break file,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && skipped < ROTATE_SKIP_MAX => {
                    skipped += 1;
                    next += 1;
                }
                Err(e) => return Err(e),
            }
        };
        let name = segment_name_n(&self.run, next);
        if let Err(error) = self.sync_for_rotation() {
            drop(file);
            if let Err(remove) = self.dir.remove_file(&name) {
                warn!(
                    segment = %name,
                    error = %remove,
                    "work journal: the unsynced new segment could not be removed; the next rotation skips its name"
                );
            }
            return Err(error);
        }
        self.file = file;
        self.segment = next;
        self.len = 0;
        self.current.store(next, Ordering::Release);
        Ok(next)
    }
}

/// How many leftover segment names (empty files failed rotations could not
/// remove) one rotation skips before it gives up.
const ROTATE_SKIP_MAX: u32 = 64;

impl Writer {
    /// The directory sync that makes a new segment's entry durable.
    fn sync_for_rotation(&self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(faults) = &self.rotate_sync_fault
            && faults
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(io::Error::other("injected rotation sync failure"));
        }
        self.dir.sync()
    }
}

/// TEST seam: holds the writer before each event until it is opened.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct WriterGate {
    open: Mutex<bool>,
    cv: Condvar,
    /// Events the writer has reached the gate with.
    pub(crate) reached: AtomicU64,
}

#[cfg(test)]
impl WriterGate {
    pub(crate) fn closed() -> Arc<Self> {
        Arc::new(WriterGate::default())
    }

    pub(crate) fn open(&self) {
        *self.open.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_all();
    }

    fn pass(&self) {
        self.reached.fetch_add(1, Ordering::SeqCst);
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        while !*open {
            open = self.cv.wait(open).unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// TEST seams on a journal.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct TestHooks {
    pub(crate) gate: Option<Arc<WriterGate>>,
    /// Called by compaction after its output is published and before the
    /// files it folded are removed.
    pub(crate) after_publish: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Names the journal's own listings treat as beyond their cap.
    pub(crate) listing_omits: Option<Arc<NameFilter>>,
    /// While above zero, each recovery attempt fails with an I/O error and
    /// takes one off.
    pub(crate) fail_recovery: Option<Arc<AtomicU64>>,
    /// While set, compaction's probe for a free chunk name fails with this
    /// error kind.
    pub(crate) chunk_probe_fault: Option<io::ErrorKind>,
    /// While above zero, each rotation's directory sync fails and takes one
    /// off.
    pub(crate) rotate_sync_fault: Option<Arc<AtomicU64>>,
    /// Holds each recovery attempt, with the lock not yet taken, until it is
    /// opened.
    pub(crate) recovery_gate: Option<Arc<WriterGate>>,
    /// Counts the compaction rounds skipped because startup recovery had not
    /// had its first attempt yet.
    pub(crate) skipped_pending: Option<Arc<AtomicU64>>,
}

#[cfg(test)]
impl std::fmt::Debug for TestHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestHooks")
            .field("gate", &self.gate.is_some())
            .field("after_publish", &self.after_publish.is_some())
            .field("listing_omits", &self.listing_omits.is_some())
            .field("fail_recovery", &self.fail_recovery)
            .field("chunk_probe_fault", &self.chunk_probe_fault)
            .field("rotate_sync_fault", &self.rotate_sync_fault)
            .field("recovery_gate", &self.recovery_gate.is_some())
            .field("skipped_pending", &self.skipped_pending)
            .finish()
    }
}

struct Store {
    dir: TrustedDir,
    /// `Some` until the store is dropped.
    tx: Option<SyncSender<Cmd>>,
    /// Disconnected once the writer thread has ended.
    writer_done: Mutex<mpsc::Receiver<()>>,
    /// The writer's current segment number.
    current: Arc<AtomicU64>,
    /// Events dropped because the queue was full or the writer gone.
    dropped: AtomicU64,
    /// When a drop was last logged.
    last_drop_warn: Mutex<Option<Instant>>,
}

impl Drop for Store {
    /// The last handle is gone: the writer writes what it holds and ends,
    /// releasing the run lock. Waits for that, within [`SHUTDOWN_DRAIN`], so
    /// the run reads as dead once its journal is dropped.
    fn drop(&mut self) {
        drop(self.tx.take());
        let done = self
            .writer_done
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = done.recv_timeout(SHUTDOWN_DRAIN);
    }
}

impl Store {
    fn tx(&self) -> Option<&SyncSender<Cmd>> {
        self.tx.as_ref()
    }

    fn note_drop(&self) {
        let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
        let log = {
            let mut last = self
                .last_drop_warn
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let due = last.is_none_or(|at| at.elapsed() >= DROP_WARN_EVERY);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if log {
            warn!(
                dropped_total = total,
                "work journal: the writer is behind; events are being dropped"
            );
        }
    }
}

struct Shared {
    run: String,
    deck: DeckId,
    homes: HomeSpellings,
    path: PathBuf,
    store: Option<Store>,
    disabled: Option<String>,
    budgets: Budgets,
    recovery_retry: Duration,
    recovery_backoff_max: Duration,
    compacted_chunk_bytes: u64,
    phase: Mutex<RecoveryPhase>,
    phase_changed: Condvar,
    /// Held across one startup-recovery attempt, so two (the retry thread's
    /// and one after a compaction) never both write interruptions.
    recovery_step: Mutex<()>,
    /// Times recovery tried the maintenance lock.
    recovery_attempts: AtomicU64,
    /// When a saturated compaction index was last logged.
    last_saturation_warn: Mutex<Option<Instant>>,
    #[cfg(test)]
    hooks: TestHooks,
}

/// The daemon's handle on its run's journal. Cheap to clone, `Send + Sync`;
/// every method takes `&self`.
#[derive(Clone)]
pub struct WorkJournal {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for WorkJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkJournal")
            .field("run", &self.shared.run)
            .field("dir", &self.shared.path)
            .field("disabled", &self.shared.disabled)
            .finish()
    }
}

/// Options for [`WorkJournal::try_open_with`].
#[derive(Debug, Clone)]
pub struct JournalOptions {
    /// The home directory paths are redacted against.
    pub home: PathBuf,
    /// Events the writer queue holds ([`QUEUE_CAPACITY`]).
    pub queue_capacity: usize,
    /// The size a segment is sealed at ([`SEGMENT_MAX_BYTES`]).
    pub segment_max_bytes: u64,
    /// How often a deferred recovery retries, and the first wait before an
    /// incomplete one is retried ([`RECOVERY_RETRY`]).
    pub recovery_retry: Duration,
    /// The longest wait between retries of an incomplete recovery
    /// ([`RECOVERY_BACKOFF_MAX`]).
    pub recovery_backoff_max: Duration,
    /// The size compaction's output chunks are cut at
    /// ([`COMPACTED_CHUNK_BYTES`]).
    pub compacted_chunk_bytes: u64,
    pub budgets: Budgets,
    #[cfg(test)]
    pub(crate) hooks: TestHooks,
}

impl Default for JournalOptions {
    fn default() -> Self {
        JournalOptions {
            home: crate::platform::paths::home_dir(),
            queue_capacity: QUEUE_CAPACITY,
            segment_max_bytes: SEGMENT_MAX_BYTES,
            recovery_retry: RECOVERY_RETRY,
            recovery_backoff_max: RECOVERY_BACKOFF_MAX,
            compacted_chunk_bytes: COMPACTED_CHUNK_BYTES,
            budgets: Budgets::default(),
            #[cfg(test)]
            hooks: TestHooks::default(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl WorkJournal {
    /// Open run `run`'s journal under `state_dir` (an absolute path). Never
    /// fails: a journal that cannot be opened is logged once and returned
    /// disabled, so every [`Self::record`] is a no-op and the daemon runs on.
    pub fn open(state_dir: &Path, run: &str, deck: &DeckId) -> Self {
        match Self::try_open_with(state_dir, run, deck, JournalOptions::default()) {
            Ok(journal) => journal,
            Err(error) => {
                warn!(
                    dir = %state_dir.join(DIR_NAME).display(),
                    %error,
                    "work journal: cannot be opened; this run records no work"
                );
                Self::disabled(state_dir, run, deck, error.to_string())
            }
        }
    }

    /// [`Self::open`], answering the error instead of a disabled journal.
    pub fn try_open(state_dir: &Path, run: &str, deck: &DeckId) -> io::Result<Self> {
        Self::try_open_with(state_dir, run, deck, JournalOptions::default())
    }

    pub fn try_open_with(
        state_dir: &Path,
        run: &str,
        deck: &DeckId,
        options: JournalOptions,
    ) -> io::Result<Self> {
        if !state_dir.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the state directory is not an absolute path",
            ));
        }
        if !valid_component(run) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{run:?} is not a plain run id"),
            ));
        }
        let anchor = TrustedDir::anchor(state_dir)?;
        let fresh = anchor.entry(DIR_NAME).is_err();
        let dir = anchor.child(DIR_NAME, true)?;
        if fresh {
            anchor.sync()?;
        }
        if dir.entry(lock_name(run)).is_ok() || dir.entry(segment_name_n(run, 0)).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("run {run} already has a journal segment"),
            ));
        }
        let pending = format!(".pending-{run}.lock");
        let lock = dir.create_new_owner_only(&pending)?;
        match crate::platform::lock::try_lock_file(&lock) {
            Ok(true) => {}
            Ok(false) => {
                let _ = dir.remove_file(&pending);
                return Err(io::Error::other("could not lock the run: it is held"));
            }
            Err(error) => {
                let _ = dir.remove_file(&pending);
                return Err(io::Error::other(format!("could not lock the run: {error}")));
            }
        }
        dir.rename(&pending, &lock_name(run))?;
        let file = dir.create_new_owner_only(&segment_name_n(run, 0))?;
        dir.sync()?;
        let homes = HomeSpellings::of(&options.home);
        let current = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::sync_channel(options.queue_capacity.max(1));
        let (done, writer_done) = mpsc::channel();
        let writer = Writer {
            dir: dir.try_clone()?,
            run: run.to_string(),
            homes: homes.clone(),
            file,
            segment: 0,
            current: Arc::clone(&current),
            segment_max: options.segment_max_bytes.max(1),
            next_seq: 1,
            len: 0,
            rotate_at: 0,
            rotate_backoff: 0,
            rotate_failures: 0,
            broken: false,
            #[cfg(test)]
            gate: options.hooks.gate.clone(),
            #[cfg(test)]
            rotate_sync_fault: options.hooks.rotate_sync_fault.clone(),
            _lock: lock,
            _done: done,
        };
        std::thread::Builder::new()
            .name("work-journal".to_string())
            .spawn(move || writer.run(rx))?;
        Ok(WorkJournal {
            shared: Arc::new(Shared {
                run: run.to_string(),
                deck: deck.clone(),
                homes,
                path: state_dir.join(DIR_NAME),
                store: Some(Store {
                    dir,
                    tx: Some(tx),
                    writer_done: Mutex::new(writer_done),
                    current,
                    dropped: AtomicU64::new(0),
                    last_drop_warn: Mutex::new(None),
                }),
                disabled: None,
                budgets: options.budgets,
                recovery_retry: options.recovery_retry,
                recovery_backoff_max: options.recovery_backoff_max,
                compacted_chunk_bytes: options.compacted_chunk_bytes.max(1),
                phase: Mutex::new(RecoveryPhase::NotRequested),
                phase_changed: Condvar::new(),
                recovery_step: Mutex::new(()),
                recovery_attempts: AtomicU64::new(0),
                last_saturation_warn: Mutex::new(None),
                #[cfg(test)]
                hooks: options.hooks,
            }),
        })
    }

    /// A journal that records nothing, but still mints work ids (an agent's
    /// `DOT_AGENT_DECK_WORK_ID` does not depend on the journal).
    pub fn disabled(state_dir: &Path, run: &str, deck: &DeckId, reason: String) -> Self {
        WorkJournal {
            shared: Arc::new(Shared {
                run: run.to_string(),
                deck: deck.clone(),
                homes: HomeSpellings::of(&crate::platform::paths::home_dir()),
                path: state_dir.join(DIR_NAME),
                store: None,
                disabled: Some(reason),
                budgets: Budgets::default(),
                recovery_retry: RECOVERY_RETRY,
                recovery_backoff_max: RECOVERY_BACKOFF_MAX,
                compacted_chunk_bytes: COMPACTED_CHUNK_BYTES,
                phase: Mutex::new(RecoveryPhase::NotRequested),
                phase_changed: Condvar::new(),
                recovery_step: Mutex::new(()),
                recovery_attempts: AtomicU64::new(0),
                last_saturation_warn: Mutex::new(None),
                #[cfg(test)]
                hooks: TestHooks::default(),
            }),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.shared.store.is_some()
    }

    /// Why the journal is disabled, if it is.
    pub fn disabled_reason(&self) -> Option<&str> {
        self.shared.disabled.as_deref()
    }

    pub fn run(&self) -> &str {
        &self.shared.run
    }

    pub fn deck(&self) -> &DeckId {
        &self.shared.deck
    }

    /// The journal's directory, for messages.
    pub fn dir(&self) -> &Path {
        &self.shared.path
    }

    /// `text` as a plain (not a path) string field, or `None` when the
    /// redaction rule would refuse it, so a caller can leave one bad ref out
    /// rather than lose the whole event. Empty text is `None` too.
    pub fn plain(&self, text: &str) -> Option<String> {
        (!text.is_empty() && check_plain("ref", text, &self.shared.homes).is_ok())
            .then(|| text.to_string())
    }

    /// A fresh work id under this deck; `None` (logged) only when the OS
    /// cannot provide randomness.
    pub fn mint_work_id(&self) -> Option<WorkId> {
        match WorkId::mint(&self.shared.deck) {
            Ok(id) => Some(id),
            Err(error) => {
                warn!(%error, "work journal: cannot mint a work id");
                None
            }
        }
    }

    /// The attempt id of agent `agent` under this run.
    pub fn attempt_id(&self, agent: u64) -> Option<AttemptId> {
        AttemptId::new(&self.shared.run, agent)
    }

    /// `path` as a workspace, its home prefix written as `~`; `None` when the
    /// redaction rule would refuse it (so the event goes without it). Reads
    /// the filesystem to resolve the path: call it with no lock held.
    pub fn workspace(&self, path: &Path) -> Option<Workspace> {
        Workspace::resolve(path, &self.shared.homes)
            .filter(|w| check_string("workspace", w.as_str()).is_ok())
    }

    /// Hand `event` to the writer and return at once. Never blocks, never
    /// does file I/O, never panics and never fails the caller: a full queue
    /// drops the event (counted, see [`Self::dropped_events`]), and a refused
    /// event or a failed write is logged by the writer with `warn!`.
    pub fn record(&self, event: Event) {
        let Some(store) = &self.shared.store else {
            return;
        };
        let cmd = Cmd::Record {
            event,
            ts_ms: now_ms(),
            reply: None,
        };
        let queued = store.tx().is_some_and(|tx| tx.try_send(cmd).is_ok());
        if !queued {
            store.note_drop();
        }
    }

    /// Write `event` and wait for it, answering the line's `seq` or why it
    /// was not written. Blocks while the queue is full and while the writer
    /// is busy: for recovery and tests, never for a lifecycle path.
    pub fn try_record(&self, event: Event) -> Result<u64, RecordError> {
        let store = self.shared.store.as_ref().ok_or(RecordError::Disabled)?;
        let (reply, answer) = mpsc::channel();
        store
            .tx()
            .ok_or(RecordError::Stopped)?
            .send(Cmd::Record {
                event,
                ts_ms: now_ms(),
                reply: Some(reply),
            })
            .map_err(|_| RecordError::Stopped)?;
        answer.recv().unwrap_or(Err(RecordError::Stopped))
    }

    /// Wait until everything handed over before this call is written.
    pub fn flush(&self) {
        let Some(store) = &self.shared.store else {
            return;
        };
        let (done, wait) = mpsc::channel();
        if store
            .tx()
            .is_some_and(|tx| tx.send(Cmd::Flush(done)).is_ok())
        {
            let _ = wait.recv();
        }
    }

    /// [`Self::flush`], waiting at most `bound`; `true` when it drained.
    pub fn flush_within(&self, bound: Duration) -> bool {
        let Some(tx) = self.shared.store.as_ref().and_then(Store::tx) else {
            return true;
        };
        let deadline = Instant::now() + bound;
        let (done, wait) = mpsc::channel();
        let mut cmd = Cmd::Flush(done);
        loop {
            match tx.try_send(cmd) {
                Ok(()) => break,
                Err(TrySendError::Disconnected(_)) => return true,
                Err(TrySendError::Full(back)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    cmd = back;
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        wait.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .is_ok()
    }

    /// Events dropped because the writer queue was full.
    pub fn dropped_events(&self) -> u64 {
        self.shared
            .store
            .as_ref()
            .map_or(0, |store| store.dropped.load(Ordering::Relaxed))
    }

    /// Every event in the journal directory, this run's included, within the
    /// journal's read budget. Waits while a recovery or compaction holds the
    /// directory.
    pub fn read(&self) -> io::Result<JournalRead> {
        let store = self.store()?;
        read_dir_all(&store.dir, self.shared.budgets.read)
    }

    fn store(&self) -> io::Result<&Store> {
        self.shared
            .store
            .as_ref()
            .ok_or_else(|| io::Error::other("the work journal is disabled"))
    }

    /// The compaction index is over its entry budget: logged at most once
    /// per [`SATURATION_WARN_EVERY`], since it stays so round after round.
    fn warn_saturated(&self, entries: usize) {
        let due = {
            let mut last = self
                .shared
                .last_saturation_warn
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let due = last.is_none_or(|at| at.elapsed() >= SATURATION_WARN_EVERY);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        if due {
            warn!(
                entries,
                "work journal: compaction's index is saturated (more distinct works and attempts than its entry budget); retention is skipped and finished works are not aged out until the budget is raised"
            );
        }
    }

    /// The journal's own listing of `dir` (with the test seam's omissions).
    fn listing(&self, dir: &TrustedDir) -> io::Result<Listing> {
        #[cfg(test)]
        if let Some(omit) = &self.shared.hooks.listing_omits {
            return list_with(dir, Some(omit.as_ref()));
        }
        list(dir)
    }

    /// The runs whose lock is held: this one, and every other run named by a
    /// listed file or lock whose `run-<run>.lock` exists and cannot be taken.
    /// Each run's lock is probed by name, whether or not the listing reached
    /// it, so a listing that stopped at its cap between a live run's segment
    /// and its lock never makes that run look dead. A lock that cannot be
    /// judged counts as live, so its files are left alone.
    fn live_runs(&self, dir: &TrustedDir, listing: &Listing) -> HashSet<String> {
        let mut live = HashSet::new();
        live.insert(self.shared.run.clone());
        let runs: HashSet<&str> = listing
            .files
            .iter()
            .map(|f| f.run.as_str())
            .chain(listing.locks.iter().map(String::as_str))
            .collect();
        for run in runs {
            if run == self.shared.run {
                continue;
            }
            let held = match dir.open_regular(lock_name(run)) {
                Ok(file) => !matches!(crate::platform::lock::try_lock_file(&file), Ok(true)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => false,
                Err(_) => true,
            };
            if held {
                live.insert(run.to_string());
            }
        }
        live
    }

    // -----------------------------------------------------------------------
    // Recovery
    // -----------------------------------------------------------------------

    fn set_phase(&self, phase: RecoveryPhase) {
        *self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = phase;
        self.shared.phase_changed.notify_all();
    }

    fn phase(&self) -> RecoveryPhase {
        *self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether this run's startup recovery has completed (or was never asked
    /// for).
    pub fn recovery_done(&self) -> bool {
        matches!(
            self.phase(),
            RecoveryPhase::Done | RecoveryPhase::NotRequested
        )
    }

    /// Wait at most `bound` for a deferred or retried recovery; `true` once it
    /// has completed.
    pub fn wait_recovered(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut phase = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while matches!(*phase, RecoveryPhase::Pending | RecoveryPhase::Retry) {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            phase = self
                .shared
                .phase_changed
                .wait_timeout(phase, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Wait at most `bound` for startup recovery's first attempt (or, while
    /// another daemon holds the maintenance lock, the attempt that finds it
    /// free); `true` once compaction no longer waits for it, which is the
    /// case as soon as the phase leaves pending (done, or retrying).
    pub fn wait_recovery_attempted(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut phase = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *phase == RecoveryPhase::Pending {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            phase = self
                .shared
                .phase_changed
                .wait_timeout(phase, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Times recovery has tried the maintenance lock.
    pub fn recovery_attempts(&self) -> u64 {
        self.shared.recovery_attempts.load(Ordering::SeqCst)
    }

    /// The daemon's startup recovery. Runs [`Self::recover`] now. When it
    /// completes, that is all. When another daemon (or a reader) holds the
    /// maintenance lock, the daemon starts anyway and recovery runs on a
    /// background thread as soon as the lock is free; compaction waits for it.
    /// When it runs but does not complete (over its budget, or an error), it
    /// is retried on that thread with a wait that doubles from
    /// [`RECOVERY_RETRY`] up to [`RECOVERY_BACKOFF_MAX`], and after every
    /// compaction round that folds files; compaction does not wait for it.
    /// Recovery never runs without the lock.
    pub fn start_recovery(&self) -> RecoveryStart {
        if !self.is_enabled() {
            return RecoveryStart::Disabled;
        }
        self.set_phase(RecoveryPhase::Pending);
        let report = self.recovery_step().unwrap_or_default();
        if report.completed() {
            return RecoveryStart::Done(report);
        }
        if report.busy {
            info!(
                "work journal: the journal is being maintained elsewhere; recovery runs once it is free"
            );
        }
        self.spawn_recovery_retries(report.busy);
        if report.busy {
            RecoveryStart::Deferred
        } else {
            RecoveryStart::Retrying(report)
        }
    }

    /// The daemon's startup recovery, off the caller's thread: as
    /// [`Self::start_recovery`], except that its first attempt also runs on
    /// the background thread, so a startup on an async path does not wait for
    /// a scan of the journal (only when that thread cannot be started does
    /// the first attempt run in place). Compaction still waits for it: the phase is
    /// pending from this call until that first attempt has run.
    pub fn start_recovery_in_background(&self) {
        if !self.is_enabled() {
            return;
        }
        self.set_phase(RecoveryPhase::Pending);
        if !self.spawn_recovery_thread(Duration::ZERO, false) {
            // No thread: recovery runs here rather than not at all.
            let _ = self.start_recovery();
        }
    }

    /// One attempt at the startup recovery the phase still owes: `None` when
    /// it owes none (done, or never asked for). Moves the phase.
    fn recovery_step(&self) -> Option<RecoveryReport> {
        let _one = self
            .shared
            .recovery_step
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if matches!(
            self.phase(),
            RecoveryPhase::Done | RecoveryPhase::NotRequested
        ) {
            return None;
        }
        #[cfg(test)]
        if let Some(gate) = &self.shared.hooks.recovery_gate {
            gate.pass();
        }
        let report = self.recover();
        if report.completed() {
            self.set_phase(RecoveryPhase::Done);
        } else if !report.busy {
            self.set_phase(RecoveryPhase::Retry);
        }
        Some(report)
    }

    /// Retry the startup recovery on a background thread until it completes:
    /// every [`RECOVERY_RETRY`] while the lock is busy, and with a doubling
    /// wait, bounded by the backoff maximum, while it runs and does not
    /// complete. The thread holds the journal weakly, so it ends when the
    /// journal is dropped.
    fn spawn_recovery_retries(&self, busy: bool) {
        if !self.spawn_recovery_thread(self.shared.recovery_retry, !busy) {
            warn!(
                "work journal: cannot retry recovery in the background; it is retried after compaction"
            );
        }
    }

    /// The recovery thread: waits `first`, then takes recovery steps until
    /// one completes, with [`Self::spawn_recovery_retries`]'s waits. False
    /// when the thread could not be started.
    fn spawn_recovery_thread(&self, first: Duration, backoff: bool) -> bool {
        let shared = Arc::downgrade(&self.shared);
        let base = self.shared.recovery_retry;
        let max = self.shared.recovery_backoff_max.max(base);
        let mut wait = first;
        let mut backoff = backoff;
        let spawned = std::thread::Builder::new()
            .name("work-journal-recovery".to_string())
            .spawn(move || {
                loop {
                    std::thread::sleep(wait);
                    let Some(shared) = shared.upgrade() else {
                        return;
                    };
                    let journal = WorkJournal { shared };
                    match journal.recovery_step() {
                        None => return,
                        Some(report) if report.completed() => return,
                        Some(report) if report.busy => {
                            if !backoff {
                                wait = base;
                            }
                        }
                        Some(_) => {
                            wait = if backoff {
                                wait.saturating_mul(2).min(max)
                            } else {
                                base
                            };
                            backoff = true;
                        }
                    }
                }
            });
        match spawned {
            Ok(_) => true,
            Err(error) => {
                warn!(%error, "work journal: cannot start the recovery thread");
                false
            }
        }
    }

    /// Recovery: append `attempt_ended {how: interrupted, cause:
    /// daemon_gone}` to this run's segment for every attempt a dead run
    /// started that has no end anywhere in the journal. Reads every file,
    /// rewrites none. Never fails: an error is logged and kept in the
    /// report's `error`.
    pub fn recover(&self) -> RecoveryReport {
        match self.try_recover() {
            Ok(report) => {
                if report.interrupted > 0 || report.failed > 0 {
                    info!(
                        interrupted = report.interrupted,
                        failed = report.failed,
                        "work journal: marked the previous runs' open attempts interrupted"
                    );
                }
                report
            }
            // A disabled journal has nothing to recover.
            Err(_) if !self.is_enabled() => RecoveryReport::default(),
            Err(error) => {
                warn!(%error, "work journal: startup recovery failed; it is tried again");
                RecoveryReport {
                    error: Some(error.to_string()),
                    ..RecoveryReport::default()
                }
            }
        }
    }

    /// [`Self::recover`], answering the error. Takes the maintenance lock
    /// exclusively without waiting; when it is held, nothing is read and the
    /// report says `busy`.
    pub fn try_recover(&self) -> io::Result<RecoveryReport> {
        let store = self.store()?;
        let mut report = RecoveryReport::default();
        self.shared.recovery_attempts.fetch_add(1, Ordering::SeqCst);
        #[cfg(test)]
        if let Some(failures) = &self.shared.hooks.fail_recovery
            && failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(io::Error::other("injected recovery failure"));
        }
        let Some(maintenance) = maintenance_exclusive(&store.dir)? else {
            report.busy = true;
            return Ok(report);
        };
        let listing = self.listing(&store.dir)?;
        let live = self.live_runs(&store.dir, &listing);
        report.live_runs = live.len().saturating_sub(1);
        let budget = self.shared.budgets.recovery;
        let mut spend = Spend::new(budget);
        let mut complete = !listing.truncated;
        // attempt -> (its work, its start's order key)
        let mut started: HashMap<AttemptId, (WorkId, (u64, String, u64))> = HashMap::new();
        let mut ended: HashSet<AttemptId> = HashSet::new();
        for file in &listing.files {
            let opened = match open_file(&store.dir, &file.name) {
                Ok(opened) => opened,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if !spend.take(opened.len) {
                complete = false;
                break;
            }
            let stats = &mut report.stats;
            let torn = pass(opened, |item| {
                tally(stats, &item);
                let Item::Event(line, _) = item else {
                    return;
                };
                let room = started.len() + ended.len() < budget.entries;
                match line.event {
                    Event::AttemptStarted { work, attempt, .. } if !live.contains(&line.run) => {
                        if room || started.contains_key(&attempt) {
                            let key = (line.ts_ms, line.run, line.seq);
                            started.entry(attempt).or_insert((work, key));
                        } else {
                            complete = false;
                        }
                    }
                    Event::AttemptEnded { attempt, .. } => {
                        if room || ended.contains(&attempt) {
                            ended.insert(attempt);
                        } else {
                            complete = false;
                        }
                    }
                    _ => {}
                }
            })?;
            report.stats.files += 1;
            if torn {
                report.stats.torn_tails += 1;
            }
            if !complete {
                break;
            }
        }
        if !complete {
            report.incomplete = true;
            warn!(
                ?budget,
                "work journal: the journal is over recovery's budget; no attempt was marked interrupted this time, and recovery is tried again"
            );
            drop(maintenance);
            return Ok(report);
        }
        let mut open: Vec<(AttemptId, WorkId, (u64, String, u64))> = started
            .into_iter()
            .filter(|(attempt, _)| !ended.contains(attempt))
            .map(|(attempt, (work, key))| (attempt, work, key))
            .collect();
        open.sort_by(|a, b| a.2.cmp(&b.2));
        for (attempt, work, _) in open {
            let event = Event::AttemptEnded {
                work,
                attempt,
                how: EndHow::Interrupted,
                code: None,
                cause: Some(EndCause::DaemonGone),
            };
            match self.try_record(event) {
                Ok(_) => report.interrupted += 1,
                Err(error) => {
                    warn!(%error, "work journal: could not mark an attempt interrupted");
                    report.failed += 1;
                }
            }
        }
        drop(maintenance);
        Ok(report)
    }

    // -----------------------------------------------------------------------
    // Compaction
    // -----------------------------------------------------------------------

    /// Compact with the default retention, now. Never fails: an error is
    /// logged. Blocking; the daemon runs it off the startup path.
    pub fn compact(&self) -> CompactReport {
        match self.compact_with(now_ms(), Retention::default()) {
            Ok(report) => {
                if report.consumed_files > 0 {
                    info!(?report, "work journal: compacted");
                }
                report
            }
            Err(error) => {
                if self.is_enabled() {
                    warn!(%error, "work journal: compaction failed");
                }
                CompactReport::default()
            }
        }
    }

    /// One compaction round: fold dead runs' segments, sealed segments (a
    /// live run's segments below its highest one) and compacted chunks into
    /// new compacted chunks of this run, dropping the works `retention` lets
    /// go. A live run's highest segment is read (a work with a line there is
    /// kept) but never touched.
    ///
    /// A round makes two passes over the files, streaming both:
    ///
    /// 1. **Index.** Every listed file is read, and only a per-work index is
    ///    kept: when each work was last seen, its parent, the attempts it
    ///    started and which work's line ended each, whether a line of it is in
    ///    a file the round does not touch, and which files hold its lines.
    ///    The index is bounded by the compaction budget's `entries`. When the
    ///    index is over that budget, or the listing was truncated, the round
    ///    drops no work (an unread line could belong to a work it would drop)
    ///    and removes no lock, but it still folds files. Folding does not
    ///    reduce the number of distinct works, so an index over its budget
    ///    stays over it ([`CompactReport::index_saturated`]).
    /// 2. **Rewrite.** The round picks the files to rewrite within the
    ///    compaction budget's `files` and `bytes`: first the files of the
    ///    oldest works retention drops (a work is dropped only in a round that
    ///    rewrites every file holding a line of it, and never while it is an
    ///    ancestor of a kept work or holds the end of a kept work's attempt),
    ///    then segments and compacted chunks under half the chunk size. Each
    ///    picked file is streamed into new chunks of at most about the chunk
    ///    size; what a round does not pick waits for a later one. A full chunk
    ///    with nothing to drop is not rewritten (the index pass still reads
    ///    it), so the rewrite does not grow every round. A work whose files
    ///    alone exceed the budget
    ///    is folded on its own when nothing else can be dropped.
    ///
    /// Nothing is held across the rewrite but a bounded de-duplication of
    /// `(run, seq)` keys and of unreadable lines (at most the budget's
    /// `entries` keys and `carried_unreadable` lines; past them a line is
    /// written as it is, or an unreadable one discarded).
    pub fn compact_with(&self, now_ms: u64, retention: Retention) -> io::Result<CompactReport> {
        let store = self.store()?;
        let dir = &store.dir;
        let mut report = CompactReport::default();
        if self.phase() == RecoveryPhase::Pending {
            report.skipped_recovery_pending = true;
            #[cfg(test)]
            if let Some(skipped) = &self.shared.hooks.skipped_pending {
                skipped.fetch_add(1, Ordering::SeqCst);
            }
            return Ok(report);
        }
        let Some(maintenance) = maintenance_exclusive(dir)? else {
            report.skipped_busy = true;
            return Ok(report);
        };
        let run = self.shared.run.as_str();
        // Read before the listing: a segment numbered below it is sealed for
        // good, whatever the writer does next.
        let current = store.current.load(Ordering::Acquire);
        let listing = self.listing(dir)?;
        let live = self.live_runs(dir, &listing);
        let top = listing.top_segments();
        let consumable = |file: &Listed| -> bool {
            match file.kind {
                FileKind::Compacted(_) => true,
                FileKind::Segment(n) => {
                    !live.contains(&file.run)
                        || (n < top.get(file.run.as_str()).copied().unwrap_or(0)
                            && (file.run != run || n < current))
                }
            }
        };
        if !listing.files.iter().any(&consumable) {
            return Ok(report);
        }

        // Pass 1: the index.
        let budget = self.shared.budgets.compaction;
        let mut index = Index::new(budget.entries, &live);
        let mut candidates: Vec<Candidate> = Vec::new();
        for file in &listing.files {
            let opened = match open_file(dir, &file.name) {
                Ok(opened) => opened,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            let slot = if consumable(file) {
                let slot = u32::try_from(candidates.len()).unwrap_or(u32::MAX);
                candidates.push(Candidate {
                    name: file.name.clone(),
                    segment: matches!(file.kind, FileKind::Segment(_)),
                    len: opened.len,
                    modified: opened.modified,
                });
                Some(slot)
            } else {
                None
            };
            if index.full {
                continue;
            }
            pass(opened, |item| {
                if let Item::Event(line, _) = item {
                    index.note(&line, slot);
                }
            })?;
        }
        if candidates.is_empty() {
            return Ok(report);
        }
        let retain = !index.full && !listing.truncated;
        if !retain {
            report.retention_skipped = true;
        }
        if index.full {
            report.index_saturated = true;
            self.warn_saturated(budget.entries);
        }
        if listing.truncated {
            warn!(
                ?budget,
                "work journal: compaction's listing of the journal was truncated; this round drops no work"
            );
        }

        // Pass 2: pick the files, decide the drops, rewrite.
        let chunk = self.shared.compacted_chunk_bytes;
        let eligible = if retain {
            index.eligible(now_ms, retention)
        } else {
            Vec::new()
        };
        let mut order: Vec<u32> = (0..candidates.len())
            .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
            .collect();
        order.sort_by(|a, b| {
            let (a, b) = (&candidates[*a as usize], &candidates[*b as usize]);
            (a.modified, &a.name).cmp(&(b.modified, &b.name))
        });
        let mut picked = vec![false; candidates.len()];
        let mut spend = Spend::new(budget);
        let take = |files: &[u32], picked: &mut Vec<bool>, spend: &mut Spend, force: bool| {
            let new: Vec<u32> = files
                .iter()
                .copied()
                .filter(|f| !picked[*f as usize])
                .collect();
            let bytes: u64 = new.iter().map(|f| candidates[*f as usize].len).sum();
            if force {
                spend.force(new.len(), bytes);
            } else if !spend.take_many(new.len(), bytes) {
                return false;
            }
            for f in new {
                picked[f as usize] = true;
            }
            true
        };
        // A work goes only with every work that needs it, so their files are
        // picked together.
        let needers = index.needers();
        for &work in &eligible {
            let files = index.group_files(work, &needers);
            if !take(&files, &mut picked, &mut spend, false) {
                report.over_budget = true;
            }
        }
        if let Some(&oldest) = eligible.first()
            && !picked.iter().any(|p| *p)
        {
            let files = index.group_files(oldest, &needers);
            warn!(
                files = files.len(),
                ?budget,
                "work journal: a work's files exceed one compaction round; folding them on their own"
            );
            take(&files, &mut picked, &mut spend, true);
        }
        let tidy: Vec<u32> = order
            .iter()
            .copied()
            .filter(|f| {
                let c = &candidates[*f as usize];
                !picked[*f as usize] && (c.segment || c.len < chunk / 2)
            })
            .collect();
        let lone_chunk =
            !picked.iter().any(|p| *p) && tidy.len() == 1 && !candidates[tidy[0] as usize].segment;
        if !lone_chunk {
            for f in tidy {
                if !take(&[f], &mut picked, &mut spend, false)
                    && candidates[f as usize].len <= budget.bytes
                {
                    report.over_budget = true;
                }
            }
        }
        if !picked.iter().any(|p| *p) {
            return Ok(report);
        }
        let dropped = index.dropped(&eligible, &picked);
        report.dropped_works = dropped.iter().filter(|d| **d).count();

        let mut out = ChunkWriter::new(dir, run, chunk);
        #[cfg(test)]
        {
            out.probe_fault = self.shared.hooks.chunk_probe_fault;
        }
        let mut seen: HashSet<(String, u64)> = HashSet::new();
        let mut seen_raw: HashSet<Vec<u8>> = HashSet::new();
        let mut held_bytes: u64 = 0;
        let mut consumed: Vec<&str> = Vec::new();
        let rewritten = (|| -> io::Result<()> {
            for &f in &order {
                if !picked[f as usize] {
                    continue;
                }
                let name = candidates[f as usize].name.as_str();
                let opened = match open_file(dir, name) {
                    Ok(opened) => opened,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                };
                let mut failed: Option<io::Error> = None;
                let torn = pass(opened, |item| {
                    if failed.is_some() {
                        return;
                    }
                    let write = match item {
                        Item::Event(line, raw) => {
                            let drop = index
                                .by_id
                                .get(line.event.work())
                                .is_some_and(|w| dropped[*w as usize]);
                            let key = (line.run, line.seq);
                            if drop {
                                report.dropped_lines += 1;
                                None
                            } else if seen.contains(&key) {
                                None
                            } else {
                                if seen.len() < budget.entries {
                                    held_bytes += key.0.len() as u64 + 8;
                                    seen.insert(key);
                                }
                                report.kept_lines += 1;
                                Some(raw)
                            }
                        }
                        Item::Skipped(_, raw) => {
                            if seen_raw.contains(raw) {
                                None
                            } else if seen_raw.len() >= self.shared.budgets.carried_unreadable {
                                report.discarded += 1;
                                None
                            } else {
                                held_bytes += raw.len() as u64;
                                seen_raw.insert(raw.to_vec());
                                report.carried_unreadable += 1;
                                Some(raw)
                            }
                        }
                        Item::TooLong => {
                            report.discarded += 1;
                            None
                        }
                    };
                    report.held_peak = report.held_peak.max(seen.len() + seen_raw.len());
                    report.held_peak_bytes = report.held_peak_bytes.max(held_bytes);
                    if let Some(raw) = write
                        && let Err(error) = out.write(raw)
                    {
                        failed = Some(error);
                    }
                })?;
                if let Some(error) = failed {
                    return Err(error);
                }
                if torn {
                    report.discarded += 1;
                }
                consumed.push(name);
            }
            out.finish()?;
            dir.sync()
        })();
        if let Err(error) = rewritten {
            out.abandon();
            return Err(error);
        }
        report.consumed_files = consumed.len();
        report.written_files = out.published.len();
        #[cfg(test)]
        if let Some(hook) = &self.shared.hooks.after_publish {
            hook();
        }

        // The folded files, then (from a round that indexed the whole journal
        // only) the dead runs' locks.
        for name in &consumed {
            match dir.remove_file(name) {
                Ok(()) => report.removed_files += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(error) => warn!(
                    file = %name,
                    %error,
                    "work journal: a folded file could not be removed; its lines are read twice until it is"
                ),
            }
        }
        if retain {
            for lock_run in listing.locks.iter().filter(|r| !live.contains(*r)) {
                match dir.remove_file(lock_name(lock_run)) {
                    Ok(()) => report.removed_files += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        warn!(run = %lock_run, %error, "work journal: a dead run's lock could not be removed");
                    }
                }
            }
        }
        let _ = dir.sync();
        drop(maintenance);
        // Folding files is what brings an over-budget journal back within
        // recovery's budget: an incomplete startup recovery is tried again.
        if report.consumed_files > 0 && self.phase() == RecoveryPhase::Retry {
            report.recovery_retried = true;
            self.recovery_step();
        }
        Ok(report)
    }
}

/// A file a compaction round may rewrite.
struct Candidate {
    name: String,
    segment: bool,
    len: u64,
    modified: SystemTime,
}

/// What compaction's index knows of one work.
#[derive(Debug)]
struct WorkFacts {
    id: WorkId,
    last_ts: u64,
    parent: Option<u32>,
    /// The attempts it started.
    started: Vec<u32>,
    /// A line of it is in a file the round does not rewrite: never dropped.
    pinned: bool,
    /// The candidate files holding its lines, each once.
    files: Vec<u32>,
    /// It was `closed`.
    closed: bool,
    /// Its `opened` names it a delegation, which never gets a new attempt.
    delegation: bool,
    /// A live run started an attempt of it: that run may still serve it (an
    /// exited pane keeps its work until it closes, and a restart of it is a
    /// new attempt of the same work).
    live_attempt: bool,
}

/// Compaction's first pass: what the journal says about every work, bounded
/// by `limit` entries, holding no line.
struct Index<'a> {
    works: Vec<WorkFacts>,
    /// The runs whose lock is held.
    live: &'a HashSet<String>,
    by_id: HashMap<WorkId, u32>,
    attempts: HashMap<AttemptId, u32>,
    /// Attempt -> the work whose `attempt_ended` line ended it.
    ender: Vec<Option<u32>>,
    entries: usize,
    limit: usize,
    /// Over `limit`: the index is incomplete and drives no retention.
    full: bool,
}

impl<'a> Index<'a> {
    fn new(limit: usize, live: &'a HashSet<String>) -> Self {
        Index {
            works: Vec::new(),
            live,
            by_id: HashMap::new(),
            attempts: HashMap::new(),
            ender: Vec::new(),
            entries: 0,
            limit,
            full: false,
        }
    }

    /// Room for `n` more entries, or the index is full.
    fn room(&mut self, n: usize) -> bool {
        if self.full || self.entries.saturating_add(n) > self.limit {
            self.full = true;
            return false;
        }
        self.entries += n;
        true
    }

    fn work(&mut self, id: &WorkId) -> Option<u32> {
        if let Some(&w) = self.by_id.get(id) {
            return Some(w);
        }
        if !self.room(1) {
            return None;
        }
        let w = u32::try_from(self.works.len()).ok()?;
        self.works.push(WorkFacts {
            id: id.clone(),
            last_ts: 0,
            parent: None,
            started: Vec::new(),
            pinned: false,
            files: Vec::new(),
            closed: false,
            delegation: false,
            live_attempt: false,
        });
        self.by_id.insert(id.clone(), w);
        Some(w)
    }

    fn attempt(&mut self, id: &AttemptId) -> Option<u32> {
        if let Some(&a) = self.attempts.get(id) {
            return Some(a);
        }
        if !self.room(1) {
            return None;
        }
        let a = u32::try_from(self.ender.len()).ok()?;
        self.ender.push(None);
        self.attempts.insert(id.clone(), a);
        Some(a)
    }

    /// Note `line`, read from candidate file `file` (`None`: a file the round
    /// keeps, which pins its work).
    fn note(&mut self, line: &Line, file: Option<u32>) {
        let Some(w) = self.work(line.event.work()) else {
            return;
        };
        let facts = &mut self.works[w as usize];
        facts.last_ts = facts.last_ts.max(line.ts_ms);
        match file {
            None => facts.pinned = true,
            Some(file) => {
                if facts.files.last() != Some(&file) {
                    if !self.room(1) {
                        return;
                    }
                    self.works[w as usize].files.push(file);
                }
            }
        }
        match &line.event {
            Event::Opened { parent, launch, .. } => {
                if *launch == Launch::Delegation {
                    self.works[w as usize].delegation = true;
                }
                if let Some(parent) = parent
                    && self.works[w as usize].parent.is_none()
                    && let Some(p) = self.work(parent)
                {
                    self.works[w as usize].parent = Some(p);
                }
            }
            Event::Closed { .. } => self.works[w as usize].closed = true,
            Event::AttemptStarted { attempt, .. } => {
                if self.live.contains(&line.run) {
                    self.works[w as usize].live_attempt = true;
                }
                if let Some(a) = self.attempt(attempt)
                    && !self.works[w as usize].started.contains(&a)
                    && self.room(1)
                {
                    self.works[w as usize].started.push(a);
                }
            }
            Event::AttemptEnded { attempt, .. } => {
                if let Some(a) = self.attempt(attempt)
                    && self.ender[a as usize].is_none()
                {
                    self.ender[a as usize] = Some(w);
                }
            }
            _ => {}
        }
    }

    /// The works retention lets go, oldest first: **finished** ones (none of
    /// their lines in a file the round keeps, every attempt they started
    /// ended — whether or not they were `closed`: a pane work whose daemon
    /// went away is never closed, but recovery ends its attempt, and it must
    /// not be kept forever) older than the age limit or beyond the count
    /// limit, less those a kept work needs ([`Self::protect`]). A work a
    /// live run started an attempt of and has not closed is not finished,
    /// unless it is a delegation: the run may still serve it (an exited
    /// pane can be restarted into its work), and it becomes finished once
    /// that run closes it or is gone.
    fn eligible(&self, now_ms: u64, retention: Retention) -> Vec<u32> {
        let max_age_ms = u64::try_from(retention.max_age.as_millis()).unwrap_or(u64::MAX);
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let mut finished: Vec<u32> = (0..self.works.len())
            .filter_map(|w| u32::try_from(w).ok())
            .filter(|w| {
                let f = &self.works[*w as usize];
                !f.pinned
                    && !f.files.is_empty()
                    && f.started.iter().all(|a| self.ender[*a as usize].is_some())
                    && (f.closed || f.delegation || !f.live_attempt)
            })
            .collect();
        // Newest first, to rank them.
        finished.sort_by(|a, b| {
            let (a, b) = (&self.works[*a as usize], &self.works[*b as usize]);
            b.last_ts.cmp(&a.last_ts).then_with(|| a.id.cmp(&b.id))
        });
        let mut eligible: Vec<u32> = finished
            .iter()
            .enumerate()
            .filter(|(rank, w)| {
                self.works[**w as usize].last_ts < cutoff || *rank >= retention.max_finished_works
            })
            .map(|(_, w)| *w)
            .collect();
        eligible.reverse();
        let mut drop = vec![false; self.works.len()];
        for &w in &eligible {
            drop[w as usize] = true;
        }
        self.protect(&mut drop);
        eligible.retain(|w| drop[*w as usize]);
        eligible
    }

    /// What a work needs kept while it is kept: its parent, and every work
    /// whose line ends an attempt it started (dropping that line would leave
    /// it unfinished for good).
    fn needs(&self, w: u32) -> impl Iterator<Item = u32> + '_ {
        let facts = &self.works[w as usize];
        facts
            .parent
            .into_iter()
            .chain(facts.started.iter().filter_map(|a| self.ender[*a as usize]))
    }

    /// Clear `drop` for everything a kept work needs, transitively.
    fn protect(&self, drop: &mut [bool]) {
        let mut kept: Vec<u32> = (0..self.works.len())
            .filter(|w| !drop[*w])
            .filter_map(|w| u32::try_from(w).ok())
            .collect();
        while let Some(w) = kept.pop() {
            for need in self.needs(w) {
                if drop[need as usize] {
                    drop[need as usize] = false;
                    kept.push(need);
                }
            }
        }
    }

    /// For each work, the works that need it ([`Self::needs`], reversed).
    fn needers(&self) -> Vec<Vec<u32>> {
        let mut needers: Vec<Vec<u32>> = vec![Vec::new(); self.works.len()];
        for w in (0..self.works.len()).filter_map(|w| u32::try_from(w).ok()) {
            for need in self.needs(w) {
                if need != w && !needers[need as usize].contains(&w) {
                    needers[need as usize].push(w);
                }
            }
        }
        needers
    }

    /// The files of `work` and of every work that needs it, transitively: a
    /// work is dropped only in a round that drops all of those, so their
    /// files are picked together.
    fn group_files(&self, work: u32, needers: &[Vec<u32>]) -> Vec<u32> {
        let mut seen: HashSet<u32> = HashSet::from([work]);
        let mut stack = vec![work];
        let mut files: Vec<u32> = Vec::new();
        while let Some(w) = stack.pop() {
            for f in &self.works[w as usize].files {
                if !files.contains(f) {
                    files.push(*f);
                }
            }
            for n in &needers[w as usize] {
                if seen.insert(*n) {
                    stack.push(*n);
                }
            }
        }
        files
    }

    /// Which works this round drops: the `eligible` ones every file of which
    /// is `picked`, less what a work kept this round needs ([`Self::protect`]).
    fn dropped(&self, eligible: &[u32], picked: &[bool]) -> Vec<bool> {
        let mut dropped = vec![false; self.works.len()];
        for &w in eligible {
            if self.works[w as usize]
                .files
                .iter()
                .all(|f| picked[*f as usize])
            {
                dropped[w as usize] = true;
            }
        }
        self.protect(&mut dropped);
        dropped
    }
}

/// Compaction's output: new chunks of run `run`, each written to a temporary
/// name, synced and renamed to the next free chunk name when it is full.
struct ChunkWriter<'a> {
    dir: &'a TrustedDir,
    run: &'a str,
    max: u64,
    temp: String,
    current: Option<(BufWriter<File>, u64)>,
    next: u64,
    published: Vec<String>,
    #[cfg(test)]
    probe_fault: Option<io::ErrorKind>,
}

impl<'a> ChunkWriter<'a> {
    fn new(dir: &'a TrustedDir, run: &'a str, max: u64) -> Self {
        ChunkWriter {
            dir,
            run,
            max,
            temp: format!(".pending-compacted-{run}.jsonl"),
            current: None,
            next: 0,
            published: Vec::new(),
            #[cfg(test)]
            probe_fault: None,
        }
    }

    fn write(&mut self, raw: &[u8]) -> io::Result<()> {
        let add = raw.len() as u64 + 1;
        if let Some((_, len)) = &self.current
            && *len > 0
            && len.saturating_add(add) > self.max
        {
            self.finish()?;
        }
        if self.current.is_none() {
            let _ = self.dir.remove_file(&self.temp);
            let file = self.dir.create_new_owner_only(&self.temp)?;
            self.current = Some((BufWriter::new(file), 0));
        }
        if let Some((out, len)) = &mut self.current {
            out.write_all(raw)?;
            out.write_all(b"\n")?;
            *len += add;
        }
        Ok(())
    }

    /// Publish the chunk being written, if any, under the next chunk name no
    /// file has (the round holds the maintenance lock, so nobody else makes
    /// one meanwhile). A probe that cannot tell whether a name is free (an
    /// error other than NotFound) fails the round, which then abandons what it
    /// wrote, rather than probing names for ever under the lock.
    fn finish(&mut self) -> io::Result<()> {
        let Some((out, _)) = self.current.take() else {
            return Ok(());
        };
        let file = out.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.sync_all()?;
        let name = loop {
            let name = compacted_name_n(self.run, self.next);
            self.next += 1;
            match self.probe(&name) {
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::NotFound => break name,
                Err(error) => return Err(error),
            }
        };
        self.dir.rename(&self.temp, &name)?;
        self.published.push(name);
        Ok(())
    }

    /// Whether `name` exists.
    fn probe(&self, name: &str) -> io::Result<()> {
        #[cfg(test)]
        if let Some(kind) = self.probe_fault {
            return Err(io::Error::from(kind));
        }
        self.dir.entry(name).map(|_| ())
    }

    /// A round that failed: remove what it wrote, so its inputs (still in
    /// place) are not read twice.
    fn abandon(&mut self) {
        self.current = None;
        let _ = self.dir.remove_file(&self.temp);
        for name in self.published.drain(..) {
            let _ = self.dir.remove_file(&name);
        }
        let _ = self.dir.sync();
    }
}

#[cfg(test)]
#[path = "work_journal_tests.rs"]
mod tests;
