//! PRD #1790 stage 1: how each agent the daemon started was launched, kept
//! across a daemon restart so a later stage can start it again.
//!
//! The work journal ([`crate::work_journal`]) holds refs, ids and paths only,
//! and its redaction refuses a path-like string outside `workspace` and keeps
//! free-text names out, so the launch command and the display name a restore
//! needs cannot go there. They are kept here instead: one JSON file per
//! attempt in `<state_dir>/launch-records/`, an owner-only directory (`0700`)
//! of owner-only files (`0600`), keyed by the attempt's journal ids so a
//! reader joins the two on `work` and `attempt`.
//!
//! **Written at spawn, by the daemon's own spawn path**, never from a client:
//! [`crate::work_tracking::WorkTracker::launch_recorded`], after the
//! generation is published and with no registry lock held. Each record is
//! written whole to a temporary name, `.pending-<16 hex>-<record name>`,
//! synced, and renamed into place, so a crash leaves the previous state or
//! the complete record, never half of one; a reader skips the temporary
//! names. A temporary file a crash left behind is removed by [`prune`] once
//! the run that wrote it (the run its record name carries) is no longer live,
//! so a temporary file of the live daemon's own run is never touched.
//!
//! **Retention follows the journal's.** A record is removed only by
//! [`prune`], which the daemon runs after each journal compaction round, and
//! only when a journal read the caller judged complete no longer holds the
//! record's work **and** the record is older than [`PRUNE_GRACE`]. The
//! daemon's judgement is `work_tracking::launch_read_incomplete`: a read
//! that was truncated, or that skipped a line of a newer schema or of a kind
//! this build does not know (either may name a work this build cannot see),
//! removes no record. A line the reader skipped as unparsable, too long or
//! torn does not stop a prune. So a record is not removed while the
//! journal read it is compared against holds its work; the grace covers a record written while its work's `opened` was still queued for the
//! journal's writer, or written after the read the prune compares against. A
//! work the journal never recorded (its event dropped from a full queue, or a
//! journal that could not open) has its record removed once the grace has
//! passed, since no restore can find that work either.
//!
//! **One listing looks at most [`MAX_LISTED`] directory entries.** A reader
//! whose listing stopped there says so ([`LaunchRead::truncated`]), and
//! [`read_all`] then answers an error rather than a partial set, so its
//! caller is not handed part of the store as if it were the whole.
//! [`prune`] lists the store in windows of that size, the next call
//! starting where the last stopped ([`PruneWindow`]), so successive calls
//! move through the store instead of repeating its first window.
//!
//! **What the reader checks, and what a consumer still must.** [`read_all`]
//! returns a record only when its `v` is [`SCHEMA_VERSION`] and its file
//! name is the one its own `attempt` names ([`file_name`]); anything else is
//! skipped and counted ([`read_all_with_stats`]). That makes a record
//! well-formed, not trustworthy as an instruction. A consumer that will act
//! on one (start a command from it, resume a session) must also: join the
//! record's `work` and `attempt` against a complete journal read, and refuse
//! a record whose attempt the journal does not hold under that work; refuse
//! an attempt with more than one candidate (two records for one attempt, or
//! an attempt the journal holds under two works) rather than pick one; check the session id against the agent's
//! own id syntax before using it; pass a resume id to the agent as one literal
//! argument, never interpolated into shell text; open the `cwd` and a saved
//! dispatch task without following symlinks and check they are owned by the
//! user and not writable by others; and keep the user's explicit confirmation
//! and the check that the old agent process is gone before starting anything.
//!
//! The maintainer page is `docs/develop/work-journal.md`.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::event::AgentType;
use crate::platform::trusted_dir::{TrustedDir, valid_component};
use crate::work_journal::{AttemptId, WorkId};

/// The directory under the state directory.
pub const DIR_NAME: &str = "launch-records";

/// The record schema this build writes.
pub const SCHEMA_VERSION: u64 = 1;

/// A record whose work the journal no longer holds is removed only once it is
/// at least this old. See the module docs.
pub const PRUNE_GRACE: Duration = Duration::from_secs(60 * 60);

/// The largest record a reader opens; a larger file is skipped.
const MAX_RECORD_BYTES: u64 = 256 * 1024;

/// The prefix of a record being written, which a reader skips.
const TEMP_PREFIX: &str = ".pending-";

/// The hex digits of a temporary name's nonce.
const NONCE_HEX: usize = 16;

/// The most crash-left temporary files one [`prune`] removes.
const MAX_PENDING_REMOVED: usize = 256;

/// The most directory entries one listing looks at.
pub const MAX_LISTED: usize = 65_536;

const EXTENSION: &str = ".json";

/// How one attempt was launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecord {
    /// [`SCHEMA_VERSION`].
    pub v: u64,
    /// The work the attempt served, as the journal names it.
    pub work: WorkId,
    /// The attempt (`<run>/<agent id>`), as the journal names it.
    pub attempt: AttemptId,
    /// The command exactly as the spawn's caller gave it, before the daemon
    /// wrapped it; `None` when the spawn ran the default shell.
    pub command: Option<String>,
    /// The `SHELL` wrapper-choice override the spawn carried, if any; `None`
    /// means it ran under the daemon's own default shell.
    pub shell: Option<String>,
    /// The agent the command runs, when the spawn said.
    pub agent_type: Option<AgentType>,
    /// The folder it started in, as an absolute path. `None` when it could
    /// not be resolved, which includes a prepared start whose directory had
    /// been moved away ([`Self::cwd_moved_from`]).
    pub cwd: Option<String>,
    /// For a prepared start only: the absolute path it asked for, when the
    /// directory it was verified against and entered was no longer at that
    /// path at launch and no other path naming that directory could be
    /// found. `cwd` is then `None`: this path names a different directory,
    /// or nothing, and a restore must not start the agent there. Absent from
    /// the file otherwise (a record written before it existed reads `None`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_moved_from: Option<String>,
    /// The pane's display name.
    pub name: Option<String>,
    /// Its role, for a role pane of an orchestration.
    pub role: Option<String>,
    /// The orchestration's name, for a role pane.
    pub orchestration: Option<String>,
    /// The orchestration instance, for a role pane.
    pub orchestration_id: Option<String>,
    /// When it was written, in milliseconds since the Unix epoch.
    pub ts_ms: u64,
}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// What [`prune`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
    pub kept: usize,
    /// It removed no record because the journal read was not complete.
    pub skipped_truncated: bool,
    /// Crash-left temporary files of runs no longer live that it removed.
    pub pending_removed: usize,
    /// The store held more entries than this call's window looked at; the
    /// next call ([`PruneWindow`]) starts after them.
    pub listing_truncated: bool,
}

/// Where the next [`prune`] call's listing starts, in directory order, so
/// that a store larger than one listing ([`MAX_LISTED`]) is cleaned over
/// successive calls instead of the same first window each time. Back at the
/// start once a listing reaches the end. Entries removed meanwhile can shift
/// a few later ones before the window; they are reached on the next pass.
#[derive(Debug, Default)]
pub struct PruneWindow(AtomicUsize);

impl PruneWindow {
    /// A window at the start of the store.
    pub const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }
}

/// What [`read_all_with_stats`] read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchRead {
    pub records: Vec<LaunchRecord>,
    /// Files skipped: unreadable, unparsable, too large, of another schema
    /// version, or whose name is not the one their `attempt` names.
    pub skipped: usize,
    /// The listing stopped at [`MAX_LISTED`] entries with more left, so
    /// `records` and `skipped` cover only the entries it reached: a record
    /// missing from `records` may still be in the store.
    pub truncated: bool,
}

/// The file name of `attempt`'s record: `<run>-<agent>.json`. `None` when the
/// run is not a plain component (an attempt id never carries one).
pub fn file_name(attempt: &AttemptId) -> Option<String> {
    let (run, agent) = attempt.as_str().rsplit_once('/')?;
    let stem = format!("{run}-{agent}");
    valid_component(&stem).then(|| format!("{stem}{EXTENSION}"))
}

fn is_record_name(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(EXTENSION)
}

/// The run of a temporary name [`write`] makes, `.pending-<16 lowercase
/// hex>-<run>-<agent>.json`; `None` for any other name.
fn pending_run(name: &str) -> Option<&str> {
    let rest = name.strip_prefix(TEMP_PREFIX)?;
    let (nonce, record) = rest.split_at_checked(NONCE_HEX)?;
    if !nonce
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let stem = record.strip_prefix('-')?.strip_suffix(EXTENSION)?;
    let (run, agent) = stem.rsplit_once('-')?;
    let attempt = AttemptId::new(run, agent.parse().ok()?)?;
    (file_name(&attempt)?.as_str() == record.strip_prefix('-')?).then_some(run)
}

/// The store's directory under `state_dir`, created owner-only when missing.
fn open_dir(state_dir: &Path, create: bool) -> io::Result<TrustedDir> {
    let anchor = TrustedDir::anchor(state_dir)?;
    let fresh = create && anchor.entry(DIR_NAME).is_err();
    let dir = anchor.child(DIR_NAME, create)?;
    if fresh {
        anchor.sync()?;
    }
    Ok(dir)
}

/// Write `record` under `state_dir`, replacing an earlier record of the same
/// attempt: whole to a temporary name, synced, then renamed into place.
pub fn write(state_dir: &Path, record: &LaunchRecord) -> io::Result<()> {
    let name = file_name(&record.attempt).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "the attempt id names no file")
    })?;
    let bytes = serde_json::to_vec(record).map_err(io::Error::other)?;
    let dir = open_dir(state_dir, true)?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).map_err(io::Error::other)?;
    let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    // The record name carries the writer's run, which is what lets `prune`
    // tell a crash-left temporary file from a live writer's.
    let temp = format!("{TEMP_PREFIX}{hex}-{name}");
    let result = (|| {
        let mut file = dir.create_new_owner_only(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        dir.rename(&temp, &name)?;
        dir.sync()
    })();
    if result.is_err() {
        let _ = dir.remove_file(&temp);
    }
    result
}

/// The valid records under `state_dir`, in no particular order, when one
/// listing reached every entry of the store; an error (`Other`) when the
/// store holds more entries than one listing looks at ([`MAX_LISTED`]), since
/// the set would then be partial ([`read_all_with_stats`] answers the part
/// it read, flagged). A missing directory is no records; a file that cannot
/// be read or parsed, whose `v` is not [`SCHEMA_VERSION`], or whose name is
/// not its attempt's ([`file_name`]) is skipped with a `warn!`, so one bad
/// file does not hide the rest. See the module docs for what a consumer must
/// still check.
pub fn read_all(state_dir: &Path) -> io::Result<Vec<LaunchRecord>> {
    read_all_limited(state_dir, MAX_LISTED)
}

/// [`read_all`] with the listing's size passed in.
fn read_all_limited(state_dir: &Path, limit: usize) -> io::Result<Vec<LaunchRecord>> {
    let read = read_limited(state_dir, limit)?;
    if read.truncated {
        return Err(io::Error::other(format!(
            "the launch-record store holds more than {limit} entries; the listing is incomplete"
        )));
    }
    Ok(read.records)
}

/// [`read_all`], counting what it skipped, and answering the part it read
/// when the listing was cut short ([`LaunchRead::truncated`]).
pub fn read_all_with_stats(state_dir: &Path) -> io::Result<LaunchRead> {
    read_limited(state_dir, MAX_LISTED)
}

/// [`read_all_with_stats`] with the listing's size passed in.
fn read_limited(state_dir: &Path, limit: usize) -> io::Result<LaunchRead> {
    let dir = match open_dir(state_dir, false) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LaunchRead::default()),
        Err(e) => return Err(e),
    };
    let listing = list_names(&dir, 0, limit)?;
    let (named, skipped) = read_named(&dir, &listing.names);
    Ok(LaunchRead {
        records: named.into_iter().map(|(_, record)| record).collect(),
        skipped,
        truncated: listing.truncated,
    })
}

/// One listing of the store's directory.
struct Listing {
    /// The UTF-8 names among the entries it looked at.
    names: Vec<String>,
    /// More entries followed the last one it looked at.
    truncated: bool,
}

/// The entries of `dir` in directory order past the first `skip`, at most
/// `limit` of them. A name that is not UTF-8 takes its place in the count
/// and is not answered.
fn list_names(dir: &TrustedDir, skip: usize, limit: usize) -> io::Result<Listing> {
    let mut stream = dir.stream()?;
    let mut names = Vec::new();
    let (mut position, mut looked) = (0usize, 0usize);
    while let Some(name) = stream.next_name() {
        let name = name?;
        position += 1;
        if position <= skip {
            continue;
        }
        if looked >= limit {
            return Ok(Listing {
                names,
                truncated: true,
            });
        }
        looked += 1;
        if let Some(name) = name.to_str() {
            names.push(name.to_string());
        }
    }
    Ok(Listing {
        names,
        truncated: false,
    })
}

/// The valid records among `names`, with their names, and how many record
/// names it skipped.
fn read_named(dir: &TrustedDir, names: &[String]) -> (Vec<(String, LaunchRecord)>, usize) {
    let mut records = Vec::new();
    let mut skipped = 0;
    for name in names {
        if !is_record_name(name) {
            continue;
        }
        match read_one(dir, name) {
            Ok(record) => records.push((name.clone(), record)),
            Err(error) => {
                skipped += 1;
                warn!(%name, %error, "launch records: skipped a record");
            }
        }
    }
    (records, skipped)
}

fn read_one(dir: &TrustedDir, name: &str) -> io::Result<LaunchRecord> {
    let file = dir.open_regular(name)?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the record is too large",
        ));
    }
    let record: LaunchRecord = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if record.v != SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("schema version {} is not {SCHEMA_VERSION}", record.v),
        ));
    }
    if file_name(&record.attempt).as_deref() != Some(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the file name is not the one its attempt names",
        ));
    }
    Ok(record)
}

/// Among the entries of this call's listing window ([`PruneWindow`], at
/// most [`MAX_LISTED`] entries), remove the records whose work
/// `journal_works` (the works of a journal read) does not hold, once they are
/// at least [`PRUNE_GRACE`] older than `now_ms`. `incomplete` says the
/// caller judged that read incomplete, and then no record is removed. Either
/// way, the temporary files a crash left behind ([`write`]) in the window
/// are removed, at most [`MAX_PENDING_REMOVED`] a call, when `run_is_live`
/// says the run their name carries is not live. A record is removed only on
/// what was seen (the record, and its work's absence from the journal read),
/// never on an entry's absence from the listing, so a window that did not
/// reach the end of the store only means fewer entries were looked at this
/// call ([`PruneReport::listing_truncated`]). See the module docs.
pub fn prune(
    state_dir: &Path,
    journal_works: &HashSet<WorkId>,
    incomplete: bool,
    now_ms: u64,
    run_is_live: impl Fn(&str) -> bool,
    window: &PruneWindow,
) -> io::Result<PruneReport> {
    prune_limited(
        state_dir,
        journal_works,
        incomplete,
        now_ms,
        run_is_live,
        window,
        MAX_LISTED,
    )
}

/// [`prune`] with the window's size passed in.
fn prune_limited(
    state_dir: &Path,
    journal_works: &HashSet<WorkId>,
    incomplete: bool,
    now_ms: u64,
    run_is_live: impl Fn(&str) -> bool,
    window: &PruneWindow,
    limit: usize,
) -> io::Result<PruneReport> {
    let mut report = PruneReport::default();
    let dir = match open_dir(state_dir, false) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e),
    };
    let mut start = window.0.load(Ordering::Relaxed);
    let mut listing = list_names(&dir, start, limit)?;
    if start > 0 && listing.names.is_empty() && !listing.truncated {
        // The store shrank below the window's start: begin again.
        start = 0;
        listing = list_names(&dir, 0, limit)?;
    }
    report.listing_truncated = listing.truncated;
    window.0.store(
        if listing.truncated { start + limit } else { 0 },
        Ordering::Relaxed,
    );
    for name in &listing.names {
        if report.pending_removed >= MAX_PENDING_REMOVED {
            break;
        }
        let Some(run) = pending_run(name) else {
            continue;
        };
        if run_is_live(run) {
            continue;
        }
        match dir.remove_file(name) {
            Ok(()) => report.pending_removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!(%name, %error, "launch records: could not remove a temporary file"),
        }
    }
    if incomplete {
        report.skipped_truncated = true;
    } else {
        let (records, _) = read_named(&dir, &listing.names);
        let grace = u64::try_from(PRUNE_GRACE.as_millis()).unwrap_or(u64::MAX);
        let cutoff = now_ms.saturating_sub(grace);
        for (name, record) in records {
            if journal_works.contains(&record.work) || record.ts_ms > cutoff {
                report.kept += 1;
                continue;
            }
            match dir.remove_file(&name) {
                Ok(()) => report.removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    report.kept += 1;
                    warn!(%name, %error, "launch records: could not remove a record");
                }
            }
        }
    }
    if report.removed > 0 || report.pending_removed > 0 {
        let _ = dir.sync();
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deck_identity::DeckId;

    fn work() -> WorkId {
        WorkId::mint(&DeckId::parse("abcdefgh").unwrap()).unwrap()
    }

    fn record(work: &WorkId, agent: u64, ts_ms: u64) -> LaunchRecord {
        LaunchRecord {
            v: SCHEMA_VERSION,
            work: work.clone(),
            attempt: AttemptId::new("0123abcd", agent).unwrap(),
            command: Some("claude --model x 'quoted arg'".into()),
            shell: None,
            agent_type: Some(AgentType::ClaudeCode),
            cwd: Some("/abs/folder".into()),
            cwd_moved_from: None,
            name: Some("A name with spaces".into()),
            role: Some("coder".into()),
            orchestration: Some("tdd".into()),
            orchestration_id: Some("orch-1".into()),
            ts_ms,
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_record_round_trips_owner_only_with_every_field() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        let r = record(&w, 7, 42);
        write(state.path(), &r).unwrap();
        let dir = state.path().join(DIR_NAME);
        let path = dir.join("0123abcd-7.json");
        #[cfg(unix)]
        {
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for field in [
            "v",
            "work",
            "attempt",
            "command",
            "shell",
            "agent_type",
            "cwd",
            "name",
            "role",
            "orchestration",
            "orchestration_id",
            "ts_ms",
        ] {
            assert!(json.get(field).is_some(), "{field} missing: {json}");
        }
        assert_eq!(json["shell"], serde_json::Value::Null);
        assert_eq!(json["agent_type"], "claude_code");
        assert_eq!(read_all(state.path()).unwrap(), vec![r]);
    }

    #[test]
    fn a_rewrite_replaces_the_record_and_leaves_no_temporary_file() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        write(state.path(), &record(&w, 1, 1)).unwrap();
        let mut newer = record(&w, 1, 2);
        newer.name = Some("renamed".into());
        write(state.path(), &newer).unwrap();
        let names: Vec<String> = std::fs::read_dir(state.path().join(DIR_NAME))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["0123abcd-1.json"]);
        assert_eq!(read_all(state.path()).unwrap(), vec![newer]);
    }

    /// A crash between creating the temporary file and the rename leaves a
    /// `.pending-` file, never a half record at the real name: the reader
    /// skips it, as it skips a corrupt record without hiding the others.
    #[test]
    fn a_reader_skips_temporary_and_corrupt_files() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        let good = record(&w, 3, 3);
        write(state.path(), &good).unwrap();
        let dir = state.path().join(DIR_NAME);
        std::fs::write(dir.join(".pending-00-0123abcd-4.json"), b"{\"v\":1,").unwrap();
        std::fs::write(dir.join("0123abcd-5.json"), b"{not json").unwrap();
        assert_eq!(read_all(state.path()).unwrap(), vec![good]);
    }

    #[test]
    fn no_directory_is_no_records() {
        let state = tempfile::tempdir().unwrap();
        assert!(read_all(state.path()).unwrap().is_empty());
        assert!(!state.path().join(DIR_NAME).exists());
    }

    #[test]
    fn prune_keeps_what_the_journal_holds_and_what_is_young() {
        let state = tempfile::tempdir().unwrap();
        let grace = u64::try_from(PRUNE_GRACE.as_millis()).unwrap();
        let now = 10 * grace;
        let (kept_work, gone_work) = (work(), work());
        let in_journal = record(&kept_work, 1, 0);
        let forgotten_old = record(&gone_work, 2, now - grace - 1);
        let forgotten_young = record(&gone_work, 3, now - grace + 1);
        for r in [&in_journal, &forgotten_old, &forgotten_young] {
            write(state.path(), r).unwrap();
        }
        let journal: HashSet<WorkId> = [kept_work].into_iter().collect();

        // A truncated read removes nothing, whatever the age.
        let report = prune(
            state.path(),
            &journal,
            true,
            now,
            |_| true,
            &PruneWindow::new(),
        )
        .unwrap();
        assert!(report.skipped_truncated);
        assert_eq!(read_all(state.path()).unwrap().len(), 3);

        let report = prune(
            state.path(),
            &journal,
            false,
            now,
            |_| true,
            &PruneWindow::new(),
        )
        .unwrap();
        assert_eq!((report.removed, report.kept), (1, 2));
        let mut left: Vec<String> = read_all(state.path())
            .unwrap()
            .into_iter()
            .map(|r| r.attempt.to_string())
            .collect();
        left.sort();
        assert_eq!(left, ["0123abcd/1", "0123abcd/3"]);
    }

    /// A5: a record of another schema version, or filed under a name that is
    /// not its attempt's, is skipped and counted, not returned.
    #[test]
    fn a_reader_skips_and_counts_other_versions_and_misfiled_records() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        let good = record(&w, 1, 1);
        write(state.path(), &good).unwrap();
        let dir = state.path().join(DIR_NAME);
        let mut newer = record(&w, 2, 2);
        newer.v = SCHEMA_VERSION + 1;
        std::fs::write(
            dir.join("0123abcd-2.json"),
            serde_json::to_vec(&newer).unwrap(),
        )
        .unwrap();
        // Attempt 3's record, filed under attempt 4's name.
        std::fs::write(
            dir.join("0123abcd-4.json"),
            serde_json::to_vec(&record(&w, 3, 3)).unwrap(),
        )
        .unwrap();
        let read = read_all_with_stats(state.path()).unwrap();
        assert_eq!(read.records, vec![good.clone()]);
        assert_eq!(read.skipped, 2);
        assert_eq!(read_all(state.path()).unwrap(), vec![good]);
    }

    /// R6/A4: a temporary file a crash left behind is removed once the run its
    /// name carries is not live, and one of a live run (a writer that may
    /// still be writing it) is kept; a dotfile of any other shape is never
    /// touched. It happens whether or not the journal read was complete.
    #[test]
    fn crash_left_temporary_files_of_dead_runs_are_removed() {
        let state = tempfile::tempdir().unwrap();
        write(state.path(), &record(&work(), 1, 1)).unwrap();
        let dir = state.path().join(DIR_NAME);
        let dead = ".pending-0123456789abcdef-deadrun-4.json";
        let live = ".pending-fedcba9876543210-liverun-5.json";
        let others = [
            ".pending-0123456789abcdef-deadrun-4.txt",
            ".pending-0123-deadrun-4.json",
            ".pending-0123456789abcdef-deadrun-x.json",
            ".something-deadrun-4.json",
        ];
        for name in std::iter::once(&dead).chain([&live]).chain(others.iter()) {
            std::fs::write(dir.join(name), b"{\"v\":1,").unwrap();
        }
        // An upper-case nonce is not a name `write` makes. Checked on the
        // name, not on disk: on a case-insensitive filesystem (macOS's
        // default) it is the same file as `dead`.
        assert_eq!(
            pending_run(".pending-0123456789ABCDEF-deadrun-4.json"),
            None,
            "an upper-case nonce is not a temporary record name"
        );
        let is_live = |run: &str| run == "liverun";
        let report = prune(
            state.path(),
            &HashSet::new(),
            true,
            0,
            is_live,
            &PruneWindow::new(),
        )
        .unwrap();
        assert_eq!(report.pending_removed, 1);
        assert!(
            !dir.join(dead).exists(),
            "the dead run's temporary file is gone"
        );
        assert!(
            dir.join(live).exists(),
            "a live writer's temporary file is kept"
        );
        for name in others {
            assert!(
                dir.join(name).exists(),
                "{name} is not a temporary record name"
            );
        }
        assert_eq!(
            read_all(state.path()).unwrap().len(),
            1,
            "records untouched"
        );
    }

    /// The temporary name `write` makes is one `prune` recognises, with the
    /// writer's run.
    #[test]
    fn the_temporary_name_carries_the_run() {
        let attempt = AttemptId::new("0123abcd", 12).unwrap();
        let name = format!(
            "{TEMP_PREFIX}{}-{}",
            "a".repeat(NONCE_HEX),
            file_name(&attempt).unwrap()
        );
        assert_eq!(pending_run(&name), Some("0123abcd"));
    }

    #[test]
    fn a_file_name_is_one_plain_component() {
        let attempt = AttemptId::new("0123abcd", 12).unwrap();
        assert_eq!(file_name(&attempt).as_deref(), Some("0123abcd-12.json"));
    }

    /// F5: a listing cut short at its limit says so: the stats read flags
    /// `truncated` and answers the part it read, `read_all` answers an
    /// error instead of a partial set, and a store that fits is complete.
    #[test]
    fn a_listing_past_its_limit_says_it_is_incomplete() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        for agent in 1..=5 {
            write(state.path(), &record(&w, agent, agent)).unwrap();
        }
        let partial = read_limited(state.path(), 3).unwrap();
        assert!(partial.truncated);
        assert_eq!((partial.records.len(), partial.skipped), (3, 0));
        assert!(read_all_limited(state.path(), 3).is_err());
        let whole = read_limited(state.path(), 5).unwrap();
        assert!(!whole.truncated, "exactly the limit is complete");
        assert_eq!(whole.records.len(), 5);
        assert_eq!(read_all_limited(state.path(), 5).unwrap().len(), 5);
        assert_eq!(read_all(state.path()).unwrap().len(), 5);
    }

    /// F5: successive windows of a capped listing cover every entry once.
    #[test]
    fn successive_windows_cover_the_whole_store() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        for agent in 1..=7 {
            write(state.path(), &record(&w, agent, agent)).unwrap();
        }
        let dir = open_dir(state.path(), false).unwrap();
        let mut seen = Vec::new();
        let mut skip = 0;
        loop {
            let listing = list_names(&dir, skip, 3).unwrap();
            seen.extend(listing.names);
            if !listing.truncated {
                break;
            }
            skip += 3;
        }
        seen.sort();
        let mut all = list_names(&dir, 0, usize::MAX).unwrap().names;
        all.sort();
        assert_eq!(seen, all);
        assert_eq!(all.len(), 7);
    }

    /// F5: pending cleanup makes progress across capped listings. With 100
    /// records that stay (the journal holds their work) and one crash-left
    /// temporary file of a dead run, a prune whose window is two entries
    /// wide reports its listing truncated and, over successive calls,
    /// reaches and removes the temporary file; no record is removed. (A
    /// prune that listed the same first window every call would reach it
    /// only if the directory happened to list it first.)
    #[test]
    fn pending_cleanup_advances_past_a_capped_listing() {
        let state = tempfile::tempdir().unwrap();
        let w = work();
        for agent in 1..=100 {
            write(state.path(), &record(&w, agent, 0)).unwrap();
        }
        let dir = state.path().join(DIR_NAME);
        let dead = ".pending-0123456789abcdef-deadrun-4.json";
        std::fs::write(dir.join(dead), b"{").unwrap();
        let journal: HashSet<WorkId> = [w].into_iter().collect();
        let window = PruneWindow::new();
        let mut truncated_seen = false;
        let mut calls = 0;
        while dir.join(dead).exists() {
            assert!(calls < 60, "the temporary file was never reached");
            let report =
                prune_limited(state.path(), &journal, false, 0, |_| false, &window, 2).unwrap();
            truncated_seen |= report.listing_truncated;
            assert_eq!(report.removed, 0);
            calls += 1;
        }
        assert!(truncated_seen);
        assert_eq!(read_all(state.path()).unwrap().len(), 100);
    }
}
