//! PRD #1885: a schedule's last fire, when it was refused because its
//! orchestration is not approved.
//!
//! Nobody watches a scheduled fire, so a refusal that only reached the daemon
//! log would look like a schedule that silently stopped working. The daemon
//! records the refusal here, per task, and `dot-agent-deck schedule list` and the
//! TUI's schedule manager show it beside the task. The next fire of the same
//! task clears it before it runs, so the record always describes the last fire.
//!
//! `<state_dir>/schedule-outcomes/last.json`, owner-only, replaced whole under
//! an exclusive lock, through the same helpers as [`crate::project_trust`]'s
//! store.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::platform::trusted_dir::TrustedDir;
use crate::project_trust::{lock_named, read_bounded, write_replacing};

const DIR: &str = "schedule-outcomes";
const FILE: &str = "last.json";
const LOCK: &str = "last.lock";

/// The last fire of one task, when it was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// RFC 3339, UTC.
    pub at: String,
    /// The refusal text: the orchestration, the project, why, and the command
    /// that approves it.
    pub message: String,
}

/// Every task's recorded refusal, by task name.
pub type Outcomes = BTreeMap<String, Refusal>;

/// Where the record lives.
#[derive(Debug, Clone)]
pub struct OutcomeStore {
    state_dir: PathBuf,
}

impl OutcomeStore {
    pub fn at(state_dir: impl Into<PathBuf>) -> Self {
        OutcomeStore {
            state_dir: state_dir.into(),
        }
    }

    /// Beside the approval store: under [`crate::config::state_dir`], or where
    /// [`crate::project_trust::TrustStore::use_state_dir_for_this_process`]
    /// pointed it.
    pub fn default_location() -> Self {
        OutcomeStore::at(
            crate::project_trust::TrustStore::default_location()
                .state_dir()
                .to_path_buf(),
        )
    }

    fn dir(&self, create: bool) -> io::Result<Option<TrustedDir>> {
        if create {
            std::fs::create_dir_all(&self.state_dir)?;
        } else if !self.state_dir.join(DIR).exists() {
            return Ok(None);
        }
        let anchor = TrustedDir::anchor(&self.state_dir)?;
        Ok(Some(anchor.child(DIR, create)?))
    }

    /// Every recorded refusal; empty when there is no record or it cannot be
    /// read.
    pub fn load(&self) -> Outcomes {
        let read = || -> io::Result<Outcomes> {
            let Some(dir) = self.dir(false)? else {
                return Ok(Outcomes::new());
            };
            match read_bounded(&dir, FILE)? {
                None => Ok(Outcomes::new()),
                Some(text) => serde_json::from_str(&text)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            }
        };
        read().unwrap_or_default()
    }

    fn update(&self, change: impl FnOnce(&mut Outcomes) -> bool) -> io::Result<()> {
        let dir = self
            .dir(true)?
            .ok_or_else(|| io::Error::other("the schedule outcome directory"))?;
        let _lock = lock_named(&dir, LOCK)?;
        let mut outcomes: Outcomes = match read_bounded(&dir, FILE)? {
            None => Outcomes::new(),
            Some(text) => serde_json::from_str(&text).unwrap_or_default(),
        };
        if change(&mut outcomes) {
            let text = serde_json::to_string_pretty(&outcomes).map_err(io::Error::other)?;
            write_replacing(&dir, FILE, &text)?;
        }
        Ok(())
    }

    /// Record that `task`'s fire was refused with `message`.
    pub fn record_refusal(&self, task: &str, message: &str) {
        let refusal = Refusal {
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            message: message.to_string(),
        };
        if let Err(error) = self.update(|outcomes| {
            outcomes.insert(task.to_string(), refusal);
            true
        }) {
            tracing::warn!(task, %error, "could not record the schedule's refused fire");
        }
    }

    /// Forget `task`'s recorded refusal, as its next fire starts. Writes
    /// nothing when there is none.
    pub fn clear(&self, task: &str) {
        if !self.load().contains_key(task) {
            return;
        }
        if let Err(error) = self.update(|outcomes| outcomes.remove(task).is_some()) {
            tracing::warn!(task, %error, "could not clear the schedule's last refusal");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_recorded_listed_and_cleared() {
        let scratch = crate::test_temp::tempdir().unwrap();
        let store = OutcomeStore::at(scratch.path());
        assert!(store.load().is_empty());
        store.clear("nightly");
        assert!(
            !scratch.path().join(DIR).exists(),
            "clearing nothing writes nothing"
        );
        store.record_refusal("nightly", "orchestration 'team' … was never approved");
        store.record_refusal("weekly", "other");
        let outcomes = store.load();
        assert_eq!(outcomes.len(), 2);
        assert!(outcomes["nightly"].message.contains("never approved"));
        store.clear("nightly");
        let outcomes = store.load();
        assert!(!outcomes.contains_key("nightly"));
        assert!(outcomes.contains_key("weekly"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(scratch.path().join(DIR).join(FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }
}
