//! PRD #1666 part 3, round 5 (plan D2, D3, D14): where the daemon keeps a
//! worker's report, or a dispatch's result, as a file it owns, so a report it
//! acknowledged is never lost because the agent it was for went away, and
//! never handed to whoever took over that agent's pane.
//!
//! # Layout
//!
//! Everything is under `<state_dir>/retained-reports/`, each directory created
//! owner-only and held by a verified handle ([`crate::platform::trusted_dir`])
//! for the run:
//!
//! - `runs/run-<run>.lock`, created under a temporary name, locked and renamed
//!   into place, and held while the run lasts;
//! - `runs/run-<run>.json`, the run's inventory: the mutable state of its own
//!   files ([`Inventory`]), replaced whole by write, `fsync` and rename;
//! - `files/<run>/`, the run's own directory. **The directory a file is in is
//!   its owner.** `r-<report>.md` is a file this run wrote;
//!   `r-<report>.d<unix-secs>.<state>.md` is one it adopted from a run that
//!   ended, whose deadline and last state are fixed in its name;
//!   `w-<report>.<tag>.part` is a write under way: a report is written and
//!   `fsync`ed there, then given its own name by a rename that never
//!   replaces (or a link, where there is none), so the own name never holds
//!   a cut-off file. A takeover removes every such name it finds.
//!
//! # Ownership transfer
//!
//! A starting run tries each other run's lock. A held lock is a live run, whose
//! files are never opened. An acquired one is a dead run: each of its files is
//! moved into this run's directory by one no-replace rename between held
//! directories, under a name carrying its deadline, so a file is in exactly
//! one owned directory at every instant of a process crash. The dead run is
//! retired only once its directory is empty and both directories are synced.
//! A host crash's effect on a rename is the filesystem's; nothing here derives
//! a guarantee for it from POSIX.
//!
//! # Budget
//!
//! Owner-local admission (D2): this run's own files, the files it adopted and
//! its in-flight reservations must fit `max_bytes`, and its own files and
//! reservations `max_files`. Adopted files take bytes but no file slot. It is
//! not a global quota across live daemons sharing a state directory.
//!
//! # What it is not
//!
//! Owner-only modes keep other users out. They do not separate agents of this
//! user from each other, and a same-user process can write a file in the
//! envelope's format: the envelope is attribution, not authentication.
//!
//! Every method that takes the store's in-memory state uses
//! `crate::lock_rank`'s non-panicking `try_lock` (rank R5); on a poisoned lock
//! it answers [`StoreRefusal::Unavailable`] or does nothing.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::lock_rank::{R5, RankedMutex, Unavailable};
use crate::platform::trusted_dir::{EntryKind, TrustedDir};

/// The byte budget a run's own directory is admitted against
/// ([`ENV_MAX_BYTES`]).
pub const DEFAULT_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// The most own files a run keeps ([`ENV_MAX_FILES`]).
pub const DEFAULT_MAX_FILES: u32 = 4096;
/// The largest `max_files` a run may be configured with, and the largest an
/// inventory's header may name.
pub const HARD_MAX_FILES: u32 = 65_536;
/// Dispatch results held in memory when their file could not be written
/// ([`ENV_FALLBACK_SLOTS`]).
pub const DEFAULT_FALLBACK_SLOTS: u32 = 16;
/// The whole file of a deck-authored dispatch result, envelope included, and
/// what a dispatch reserves for it.
pub const DISPATCH_RESULT_MAX_FILE: u64 = 73_728;
/// The longest inventory line, newline excluded.
pub const INVENTORY_LINE_MAX: usize = 256;
/// The longest envelope line.
pub const ENVELOPE_MAX: usize = 2048;
/// How long a file is kept after its message was acknowledged.
pub const ACK_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a file is kept after it entered recovery, and after a takeover
/// for a file whose state was not settled.
pub const RECOVERY_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

pub const ENV_MAX_BYTES: &str = "DOT_AGENT_DECK_RETAINED_REPORT_MAX_BYTES";
pub const ENV_MAX_FILES: &str = "DOT_AGENT_DECK_RETAINED_REPORT_MAX_FILES";
pub const ENV_FALLBACK_SLOTS: &str = "DOT_AGENT_DECK_DISPATCH_FALLBACK_SLOTS";

/// The first line of every retained file.
pub const MAGIC: &str = "dot-agent-deck retained report v1";

const ROOT: &str = "retained-reports";
const FILES: &str = "files";
const RUNS: &str = "runs";
const INVENTORY_VERSION: u32 = 1;

/// The largest file adoption accepts: a report at its limit, framed (the frame
/// can lengthen it), plus its envelope. Anything larger is left in place.
const MAX_ADOPTABLE_FILE: u64 = 2 * crate::bounded_read::MAX_TASK_BYTES + 4 * 1024;

/// Wall-clock seconds and a monotonic instant, read together. Persisted times
/// are wall seconds; deadlines inside one run are monotonic, so a wall clock
/// that jumps backwards cannot shorten one.
#[derive(Debug, Clone, Copy)]
pub struct Now {
    pub wall: i64,
    pub mono: Instant,
}

impl Now {
    pub fn current() -> Self {
        Now {
            wall: chrono::Utc::now().timestamp(),
            mono: Instant::now(),
        }
    }

    /// `self`, `by` later.
    pub fn plus(self, by: Duration) -> Self {
        Now {
            wall: self.wall.saturating_add(by.as_secs() as i64),
            mono: self.mono + by,
        }
    }
}

/// Where a retained report, or a dispatch result held in memory in its place,
/// is in its lifetime (D3), with when that started. The rules are the own
/// file's: an acknowledgement starts 24 hours from any state; recovery starts
/// seven days only from `Referenced`.
#[derive(Debug, Clone, Copy)]
pub enum Lifetime {
    /// Referenced by a claim, a ledger entry or a listed row: kept.
    Referenced,
    Acknowledged(Now),
    Recovered(Now),
}

impl Lifetime {
    /// Its message was acknowledged at `now`.
    pub fn acknowledge(&mut self, now: Now) {
        *self = Lifetime::Acknowledged(now);
    }

    /// It entered recovery at `now`, if it was still referenced.
    pub fn recover(&mut self, now: Now) {
        if matches!(self, Lifetime::Referenced) {
            *self = Lifetime::Recovered(now);
        }
    }

    /// When it ends, or `None` while it is referenced.
    pub fn ends(&self) -> Option<Instant> {
        match self {
            Lifetime::Referenced => None,
            Lifetime::Acknowledged(at) => Some(at.mono + ACK_RETENTION),
            Lifetime::Recovered(at) => Some(at.mono + RECOVERY_RETENTION),
        }
    }
}

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// A report id: 1 to 64 of `[0-9a-z-]`.
pub fn valid_report_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase() || b == b'-')
}

/// A fresh report id.
pub fn mint_report_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("OS randomness is unavailable; cannot mint a report id");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The name of a file this run wrote: `r-<report>.md`.
pub fn own_name(report_id: &str) -> String {
    format!("r-{report_id}.md")
}

/// The report id of an own file's name, which carries no suffix.
pub fn parse_own_name(name: &str) -> Option<&str> {
    let id = name.strip_prefix("r-")?.strip_suffix(".md")?;
    valid_report_id(id).then_some(id)
}

/// The name a report is written under before it is linked to its own
/// name: `w-<report>.<16 hex>.part`, fresh for every attempt. It is never a
/// report: a takeover removes it wherever it finds one.
fn temp_name(report_id: &str) -> io::Result<String> {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    let tag: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("w-{report_id}.{tag}.part"))
}

/// Whether `name` is an attempt's temporary name ([`temp_name`]).
fn is_temp_name(name: &str) -> bool {
    name.strip_prefix("w-")
        .and_then(|rest| rest.strip_suffix(".part"))
        .and_then(|rest| rest.rsplit_once('.'))
        .is_some_and(|(id, tag)| {
            valid_report_id(id) && tag.len() == 16 && tag.bytes().all(|b| b.is_ascii_hexdigit())
        })
}

/// The recorded state an adopted file's name carries (D14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptedState {
    /// `a`: its message was acknowledged.
    Acknowledged,
    /// `p`: pinned when its run ended.
    Pinned,
    /// `r`: in recovery.
    Recovered,
    /// `u`: not yet in its run's inventory when the run ended.
    Unlisted,
    /// `o`: an attempt whose removal failed; never a report.
    Orphaned,
}

impl AdoptedState {
    pub fn letter(self) -> char {
        match self {
            AdoptedState::Acknowledged => 'a',
            AdoptedState::Pinned => 'p',
            AdoptedState::Recovered => 'r',
            AdoptedState::Unlisted => 'u',
            AdoptedState::Orphaned => 'o',
        }
    }

    pub fn from_letter(letter: char) -> Option<Self> {
        Some(match letter {
            'a' => AdoptedState::Acknowledged,
            'p' => AdoptedState::Pinned,
            'r' => AdoptedState::Recovered,
            'u' => AdoptedState::Unlisted,
            'o' => AdoptedState::Orphaned,
            _ => return None,
        })
    }
}

/// The name of an adopted file: `r-<report>.d<deadline>.<state>.md`.
pub fn adopted_name(report_id: &str, deadline: i64, state: AdoptedState) -> String {
    format!("r-{report_id}.d{deadline}.{}.md", state.letter())
}

/// An adopted file's report id, deadline and state, from its name: the
/// deadline all decimal digits, at most 12 of them, then exactly one state
/// letter.
pub fn parse_adopted_name(name: &str) -> Option<(&str, i64, AdoptedState)> {
    let rest = name.strip_prefix("r-")?.strip_suffix(".md")?;
    let (head, letter) = rest.rsplit_once('.')?;
    let mut letters = letter.chars();
    let state = AdoptedState::from_letter(letters.next()?)?;
    if letters.next().is_some() {
        return None;
    }
    let (id, deadline) = head.rsplit_once(".d")?;
    if deadline.is_empty() || deadline.len() > 12 || !deadline.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if !valid_report_id(id) {
        return None;
    }
    Some((id, deadline.parse().ok()?, state))
}

fn valid_run_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn mint_run_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn lock_name(run: &str) -> String {
    format!("run-{run}.lock")
}

fn inventory_name(run: &str) -> String {
    format!("run-{run}.json")
}

fn run_of_lock_name(name: &str) -> Option<&str> {
    let run = name.strip_prefix("run-")?.strip_suffix(".lock")?;
    valid_run_id(run).then_some(run)
}

// ---------------------------------------------------------------------------
// The envelope
// ---------------------------------------------------------------------------

/// What a retained file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    WorkerReport,
    DispatchStarted,
    DispatchFailed,
    DispatchFinished,
}

/// The strings in an envelope a producer influenced: quoted, normalised and
/// capped where they are written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerSupplied {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub producer_pane_id: String,
    pub recipient_pane_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<String>,
}

/// A retained file's second line: attribution written by the daemon (D3).
/// Every field outside [`ProducerSupplied`] is deck-authored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub report_id: String,
    pub kind: ArtifactKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<String>,
    pub deck_id: String,
    pub run_id: String,
    pub created_at: String,
    pub producer_agent_id: String,
    pub recipient_agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub producer_supplied: ProducerSupplied,
    /// The `producer_supplied` fields that were cut to fit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated: Vec<String>,
}

/// Every `producer_supplied` string is cut to this many characters.
const PRODUCER_FIELD_CHARS: usize = 256;

/// A JSON formatter that writes every non-ASCII character as an escape, so
/// the line holds no raw newline, control or bidi mark.
struct AsciiFormatter;

impl serde_json::ser::Formatter for AsciiFormatter {
    fn write_string_fragment<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        for c in fragment.chars() {
            if c.is_ascii() {
                writer.write_all(&[c as u8])?;
            } else {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    write!(writer, "\\u{unit:04x}")?;
                }
            }
        }
        Ok(())
    }
}

fn cut_chars(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (text[..at].to_string(), true),
        None => (text.to_string(), false),
    }
}

impl Envelope {
    /// Normalise and cap every producer-supplied string, then serialise as
    /// one ASCII line of at most [`ENVELOPE_MAX`] bytes, cutting the longest
    /// producer fields further while it is over.
    pub fn to_line(&self) -> String {
        let mut env = self.clone();
        let mut truncated: Vec<String> = Vec::new();
        {
            let ps = &mut env.producer_supplied;
            let mut fields: Vec<(&str, &mut String)> = vec![
                ("producer_pane_id", &mut ps.producer_pane_id),
                ("recipient_pane_id", &mut ps.recipient_pane_id),
            ];
            if let Some(v) = ps.unit_name.as_mut() {
                fields.push(("unit_name", v));
            }
            if let Some(v) = ps.role.as_mut() {
                fields.push(("role", v));
            }
            if let Some(v) = ps.orchestration.as_mut() {
                fields.push(("orchestration", v));
            }
            for (name, value) in fields {
                let normal = crate::state::normalise_untrusted_text(value);
                let (kept, cut) = cut_chars(&normal, PRODUCER_FIELD_CHARS);
                *value = kept;
                if cut {
                    truncated.push(name.to_string());
                }
            }
        }
        loop {
            env.truncated = truncated.clone();
            env.truncated.sort();
            env.truncated.dedup();
            let line = env.serialise();
            if line.len() <= ENVELOPE_MAX {
                return line;
            }
            // Cut the longest producer field by a quarter and try again.
            let ps = &mut env.producer_supplied;
            let mut longest: Option<(&str, &mut String)> = None;
            for (name, value) in [
                ("producer_pane_id", Some(&mut ps.producer_pane_id)),
                ("recipient_pane_id", Some(&mut ps.recipient_pane_id)),
                ("unit_name", ps.unit_name.as_mut()),
                ("role", ps.role.as_mut()),
                ("orchestration", ps.orchestration.as_mut()),
            ] {
                if let Some(value) = value
                    && longest
                        .as_ref()
                        .is_none_or(|(_, l)| value.chars().count() > l.chars().count())
                {
                    longest = Some((name, value));
                }
            }
            let Some((name, value)) = longest else {
                break;
            };
            let chars = value.chars().count();
            if chars == 0 {
                break;
            }
            let (kept, _) = cut_chars(value, chars - chars.div_ceil(4));
            *value = kept;
            truncated.push(name.to_string());
        }
        // Only deck-authored fields are left over the bound: a bug.
        let line = env.serialise();
        debug_assert!(line.len() <= ENVELOPE_MAX, "envelope over its bound");
        line
    }

    fn serialise(&self) -> String {
        let mut out = Vec::new();
        let mut ser = serde_json::Serializer::with_formatter(&mut out, AsciiFormatter);
        // Serialising plain strings and enums cannot fail.
        if self.serialize(&mut ser).is_err() {
            return String::from("{}");
        }
        String::from_utf8(out).unwrap_or_else(|_| String::from("{}"))
    }
}

/// A retained file's whole contents: the magic line, the envelope, a blank
/// line, then `framed_body`, which the caller framed
/// (`state::frame_untrusted_report_for_file`).
pub fn compose_file(envelope: &Envelope, framed_body: &str) -> Vec<u8> {
    format!("{MAGIC}\n{}\n\n{framed_body}", envelope.to_line()).into_bytes()
}

/// The note appended to a dispatch result cut to fit its file.
pub const DISPATCH_CUT_NOTE: &str = "[the deck cut this result to fit its saved-results limit]";

/// A deck-authored dispatch result's whole file, at most
/// [`DISPATCH_RESULT_MAX_FILE`] bytes (plan §3.2): composed and measured, and
/// when it is over, the text is cut by the excess plus the note's length, at a
/// char boundary, the note appended, and the file composed again.
pub fn compose_dispatch_result(envelope: &Envelope, deck_text: &str) -> Vec<u8> {
    let limit = DISPATCH_RESULT_MAX_FILE as usize;
    let whole = compose_file(
        envelope,
        &crate::state::frame_untrusted_report_for_file(deck_text),
    );
    if whole.len() <= limit {
        return whole;
    }
    let mut keep = deck_text
        .len()
        .saturating_sub(whole.len() - limit)
        .saturating_sub(DISPATCH_CUT_NOTE.len() + 1);
    loop {
        while !deck_text.is_char_boundary(keep) {
            keep -= 1;
        }
        let text = format!("{}\n{DISPATCH_CUT_NOTE}", &deck_text[..keep]);
        let file = compose_file(
            envelope,
            &crate::state::frame_untrusted_report_for_file(&text),
        );
        if file.len() <= limit || keep == 0 {
            assert!(file.len() <= limit, "a dispatch result over its file limit");
            return file;
        }
        // Framing can lengthen the text: cut further by what is still over.
        keep = keep.saturating_sub(file.len() - limit);
    }
}

/// The attribution a retained file's first two lines give, and nothing from
/// after them: `None` when they do not parse as a magic line and an
/// envelope, which a reader shows as "unattributed".
pub fn read_attribution(mut reader: impl Read) -> Option<Envelope> {
    let mut lines = BoundedLines::new(&mut reader, ENVELOPE_MAX);
    let magic = lines.next_line().ok()??;
    if magic != MAGIC.as_bytes() {
        return None;
    }
    let envelope = lines.next_line().ok()??;
    serde_json::from_slice(&envelope).ok()
}

// ---------------------------------------------------------------------------
// The bounded line reader
// ---------------------------------------------------------------------------

/// Lines of at most `max` bytes, each refused before its buffer grows past
/// `max + 1` bytes.
struct BoundedLines<R> {
    inner: R,
    max: usize,
    chunk: [u8; 512],
    filled: usize,
    at: usize,
    /// The largest a line buffer ever got, for the reader's bound test.
    #[cfg(test)]
    peak: usize,
    ended: bool,
    /// Bytes after the last newline.
    trailing: bool,
}

#[derive(Debug)]
enum LineError {
    TooLong,
    Io(io::Error),
}

impl<R: Read> BoundedLines<R> {
    fn new(inner: R, max: usize) -> Self {
        BoundedLines {
            inner,
            max,
            chunk: [0; 512],
            filled: 0,
            at: 0,
            #[cfg(test)]
            peak: 0,
            ended: false,
            trailing: false,
        }
    }

    /// The next whole line, without its newline; `None` at the end. Bytes
    /// after the last newline are not a line: they set [`Self::trailing`].
    fn next_line(&mut self) -> Result<Option<Vec<u8>>, LineError> {
        let mut line: Vec<u8> = Vec::new();
        loop {
            if self.at == self.filled {
                if self.ended {
                    if !line.is_empty() {
                        self.trailing = true;
                    }
                    return Ok(None);
                }
                self.filled = loop {
                    match self.inner.read(&mut self.chunk) {
                        Ok(n) => break n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(LineError::Io(e)),
                    }
                };
                self.at = 0;
                if self.filled == 0 {
                    self.ended = true;
                    continue;
                }
            }
            let byte = self.chunk[self.at];
            self.at += 1;
            if byte == b'\n' {
                return Ok(Some(line));
            }
            if line.len() >= self.max {
                return Err(LineError::TooLong);
            }
            line.push(byte);
            #[cfg(test)]
            {
                self.peak = self.peak.max(line.len());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The inventory
// ---------------------------------------------------------------------------

/// An own file's recorded state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnState {
    /// Referenced: a claim, a ledger entry or a listed row holds it.
    Pinned,
    /// Its message was acknowledged at this wall time.
    Acknowledged(i64),
    /// It entered recovery at this wall time.
    Recovered(i64),
    /// An attempt whose removal failed; removal is retried.
    OrphanUnlink,
}

impl OwnState {
    fn word(self) -> &'static str {
        match self {
            OwnState::Pinned => "pinned",
            OwnState::Acknowledged(_) => "acked",
            OwnState::Recovered(_) => "recovered",
            OwnState::OrphanUnlink => "orphan_unlink",
        }
    }

    fn time(self) -> Option<i64> {
        match self {
            OwnState::Acknowledged(t) | OwnState::Recovered(t) => Some(t),
            _ => None,
        }
    }

    fn from_word(word: &str, time: Option<i64>) -> Option<Self> {
        Some(match (word, time) {
            ("pinned", None) => OwnState::Pinned,
            ("acked", Some(t)) => OwnState::Acknowledged(t),
            ("recovered", Some(t)) => OwnState::Recovered(t),
            ("orphan_unlink", None) => OwnState::OrphanUnlink,
            _ => return None,
        })
    }

    fn adopted_state(self) -> AdoptedState {
        match self {
            OwnState::Pinned => AdoptedState::Pinned,
            OwnState::Acknowledged(_) => AdoptedState::Acknowledged,
            OwnState::Recovered(_) => AdoptedState::Recovered,
            OwnState::OrphanUnlink => AdoptedState::Orphaned,
        }
    }

    /// The fixed deadline a takeover gives a file left in this state (D14
    /// step 2).
    fn takeover_deadline(self, now_wall: i64) -> i64 {
        match self {
            OwnState::Acknowledged(at) => at.saturating_add(secs(ACK_RETENTION)),
            OwnState::Recovered(at) => at.saturating_add(secs(RECOVERY_RETENTION)),
            OwnState::Pinned | OwnState::OrphanUnlink => {
                now_wall.saturating_add(secs(RECOVERY_RETENTION))
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct InventoryHeader {
    v: u32,
    run: String,
    max_files: u32,
    count: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InventoryLine {
    n: String,
    c: i64,
    s: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    t: Option<i64>,
}

/// One own file, as an inventory lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryEntry {
    pub name: String,
    pub created_at: i64,
    pub state: OwnState,
}

/// Write an inventory: a header, then one line per own file. `count` above
/// `max_files` is a bug: debug builds assert, and release writes the full
/// list and logs, rather than a list a successor would read as complete.
fn write_inventory_text(run: &str, max_files: u32, entries: &[InventoryEntry]) -> String {
    let count = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    debug_assert!(count <= max_files, "an inventory over its own max_files");
    if count > max_files {
        error!(
            count,
            max_files, "retained reports: the inventory lists more files than its cap"
        );
    }
    let header = InventoryHeader {
        v: INVENTORY_VERSION,
        run: run.to_string(),
        max_files,
        count,
    };
    let mut text = serde_json::to_string(&header).unwrap_or_default();
    text.push('\n');
    for entry in entries {
        let line = InventoryLine {
            n: entry.name.clone(),
            c: entry.created_at,
            s: entry.state.word().to_string(),
            t: entry.state.time(),
        };
        let line = serde_json::to_string(&line).unwrap_or_default();
        debug_assert!(
            line.len() <= INVENTORY_LINE_MAX,
            "an inventory line over its bound"
        );
        text.push_str(&line);
        text.push('\n');
    }
    text
}

/// Why an inventory could not be read: its unsuffixed files are then not
/// adoptable (fail closed).
#[derive(Debug, PartialEq, Eq)]
pub enum InventoryError {
    Unreadable(String),
    Invalid(&'static str),
}

/// Read an inventory, bounded by its own header (D14): a line over
/// [`INVENTORY_LINE_MAX`] is refused before it is allocated, the header must
/// name `run`, version 1 and `count <= max_files <= HARD_MAX_FILES`, exactly
/// `count` entries must follow with no trailing bytes, and every entry must be
/// a unique, valid unsuffixed own name with a known state.
fn read_inventory(
    reader: impl Read,
    run: &str,
) -> Result<(u32, Vec<InventoryEntry>), InventoryError> {
    let mut lines = BoundedLines::new(reader, INVENTORY_LINE_MAX);
    let read = |lines: &mut BoundedLines<_>| match lines.next_line() {
        Ok(line) => Ok(line),
        Err(LineError::TooLong) => Err(InventoryError::Invalid("a line over its bound")),
        Err(LineError::Io(e)) => Err(InventoryError::Unreadable(e.to_string())),
    };
    let header = read(&mut lines)?.ok_or(InventoryError::Invalid("no header"))?;
    let header: InventoryHeader =
        serde_json::from_slice(&header).map_err(|_| InventoryError::Invalid("a bad header"))?;
    if header.v != INVENTORY_VERSION {
        return Err(InventoryError::Invalid("an unknown version"));
    }
    if header.run != run {
        return Err(InventoryError::Invalid("another run's inventory"));
    }
    if header.max_files > HARD_MAX_FILES || header.count > header.max_files {
        return Err(InventoryError::Invalid("a count over its bound"));
    }
    let mut entries = Vec::with_capacity(header.count as usize);
    let mut seen = HashSet::new();
    for _ in 0..header.count {
        let line =
            read(&mut lines)?.ok_or(InventoryError::Invalid("fewer lines than its count"))?;
        let line: InventoryLine =
            serde_json::from_slice(&line).map_err(|_| InventoryError::Invalid("a bad entry"))?;
        if parse_own_name(&line.n).is_none() || !seen.insert(line.n.clone()) {
            return Err(InventoryError::Invalid("an invalid or repeated name"));
        }
        if line.c < 0 || line.t.is_some_and(|t| t < 0) {
            return Err(InventoryError::Invalid("a negative time"));
        }
        let state = OwnState::from_word(&line.s, line.t)
            .ok_or(InventoryError::Invalid("an unknown state"))?;
        entries.push(InventoryEntry {
            name: line.n,
            created_at: line.c,
            state,
        });
    }
    if read(&mut lines)?.is_some() || lines.trailing {
        return Err(InventoryError::Invalid("more than its count"));
    }
    Ok((header.max_files, entries))
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// A run's admission bounds (D2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    pub max_bytes: u64,
    pub max_files: u32,
    pub fallback_slots: u32,
}

impl Default for StoreConfig {
    fn default() -> Self {
        StoreConfig {
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: DEFAULT_MAX_FILES,
            fallback_slots: DEFAULT_FALLBACK_SLOTS,
        }
    }
}

impl StoreConfig {
    /// The defaults, each overridden by its environment variable. A file
    /// count outside 1..=[`HARD_MAX_FILES`] is clamped, with a warning.
    pub fn from_env() -> Self {
        let mut config = StoreConfig::default();
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        };
        if let Some(bytes) = read(ENV_MAX_BYTES) {
            config.max_bytes = bytes;
        }
        if let Some(files) = read(ENV_MAX_FILES) {
            let clamped = files.clamp(1, u64::from(HARD_MAX_FILES));
            if clamped != files {
                warn!(
                    variable = ENV_MAX_FILES,
                    requested = files,
                    used = clamped,
                    "retained reports: the file count was clamped"
                );
            }
            config.max_files = clamped as u32;
        }
        if let Some(slots) = read(ENV_FALLBACK_SLOTS) {
            config.fallback_slots = u32::try_from(slots).unwrap_or(u32::MAX);
        }
        config
    }
}

/// Why the store would not reserve room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreRefusal {
    /// The byte budget.
    Bytes { dir: PathBuf },
    /// The own-file count.
    Files { max: u32, dir: PathBuf },
    /// The store's directories were refused or could not be made durable.
    Unusable { dir: PathBuf, reason: String },
    /// Its in-memory state is poisoned by an earlier panic.
    Unavailable,
    /// Every dispatch-result slot is taken: `in_flight` by dispatches still
    /// starting, `unsaved` by results held in memory.
    Slots { in_flight: u32, unsaved: u32 },
}

/// Room reserved for one file: its bytes and one file slot. Plain data: it
/// is handed to [`RetainedStore::write`] or [`RetainedStore::release`].
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a reservation is returned with RetainedStore::release"]
pub struct Reservation {
    bytes: u64,
}

impl Reservation {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// A dispatch's reserved room: its file, and one fallback slot counted as in
/// flight until its result is saved or held in memory.
#[derive(Debug)]
#[must_use = "a dispatch reservation is settled with RetainedStore::dispatch_result"]
pub struct DispatchReservation {
    file: Reservation,
}

/// A dispatch result whose file could not be written: its file slot and
/// bytes stay reserved, and its fallback slot counts as unsaved, until a
/// retry saves it or its lifetime ends.
#[derive(Debug)]
#[must_use = "a held result is retried or ended"]
pub struct HeldResult {
    file: Reservation,
}

/// Why a write did not leave a saved file.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteFailure {
    /// The store cannot be used.
    Unusable,
    /// The write failed and the attempt's file was removed; the room was
    /// returned.
    Io(String),
    /// The write failed and a file it made could not be removed: it stays
    /// charged and its removal is retried. The own name keeps its slot; a
    /// temporary name takes none.
    IoOrphaned(String),
}

/// The counts a dispatch refusal is worded from, and a test reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreCounts {
    pub own_files: u32,
    pub own_bytes: u64,
    pub adopted_bytes: u64,
    pub reserved_bytes: u64,
    pub reserved_slots: u32,
    pub in_flight: u32,
    pub unsaved: u32,
    pub adopted_generation: u64,
    /// Attempts' temporary files whose removal failed ([`StoreState::stray`]).
    pub stray_files: u32,
    pub stray_bytes: u64,
}

#[derive(Debug, Clone)]
struct OwnFile {
    created_at: i64,
    bytes: u64,
    state: OwnState,
    /// When the state's retention started, in this run.
    since: Option<Instant>,
}

impl OwnFile {
    fn due(&self, now: Now) -> bool {
        match (self.state, self.since) {
            (OwnState::OrphanUnlink, _) => true,
            (OwnState::Acknowledged(_), Some(since)) => now.mono >= since + ACK_RETENTION,
            (OwnState::Recovered(_), Some(since)) => now.mono >= since + RECOVERY_RETENTION,
            _ => false,
        }
    }
}

#[derive(Debug)]
struct StoreState {
    config: StoreConfig,
    own: HashMap<String, OwnFile>,
    own_bytes: u64,
    adopted_bytes: u64,
    adopted_generation: u64,
    reserved_bytes: u64,
    reserved_slots: u32,
    in_flight: u32,
    unsaved: u32,
    revision: u64,
    /// Attempts' temporary files this run could not remove, by name, each
    /// charged its attempt's bytes until a sweep removes it. Like a file
    /// inherited for cleanup, one takes no file slot and is in no inventory:
    /// a run that ends with one leaves it to the next run's takeover, which
    /// removes every temporary name it finds.
    stray: HashMap<String, u64>,
}

impl StoreState {
    fn used_bytes(&self) -> u64 {
        self.own_bytes
            .saturating_add(self.adopted_bytes)
            .saturating_add(self.reserved_bytes)
            .saturating_add(self.stray_bytes())
    }

    fn stray_bytes(&self) -> u64 {
        self.stray
            .values()
            .fold(0u64, |sum, bytes| sum.saturating_add(*bytes))
    }

    fn used_slots(&self) -> u64 {
        self.own.len() as u64 + u64::from(self.reserved_slots)
    }

    /// Charge `bytes` adopted. Checked: an overflow logs and saturates.
    fn adopt(&mut self, bytes: u64) {
        self.adopted_bytes = self.adopted_bytes.checked_add(bytes).unwrap_or_else(|| {
            error!("retained reports: the adopted byte count overflowed; it saturates");
            u64::MAX
        });
        self.adopted_generation = self.adopted_generation.wrapping_add(1);
    }

    /// Release `bytes` adopted. Checked: an underflow logs and saturates.
    fn unadopt(&mut self, bytes: u64) {
        self.adopted_bytes = self.adopted_bytes.checked_sub(bytes).unwrap_or_else(|| {
            error!("retained reports: the adopted byte count underflowed; it saturates");
            0
        });
        self.adopted_generation = self.adopted_generation.wrapping_add(1);
    }

    fn release(&mut self, bytes: u64) {
        self.reserved_bytes = self.reserved_bytes.saturating_sub(bytes);
        self.reserved_slots = self.reserved_slots.saturating_sub(1);
    }

    fn drop_own(&mut self, name: &str) {
        if let Some(file) = self.own.remove(name) {
            self.own_bytes = self.own_bytes.saturating_sub(file.bytes);
            self.revision += 1;
        }
    }

    fn entries(&self) -> Vec<InventoryEntry> {
        let mut entries: Vec<InventoryEntry> = self
            .own
            .iter()
            .map(|(name, file)| InventoryEntry {
                name: name.clone(),
                created_at: file.created_at,
                state: file.state,
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        entries
    }
}

/// The verified directories a usable store holds for the run.
#[derive(Debug)]
struct Dirs {
    files: TrustedDir,
    runs: TrustedDir,
    mine: TrustedDir,
    _lock: std::fs::File,
}

/// What one sweep or takeover pass did, for its log line and its tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Files removed.
    pub removed: u32,
    /// Files moved in from dead runs.
    pub adopted: u32,
    /// Removals or moves that failed and stay for the next pass.
    pub failed: u32,
    /// Entries whose state the pass could not read.
    pub uncertain: u32,
    /// Whether the pass published a recount of the adopted bytes.
    pub recounted: bool,
    /// Dead runs retired.
    pub retired: u32,
}

/// The daemon's retained-report store (R5). See the module documentation.
pub struct RetainedStore {
    state: RankedMutex<{ R5 }, StoreState>,
    dirs: Option<Dirs>,
    unusable: Option<String>,
    /// `<state_dir>/retained-reports/files/<run>`, for messages.
    dir: PathBuf,
    run_id: String,
    /// Serialises takeover and every sweep: one writer of the adopted sum at
    /// a time. Never held with a ranked lock.
    sweep_lock: Mutex<()>,
    /// The inventory revision last written, held while one is written.
    written: Mutex<u64>,
    #[cfg(test)]
    seams: seams::Seams,
}

impl std::fmt::Debug for RetainedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetainedStore")
            .field("dir", &self.dir)
            .field("unusable", &self.unusable)
            .finish()
    }
}

/// The store's state, held: see [`RetainedStore::lock`].
pub struct StoreLock<'a> {
    st: crate::lock_rank::RankedGuard<'a, { R5 }, StoreState>,
    store: &'a RetainedStore,
}

impl StoreLock<'_> {
    /// See [`RetainedStore::acknowledged`].
    pub fn acknowledged(&mut self, report_id: &str, now: Now) {
        self.transition(report_id, now, |state| match state {
            OwnState::OrphanUnlink => None,
            _ => Some(OwnState::Acknowledged(now.wall)),
        });
    }

    /// See [`RetainedStore::recovered`].
    pub fn recovered(&mut self, report_id: &str, now: Now) {
        self.transition(report_id, now, |state| match state {
            OwnState::Pinned => Some(OwnState::Recovered(now.wall)),
            _ => None,
        });
    }

    /// See [`RetainedStore::continue_lifetime`].
    pub fn continue_lifetime(&mut self, report_id: &str, lifetime: Lifetime) {
        let (state, since) = match lifetime {
            Lifetime::Referenced => return,
            Lifetime::Acknowledged(at) => (OwnState::Acknowledged(at.wall), at.mono),
            Lifetime::Recovered(at) => (OwnState::Recovered(at.wall), at.mono),
        };
        if let Some(file) = self.st.own.get_mut(&own_name(report_id))
            && file.state != OwnState::OrphanUnlink
        {
            file.state = state;
            file.since = Some(since);
            self.st.revision += 1;
        }
    }

    /// See [`RetainedStore::reserve`].
    pub fn reserve(&mut self, bytes: u64) -> Result<Reservation, StoreRefusal> {
        let bytes = bytes.max(1);
        self.store.refuse_unusable()?;
        self.store.admit(&self.st, bytes)?;
        self.st.reserved_bytes += bytes;
        self.st.reserved_slots += 1;
        Ok(Reservation { bytes })
    }

    /// See [`RetainedStore::release`].
    pub fn release(&mut self, reservation: Reservation) {
        self.st.release(reservation.bytes);
    }

    fn transition(
        &mut self,
        report_id: &str,
        now: Now,
        to: impl FnOnce(OwnState) -> Option<OwnState>,
    ) {
        let changed = match self.st.own.get_mut(&own_name(report_id)) {
            Some(file) => match to(file.state) {
                Some(next) => {
                    file.state = next;
                    file.since = Some(now.mono);
                    true
                }
                None => false,
            },
            None => false,
        };
        if changed {
            self.st.revision += 1;
        }
    }
}

/// A write's reserved room while the write runs: an unwind before the write's
/// own accounting returns it, through `lock_in_drop`, which never panics. It
/// does not record a file the write already created. The caller's `discard`
/// knows only the own name, which an unwind before publication has not
/// taken: such an unwind leaves the attempt's temporary name, uncharged and
/// untracked by this run, for the next run's takeover to remove. One after
/// publication leaves the own name, for `discard`; one that cannot be removed
/// stays uncharged.
struct RoomOnUnwind<'a> {
    store: &'a RetainedStore,
    bytes: Option<u64>,
}

impl Drop for RoomOnUnwind<'_> {
    fn drop(&mut self) {
        if let Some(bytes) = self.bytes.take()
            && let Some(mut st) = self
                .store
                .state
                .lock_in_drop(crate::lock_rank::OnPoison::Skip)
        {
            st.release(bytes);
        }
    }
}

/// Whether `error` says the platform or filesystem has no no-replace rename.
fn unsupported_rename(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        matches!(
            error.raw_os_error(),
            Some(libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)
        )
    }
    #[cfg(not(unix))]
    {
        error.kind() == io::ErrorKind::Unsupported
    }
}

fn exists_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AlreadyExists
}

/// The error the test seams inject for a failed unlink or sync: `EIO` on
/// Unix; `libc` is a Unix-only dependency, so elsewhere an error of its own.
#[cfg(all(test, unix))]
fn injected_io_error() -> io::Error {
    io::Error::from_raw_os_error(libc::EIO)
}

#[cfg(all(test, not(unix)))]
fn injected_io_error() -> io::Error {
    io::Error::other("injected I/O error")
}

impl RetainedStore {
    /// Open this run's store under `state_dir` (an absolute path), take over
    /// every ended run's files, and sweep once. Never fails: a directory that
    /// is refused or cannot be made durable gives a store whose every
    /// reservation answers [`StoreRefusal::Unusable`], logged once here.
    pub fn open(state_dir: &Path, config: StoreConfig, now: Now) -> Arc<Self> {
        Self::open_with(state_dir, config, now, |_| {})
    }

    /// [`Self::open`], with a hook run on the store before its takeover (test
    /// seams are armed there).
    fn open_with(
        state_dir: &Path,
        config: StoreConfig,
        now: Now,
        before_takeover: impl FnOnce(&RetainedStore),
    ) -> Arc<Self> {
        let state = StoreState {
            config,
            own: HashMap::new(),
            own_bytes: 0,
            adopted_bytes: 0,
            adopted_generation: 0,
            reserved_bytes: 0,
            reserved_slots: 0,
            in_flight: 0,
            unsaved: 0,
            revision: 0,
            stray: HashMap::new(),
        };
        let mut store = RetainedStore {
            state: RankedMutex::new(state),
            dirs: None,
            unusable: None,
            dir: state_dir.join(ROOT).join(FILES),
            run_id: String::new(),
            sweep_lock: Mutex::new(()),
            written: Mutex::new(0),
            #[cfg(test)]
            seams: seams::Seams::default(),
        };
        before_takeover(&store);
        match store.open_dirs(state_dir) {
            Ok((dirs, run)) => {
                store.dir = state_dir.join(ROOT).join(FILES).join(&run);
                store.run_id = run;
                store.dirs = Some(dirs);
            }
            Err(error) => {
                error!(
                    dir = %store.dir.display(),
                    %error,
                    "retained reports: the store's directory cannot be used; every report the \
                     deck would keep is refused (report_save_failed) until the daemon restarts \
                     with a usable directory"
                );
                store.unusable = Some(error.to_string());
            }
        }
        let store = Arc::new(store);
        if store.dirs.is_some() {
            let report = store.take_over(now);
            if report != PassReport::default() {
                tracing::info!(?report, "retained reports: took over ended runs");
            }
            store.sweep(now);
        }
        store
    }

    fn open_dirs(&self, state_dir: &Path) -> io::Result<(Dirs, String)> {
        let anchor = TrustedDir::anchor(state_dir)?;
        let created = |dir: &TrustedDir, name: &str| dir.entry(name).is_err();
        let root_new = created(&anchor, ROOT);
        let root = anchor.child(ROOT, true)?;
        if root_new {
            self.sync(&anchor, "state")?;
        }
        let (files_new, runs_new) = (created(&root, FILES), created(&root, RUNS));
        let files = root.child(FILES, true)?;
        let runs = root.child(RUNS, true)?;
        if files_new || runs_new {
            self.sync(&root, "root")?;
        }
        let run = mint_run_id()?;
        let pending = format!(".pending-{run}");
        let lock = runs.create_new_owner_only(&pending)?;
        if let Err(error) = lock.try_lock() {
            let _ = runs.remove_file(&pending);
            return Err(io::Error::other(format!("could not lock the run: {error}")));
        }
        runs.rename(&pending, &lock_name(&run))?;
        self.sync(&runs, "runs")?;
        let mine = files.child(&run, true)?;
        self.sync(&files, "files")?;
        Ok((
            Dirs {
                files,
                runs,
                mine,
                _lock: lock,
            },
            run,
        ))
    }

    /// This run's directory, for messages.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Why the store cannot be used, if it cannot.
    pub fn unusable_reason(&self) -> Option<&str> {
        self.unusable.as_deref()
    }

    /// Test seam: every write fails, as a full or broken disk would, until
    /// it is switched off.
    #[cfg(test)]
    pub(crate) fn fail_writes_for_test(&self, fail: bool) {
        self.seams.fail_all_writes.store(fail, Ordering::SeqCst);
    }

    /// Test seam: every removal fails, as a broken disk would, until it is
    /// switched off.
    #[cfg(test)]
    pub(crate) fn fail_unlinks_for_test(&self, fail: bool) {
        self.seams.fail_unlink.store(fail, Ordering::SeqCst);
    }

    /// Whether a thread panicked holding the store's state.
    pub fn is_poisoned(&self) -> bool {
        self.state.is_poisoned()
    }

    /// TEST: poison the store's lock (R5), as a panic under it would.
    #[cfg(test)]
    pub(crate) fn poison_for_test(self: &Arc<Self>) {
        let store = Arc::clone(self);
        let _ = std::thread::spawn(move || {
            let _held = store.state.lock();
            panic!("poison the retained store (test)");
        })
        .join();
    }

    pub fn counts(&self) -> Option<StoreCounts> {
        let st = self.state.try_lock().ok()?;
        Some(StoreCounts {
            own_files: st.own.len() as u32,
            own_bytes: st.own_bytes,
            adopted_bytes: st.adopted_bytes,
            reserved_bytes: st.reserved_bytes,
            reserved_slots: st.reserved_slots,
            in_flight: st.in_flight,
            unsaved: st.unsaved,
            adopted_generation: st.adopted_generation,
            stray_files: st.stray.len() as u32,
            stray_bytes: st.stray_bytes(),
        })
    }

    fn refuse_unusable(&self) -> Result<(), StoreRefusal> {
        match &self.unusable {
            Some(reason) => Err(StoreRefusal::Unusable {
                dir: self.dir.clone(),
                reason: reason.clone(),
            }),
            None => Ok(()),
        }
    }

    fn admit(&self, st: &StoreState, bytes: u64) -> Result<(), StoreRefusal> {
        if st.used_bytes().saturating_add(bytes) > st.config.max_bytes {
            return Err(StoreRefusal::Bytes {
                dir: self.dir.clone(),
            });
        }
        if st.used_slots() + 1 > u64::from(st.config.max_files) {
            return Err(StoreRefusal::Files {
                max: st.config.max_files,
                dir: self.dir.clone(),
            });
        }
        Ok(())
    }

    /// Reserve `bytes` and one file slot, or refuse, consuming nothing. A
    /// reservation is at least one byte, so a stray, which is charged its
    /// write's reservation and takes no file slot, always charges at least
    /// one: the byte budget bounds how many there can be. No caller in the
    /// deck asks for less; a report's and a dispatch result's files are
    /// sized from nonzero bounds.
    pub fn reserve(&self, bytes: u64) -> Result<Reservation, StoreRefusal> {
        self.refuse_unusable()?;
        self.lock()
            .map_err(|_: Unavailable| StoreRefusal::Unavailable)?
            .reserve(bytes)
    }

    /// Give a reservation back unused.
    pub fn release(&self, reservation: Reservation) {
        if let Ok(mut st) = self.state.try_lock() {
            st.release(reservation.bytes);
        }
    }

    /// Write `contents` as `report_id`'s own file under `reservation`: written
    /// owner-only and exclusive under a fresh temporary name, `fsync`ed, then
    /// moved to its own name by a rename that never replaces (a link and an
    /// unlink of the temporary name where the filesystem has no such rename),
    /// and the directory synced. So the own name only ever holds a
    /// whole file, and a crash mid-write leaves a temporary name that the
    /// next takeover removes, never a cut-off report it would adopt. On
    /// success the file is pinned. On failure the attempt's files are removed
    /// and the room returned, or, when a removal fails, what stays is charged
    /// and its removal retried by the sweep: the own name as an orphan, a
    /// temporary name as a stray.
    pub fn write(
        &self,
        reservation: Reservation,
        report_id: &str,
        contents: &[u8],
        now: Now,
    ) -> Result<(), WriteFailure> {
        let Some(dirs) = &self.dirs else {
            self.release(reservation);
            return Err(WriteFailure::Unusable);
        };
        // Until the accounting below runs, an unwind returns the room, once.
        let mut room = RoomOnUnwind {
            store: self,
            bytes: Some(reservation.bytes),
        };
        debug_assert!(valid_report_id(report_id), "an invalid report id");
        debug_assert!(
            contents.len() as u64 <= reservation.bytes,
            "a file larger than its reservation"
        );
        let name = own_name(report_id);
        let mut temp = None;
        let mut linked = false;
        let mut stray = None;
        let written = (|| {
            let attempt = temp_name(report_id)?;
            let mut file = match dirs.mine.create_new_owner_only(&attempt) {
                Ok(file) => file,
                // Created, but neither made owner-only nor removed: the
                // cleanup below retries it, and keeps it as a stray.
                Err(error) if crate::platform::trusted_dir::left_behind(&error) => {
                    temp = Some(attempt);
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            temp = Some(attempt.clone());
            #[cfg(test)]
            if self.seams.fail_write.swap(false, Ordering::SeqCst)
                || self.seams.fail_all_writes.load(Ordering::SeqCst)
            {
                return Err(io::Error::other("injected write failure"));
            }
            #[cfg(test)]
            if self.seams.crash_at(seams::Crash::MidWrite) {
                file.write_all(&contents[..contents.len() / 2])?;
                return Err(io::Error::other(seams::CRASHED));
            }
            file.write_all(contents)?;
            file.sync_all()?;
            drop(file);
            match dirs.mine.rename_into(&attempt, &dirs.mine, &name) {
                Ok(()) => {
                    temp = None;
                    linked = true;
                }
                // No no-replace rename here: a link, which never replaces
                // either, then the temporary name, a second name for the
                // whole file, removed. One that cannot go is kept as a stray.
                Err(error) if unsupported_rename(&error) => {
                    dirs.mine.link(&attempt, &name)?;
                    linked = true;
                    #[cfg(test)]
                    if self.seams.crash_at(seams::Crash::AfterLink) {
                        return Err(io::Error::other(seams::CRASHED));
                    }
                    if self.unlink(&dirs.mine, &attempt).is_ok() {
                        temp = None;
                    } else {
                        stray = temp.take();
                    }
                }
                Err(error) => return Err(error),
            }
            self.sync(&dirs.mine, "mine")
        })();
        #[cfg(test)]
        if written
            .as_ref()
            .is_err_and(|error| error.to_string() == seams::CRASHED)
        {
            // A process that died here runs nothing more: its files stay as
            // they are, for the next run's takeover. Only the room returns
            // (the test's store lives on).
            return Err(WriteFailure::Io(seams::CRASHED.to_string()));
        }
        room.bytes = None;
        match written {
            Ok(()) => {
                if let Ok(mut st) = self.state.try_lock() {
                    // The reservation's slot moves to the file.
                    st.release(reservation.bytes);
                    let bytes = contents.len() as u64;
                    st.own.insert(
                        name,
                        OwnFile {
                            created_at: now.wall,
                            bytes,
                            state: OwnState::Pinned,
                            since: None,
                        },
                    );
                    st.own_bytes = st.own_bytes.saturating_add(bytes);
                    if let Some(stray) = stray {
                        st.stray.insert(stray, bytes);
                    }
                    st.revision += 1;
                }
                Ok(())
            }
            Err(error) => {
                room.bytes = Some(reservation.bytes);
                let mut stray = stray;
                if let Some(attempt) = temp
                    && self.unlink(&dirs.mine, &attempt).is_err()
                {
                    stray = Some(attempt);
                }
                let removed = !linked || self.unlink(&dirs.mine, &name).is_ok();
                room.bytes = None;
                let Ok(mut st) = self.state.try_lock() else {
                    return Err(WriteFailure::Io(error.to_string()));
                };
                st.release(reservation.bytes);
                let orphaned = !removed || stray.is_some();
                if let Some(stray) = stray {
                    st.stray.insert(stray, reservation.bytes);
                }
                if !removed {
                    st.own.insert(
                        name,
                        OwnFile {
                            created_at: now.wall,
                            bytes: reservation.bytes,
                            state: OwnState::OrphanUnlink,
                            since: None,
                        },
                    );
                    st.own_bytes = st.own_bytes.saturating_add(reservation.bytes);
                    st.revision += 1;
                }
                if orphaned {
                    Err(WriteFailure::IoOrphaned(error.to_string()))
                } else {
                    Err(WriteFailure::Io(error.to_string()))
                }
            }
        }
    }

    /// A reservation dropped unused from a destructor or an unwind: its room
    /// is returned through `lock_in_drop`, which never panics.
    pub fn release_in_drop(&self, reservation: Reservation) {
        if let Some(mut st) = self.state.lock_in_drop(crate::lock_rank::OnPoison::Skip) {
            st.release(reservation.bytes);
        }
    }

    /// Remove `report_id`'s own file, written and not published (its save
    /// failed after the write, or its save task stopped before staging it),
    /// and give its room back. A name with no file is left alone. When the removal
    /// fails, the file stays charged as an orphan whose removal the sweep
    /// retries, as a failed write's does.
    pub fn discard(&self, report_id: &str) {
        let Some(dirs) = &self.dirs else {
            return;
        };
        let name = own_name(report_id);
        let removed = match self.unlink(&dirs.mine, &name) {
            Ok(()) => true,
            Err(error) => error.kind() == io::ErrorKind::NotFound,
        };
        if let Ok(mut st) = self.state.try_lock() {
            if removed {
                st.drop_own(&name);
            } else if let Some(file) = st.own.get_mut(&name) {
                file.state = OwnState::OrphanUnlink;
                st.revision += 1;
            }
        }
    }

    /// `report_id`'s message was acknowledged: kept [`ACK_RETENTION`] more.
    pub fn acknowledged(&self, report_id: &str, now: Now) {
        if let Ok(mut held) = self.lock() {
            held.acknowledged(report_id, now);
        }
    }

    /// `report_id` entered recovery: kept [`RECOVERY_RETENTION`] from now, if
    /// it was not already in recovery or acknowledged.
    pub fn recovered(&self, report_id: &str, now: Now) {
        if let Ok(mut held) = self.lock() {
            held.recovered(report_id, now);
        }
    }

    /// A held result that a retry just saved as `report_id`'s own file takes
    /// the lifetime its body had in memory, from when that started, so a
    /// retry never renews it.
    pub fn continue_lifetime(&self, report_id: &str, lifetime: Lifetime) {
        if let Ok(mut held) = self.lock() {
            held.continue_lifetime(report_id, lifetime);
        }
    }

    /// The store's state, held (R5), for code that does several things under
    /// one acquisition, or already holds it and must not take it again.
    pub fn lock(&self) -> Result<StoreLock<'_>, Unavailable> {
        Ok(StoreLock {
            st: self.state.try_lock()?,
            store: self,
        })
    }

    /// The state the store holds for `report_id`'s own file.
    pub fn own_state(&self, report_id: &str) -> Option<OwnState> {
        let st = self.state.try_lock().ok()?;
        st.own.get(&own_name(report_id)).map(|file| file.state)
    }

    // -- dispatch results (plan §3.2) --------------------------------------

    /// Reserve a dispatch result's whole file, a file slot and a fallback
    /// slot, before the dispatch starts; or refuse, consuming nothing.
    pub fn reserve_dispatch(&self) -> Result<DispatchReservation, StoreRefusal> {
        self.refuse_unusable()?;
        let mut st = self
            .state
            .try_lock()
            .map_err(|_: Unavailable| StoreRefusal::Unavailable)?;
        if st.in_flight + st.unsaved >= st.config.fallback_slots {
            return Err(StoreRefusal::Slots {
                in_flight: st.in_flight,
                unsaved: st.unsaved,
            });
        }
        self.admit(&st, DISPATCH_RESULT_MAX_FILE)?;
        st.reserved_bytes += DISPATCH_RESULT_MAX_FILE;
        st.reserved_slots += 1;
        st.in_flight += 1;
        Ok(DispatchReservation {
            file: Reservation {
                bytes: DISPATCH_RESULT_MAX_FILE,
            },
        })
    }

    /// Save a dispatch's result. Saved, its fallback slot is free. Not saved,
    /// the result is held: its slot counts as unsaved, and its file room stays
    /// reserved for a retry.
    pub fn dispatch_result(
        &self,
        reservation: DispatchReservation,
        report_id: &str,
        contents: &[u8],
        now: Now,
    ) -> Result<(), HeldResult> {
        let file = Reservation {
            bytes: reservation.file.bytes,
        };
        let held = self.save_held(file, report_id, contents, now);
        if let Ok(mut st) = self.state.try_lock() {
            st.in_flight = st.in_flight.saturating_sub(1);
            if held.is_err() {
                st.unsaved += 1;
            }
        }
        held
    }

    /// A dispatch reservation dropped unused — the dispatch never reached its
    /// result — from a destructor: its room and slot are freed, through
    /// `lock_in_drop`, which never panics.
    pub fn release_dispatch_in_drop(&self, reservation: DispatchReservation) {
        if let Some(mut st) = self.state.lock_in_drop(crate::lock_rank::OnPoison::Skip) {
            st.release(reservation.file.bytes);
            st.in_flight = st.in_flight.saturating_sub(1);
        }
    }

    /// Retry saving a held result. Saved, its fallback slot is free.
    pub fn retry_held(
        &self,
        held: HeldResult,
        report_id: &str,
        contents: &[u8],
        now: Now,
    ) -> Result<(), HeldResult> {
        let result = self.save_held(held.file, report_id, contents, now);
        if result.is_ok()
            && let Ok(mut st) = self.state.try_lock()
        {
            st.unsaved = st.unsaved.saturating_sub(1);
        }
        result
    }

    /// A held result's lifetime ended unsaved: its room and slot are freed.
    pub fn end_held(&self, held: HeldResult) {
        if let Ok(mut st) = self.state.try_lock() {
            st.release(held.file.bytes);
            st.unsaved = st.unsaved.saturating_sub(1);
        }
    }

    /// Write with `file`'s room, keeping the room reserved on failure.
    fn save_held(
        &self,
        file: Reservation,
        report_id: &str,
        contents: &[u8],
        now: Now,
    ) -> Result<(), HeldResult> {
        let bytes = file.bytes;
        match self.write(file, report_id, contents, now) {
            Ok(()) => Ok(()),
            Err(_) => {
                // `write` returned the room; take it back for the held body.
                // A store that refuses even that is out of room: the body is
                // still held in memory, uncharged.
                if let Ok(mut st) = self.state.try_lock() {
                    st.reserved_bytes += bytes;
                    st.reserved_slots += 1;
                }
                Err(HeldResult {
                    file: Reservation { bytes },
                })
            }
        }
    }

    // -- the inventory ------------------------------------------------------

    /// Write this run's inventory if it changed since the last write. The
    /// text is snapshotted under the store's lock and written off it, whole,
    /// by `fsync` and rename; a snapshot not newer than the one already
    /// written is skipped, so an older one never replaces a newer one.
    pub fn flush_inventory(&self) -> io::Result<bool> {
        let (revision, text) = {
            let Ok(st) = self.state.try_lock() else {
                return Ok(false);
            };
            let entries = st.entries();
            (
                st.revision,
                write_inventory_text(&self.run_id, st.config.max_files, &entries),
            )
        };
        self.write_inventory(revision, &text)
    }

    fn write_inventory(&self, revision: u64, text: &str) -> io::Result<bool> {
        let Some(dirs) = &self.dirs else {
            return Ok(false);
        };
        let mut written = self.written.lock().unwrap_or_else(|p| p.into_inner());
        if revision <= *written && *written != 0 {
            return Ok(false);
        }
        let pending = format!(".pending-{}.json", self.run_id);
        let _ = dirs.runs.remove_file(&pending);
        let result = (|| {
            let mut file = dirs.runs.create_new_owner_only(&pending)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            dirs.runs.rename(&pending, &inventory_name(&self.run_id))?;
            dirs.runs.sync()
        })();
        if result.is_err() {
            let _ = dirs.runs.remove_file(&pending);
        }
        result?;
        *written = revision;
        Ok(true)
    }

    // -- takeover (D14 "The transfer") -------------------------------------

    /// Take over every ended run: see the module documentation.
    fn take_over(&self, now: Now) -> PassReport {
        let mut report = PassReport::default();
        let Some(dirs) = &self.dirs else {
            return report;
        };
        let _serial = self.sweep_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut runs = Vec::new();
        match dirs.runs.stream() {
            Ok(mut stream) => {
                while let Some(name) = stream.next_name() {
                    let Ok(name) = name else {
                        break;
                    };
                    if let Some(run) = name.to_str().and_then(run_of_lock_name)
                        && run != self.run_id
                    {
                        runs.push(run.to_string());
                    }
                }
            }
            Err(error) => {
                warn!(%error, "retained reports: could not list the runs to take over");
                return report;
            }
        }
        for run in runs {
            let Ok(lock) = dirs.runs.open_regular(lock_name(&run)) else {
                continue;
            };
            if lock.try_lock().is_err() {
                // A live run: its files are never opened, at any age.
                continue;
            }
            #[cfg(test)]
            self.seams.note_taken(&run);
            self.transfer(dirs, &run, now, &mut report);
        }
        report
    }

    /// Move run `run`'s files here, under its held lock (D14 steps 1–7).
    fn transfer(&self, dirs: &Dirs, run: &str, now: Now, report: &mut PassReport) {
        // 1. Its inventory: missing reads as empty; unreadable or invalid
        //    leaves its unsuffixed files unadoptable.
        let listed: Option<HashMap<String, InventoryEntry>> = match dirs
            .runs
            .open_regular(inventory_name(run))
        {
            Ok(file) => match read_inventory(file, run) {
                Ok((_, entries)) => Some(
                    entries
                        .into_iter()
                        .map(|entry| (entry.name.clone(), entry))
                        .collect(),
                ),
                Err(why) => {
                    warn!(
                        run,
                        ?why,
                        "retained reports: an ended run's inventory cannot be used; its own files are left in place"
                    );
                    None
                }
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(HashMap::new()),
            Err(error) => {
                warn!(run, %error, "retained reports: an ended run's inventory cannot be read; its own files are left in place");
                None
            }
        };
        let theirs = match dirs.files.child(run, false) {
            Ok(dir) => Some(dir),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                warn!(run, %error, "retained reports: an ended run's directory cannot be used");
                return;
            }
        };
        let mut left = 0u32;
        let mut stopped = false;
        let mut synced = true;
        if let Some(theirs) = &theirs {
            let names = match collect_names(theirs) {
                Ok(names) => names,
                Err(error) => {
                    warn!(run, %error, "retained reports: could not list an ended run's files");
                    return;
                }
            };
            for name in names {
                if stopped {
                    left += 1;
                    continue;
                }
                #[cfg(test)]
                if self.seams.stop_transfer(report.adopted + report.removed) {
                    stopped = true;
                    left += 1;
                    continue;
                }
                let Some(name) = name.to_str() else {
                    left += 1;
                    continue;
                };
                // An attempt's temporary name is never a report: a write that
                // died before linking it left it cut off, and one that died
                // after left a second name for a file adopted by its own.
                if is_temp_name(name) {
                    match self.unlink(theirs, name) {
                        Ok(()) => report.removed += 1,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => {
                            warn!(run, %error, "retained reports: could not remove an ended run's unfinished write; it is tried again at the next start");
                            report.failed += 1;
                            left += 1;
                        }
                    }
                    continue;
                }
                // 2. Its fixed deadline and state letter.
                let decided = if let Some((id, deadline, state)) = parse_adopted_name(name) {
                    Some((id.to_string(), deadline, state))
                } else if let Some(id) = parse_own_name(name) {
                    listed.as_ref().map(|listed| match listed.get(name) {
                        Some(entry) => (
                            id.to_string(),
                            entry.state.takeover_deadline(now.wall),
                            entry.state.adopted_state(),
                        ),
                        None => (
                            id.to_string(),
                            now.wall.saturating_add(secs(RECOVERY_RETENTION)),
                            AdoptedState::Unlisted,
                        ),
                    })
                } else {
                    None
                };
                let Some((id, deadline, state)) = decided else {
                    warn!(run, name = %crate::config_validation::escape_id_for_log(name), "retained reports: left a file in an ended run's directory that cannot be adopted");
                    left += 1;
                    continue;
                };
                // 3. A regular file, within the largest a report can be.
                let size = match self.fstat(theirs, name) {
                    Ok(info) if info.kind == EntryKind::File && info.len <= MAX_ADOPTABLE_FILE => {
                        info.len
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    _ => {
                        warn!(
                            run,
                            "retained reports: left an entry that is not a report file in an ended run's directory"
                        );
                        left += 1;
                        continue;
                    }
                };
                let target = adopted_name(&id, deadline, state);
                // 4. Past its deadline: removed where it is, or taken over for
                //    cleanup when that fails.
                if deadline <= now.wall {
                    match self.unlink(theirs, name) {
                        Ok(()) => {
                            report.removed += 1;
                            continue;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                        Err(error) => {
                            warn!(run, %error, "retained reports: could not remove an expired file of an ended run; taking it over to retry");
                        }
                    }
                }
                // 5. The no-replace move.
                match self.move_in(theirs, name, &dirs.mine, &target) {
                    Ok(()) => {
                        if let Ok(mut st) = self.state.try_lock() {
                            st.adopt(size);
                        }
                        report.adopted += 1;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) if exists_error(&error) => {
                        warn!(
                            run,
                            "retained reports: a report id collides with one already here; the file is left in place"
                        );
                        left += 1;
                    }
                    Err(error) if unsupported_rename(&error) => {
                        error!(run, %error, "retained reports: this filesystem has no no-replace rename, so an ended run's files are left where they are, uncharged");
                        left += 1;
                        stopped = true;
                    }
                    Err(error) => {
                        warn!(run, %error, "retained reports: could not move an ended run's file; it is tried again at the next start");
                        report.failed += 1;
                        left += 1;
                    }
                }
            }
            // 6. Both directories durable before the source can be retired.
            synced = self.sync(&dirs.mine, "mine").is_ok() && self.sync(theirs, "theirs").is_ok();
        }
        // 7. Retire only an empty, synced source.
        let empty = match &theirs {
            Some(theirs) => left == 0 && collect_names(theirs).is_ok_and(|names| names.is_empty()),
            None => true,
        };
        if !(synced && empty) {
            return;
        }
        match dirs.runs.remove_file(inventory_name(run)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(run, %error, "retained reports: could not remove an ended run's inventory");
                return;
            }
        }
        if theirs.is_some()
            && let Err(error) = dirs.files.remove_dir(run)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(run, %error, "retained reports: could not remove an ended run's directory");
            return;
        }
        if self.sync(&dirs.runs, "runs").is_err() || self.sync(&dirs.files, "files").is_err() {
            return;
        }
        if let Err(error) = dirs.runs.remove_file(lock_name(run)) {
            warn!(run, %error, "retained reports: could not remove an ended run's lock");
            return;
        }
        report.retired += 1;
    }

    // -- the sweep (D14 "Expiry sweep") ------------------------------------

    /// Remove what is due in this run's directory, retry failed removals and
    /// recount the adopted bytes; at startup, then hourly. Serialised with
    /// every other sweep and with takeover. A recount replaces the stored sum
    /// only from a complete scan, and only when nothing else changed the sum
    /// during the pass; otherwise the stored sum stands, so an entry the deck
    /// could not read never lowers a charge.
    pub fn sweep(&self, now: Now) -> PassReport {
        let mut report = PassReport::default();
        let Some(dirs) = &self.dirs else {
            return report;
        };
        let _serial = self.sweep_lock.lock().unwrap_or_else(|p| p.into_inner());
        #[cfg(test)]
        self.seams.sweep_entered();
        let (start, own) = match self.state.try_lock() {
            Ok(st) => (
                st.adopted_generation,
                st.own
                    .iter()
                    .map(|(name, file)| (name.clone(), file.clone()))
                    .collect::<Vec<_>>(),
            ),
            Err(_) => return report,
        };
        // Own files, each by name.
        for (name, file) in own {
            match self.fstat(&dirs.mine, &name) {
                Ok(_) if file.due(now) => {
                    // The snapshot only nominates the file. It is decided
                    // again under the lock and claimed there, as being
                    // removed: an acknowledgement or recovery that renewed it
                    // since the snapshot keeps it, and one that comes after
                    // the claim finds it being removed and renews nothing.
                    #[cfg(test)]
                    self.seams.pause(seams::Pause::BeforeClaim);
                    let claimed = match self.state.try_lock() {
                        Ok(mut st) => match st.own.get_mut(&name) {
                            Some(own) if own.due(now) => {
                                let changed = own.state != OwnState::OrphanUnlink;
                                own.state = OwnState::OrphanUnlink;
                                if changed {
                                    st.revision += 1;
                                }
                                true
                            }
                            _ => false,
                        },
                        Err(_) => false,
                    };
                    if !claimed {
                        continue;
                    }
                    #[cfg(test)]
                    self.seams.pause(seams::Pause::BeforeUnlink);
                    match self.unlink(&dirs.mine, &name) {
                        Ok(()) => {
                            if let Ok(mut st) = self.state.try_lock() {
                                st.drop_own(&name);
                            }
                            report.removed += 1;
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            if let Ok(mut st) = self.state.try_lock() {
                                st.drop_own(&name);
                            }
                        }
                        // Claimed, it stays an orphan whose removal the next
                        // pass retries.
                        Err(_) => report.failed += 1,
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    warn!(
                        name = %name,
                        "retained reports: a saved report was deleted from outside the deck; its room is released"
                    );
                    if let Ok(mut st) = self.state.try_lock() {
                        st.drop_own(&name);
                    }
                }
                Err(_) => report.uncertain += 1,
            }
        }
        // Attempts' temporary files that could not be removed when they were
        // made.
        let strays: Vec<String> = match self.state.try_lock() {
            Ok(st) => st.stray.keys().cloned().collect(),
            Err(_) => Vec::new(),
        };
        for temp in strays {
            let gone = match self.unlink(&dirs.mine, &temp) {
                Ok(()) => {
                    report.removed += 1;
                    true
                }
                Err(error) => {
                    let gone = error.kind() == io::ErrorKind::NotFound;
                    if !gone {
                        report.failed += 1;
                    }
                    gone
                }
            };
            if gone && let Ok(mut st) = self.state.try_lock() {
                st.stray.remove(&temp);
            }
        }
        // Adopted files, streamed: removed by their suffix, and recounted.
        let mut recount = 0u64;
        let mut complete = true;
        let mut ours = 0u64;
        match dirs.mine.stream() {
            Ok(mut stream) => {
                #[cfg(test)]
                let mut seen = 0u32;
                while let Some(name) = stream.next_name() {
                    #[cfg(test)]
                    {
                        if self.seams.stream_fails_at(seen) {
                            complete = false;
                            break;
                        }
                        seen += 1;
                    }
                    let Ok(name) = name else {
                        complete = false;
                        break;
                    };
                    let Some((_, deadline, _)) = name.to_str().and_then(parse_adopted_name) else {
                        continue;
                    };
                    #[cfg(test)]
                    self.seams.mid_pass(&self.state);
                    let name = name.to_str().unwrap_or_default();
                    match self.fstat(&dirs.mine, name) {
                        Ok(info) if info.kind == EntryKind::File => {
                            if deadline <= now.wall {
                                match self.unlink(&dirs.mine, name) {
                                    Ok(()) => {
                                        if let Ok(mut st) = self.state.try_lock() {
                                            st.unadopt(info.len);
                                        }
                                        ours += 1;
                                        report.removed += 1;
                                    }
                                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                    Err(_) => {
                                        recount = recount.saturating_add(info.len);
                                        report.failed += 1;
                                    }
                                }
                            } else {
                                recount = recount.saturating_add(info.len);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        _ => {
                            complete = false;
                            report.uncertain += 1;
                        }
                    }
                }
            }
            Err(_) => complete = false,
        }
        if let Ok(mut st) = self.state.try_lock() {
            let expected = start.wrapping_add(ours);
            if complete && st.adopted_generation == expected {
                st.adopted_bytes = recount;
                report.recounted = true;
            }
        }
        if !report.recounted && (report.uncertain > 0 || !complete) {
            warn!(
                uncertain = report.uncertain,
                "retained reports: the cleanup could not read every saved report; the charge for \
                 them stands until it can"
            );
        }
        report
    }

    // -- IO, with test seams ------------------------------------------------

    fn fstat(
        &self,
        dir: &TrustedDir,
        name: &str,
    ) -> io::Result<crate::platform::trusted_dir::EntryInfo> {
        #[cfg(test)]
        if let Some(errno) = self.seams.fstat_fault(name) {
            return Err(io::Error::from_raw_os_error(errno));
        }
        dir.entry(name)
    }

    fn unlink(&self, dir: &TrustedDir, name: &str) -> io::Result<()> {
        #[cfg(test)]
        if self.seams.fail_unlink.load(Ordering::SeqCst) {
            return Err(injected_io_error());
        }
        dir.remove_file(name)
    }

    fn move_in(
        &self,
        from: &TrustedDir,
        name: &str,
        to: &TrustedDir,
        target: &str,
    ) -> io::Result<()> {
        #[cfg(test)]
        if let Some(errno) = self.seams.move_fault() {
            return Err(io::Error::from_raw_os_error(errno));
        }
        from.rename_into(name, to, target)
    }

    fn sync(&self, dir: &TrustedDir, _which: &str) -> io::Result<()> {
        #[cfg(test)]
        if self.seams.sync_fails(_which) {
            return Err(injected_io_error());
        }
        dir.sync()
    }
}

/// How often the inventory is written when it changed.
const INVENTORY_FLUSH_EVERY: Duration = Duration::from_secs(2);
/// How often the store is swept, and held dispatch results retried.
const SWEEP_EVERY: Duration = Duration::from_secs(60 * 60);

/// Run the store's upkeep for the daemon's life (plan D14): write the
/// inventory whenever it changed, every two seconds at most, and sweep the
/// store and retry the dispatch results held in memory every hour. Every pass
/// runs on the blocking pool, holding no ranked lock.
pub fn spawn_maintenance(
    hub: Arc<crate::inbox_hub::InboxHub>,
    store: Arc<RetainedStore>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut flush = tokio::time::interval(INVENTORY_FLUSH_EVERY);
        let mut sweep = tokio::time::interval(SWEEP_EVERY);
        sweep.tick().await;
        loop {
            tokio::select! {
                _ = flush.tick() => {
                    let store = Arc::clone(&store);
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(error) = store.flush_inventory() {
                            warn!(%error, "retained reports: could not write this run's inventory");
                        }
                    })
                    .await;
                }
                _ = sweep.tick() => {
                    let store = Arc::clone(&store);
                    let hub = Arc::clone(&hub);
                    let _ = tokio::task::spawn_blocking(move || {
                        store.sweep(Now::current());
                        hub.retry_fallback_bodies(&store);
                    })
                    .await;
                }
            }
        }
    })
}

fn collect_names(dir: &TrustedDir) -> io::Result<Vec<std::ffi::OsString>> {
    let mut stream = dir.stream()?;
    let mut names = Vec::new();
    while let Some(name) = stream.next_name() {
        names.push(name?);
    }
    Ok(names)
}

/// Test seams: injected IO faults and pass hooks. Every seam is off unless a
/// test arms it.
#[cfg(test)]
mod seams {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    use super::StoreState;
    use crate::lock_rank::{R5, RankedMutex};

    /// The message of the error a simulated crash returns.
    pub(super) const CRASHED: &str = "injected crash";

    /// Where a write simulates its process dying, leaving its files as they
    /// are.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum Crash {
        /// Half the report written to its temporary name.
        MidWrite = 1,
        /// Linked to its own name, the temporary name not yet removed: the
        /// fallback where the filesystem has no no-replace rename.
        AfterLink = 2,
    }

    /// Where a sweep can be held while a test acts.
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    pub(super) enum Pause {
        /// A due own file nominated from the snapshot, before the lock.
        BeforeClaim,
        /// Claimed under the lock, before its unlink.
        BeforeUnlink,
    }

    #[derive(Default)]
    pub(super) struct Seams {
        pub(super) fail_write: AtomicBool,
        /// The next write's [`Crash`] point, 0 for none.
        pub(super) crash: AtomicU8,
        /// A barrier the sweep meets twice at a [`Pause`]: once on arriving,
        /// once to go on.
        pub(super) pauses: Mutex<HashMap<Pause, Arc<Barrier>>>,
        pub(super) fail_all_writes: AtomicBool,
        pub(super) fail_unlink: AtomicBool,
        /// A persistent errno for every move, 0 for none.
        pub(super) move_errno: AtomicI32,
        /// Which named syncs fail.
        pub(super) failing_syncs: Mutex<HashSet<&'static str>>,
        /// Stop a transfer once this many files were moved or removed;
        /// `u32::MAX` (unarmed) by default through `stop_armed`.
        pub(super) stop_after: AtomicU32,
        pub(super) stop_armed: AtomicBool,
        /// Per name, the errno its `fstatat` fails with.
        pub(super) fstat_faults: Mutex<HashMap<String, i32>>,
        /// Fail the sweep's directory stream after this many entries.
        pub(super) stream_fail_at: AtomicU32,
        pub(super) stream_fail_armed: AtomicBool,
        /// Bump the adopted generation once, mid-pass.
        pub(super) bump_mid_pass: AtomicBool,
        /// Sweeps inside the serialised section now, and the most at once.
        pub(super) active: AtomicU32,
        pub(super) most_active: AtomicU32,
        pub(super) taken: Mutex<Vec<String>>,
    }

    impl Seams {
        /// Whether this write crashes at `at`: once, then the seam is off.
        pub(super) fn crash_at(&self, at: Crash) -> bool {
            self.crash
                .compare_exchange(at as u8, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        }

        /// Arm a pause at `at`, once: the sweep and the test each meet the
        /// returned barrier twice.
        pub(super) fn pause_at(&self, at: Pause) -> Arc<Barrier> {
            let barrier = Arc::new(Barrier::new(2));
            self.pauses.lock().unwrap().insert(at, Arc::clone(&barrier));
            barrier
        }

        pub(super) fn pause(&self, at: Pause) {
            let armed = self.pauses.lock().unwrap().remove(&at);
            if let Some(barrier) = armed {
                barrier.wait();
                barrier.wait();
            }
        }

        pub(super) fn note_taken(&self, run: &str) {
            self.taken.lock().unwrap().push(run.to_string());
        }

        pub(super) fn stop_transfer(&self, done: u32) -> bool {
            self.stop_armed.load(Ordering::SeqCst) && done >= self.stop_after.load(Ordering::SeqCst)
        }

        pub(super) fn fstat_fault(&self, name: &str) -> Option<i32> {
            self.fstat_faults.lock().unwrap().get(name).copied()
        }

        pub(super) fn move_fault(&self) -> Option<i32> {
            match self.move_errno.load(Ordering::SeqCst) {
                0 => None,
                errno => Some(errno),
            }
        }

        pub(super) fn sync_fails(&self, which: &str) -> bool {
            self.failing_syncs.lock().unwrap().contains(which)
        }

        pub(super) fn stream_fails_at(&self, seen: u32) -> bool {
            self.stream_fail_armed.load(Ordering::SeqCst)
                && seen >= self.stream_fail_at.load(Ordering::SeqCst)
        }

        pub(super) fn mid_pass(&self, state: &RankedMutex<{ R5 }, StoreState>) {
            if self.bump_mid_pass.swap(false, Ordering::SeqCst)
                && let Ok(mut st) = state.try_lock()
            {
                st.adopted_generation = st.adopted_generation.wrapping_add(1);
            }
        }

        /// A sweep entered the serialised section: record how many are in it,
        /// and leave it after a moment, so an overlap would be seen.
        pub(super) fn sweep_entered(&self) {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_active.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(20));
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
#[path = "retained_store_tests.rs"]
mod tests;
