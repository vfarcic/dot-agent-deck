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
//! written whole to a temporary name, synced, and renamed into place, so a
//! crash leaves the previous state or the complete record, never half of
//! one; a reader skips the temporary names.
//!
//! **Retention follows the journal's.** A record is removed only by
//! [`prune`], which the daemon runs after each journal compaction round, and
//! only when a complete (not truncated) read of the journal no longer holds
//! the record's work **and** the record is older than [`PRUNE_GRACE`]. So no
//! record goes before the journal has forgotten its work; the grace covers a
//! record written while its work's `opened` was still queued for the
//! journal's writer, or written after the read the prune compares against. A
//! truncated read removes nothing: absence cannot be shown from it, and
//! neither can a read that skipped a line of a newer schema. A work
//! the journal never recorded (its event dropped from a full queue, or a
//! journal that could not open) has its record removed once the grace has
//! passed, since no restore can find that work either.
//!
//! The maintainer page is `docs/develop/work-journal.md`.

use std::collections::HashSet;
use std::io::{self, Read, Write};
use std::path::Path;
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
    /// The folder it started in, as an absolute path.
    pub cwd: Option<String>,
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
    /// It removed nothing because the journal read was truncated.
    pub skipped_truncated: bool,
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

/// Every complete record under `state_dir`, in no particular order. A missing
/// directory is no records; a file that cannot be read or parsed is skipped
/// with a `warn!`, so one bad file does not hide the rest.
pub fn read_all(state_dir: &Path) -> io::Result<Vec<LaunchRecord>> {
    Ok(read_named(state_dir)?
        .into_iter()
        .map(|(_, record)| record)
        .collect())
}

fn read_named(state_dir: &Path) -> io::Result<Vec<(String, LaunchRecord)>> {
    let dir = match open_dir(state_dir, false) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut stream = dir.stream()?;
    let mut records = Vec::new();
    while let Some(name) = stream.next_name() {
        let Some(name) = name?.to_str().map(str::to_string) else {
            continue;
        };
        if !is_record_name(&name) {
            continue;
        }
        match read_one(&dir, &name) {
            Ok(record) => records.push((name, record)),
            Err(error) => warn!(%name, %error, "launch records: skipped a record"),
        }
    }
    Ok(records)
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
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Remove the records whose work `journal_works` (the works of a complete
/// journal read) does not hold, once they are at least [`PRUNE_GRACE`] older
/// than `now_ms`. `truncated` says the read was not complete, and then
/// nothing is removed. See the module docs.
pub fn prune(
    state_dir: &Path,
    journal_works: &HashSet<WorkId>,
    truncated: bool,
    now_ms: u64,
) -> io::Result<PruneReport> {
    let mut report = PruneReport::default();
    if truncated {
        report.skipped_truncated = true;
        return Ok(report);
    }
    let records = read_named(state_dir)?;
    if records.is_empty() {
        return Ok(report);
    }
    let dir = open_dir(state_dir, false)?;
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
    if report.removed > 0 {
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
        let report = prune(state.path(), &journal, true, now).unwrap();
        assert!(report.skipped_truncated);
        assert_eq!(read_all(state.path()).unwrap().len(), 3);

        let report = prune(state.path(), &journal, false, now).unwrap();
        assert_eq!((report.removed, report.kept), (1, 2));
        let mut left: Vec<String> = read_all(state.path())
            .unwrap()
            .into_iter()
            .map(|r| r.attempt.to_string())
            .collect();
        left.sort();
        assert_eq!(left, ["0123abcd/1", "0123abcd/3"]);
    }

    #[test]
    fn a_file_name_is_one_plain_component() {
        let attempt = AttemptId::new("0123abcd", 12).unwrap();
        assert_eq!(file_name(&attempt).as_deref(), Some("0123abcd-12.json"));
    }
}
