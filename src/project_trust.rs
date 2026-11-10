//! PRD #1885: remembered approvals of a project's orchestration commands and
//! prompts.
//!
//! A project's `.dot-agent-deck.toml` names commands the deck runs as the user
//! and prompts it hands to the user's agents. Before this module, choosing an
//! orchestration ran them with no preview, and every unattended path (a
//! schedule, an issue dispatch, a delegate that restarts a role) ran whatever
//! the file said at that moment. This module is the consent half: it records
//! which exact text the user approved, per project and per orchestration, and
//! answers whether the text a spawn is about to use is still that text.
//!
//! # What an approval covers (decision 1 of the PRD's decision record)
//!
//! One orchestration of one project. It records, for every resolved role (after
//! `extends`), the exact text of each field that is executed or handed to an
//! agent ([`ApprovedRole`]): `name`, `command`, `agent`, `start`, `clear`,
//! `description` and `prompt_template`. One character of any of them changing
//! voids the approval; a comment, an unrelated key (`subscribe`,
//! `take_offers`, `worker_response_timeout_minutes`, …) or a *different*
//! orchestration changing does not, because none of those reach this record.
//! The store keeps the text itself, so a prompt can say which roles changed;
//! [`digest`] is SHA-256 over that text and is only ever used to make sure a
//! client approves exactly what it showed. The config revision
//! (`project_resolve`'s FNV hash) is never used here: it is a staleness hint,
//! not collision-resistant.
//!
//! # What a project is (decision 2)
//!
//! [`project_identity`]: for a directory in a git repository, the repository's
//! **main** working tree (a linked worktree resolves to it, so every dispatched
//! unit shares the approval its main checkout was given), joined with the
//! config directory's position inside its own worktree, so two configs in two
//! subdirectories of one repository stay two projects. Otherwise the canonical
//! config directory.
//!
//! # Where approvals live (decision 3)
//!
//! `<state_dir>/project-trust/approvals.json`, the directory created
//! owner-only and held through [`TrustedDir`], the file replaced whole by an
//! owner-only temporary file, `fsync` and rename, under an exclusive lock on
//! `approvals.lock` so a CLI approval and a daemon approval cannot lose each
//! other's write. Every check reads the file again, so an approval made from the
//! CLI takes effect without restarting the daemon.
//!
//! **Honest limit:** owner-only keeps other users out. Any process running as
//! this user — agents included — can still write the file. Closing that is
//! control-plane scoping's (#1886), not this module's.

use std::collections::BTreeSet;
use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::platform::trusted_dir::TrustedDir;
use crate::project_config::{OrchestrationConfig, OrchestrationRoleConfig, ProjectConfig};

/// The directory under the state dir that holds the store.
pub const STORE_DIR: &str = "project-trust";
/// The store file.
pub const STORE_FILE: &str = "approvals.json";
/// The file whose exclusive lock serializes writers.
const LOCK_FILE: &str = "approvals.lock";
/// The store's format version.
const STORE_VERSION: u32 = 1;
/// The largest store this reads. Far above any real one (an approval is the
/// text of one orchestration), and small enough that a hostile file cannot
/// make every spawn check allocate without bound.
const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;

/// The CLI verb a refusal names as the fix.
pub const APPROVE_VERB: &str = "dot-agent-deck trust approve";

/// The text of one role that an approval records: every field of
/// [`OrchestrationRoleConfig`] that is executed (`command`, and `agent`, which
/// decides how it is launched), or handed to an agent (`name`, `description`
/// and `clear` reach the orchestrator's context; `prompt_template` reaches the
/// role; `start` decides which role's template becomes the orchestrator's
/// instructions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedRole {
    pub name: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default)]
    pub start: bool,
    #[serde(default)]
    pub clear: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_template: Option<String>,
}

impl ApprovedRole {
    pub fn from_config(role: &OrchestrationRoleConfig) -> Self {
        ApprovedRole {
            name: role.name.clone(),
            command: role.command.clone(),
            agent: role.agent.clone(),
            start: role.start,
            clear: role.clear,
            description: role.description.clone(),
            prompt_template: role.prompt_template.clone(),
        }
    }
}

/// The recorded text of every role of `orchestration`, in order.
pub fn role_texts(orchestration: &OrchestrationConfig) -> Vec<ApprovedRole> {
    orchestration
        .roles
        .iter()
        .map(ApprovedRole::from_config)
        .collect()
}

/// One remembered approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// [`project_identity`] of the config it was given for.
    pub project: PathBuf,
    pub orchestration: String,
    /// RFC 3339, UTC.
    pub approved_at: String,
    pub roles: Vec<ApprovedRole>,
}

/// The whole store, as the file holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalStore {
    pub version: u32,
    #[serde(default)]
    pub approvals: Vec<Approval>,
}

impl Default for ApprovalStore {
    fn default() -> Self {
        ApprovalStore {
            version: STORE_VERSION,
            approvals: Vec::new(),
        }
    }
}

/// How a role's current text compares with the approved text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleChange {
    /// Its text differs from the approved text.
    Changed,
    /// The approval has no role of that name.
    New,
}

impl RoleChange {
    pub fn word(self) -> &'static str {
        match self {
            RoleChange::Changed => "changed",
            RoleChange::New => "new",
        }
    }
}

/// What the store says about an orchestration's current text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustVerdict {
    /// The current text is exactly the approved text.
    Approved,
    /// Nothing was ever approved for this orchestration of this project.
    NeverApproved,
    /// An approval exists, for different text. `roles` names each current role
    /// whose text differs or that is new; `removed` each approved role the
    /// orchestration no longer has. Both can be empty when only the roles'
    /// order moved.
    Changed {
        roles: Vec<(String, RoleChange)>,
        removed: Vec<String>,
    },
}

impl TrustVerdict {
    pub fn is_approved(&self) -> bool {
        matches!(self, TrustVerdict::Approved)
    }

    /// The wire / CLI word for this verdict.
    pub fn word(&self) -> &'static str {
        match self {
            TrustVerdict::Approved => "approved",
            TrustVerdict::NeverApproved => "never_approved",
            TrustVerdict::Changed { .. } => "changed",
        }
    }

    /// How `role` compares, when the verdict is [`TrustVerdict::Changed`].
    pub fn change_of(&self, role: &str) -> Option<RoleChange> {
        match self {
            TrustVerdict::Changed { roles, .. } => roles
                .iter()
                .find(|(name, _)| name == role)
                .map(|(_, change)| *change),
            _ => None,
        }
    }
}

impl ApprovalStore {
    pub fn find(&self, project: &Path, orchestration: &str) -> Option<&Approval> {
        self.approvals
            .iter()
            .find(|a| a.project == project && a.orchestration == orchestration)
    }

    /// Compare `current` (the orchestration's role texts now) with what was
    /// approved for `orchestration` of `project`.
    pub fn evaluate(
        &self,
        project: &Path,
        orchestration: &str,
        current: &[ApprovedRole],
    ) -> TrustVerdict {
        let Some(approval) = self.find(project, orchestration) else {
            return TrustVerdict::NeverApproved;
        };
        if approval.roles == current {
            return TrustVerdict::Approved;
        }
        let mut roles = Vec::new();
        for role in current {
            match approval.roles.iter().find(|r| r.name == role.name) {
                None => roles.push((role.name.clone(), RoleChange::New)),
                Some(approved) if approved != role => {
                    roles.push((role.name.clone(), RoleChange::Changed))
                }
                Some(_) => {}
            }
        }
        let removed = approval
            .roles
            .iter()
            .filter(|r| !current.iter().any(|c| c.name == r.name))
            .map(|r| r.name.clone())
            .collect();
        TrustVerdict::Changed { roles, removed }
    }

    /// Record `roles` as the approved text of `orchestration` of `project`,
    /// replacing any earlier approval of it.
    pub fn approve(
        &mut self,
        project: &Path,
        orchestration: &str,
        roles: Vec<ApprovedRole>,
        approved_at: chrono::DateTime<chrono::Utc>,
    ) {
        self.approvals
            .retain(|a| !(a.project == project && a.orchestration == orchestration));
        self.approvals.push(Approval {
            project: project.to_path_buf(),
            orchestration: orchestration.to_string(),
            approved_at: approved_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            roles,
        });
    }

    /// Remove the approvals of `project` — of `orchestration` only, when
    /// named. Answers how many were removed.
    pub fn revoke(&mut self, project: &Path, orchestration: Option<&str>) -> usize {
        let before = self.approvals.len();
        self.approvals.retain(|a| {
            !(a.project == project && orchestration.is_none_or(|name| a.orchestration == name))
        });
        before - self.approvals.len()
    }
}

/// SHA-256, hex, over exactly the text a client shows for `orchestration` of
/// `project`: the identity and every role's recorded text, in order. A client
/// that approves sends it back, and the daemon approves only when the text it
/// reads at that moment still hashes the same.
pub fn digest(project: &Path, orchestration: &str, roles: &[ApprovedRole]) -> String {
    #[derive(Serialize)]
    struct Shown<'a> {
        project: &'a Path,
        orchestration: &'a str,
        roles: &'a [ApprovedRole],
    }
    let bytes = serde_json::to_vec(&Shown {
        project,
        orchestration,
        roles,
    })
    .unwrap_or_default();
    let hash = Sha256::digest(&bytes);
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Project identity
// ---------------------------------------------------------------------------

/// The project a config in `config_dir` belongs to: see the module docs. Falls
/// back to the canonical `config_dir` whenever git cannot say (not a
/// repository, `git` missing, a linked worktree of a bare or relocated-store
/// repository) — which can only make two worktrees separate projects, never
/// make two projects one.
pub fn project_identity(config_dir: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(config_dir)?;
    // One verified geometry, not separate git answers: a `.git` file copied
    // from another checkout's linked worktree fails its back-pointer check
    // and keys the directory on itself (never approved).
    let Some(geometry) = crate::worktree_owner::verified_geometry_of(&canonical) else {
        return Ok(canonical);
    };
    match canonical.strip_prefix(&geometry.toplevel) {
        Ok(rel) if rel.as_os_str().is_empty() => Ok(geometry.main),
        Ok(rel) => Ok(geometry.main.join(rel)),
        Err(_) => Ok(canonical),
    }
}

/// The best spelling of `dir` for a store key when it may no longer exist
/// (revoking an approval whose project is gone): its identity when it
/// resolves, otherwise the path made absolute as written.
pub fn project_key_for_revoke(dir: &Path) -> PathBuf {
    match project_identity(dir) {
        Ok(project) => project,
        Err(_) => std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()),
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// [`TrustStore::use_state_dir_for_this_process`]'s setting.
static PROCESS_STATE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where the store lives: `<state_dir>/project-trust/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustStore {
    state_dir: PathBuf,
}

impl TrustStore {
    /// The store under `state_dir`. Public so a harness can pre-approve
    /// through the real store rather than around it.
    pub fn at(state_dir: impl Into<PathBuf>) -> Self {
        TrustStore {
            state_dir: state_dir.into(),
        }
    }

    /// The store every daemon check and the CLI use: under
    /// [`crate::config::state_dir`]. A unit-test process uses a private
    /// directory of its own instead, so no test reads or writes the
    /// developer's real approvals.
    pub fn default_location() -> Self {
        if let Some(dir) = PROCESS_STATE_DIR.get() {
            return TrustStore::at(dir.clone());
        }
        #[cfg(test)]
        {
            TrustStore::at(test_state_dir())
        }
        #[cfg(not(test))]
        {
            TrustStore::at(crate::config::state_dir())
        }
    }

    /// Point [`Self::default_location`] — and [`crate::schedule_outcome`]'s
    /// record beside it — at `state_dir` for the rest of this process. For a
    /// process that runs the daemon's code in-process and must not read or
    /// write the user's own approvals: a test harness. The first call wins;
    /// a later one answers the directory already in use. It changes WHERE
    /// approvals are kept, never whether a spawn is checked.
    pub fn use_state_dir_for_this_process(state_dir: PathBuf) -> Result<(), PathBuf> {
        PROCESS_STATE_DIR
            .set(state_dir)
            .map_err(|_| PROCESS_STATE_DIR.get().cloned().unwrap_or_default())
    }

    /// The state directory this store lives under.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The store file's path, for messages.
    pub fn file_path(&self) -> PathBuf {
        self.state_dir.join(STORE_DIR).join(STORE_FILE)
    }

    /// The owner-only store directory, created when `create`. `Ok(None)` when it
    /// does not exist and was not to be created.
    fn dir(&self, create: bool) -> io::Result<Option<TrustedDir>> {
        if create {
            std::fs::create_dir_all(&self.state_dir)?;
        } else if !self.state_dir.join(STORE_DIR).exists() {
            return Ok(None);
        }
        let anchor = TrustedDir::anchor(&self.state_dir)?;
        Ok(Some(anchor.child(STORE_DIR, create)?))
    }

    /// Read the store. A missing store is an empty one; an unreadable or
    /// unparsable one is an error, which every daemon check treats as "never
    /// approved".
    pub fn load(&self) -> io::Result<ApprovalStore> {
        match self.dir(false)? {
            None => Ok(ApprovalStore::default()),
            Some(dir) => read_store(&dir),
        }
    }

    /// Read, change and write the store under its exclusive lock, so two
    /// writers cannot lose each other's change. `change` answers whether to
    /// write.
    pub fn update<R>(&self, change: impl FnOnce(&mut ApprovalStore) -> (R, bool)) -> io::Result<R> {
        let dir = self
            .dir(true)?
            .ok_or_else(|| io::Error::other("the trust store directory could not be created"))?;
        let _lock = lock(&dir)?;
        let mut store = read_store(&dir)?;
        let (answer, write) = change(&mut store);
        if write {
            store.version = STORE_VERSION;
            write_store(&dir, &store)?;
        }
        Ok(answer)
    }
}

/// Read `name` in `dir` as bounded text; `Ok(None)` when it does not exist.
/// Shared with [`crate::schedule_outcome`].
pub(crate) fn read_bounded(dir: &TrustedDir, name: &str) -> io::Result<Option<String>> {
    let mut file = match dir.open_regular(name) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(MAX_STORE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_STORE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} is larger than any this build writes"),
        ));
    }
    Ok(Some(text))
}

fn read_store(dir: &TrustedDir) -> io::Result<ApprovalStore> {
    let Some(text) = read_bounded(dir, STORE_FILE)? else {
        return Ok(ApprovalStore::default());
    };
    let store: ApprovalStore =
        serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if store.version != STORE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "the trust store has format version {}, this build reads {STORE_VERSION}",
                store.version
            ),
        ));
    }
    Ok(store)
}

fn write_store(dir: &TrustedDir, store: &ApprovalStore) -> io::Result<()> {
    let text = serde_json::to_string_pretty(store).map_err(io::Error::other)?;
    write_replacing(dir, STORE_FILE, &text)
}

/// Replace `name` in `dir` whole with `text`: an owner-only temporary file,
/// `fsync`, rename, directory `fsync`. Shared with
/// [`crate::schedule_outcome`].
pub(crate) fn write_replacing(dir: &TrustedDir, name: &str, text: &str) -> io::Result<()> {
    let pending = format!(".pending-{}", std::process::id());
    let _ = dir.remove_file(&pending);
    let result = (|| {
        let mut file = dir.create_new_owner_only(&pending)?;
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        dir.rename(&pending, name)?;
        dir.sync()
    })();
    if result.is_err() {
        let _ = dir.remove_file(&pending);
    }
    result
}

/// Hold the store's exclusive lock until the returned file is dropped.
fn lock(dir: &TrustedDir) -> io::Result<std::fs::File> {
    lock_named(dir, LOCK_FILE)
}

/// Hold an exclusive lock on `name` in `dir` until the returned file is
/// dropped, creating it owner-only first. Shared with
/// [`crate::schedule_outcome`].
pub(crate) fn lock_named(dir: &TrustedDir, name: &str) -> io::Result<std::fs::File> {
    match dir.create_new_owner_only(name) {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let file = dir.open_regular(name)?;
    file.lock()?;
    Ok(file)
}

#[cfg(test)]
pub(crate) fn test_state_dir() -> PathBuf {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| crate::test_temp::tempdir().expect("a private trust-store directory"))
        .path()
        .to_path_buf()
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// The trust state of one orchestration of one config, with everything a
/// refusal or a prompt needs to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustCheck {
    /// [`project_identity`] of `config_dir`.
    pub project: PathBuf,
    /// Where the config was read.
    pub config_dir: PathBuf,
    pub orchestration: String,
    pub roles: Vec<ApprovedRole>,
    pub verdict: TrustVerdict,
}

impl TrustCheck {
    /// Evaluate `orchestration` of the config in `config_dir` against `store`'s
    /// contents as already read.
    pub fn evaluate(
        store: &ApprovalStore,
        project: PathBuf,
        config_dir: &Path,
        orchestration: &OrchestrationConfig,
    ) -> Self {
        let roles = role_texts(orchestration);
        let verdict = store.evaluate(&project, &orchestration.name, &roles);
        TrustCheck {
            project,
            config_dir: config_dir.to_path_buf(),
            orchestration: orchestration.name.clone(),
            roles,
            verdict,
        }
    }

    /// [`digest`] of this check's text.
    pub fn digest(&self) -> String {
        digest(&self.project, &self.orchestration, &self.roles)
    }

    /// The refusal an unattended path reports, or `None` when approved.
    pub fn refusal(&self) -> Option<String> {
        if self.verdict.is_approved() {
            return None;
        }
        Some(refusal_text(
            &self.project,
            &self.config_dir,
            &self.orchestration,
            &self.verdict,
        ))
    }
}

/// The refusal text: names the orchestration, the project, whether it was never
/// approved or changed since, and the exact command that approves it.
///
/// Escaped for display ([`crate::untrusted_text::escape_control_and_bidi`]):
/// every name and path in it comes from the project's config, and the text
/// reaches the scheduler's stderr, the daemon log and agent-verb diagnostics,
/// so a control or bidi character in it is shown, never obeyed. The raw names
/// stay in [`TrustCheck`] for identity; for an ordinary name the escaping
/// changes nothing and the approve command stays copy-pasteable.
pub fn refusal_text(
    project: &Path,
    config_dir: &Path,
    orchestration: &str,
    verdict: &TrustVerdict,
) -> String {
    let state = match verdict {
        TrustVerdict::Approved => return String::new(),
        TrustVerdict::NeverApproved => "was never approved".to_string(),
        TrustVerdict::Changed { roles, removed } => {
            let mut named: Vec<String> = roles
                .iter()
                .map(|(name, change)| format!("{name} ({})", change.word()))
                .collect();
            named.extend(removed.iter().map(|name| format!("{name} (removed)")));
            if named.is_empty() {
                "changed since you approved it (the roles' order moved)".to_string()
            } else {
                format!(
                    "changed since you approved it (roles: {})",
                    named.join(", ")
                )
            }
        }
    };
    crate::untrusted_text::escape_control_and_bidi(&format!(
        "orchestration '{orchestration}' of project {} {state}, so nothing from it was \
         started. Review and approve it from a terminal outside the deck with: {}",
        project.display(),
        approve_command(config_dir, orchestration)
    ))
}

/// `dot-agent-deck trust approve <dir> --orchestration <name>`, each argument
/// quoted for a POSIX shell when it needs it.
pub fn approve_command(config_dir: &Path, orchestration: &str) -> String {
    format!(
        "{APPROVE_VERB} {} --orchestration {}",
        crate::platform::paths::shell_quote_if_needed(&config_dir.to_string_lossy()),
        crate::platform::paths::shell_quote_if_needed(orchestration)
    )
}

/// The one check every daemon-resolved unattended spawn makes: re-read the
/// store, resolve `config_dir`'s project and evaluate `orchestration`.
/// Blocking — it reads a file and may run `git`; call it off the async
/// runtime's threads. A store that cannot be read is treated as holding no
/// approvals, and logged.
pub fn check_blocking(
    store: &TrustStore,
    config_dir: &Path,
    orchestration: &OrchestrationConfig,
) -> TrustCheck {
    let approvals = load_or_empty(store);
    let project = project_identity(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    TrustCheck::evaluate(&approvals, project, config_dir, orchestration)
}

/// [`TrustStore::load`], or an empty store (and a warning) when it fails.
pub fn load_or_empty(store: &TrustStore) -> ApprovalStore {
    match store.load() {
        Ok(approvals) => approvals,
        Err(error) => {
            tracing::warn!(
                path = %store.file_path().display(),
                %error,
                "the project trust store could not be read; treating every orchestration as \
                 not approved"
            );
            ApprovalStore::default()
        }
    }
}

/// [`check_blocking`] for the orchestration named `name` in `config` (read
/// from `config_dir`), or `None` when the config has no such orchestration.
pub fn check_named_blocking(
    store: &TrustStore,
    config_dir: &Path,
    config: &ProjectConfig,
    name: &str,
) -> Option<TrustCheck> {
    let orchestration = config.orchestrations.iter().find(|o| o.name == name)?;
    Some(check_blocking(store, config_dir, orchestration))
}

/// Log a refusal where the daemon log carries it, at warn.
pub fn log_refusal(path: &str, check: &TrustCheck) {
    if let Some(refusal) = check.refusal() {
        tracing::warn!(
            path,
            project = %crate::untrusted_text::escape_control_and_bidi(
                &check.project.display().to_string()
            ),
            orchestration = %crate::untrusted_text::escape_control_and_bidi(&check.orchestration),
            verdict = check.verdict.word(),
            "{refusal}"
        );
    }
}

/// What approving with a digest did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveOutcome {
    /// Recorded; the text matched the digest.
    Approved,
    /// The text read now does not hash to the digest the approver was shown,
    /// so nothing was recorded.
    Stale,
}

/// Approve `check`'s orchestration — but only after re-reading its config
/// under the store's lock and confirming the text still hashes to
/// `expected_digest`, so what is recorded is exactly what the approver saw.
pub fn approve_if_unchanged(
    store: &TrustStore,
    config_dir: &Path,
    orchestration: &str,
    expected_digest: &str,
) -> Result<ApproveOutcome, TrustError> {
    approve_if_unchanged_with(
        store,
        config_dir,
        orchestration,
        expected_digest,
        &read_config,
    )
}

/// [`approve_if_unchanged`], reading the config through `read` — the daemon
/// passes its bounded, symlink-safe reader for a path a client selected.
pub fn approve_if_unchanged_with(
    store: &TrustStore,
    config_dir: &Path,
    orchestration: &str,
    expected_digest: &str,
    read: &dyn Fn(&Path) -> Result<ProjectConfig, TrustError>,
) -> Result<ApproveOutcome, TrustError> {
    store
        .update(|approvals| {
            let fresh = match read(config_dir)
                .and_then(|config| inspect_config(approvals, config_dir, &config, orchestration))
            {
                Ok(fresh) => fresh,
                Err(e) => return (Err(e), false),
            };
            if fresh.digest() != expected_digest {
                return (Ok(ApproveOutcome::Stale), false);
            }
            approvals.approve(
                &fresh.project,
                &fresh.orchestration,
                fresh.roles,
                chrono::Utc::now(),
            );
            (Ok(ApproveOutcome::Approved), true)
        })
        .map_err(TrustError::Store)?
}

/// What approving several orchestrations as one batch did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchApproveOutcome {
    /// Every one was recorded; each text matched its digest.
    Approved,
    /// These orchestrations no longer hash to the digest the approver was
    /// shown, so nothing in the batch was recorded.
    Stale(Vec<String>),
}

/// Approve every `(config_dir, orchestration, expected_digest)` in `batch` as
/// one change: all of them are re-read and compared under the store's lock,
/// and either every one is recorded in a single store write or, when any of
/// them changed or cannot be read, none is. One answer that approved several
/// orchestrations therefore never leaves some of them approved.
pub fn approve_batch_if_unchanged(
    store: &TrustStore,
    batch: &[(&Path, &str, &str)],
) -> Result<BatchApproveOutcome, TrustError> {
    store
        .update(|approvals| {
            let mut fresh = Vec::with_capacity(batch.len());
            let mut stale = Vec::new();
            for &(config_dir, orchestration, expected_digest) in batch {
                let check = match read_config(config_dir).and_then(|config| {
                    inspect_config(approvals, config_dir, &config, orchestration)
                }) {
                    Ok(check) => check,
                    Err(e) => return (Err(e), false),
                };
                if check.digest() != expected_digest {
                    stale.push(orchestration.to_string());
                }
                fresh.push(check);
            }
            if !stale.is_empty() {
                return (Ok(BatchApproveOutcome::Stale(stale)), false);
            }
            let now = chrono::Utc::now();
            for check in fresh {
                approvals.approve(&check.project, &check.orchestration, check.roles, now);
            }
            (Ok(BatchApproveOutcome::Approved), true)
        })
        .map_err(TrustError::Store)?
}

/// Why [`inspect`] or an approval could not answer.
#[derive(Debug)]
pub enum TrustError {
    /// No `.dot-agent-deck.toml` in the directory.
    NoConfig(PathBuf),
    /// The config could not be read or parsed.
    Config(String),
    /// The config has no orchestration of that name.
    NoOrchestration { config_dir: PathBuf, name: String },
    /// The directory or the store could not be read or written.
    Store(io::Error),
}

impl std::fmt::Display for TrustError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustError::NoConfig(dir) => write!(
                f,
                "{} has no {}",
                dir.display(),
                crate::project_config::CONFIG_FILE_NAME
            ),
            TrustError::Config(e) => write!(f, "{e}"),
            TrustError::NoOrchestration { config_dir, name } => write!(
                f,
                "{} declares no orchestration named '{name}'",
                config_dir
                    .join(crate::project_config::CONFIG_FILE_NAME)
                    .display()
            ),
            TrustError::Store(e) => write!(f, "the project trust store: {e}"),
        }
    }
}

impl std::error::Error for TrustError {}

/// Read the config in `config_dir`.
pub fn read_config(config_dir: &Path) -> Result<ProjectConfig, TrustError> {
    match crate::project_config::load_project_config(config_dir) {
        Ok(Some(config)) => Ok(config),
        Ok(None) => Err(TrustError::NoConfig(config_dir.to_path_buf())),
        Err(e) => Err(TrustError::Config(e.to_string())),
    }
}

/// The trust state of `orchestration` in the config in `config_dir`, read now.
pub fn inspect(
    store: &TrustStore,
    config_dir: &Path,
    orchestration: &str,
) -> Result<TrustCheck, TrustError> {
    let approvals = store.load().map_err(TrustError::Store)?;
    let config = read_config(config_dir)?;
    inspect_config(&approvals, config_dir, &config, orchestration)
}

/// The trust state of `orchestration` in `config` (read from `config_dir`)
/// against `approvals`.
pub fn inspect_config(
    approvals: &ApprovalStore,
    config_dir: &Path,
    config: &ProjectConfig,
    orchestration: &str,
) -> Result<TrustCheck, TrustError> {
    let project = project_identity(config_dir).map_err(TrustError::Store)?;
    let orch = config
        .orchestrations
        .iter()
        .find(|o| o.name == orchestration)
        .ok_or_else(|| TrustError::NoOrchestration {
            config_dir: config_dir.to_path_buf(),
            name: orchestration.to_string(),
        })?;
    Ok(TrustCheck::evaluate(approvals, project, config_dir, orch))
}

/// What a client is shown before it asks the user to approve an orchestration
/// (`ProjectTrustState` on the attach socket): the full text of every role, how
/// each compares with the stored approval, and [`digest`] of exactly that text,
/// which `ApproveProjectTrust` carries back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectTrustView {
    /// [`project_identity`] of the config directory.
    pub project: String,
    /// The directory the config was read from.
    pub config_dir: String,
    pub orchestration: String,
    /// `approved`, `never_approved` or `changed`.
    pub state: String,
    pub roles: Vec<TrustRoleView>,
    /// Roles the approval had that the orchestration no longer has.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_roles: Vec<String>,
    /// [`digest`] of the text in `roles`, for `project`/`orchestration`.
    pub digest: String,
    /// The [`crate::project_resolve::config_revision`] of the config bytes
    /// this view was read from, so a client that approves the text shown can
    /// launch with the revision of that same read rather than one it listed
    /// earlier. `None` from [`TrustCheck::view`], which has no bytes; the
    /// daemon's trust replies fill it in. Additive and optional on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_revision: Option<String>,
}

/// One role in a [`ProjectTrustView`]: its recorded text and how it compares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustRoleView {
    #[serde(flatten)]
    pub text: ApprovedRole,
    /// `changed` or `new` relative to the stored approval; absent when the role
    /// is unchanged or nothing was ever approved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<RoleChange>,
}

impl TrustCheck {
    /// This check as a client is shown it.
    pub fn view(&self) -> ProjectTrustView {
        ProjectTrustView {
            project: self.project.display().to_string(),
            config_dir: self.config_dir.display().to_string(),
            orchestration: self.orchestration.clone(),
            state: self.verdict.word().to_string(),
            roles: self
                .roles
                .iter()
                .map(|role| TrustRoleView {
                    text: role.clone(),
                    change: self.verdict.change_of(&role.name),
                })
                .collect(),
            removed_roles: match &self.verdict {
                TrustVerdict::Changed { removed, .. } => removed.clone(),
                _ => Vec::new(),
            },
            digest: self.digest(),
            config_revision: None,
        }
    }
}

/// The names of every orchestration `config` declares, each once, in order.
pub fn orchestration_names(config: &ProjectConfig) -> Vec<String> {
    let mut seen = BTreeSet::new();
    config
        .orchestrations
        .iter()
        .filter(|o| seen.insert(o.name.clone()))
        .map(|o| o.name.clone())
        .collect()
}

/// Approve every orchestration of the config in `config_dir` as it reads now,
/// in `store` — what `trust approve <dir> --yes` does, without the printing.
/// Answers the names approved. For a harness preparing a fixture whose subject
/// is not the approval itself.
pub fn approve_all(store: &TrustStore, config_dir: &Path) -> Result<Vec<String>, TrustError> {
    let config = read_config(config_dir)?;
    let names = orchestration_names(&config);
    for name in &names {
        let check = inspect(store, config_dir, name)?;
        if approve_if_unchanged(store, config_dir, name, &check.digest())?
            != ApproveOutcome::Approved
        {
            return Err(TrustError::Config(format!(
                "orchestration '{name}' changed while it was being approved"
            )));
        }
    }
    Ok(names)
}

/// Test-only: [`approve_all`] in the unit-test process's own store — for
/// fixtures of paths this PRD gated whose subject is not the gate.
#[cfg(test)]
pub(crate) fn approve_all_for_test(config_dir: &Path) {
    approve_all(&TrustStore::default_location(), config_dir)
        .expect("fixture precondition: approve the fixture config");
}

/// Test-only: approve `orchestration`, as it is held in memory, for the
/// project of `config_dir` in the unit-test process's own store — for a UI
/// fixture whose orchestration is never written to disk and whose subject is
/// not the gate.
#[cfg(test)]
pub(crate) fn approve_orchestration_for_test(
    config_dir: &Path,
    orchestration: &OrchestrationConfig,
) {
    let project = project_identity(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
    TrustStore::default_location()
        .update(|approvals| {
            approvals.approve(
                &project,
                &orchestration.name,
                role_texts(orchestration),
                chrono::Utc::now(),
            );
            ((), true)
        })
        .expect("fixture precondition: approve the in-memory orchestration");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, text: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(crate::project_config::CONFIG_FILE_NAME), text).unwrap();
    }

    const TEAM: &str = r#"
# a comment
[[orchestrations]]
name = "team"

[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
prompt_template = "You lead."

[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"
description = "Writes code"
prompt_template = "You code."

[[orchestrations]]
name = "other"

[[orchestrations.roles]]
name = "solo"
command = "cat"
start = true
"#;

    fn approve_now(store: &TrustStore, dir: &Path, name: &str) {
        let check = inspect(store, dir, name).unwrap();
        assert_eq!(
            approve_if_unchanged(store, dir, name, &check.digest()).unwrap(),
            ApproveOutcome::Approved
        );
    }

    fn verdict(store: &TrustStore, dir: &Path, name: &str) -> TrustVerdict {
        inspect(store, dir, name).unwrap().verdict
    }

    fn fixture() -> (tempfile::TempDir, TrustStore, PathBuf) {
        let scratch = crate::test_temp::tempdir().unwrap();
        let store = TrustStore::at(scratch.path().join("state"));
        let project = scratch.path().join("project");
        write_config(&project, TEAM);
        (scratch, store, project)
    }

    #[test]
    fn a_never_approved_orchestration_reads_never_approved_and_refuses() {
        let (_scratch, store, project) = fixture();
        let check = inspect(&store, &project, "team").unwrap();
        assert_eq!(check.verdict, TrustVerdict::NeverApproved);
        let refusal = check
            .refusal()
            .expect("an unapproved orchestration is refused");
        assert!(refusal.contains("'team'"), "{refusal}");
        assert!(refusal.contains("never approved"), "{refusal}");
        assert!(
            refusal.contains(&check.project.display().to_string()),
            "{refusal}"
        );
        assert!(
            refusal.contains(&format!(
                "dot-agent-deck trust approve {} --orchestration team",
                project.display()
            )),
            "{refusal}"
        );
    }

    #[test]
    fn approving_makes_it_approved_and_a_one_character_change_voids_it() {
        let (_scratch, store, project) = fixture();
        approve_now(&store, &project, "team");
        assert_eq!(verdict(&store, &project, "team"), TrustVerdict::Approved);
        assert!(
            inspect(&store, &project, "team")
                .unwrap()
                .refusal()
                .is_none()
        );

        write_config(&project, &TEAM.replace("agent-coder", "agent-coden"));
        let check = inspect(&store, &project, "team").unwrap();
        assert_eq!(
            check.verdict,
            TrustVerdict::Changed {
                roles: vec![("coder".into(), RoleChange::Changed)],
                removed: vec![],
            }
        );
        let refusal = check.refusal().unwrap();
        assert!(
            refusal.contains("changed since you approved it"),
            "{refusal}"
        );
        assert!(refusal.contains("coder (changed)"), "{refusal}");
    }

    #[test]
    fn every_recorded_field_voids_the_approval_when_it_changes() {
        for (from, to) in [
            ("You lead.", "You lead!"),
            ("Writes code", "Writes codes"),
            (
                "start = true\nprompt_template = \"You lead.\"",
                "prompt_template = \"You lead.\"",
            ),
            (
                "command = \"claude\"",
                "command = \"claude\"\nagent = \"codex\"",
            ),
            (
                "description = \"Writes code\"",
                "description = \"Writes code\"\nclear = false",
            ),
            ("name = \"coder\"", "name = \"coders\""),
        ] {
            let (_scratch, store, project) = fixture();
            approve_now(&store, &project, "team");
            assert!(TEAM.contains(from), "fixture precondition: {from:?}");
            write_config(&project, &TEAM.replacen(from, to, 1));
            assert!(
                !verdict(&store, &project, "team").is_approved(),
                "changing {from:?} to {to:?} must void the approval"
            );
        }
    }

    #[test]
    fn comments_unrelated_keys_and_other_orchestrations_do_not_void_it() {
        let (_scratch, store, project) = fixture();
        approve_now(&store, &project, "team");
        let edited = TEAM
            .replace("# a comment", "# a different comment\n# and another")
            .replace(
                "name = \"coder\"",
                "name = \"coder\"\nsubscribe = [\"news\"]",
            )
            .replace("command = \"cat\"", "command = \"cat -v\"");
        let edited = format!("worker_response_timeout_minutes = 30\n{edited}");
        write_config(&project, &edited);
        assert_eq!(verdict(&store, &project, "team"), TrustVerdict::Approved);
        assert!(
            !verdict(&store, &project, "other").is_approved(),
            "control: the other orchestration was never approved"
        );
    }

    #[test]
    fn approving_one_orchestration_does_not_approve_another() {
        let (_scratch, store, project) = fixture();
        approve_now(&store, &project, "other");
        assert_eq!(
            verdict(&store, &project, "team"),
            TrustVerdict::NeverApproved
        );
        assert_eq!(verdict(&store, &project, "other"), TrustVerdict::Approved);
    }

    #[test]
    fn extends_is_resolved_before_the_text_is_recorded() {
        let (_scratch, store, project) = fixture();
        let base = r#"
[[orchestrations]]
name = "base"
[[orchestrations.roles]]
name = "orchestrator"
command = "claude"
start = true
[[orchestrations.roles]]
name = "coder"
command = "devbox run agent-coder"

[[orchestrations]]
name = "derived"
extends = "base"
[[orchestrations.roles]]
name = "coder"
prompt_template = "Be brief."
"#;
        write_config(&project, base);
        let check = inspect(&store, &project, "derived").unwrap();
        let coder = check.roles.iter().find(|r| r.name == "coder").unwrap();
        assert_eq!(
            coder.command, "devbox run agent-coder",
            "the inherited command is part of what is approved"
        );
        approve_now(&store, &project, "derived");
        write_config(
            &project,
            &base.replace("devbox run agent-coder", "devbox run agent-evil"),
        );
        assert_eq!(
            verdict(&store, &project, "derived"),
            TrustVerdict::Changed {
                roles: vec![("coder".into(), RoleChange::Changed)],
                removed: vec![],
            },
            "a change in the base reaches every orchestration that extends it"
        );
    }

    #[test]
    fn new_and_removed_roles_are_named() {
        let (_scratch, store, project) = fixture();
        approve_now(&store, &project, "team");
        let edited = TEAM.replace(
            "[[orchestrations]]\nname = \"other\"",
            "[[orchestrations.roles]]\nname = \"tester\"\ncommand = \"cat\"\n\n\
             [[orchestrations]]\nname = \"other\"",
        );
        write_config(&project, &edited);
        assert_eq!(
            verdict(&store, &project, "team"),
            TrustVerdict::Changed {
                roles: vec![("tester".into(), RoleChange::New)],
                removed: vec![],
            }
        );
        write_config(
            &project,
            &TEAM.replace("name = \"coder\"", "name = \"writer\""),
        );
        let TrustVerdict::Changed { roles, removed } = verdict(&store, &project, "team") else {
            panic!("a renamed role is a change");
        };
        assert_eq!(roles, vec![("writer".into(), RoleChange::New)]);
        assert_eq!(removed, vec!["coder".to_string()]);
    }

    #[test]
    fn a_stale_digest_records_nothing() {
        let (_scratch, store, project) = fixture();
        let shown = inspect(&store, &project, "team").unwrap();
        write_config(&project, &TEAM.replace("You code.", "You code, and more."));
        assert_eq!(
            approve_if_unchanged(&store, &project, "team", &shown.digest()).unwrap(),
            ApproveOutcome::Stale
        );
        assert_eq!(
            verdict(&store, &project, "team"),
            TrustVerdict::NeverApproved
        );
    }

    #[test]
    fn the_store_round_trips_and_is_owner_only() {
        let (_scratch, store, project) = fixture();
        approve_now(&store, &project, "team");
        approve_now(&store, &project, "other");
        let loaded = store.load().unwrap();
        assert_eq!(loaded.approvals.len(), 2);
        let text = std::fs::read_to_string(store.file_path()).unwrap();
        let parsed: ApprovalStore = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, loaded);
        assert!(
            text.contains("devbox run agent-coder") && text.contains("You code."),
            "the store keeps the approved text itself"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let file_mode = std::fs::metadata(store.file_path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(file_mode & 0o077, 0, "the store file is owner-only");
            let dir_mode = std::fs::metadata(store.file_path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(dir_mode & 0o077, 0, "the store directory is owner-only");
        }
        // Revoking one leaves the other.
        let project_key = project_identity(&project).unwrap();
        let removed = store
            .update(|s| {
                let n = s.revoke(&project_key, Some("team"));
                (n, n > 0)
            })
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(
            verdict(&store, &project, "team"),
            TrustVerdict::NeverApproved
        );
        assert_eq!(verdict(&store, &project, "other"), TrustVerdict::Approved);
    }

    #[test]
    fn an_unreadable_store_is_treated_as_holding_no_approvals() {
        let (_scratch, store, project) = fixture();
        approve_now(&store, &project, "team");
        std::fs::write(store.file_path(), "not json").unwrap();
        let config = read_config(&project).unwrap();
        let check = check_named_blocking(&store, &project, &config, "team").unwrap();
        assert_eq!(check.verdict, TrustVerdict::NeverApproved);
        assert!(
            approve_if_unchanged(&store, &project, "team", &check.digest()).is_err(),
            "an approval never overwrites a store it cannot read"
        );
    }

    fn git(dir: &Path, sandbox: &Path, args: &[&str]) {
        let out = crate::git_env::fixture_git(dir, sandbox)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "fixture precondition: git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_linked_worktree_is_keyed_on_its_main_checkout() {
        let scratch = crate::test_temp::tempdir().unwrap();
        let store = TrustStore::at(scratch.path().join("state"));
        let repo = scratch.path().join("repo");
        write_config(&repo, TEAM);
        git(&repo, scratch.path(), &["init", "--quiet"]);
        crate::worktree_owner::pin_fixture_eol(&repo, scratch.path());
        git(&repo, scratch.path(), &["add", "."]);
        git(&repo, scratch.path(), &["commit", "--quiet", "-m", "init"]);
        let linked = scratch.path().join("repo-unit");
        git(
            &repo,
            scratch.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "unit",
                &linked.to_string_lossy(),
            ],
        );
        let main = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(project_identity(&linked).unwrap(), main);
        assert_eq!(project_identity(&repo).unwrap(), main);

        approve_now(&store, &repo, "team");
        assert_eq!(
            verdict(&store, &linked, "team"),
            TrustVerdict::Approved,
            "a dispatched unit's worktree shares its main checkout's approval"
        );
        write_config(&linked, &TEAM.replace("claude", "claude --yolo"));
        assert!(
            !verdict(&store, &linked, "team").is_approved(),
            "but only for the same text"
        );

        // A config in a subdirectory is its own project.
        let sub = repo.join("sub");
        write_config(&sub, TEAM);
        assert_eq!(project_identity(&sub).unwrap(), main.join("sub"));
        assert_eq!(verdict(&store, &sub, "team"), TrustVerdict::NeverApproved);

        // A `.git` file copied from the linked worktree into an unrelated
        // directory makes git call that directory part of the repository, but
        // the administrative directory points back at the real worktree, so
        // the copy is keyed on itself and borrows nothing.
        let forged = scratch.path().join("forged");
        write_config(&forged, TEAM);
        std::fs::copy(linked.join(".git"), forged.join(".git")).unwrap();
        assert_eq!(
            project_identity(&forged).unwrap(),
            std::fs::canonicalize(&forged).unwrap()
        );
        assert_eq!(
            verdict(&store, &forged, "team"),
            TrustVerdict::NeverApproved
        );
    }

    #[test]
    fn a_directory_outside_git_is_its_own_canonical_path() {
        let (_scratch, _store, project) = fixture();
        assert_eq!(
            project_identity(&project).unwrap(),
            std::fs::canonicalize(&project).unwrap()
        );
    }

    #[test]
    fn the_digest_covers_identity_name_and_every_role() {
        let roles = vec![ApprovedRole {
            name: "a".into(),
            command: "cat".into(),
            agent: None,
            start: true,
            clear: true,
            description: None,
            prompt_template: None,
        }];
        let base = digest(Path::new("/p"), "o", &roles);
        assert_eq!(base.len(), 64);
        assert_ne!(base, digest(Path::new("/q"), "o", &roles));
        assert_ne!(base, digest(Path::new("/p"), "x", &roles));
        let mut other = roles.clone();
        other[0].command = "cat ".into();
        assert_ne!(base, digest(Path::new("/p"), "o", &other));
    }

    #[test]
    fn the_approve_command_quotes_what_a_shell_would_split() {
        assert_eq!(
            approve_command(Path::new("/a b"), "my team"),
            "dot-agent-deck trust approve '/a b' --orchestration 'my team'"
        );
    }
}
