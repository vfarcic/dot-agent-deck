//! Issue #1350 — the desktop's side of the deck list it shares with the CLI.
//!
//! The remote decks live in `remotes.toml`, read and written through
//! [`dot_agent_deck::deck_list`]; `desktop.toml` keeps what describes the app
//! (appearance, zoom, which deck is selected). This module translates between
//! the two shapes:
//!
//! - a registry row ([`RemoteEntry`]) becomes a [`RemoteEndpointSettings`] —
//!   the row type the rest of the desktop already works with, so the webview,
//!   the fleet view and the tunnels are unchanged;
//! - a save becomes a list of one-row edits ([`edits`]), each applied against
//!   a fresh read of the file ([`apply`]), so a deck `remote add` wrote while
//!   the app was open is never overwritten by the list the app loaded;
//! - the rows a pre-#1350 build kept in `desktop.toml` are merged into the
//!   registry once ([`migrate`]).
//!
//! # The field names do not cross
//!
//! The registry calls the key file `key`; the desktop calls it `identity`,
//! because `key` trips the settings credential tripwire
//! (`no_settings_key_name_trips_the_credential_tripwire`). The rename happens
//! here, between two typed values, so no `key` name ever reaches the settings
//! document or the webview. Both are paths — never key material.

use std::path::{Path, PathBuf};

use dot_agent_deck::deck_list::{self, DeckRef};
use dot_agent_deck::remote::{RemoteConfigError, RemoteEntry, RemotesFile};
use dot_agent_deck::remote_tunnel::{
    HostAlias, Hostname, KeyPath, RemoteSocketPath, SshPort, SshUser,
};

use crate::settings::{EndpointId, RemoteEndpointSettings};

/// The registry file — the CLI's own resolver, so the two clients cannot
/// disagree about which file is the deck list (it honours
/// `DOT_AGENT_DECK_REMOTES` exactly as `connect` does).
pub fn remotes_path() -> PathBuf {
    dot_agent_deck::remote::default_remotes_path()
}

/// Every deck in the registry at `path` that the desktop can represent, in file
/// order.
///
/// A row it cannot represent is **skipped and logged, never refused**: the file
/// is the CLI's too, and a row the CLI accepts — a `kubernetes` entry, a host
/// alias with a character the desktop's ssh validation refuses, a relative key
/// path — must not hide every other deck. The skipped row stays in the file
/// untouched, since every desktop edit names a row by id and the desktop never
/// holds this one's. A second row claiming an id already seen is skipped the
/// same way, so the webview never holds two rows with one id.
pub fn load_rows(path: &Path) -> Result<Vec<RemoteEndpointSettings>, RemoteConfigError> {
    let file = RemotesFile::load(path)?;
    let mut rows: Vec<RemoteEndpointSettings> = Vec::with_capacity(file.remotes.len());
    for (index, entry) in file.remotes.iter().enumerate() {
        match row_from_entry(entry) {
            Ok(row) if rows.iter().any(|seen| seen.id == row.id) => eprintln!(
                "desktop decks: skipping row {} of {}: its id is already used by an earlier row",
                index + 1,
                path.display()
            ),
            Ok(row) => rows.push(row),
            // The row number rather than its name: the name is free text from
            // a file the CLI accepted anything into.
            Err(reason) => eprintln!(
                "desktop decks: skipping row {} of {}: {reason}",
                index + 1,
                path.display()
            ),
        }
    }
    Ok(rows)
}

/// One registry row as a desktop row, or why it cannot be one.
///
/// Every field goes through the same validating newtype a settings form
/// applies, so a hand-edited `remotes.toml` cannot smuggle past what
/// `desktop.toml` would have refused.
pub fn row_from_entry(entry: &RemoteEntry) -> Result<RemoteEndpointSettings, String> {
    if entry.kind != "ssh" {
        return Err("it is not an ssh deck".to_string());
    }
    let (user, host) = deck_list::login_and_host(entry);
    let invalid = |field: &str, error: &dyn std::fmt::Display| format!("its {field} is {error}");
    Ok(RemoteEndpointSettings {
        host: Hostname::parse(host).map_err(|error| invalid("host", &error))?,
        id: EndpointId::parse(&deck_list::deck_id(entry)).map_err(|error| invalid("id", &error))?,
        identity: entry
            .key
            .as_deref()
            .map(KeyPath::parse)
            .transpose()
            .map_err(|error| invalid("key", &error))?,
        jump: entry
            .jump_host
            .as_deref()
            .map(HostAlias::parse)
            .transpose()
            .map_err(|error| invalid("jump_host", &error))?,
        port: SshPort::parse(entry.port).map_err(|error| invalid("port", &error))?,
        socket: entry
            .socket
            .as_deref()
            .map(RemoteSocketPath::parse)
            .transpose()
            .map_err(|error| invalid("socket", &error))?,
        user: user
            .map(SshUser::parse)
            .transpose()
            .map_err(|error| invalid("user", &error))?,
    })
}

/// A new registry row for a deck the desktop added.
///
/// Carries everything an older CLI requires of a row — `type`, `version`,
/// `added_at` — with [`deck_list::UNMANAGED_VERSION`] as the version, since no
/// binary was installed by `remote add`. The name is derived from the host
/// (and user) and unique among `taken`; the desktop takes no free-form label.
fn new_entry(row: &RemoteEndpointSettings, taken: &[RemoteEntry]) -> RemoteEntry {
    let user = row.user.as_ref().map(SshUser::as_str);
    let (host, user_field) = deck_list::host_fields(row.host.as_str(), user);
    RemoteEntry {
        name: deck_list::derive_deck_name(row.host.as_str(), user, |name| {
            taken.iter().any(|entry| entry.name == name)
        }),
        kind: "ssh".to_string(),
        host,
        port: row.port.get(),
        key: row.identity.as_ref().map(|key| key.as_str().to_string()),
        version: deck_list::UNMANAGED_VERSION.to_string(),
        added_at: deck_list::timestamp_now(),
        upgraded_at: None,
        last_connected: None,
        id: Some(row.id.as_str().to_string()),
        user: user_field,
        jump_host: row.jump.as_ref().map(|jump| jump.as_str().to_string()),
        socket: row
            .socket
            .as_ref()
            .map(|socket| socket.as_str().to_string()),
    }
}

/// Write onto `entry` the fields that differ between `before` and `after` —
/// or, with no `before`, every desktop field of `after`.
///
/// Field by field rather than row by row, so a `remote upgrade` or a hand edit
/// of a field the desktop did not touch survives the desktop's save of another.
/// The CLI-only fields (`name`, `version`, `added_at`, …) are never written.
fn apply_fields(
    entry: &mut RemoteEntry,
    before: Option<&RemoteEndpointSettings>,
    after: &RemoteEndpointSettings,
) {
    let changed = |same: bool| before.is_none() || !same;
    let before_host = before.map(|row| &row.host);
    let before_user = before.map(|row| &row.user);
    let host_changed = changed(before_host == Some(&after.host));
    let user_changed = changed(before_user == Some(&after.user));
    if host_changed || user_changed {
        let (current_user, current_host) = deck_list::login_and_host(entry);
        let host = if host_changed {
            after.host.as_str().to_string()
        } else {
            current_host.to_string()
        };
        let user = if user_changed {
            after.user.as_ref().map(|user| user.as_str().to_string())
        } else {
            current_user.map(str::to_string)
        };
        let (host, user) = deck_list::host_fields(&host, user.as_deref());
        entry.host = host;
        entry.user = user;
    }
    if changed(before.map(|row| row.port) == Some(after.port)) {
        entry.port = after.port.get();
    }
    if changed(before.map(|row| &row.identity) == Some(&after.identity)) {
        entry.key = after.identity.as_ref().map(|key| key.as_str().to_string());
    }
    if changed(before.map(|row| &row.jump) == Some(&after.jump)) {
        entry.jump_host = after.jump.as_ref().map(|jump| jump.as_str().to_string());
    }
    if changed(before.map(|row| &row.socket) == Some(&after.socket)) {
        entry.socket = after
            .socket
            .as_ref()
            .map(|socket| socket.as_str().to_string());
    }
}

/// One change the user made to the deck list, as the registry receives it.
///
/// An update and a removal carry the row **as the base held it**, not just its
/// id: the id alone does not say the row on disk is still that deck (see
/// [`apply`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeckEdit {
    Add(RemoteEndpointSettings),
    Update {
        before: RemoteEndpointSettings,
        after: RemoteEndpointSettings,
    },
    Remove(RemoteEndpointSettings),
}

/// Why [`apply`] published nothing.
#[derive(Debug)]
pub enum ApplyError {
    /// The registry could not be read, locked, or written.
    Config(RemoteConfigError),
    /// A row an update or a removal was aimed at is, on disk, no longer the
    /// deck the edit was made against — see [`apply`].
    Conflict,
}

impl From<RemoteConfigError> for ApplyError {
    fn from(error: RemoteConfigError) -> Self {
        Self::Config(error)
    }
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Conflict => f.write_str(
                "a deck this save changes or removes is no longer the deck it was made against",
            ),
        }
    }
}

impl std::error::Error for ApplyError {}

/// What an update or a removal checks the row on disk against: the address
/// the base row reached — the same key [`deck_list::address_key`] computes for
/// a registry row, so a row loaded by [`row_from_entry`] matches its entry.
fn base_address(row: &RemoteEndpointSettings) -> (String, Option<String>, u16) {
    (
        row.host.as_str().to_ascii_lowercase(),
        row.user.as_ref().map(|user| user.as_str().to_string()),
        row.port.get(),
    )
}

/// What changed between the rows an edit was made against (`base`) and the
/// rows after it (`next`), matched by id. An unchanged row yields nothing, so
/// a save that only changed the theme touches no deck.
pub fn edits(base: &[RemoteEndpointSettings], next: &[RemoteEndpointSettings]) -> Vec<DeckEdit> {
    let mut out = Vec::new();
    for row in next {
        match base.iter().find(|old| old.id == row.id) {
            Some(old) if old == row => {}
            Some(old) => out.push(DeckEdit::Update {
                before: old.clone(),
                after: row.clone(),
            }),
            None => out.push(DeckEdit::Add(row.clone())),
        }
    }
    for old in base {
        if !next.iter().any(|row| row.id == old.id) {
            out.push(DeckEdit::Remove(old.clone()));
        }
    }
    out
}

/// Apply `edits` to the registry at `path` as **one** edit: every change lands
/// against one fresh read of the file, and all of them are published by one
/// rename or none is (issue #1350's review — a save used to publish each edit
/// separately, so a later one failing left the earlier ones on disk while the
/// save reported failure).
///
/// - **Add** appends a row — or, if a row with that id already exists (the
///   webview's base was older than the file), updates it instead of adding a
///   second.
/// - **Update** changes only the fields the user changed; a row another writer
///   removed meanwhile stays removed.
/// - **Remove** drops the row and nothing else. It does not offer `remote
///   remove`'s cleanup of a binary the CLI installed on the host; that is a
///   follow-up, and dropping the row is what `remote remove` itself does.
///
/// # An id is not proof the row is still that deck
///
/// An update or a removal finds its row by id, then applies only if that row
/// still reaches the address the **base** row did (host, login, port —
/// [`base_address`]; for an update that is `before`'s, not the new values).
/// A row without an `id` answers to one derived from its name, so a `remote
/// remove prod` and `remote add prod <other host>` in a terminal hands the
/// old deck's id to an unrelated one (issue #1350's review); a save made
/// against the window's older list used to rewrite or delete that new deck.
/// Now any mismatch is [`ApplyError::Conflict`] and **nothing** from the save
/// is published — the batch is one transaction already — so the caller can
/// show the list as it is and let the user make the change again. A row
/// whose address was edited elsewhere (a hand edit, another window) reads the
/// same way; refusing that too is the price of not guessing which deck the
/// user meant.
///
/// No edits touch nothing — not even the lock — so a save that changed only
/// the theme never waits on a `remote add` in a terminal.
pub fn apply(path: &Path, edits: &[DeckEdit]) -> Result<(), ApplyError> {
    if edits.is_empty() {
        return Ok(());
    }
    deck_list::edit(path, |document| {
        for edit in edits {
            let entries = document.entries()?;
            let position = |id: &EndpointId| {
                entries
                    .iter()
                    .position(|entry| DeckRef::Id(id.as_str()).matches(entry))
            };
            // The row `base` was made against, if it is still there — and a
            // conflict if its id now names a different deck.
            let target = |base: &RemoteEndpointSettings| match position(&base.id) {
                Some(index) if deck_list::address_key(&entries[index]) != base_address(base) => {
                    Err(ApplyError::Conflict)
                }
                found => Ok(found),
            };
            match edit {
                DeckEdit::Add(row) => match position(&row.id) {
                    Some(index) => {
                        let mut entry = entries[index].clone();
                        apply_fields(&mut entry, None, row);
                        document.replace(index, &entry)?;
                    }
                    None => document.push(&new_entry(row, &entries))?,
                },
                DeckEdit::Update { before, after } => {
                    if let Some(index) = target(before)? {
                        let mut entry = entries[index].clone();
                        apply_fields(&mut entry, Some(before), after);
                        if entry != entries[index] {
                            document.replace(index, &entry)?;
                        }
                    }
                }
                DeckEdit::Remove(base) => {
                    if let Some(index) = target(base)? {
                        document.remove(index);
                    }
                }
            }
        }
        Ok(())
    })
}

/// Merge the rows a pre-#1350 build kept in `desktop.toml` into the registry
/// at `path`, and return the selection remap the merge implies.
///
/// A row is the same deck as a registry row when their host, login and port
/// agree ([`deck_list::address_key`]) — so a deck the user configured in both
/// clients is not listed twice. Such a row is not re-added: the registry row
/// gains the desktop's id if it had none, and any desktop-only field it lacks
/// (jump host, socket, key) — also when it already answers to the legacy id,
/// explicitly or derived from its name; a registry row that already has an id of its own
/// keeps it, and the returned pair `(desktop id, registry id)` is how the
/// caller re-points a selection at it. Every other row is appended with a
/// derived name and [`deck_list::UNMANAGED_VERSION`].
///
/// **An id is not an address.** A registry row carrying the legacy row's id
/// counts as that row already migrated only when its address matches too.
/// When it does not — an unrelated deck happens to hold the id (issue #1350's
/// review: the legacy deck used to be skipped here, then deleted from
/// `desktop.toml`, and so lost from both) — the legacy deck is migrated as a
/// distinct deck under a fresh id ([`fresh_id`]), and the remap re-points a
/// selection of it there rather than at the unrelated deck.
///
/// **Idempotent**, which is what makes the two-file order crash-safe: running
/// this again after the registry was written but before `desktop.toml` was —
/// or at all — adds nothing, changes nothing and yields the same remap. A row
/// already migrated is found by id and address, or, after a collision, by its
/// address alone, where it now has an id of its own.
///
/// One residual is accepted: a crash between the two writes *and* an edit of
/// that deck's address in the registry before the next launch makes the retry
/// see an id match with a different address, which reads as a collision and
/// adds the legacy deck a second time. A duplicate the user can remove beats
/// the loss the id-only rule produced.
pub fn migrate(
    path: &Path,
    legacy: &[RemoteEndpointSettings],
) -> Result<Vec<(EndpointId, EndpointId)>, RemoteConfigError> {
    deck_list::edit(path, |document| {
        let mut remap = Vec::new();
        for row in legacy {
            let entries = document.entries()?;
            let mut candidate = new_entry(row, &entries);
            let address = deck_list::address_key(&candidate);
            let holder = entries
                .iter()
                .position(|entry| deck_list::deck_id(entry) == row.id.as_str());
            if let Some(index) = holder
                && deck_list::address_key(&entries[index]) == address
            {
                // The same deck under the same id — migrated already, or a CLI
                // row whose derived id happens to be the legacy one. Either
                // way it still gains what only the legacy row carried (issue
                // #1350's review: this path used to skip before the fill, and
                // `desktop.toml` then lost those fields for good). Filling
                // only what is missing makes a retry write nothing.
                let mut entry = entries[index].clone();
                fill_missing(&mut entry, &candidate);
                if entry != entries[index] {
                    document.replace(index, &entry)?;
                }
                continue;
            }
            // Another deck holds this id: the legacy deck needs one of its own.
            let collides = holder.is_some();
            let Some(index) = entries
                .iter()
                .position(|entry| deck_list::address_key(entry) == address)
            else {
                if collides {
                    let id = fresh_id(&row.id, &entries);
                    candidate.id = Some(id.as_str().to_string());
                    remap.push((row.id.clone(), id));
                }
                document.push(&candidate)?;
                continue;
            };
            let mut entry = entries[index].clone();
            let has_own_id = entry
                .id
                .as_deref()
                .is_some_and(deck_list::is_usable_deck_id);
            if has_own_id {
                if let Ok(target) = EndpointId::parse(&deck_list::deck_id(&entry)) {
                    remap.push((row.id.clone(), target));
                }
            } else if collides {
                let id = fresh_id(&row.id, &entries);
                entry.id = Some(id.as_str().to_string());
                remap.push((row.id.clone(), id));
            } else {
                entry.id = Some(row.id.as_str().to_string());
            }
            fill_missing(&mut entry, &candidate);
            document.replace(index, &entry)?;
        }
        Ok(remap)
    })
}

/// Give `entry` each desktop-only connection field it lacks — key, jump host,
/// socket — from `legacy`. A value `entry` already has is never overwritten:
/// the registry row is the one both clients now edit.
fn fill_missing(entry: &mut RemoteEntry, legacy: &RemoteEntry) {
    if entry.key.is_none() {
        entry.key.clone_from(&legacy.key);
    }
    if entry.jump_host.is_none() {
        entry.jump_host.clone_from(&legacy.jump_host);
    }
    if entry.socket.is_none() {
        entry.socket.clone_from(&legacy.socket);
    }
}

/// An id no row in `entries` holds, derived from `base`: `base-2`, `base-3`, …
/// shortened to fit [`deck_list::MAX_DECK_ID_BYTES`]. Deterministic, so a test
/// can name it; uniqueness is what matters, and it is checked.
fn fresh_id(base: &EndpointId, entries: &[RemoteEntry]) -> EndpointId {
    (2u64..)
        .filter_map(|n| {
            let suffix = format!("-{n}");
            let stem = base.as_str();
            let keep = stem.len().min(deck_list::MAX_DECK_ID_BYTES - suffix.len());
            // An id is ASCII (`EndpointId::parse`), so any byte offset is a
            // character boundary.
            EndpointId::parse(&format!("{}{suffix}", &stem[..keep])).ok()
        })
        .find(|id| {
            !entries
                .iter()
                .any(|entry| deck_list::deck_id(entry) == id.as_str())
        })
        .expect("an unbounded suffix search always finds a free id")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, host: &str) -> RemoteEndpointSettings {
        RemoteEndpointSettings::new(
            EndpointId::parse(id).unwrap(),
            Hostname::parse(host).unwrap(),
        )
    }

    fn full_row(id: &str, host: &str) -> RemoteEndpointSettings {
        RemoteEndpointSettings {
            identity: Some(KeyPath::parse("~/.ssh/id_ed25519").unwrap()),
            jump: Some(HostAlias::parse("bastion").unwrap()),
            port: SshPort::parse(2222).unwrap(),
            socket: Some(RemoteSocketPath::parse("/run/user/1000/dad.sock").unwrap()),
            user: Some(SshUser::parse("dev").unwrap()),
            ..row(id, host)
        }
    }

    fn registry(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    const CLI_ROW: &str = "[[remotes]]\nname = \"prod\"\ntype = \"ssh\"\n\
        host = \"dev@build.example.com\"\nport = 2222\nversion = \"0.40.0\"\n\
        added_at = \"2026-01-01T00:00:00Z\"\n";

    #[test]
    fn a_cli_row_reads_as_a_desktop_row_with_a_derived_id() {
        let (_dir, path) = registry(CLI_ROW);
        let rows = load_rows(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id.as_str(), "n-prod");
        assert_eq!(rows[0].host.as_str(), "build.example.com");
        assert_eq!(rows[0].user.as_ref().unwrap().as_str(), "dev");
        assert_eq!(rows[0].port.get(), 2222);
        assert_eq!(rows[0].socket, None);
    }

    #[test]
    fn a_row_the_desktop_cannot_represent_is_skipped_and_the_rest_still_load() {
        let (_dir, path) = registry(&format!(
            "{CLI_ROW}\n[[remotes]]\nname = \"k8s\"\ntype = \"kubernetes\"\nhost = \"c\"\n\
             port = 22\nversion = \"1.0.0\"\nadded_at = \"x\"\n\n\
             [[remotes]]\nname = \"rel\"\ntype = \"ssh\"\nhost = \"h\"\nport = 22\n\
             key = \"relative/key\"\nversion = \"1.0.0\"\nadded_at = \"x\"\n\n\
             [[remotes]]\nname = \"dup\"\ntype = \"ssh\"\nhost = \"h2\"\nport = 22\n\
             id = \"n-prod\"\nversion = \"1.0.0\"\nadded_at = \"x\"\n"
        ));
        let rows = load_rows(&path).unwrap();
        let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n-prod"]);
    }

    #[test]
    fn a_desktop_row_round_trips_through_the_registry() {
        let (_dir, path) = registry("");
        let added = full_row("0123456789abcdef", "build.example.com");
        apply(&path, &[DeckEdit::Add(added.clone())]).unwrap();

        assert_eq!(load_rows(&path).unwrap(), [added]);
        let entry = &RemotesFile::load(&path).unwrap().remotes[0];
        assert_eq!(entry.name, "build.example.com");
        assert_eq!(entry.host, "dev@build.example.com", "folded for older CLIs");
        assert_eq!(entry.user, None);
        assert_eq!(entry.key.as_deref(), Some("~/.ssh/id_ed25519"));
        assert_eq!(entry.version, deck_list::UNMANAGED_VERSION);
        assert!(chrono::DateTime::parse_from_rfc3339(&entry.added_at).is_ok());
    }

    #[test]
    fn a_login_containing_an_at_sign_is_stored_in_its_own_field() {
        let (_dir, path) = registry("");
        let added = RemoteEndpointSettings {
            user: Some(SshUser::parse("dev@REALM").unwrap()),
            ..row("abc", "h.example")
        };
        apply(&path, &[DeckEdit::Add(added.clone())]).unwrap();
        let entry = &RemotesFile::load(&path).unwrap().remotes[0];
        assert_eq!(entry.host, "h.example");
        assert_eq!(entry.user.as_deref(), Some("dev@REALM"));
        assert_eq!(load_rows(&path).unwrap(), [added]);
    }

    /// Issue #1350's review: `load_rows` runs on startup and on every settings
    /// snapshot, so a FIFO at the registry path must come back as the ordinary
    /// "cannot read the deck list" error — which the snapshot logs and shows
    /// as no remote decks — instead of blocking the app.
    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_registry_path_is_an_error_not_a_hang() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
        // call, and `mkfifo` only reads through it.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(load_rows(&path).map(|rows| rows.len()));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("load_rows must return promptly on a FIFO");
        assert!(
            matches!(result, Err(RemoteConfigError::Io { .. })),
            "{result:?}"
        );
    }

    /// Issue #1350's review: `remote add dev@REALM@host` stores the UPN login
    /// folded into `host`, which the CLI's validation accepts (it splits at the
    /// last `@`, as ssh does). The desktop's conversion used to split at the
    /// first, read the host as `REALM@host`, refuse it and skip the deck.
    #[test]
    fn a_cli_added_deck_with_a_upn_login_is_shown_by_the_desktop() {
        let (_dir, path) = registry("");
        deck_list::add(
            &path,
            RemoteEntry {
                name: "upn".to_string(),
                kind: "ssh".to_string(),
                host: "dev@REALM@build.example.com".to_string(),
                port: 22,
                key: None,
                version: "0.43.0".to_string(),
                added_at: "2026-09-01T00:00:00Z".to_string(),
                upgraded_at: None,
                last_connected: None,
                id: None,
                user: None,
                jump_host: None,
                socket: None,
            },
        )
        .unwrap();

        let rows = load_rows(&path).unwrap();
        assert_eq!(rows.len(), 1, "the deck is not skipped");
        assert_eq!(rows[0].host.as_str(), "build.example.com");
        assert_eq!(rows[0].user.as_ref().unwrap().as_str(), "dev@REALM");

        // And an edit of another field from the desktop keeps the login.
        let mut after = rows[0].clone();
        after.port = SshPort::parse(2222).unwrap();
        apply(
            &path,
            &[DeckEdit::Update {
                before: rows[0].clone(),
                after: after.clone(),
            }],
        )
        .unwrap();
        assert_eq!(load_rows(&path).unwrap(), [after]);
    }

    /// The #828 stale-copy scenario, moved to the shared file: the app loads
    /// the list, `remote add` appends a deck from a terminal, and the app then
    /// saves an edit made against its old copy. The terminal's deck survives.
    #[test]
    fn a_save_does_not_clobber_a_deck_another_writer_added_after_the_load() {
        let (_dir, path) = registry(CLI_ROW);
        let loaded = load_rows(&path).unwrap();

        deck_list::add(
            &path,
            RemoteEntry {
                name: "added-in-a-terminal".to_string(),
                kind: "ssh".to_string(),
                host: "late.example".to_string(),
                port: 22,
                key: None,
                version: "0.43.0".to_string(),
                added_at: "2026-09-01T00:00:00Z".to_string(),
                upgraded_at: None,
                last_connected: None,
                id: None,
                user: None,
                jump_host: None,
                socket: None,
            },
        )
        .unwrap();

        let mut next = loaded.clone();
        next[0].socket = Some(RemoteSocketPath::parse("/run/user/1000/found.sock").unwrap());
        next.push(row("fedcba9876543210", "new.example"));
        apply(&path, &edits(&loaded, &next)).unwrap();

        let names: Vec<_> = RemotesFile::load(&path)
            .unwrap()
            .remotes
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(names, ["prod", "added-in-a-terminal", "new.example"]);
        let rows = load_rows(&path).unwrap();
        assert_eq!(
            rows[0].socket.as_ref().map(RemoteSocketPath::as_str),
            Some("/run/user/1000/found.sock")
        );
    }

    #[test]
    fn an_update_writes_only_the_fields_the_user_changed() {
        let (_dir, path) = registry(CLI_ROW);
        let before = load_rows(&path).unwrap().remove(0);
        // Another writer upgrades the deck and sets its socket.
        deck_list::update(&path, DeckRef::Name("prod"), |entry| {
            entry.version = "0.43.0".to_string();
            entry.socket = Some("/run/user/1000/other.sock".to_string());
        })
        .unwrap();

        let mut after = before.clone();
        after.jump = Some(HostAlias::parse("bastion").unwrap());
        apply(
            &path,
            &[DeckEdit::Update {
                before: before.clone(),
                after,
            }],
        )
        .unwrap();

        let entry = &RemotesFile::load(&path).unwrap().remotes[0];
        assert_eq!(
            entry.socket.as_deref(),
            Some("/run/user/1000/other.sock"),
            "the other writer's socket stands"
        );
        assert_eq!(entry.version, "0.43.0");
        assert_eq!(entry.jump_host.as_deref(), Some("bastion"));
        assert_eq!(entry.host, "dev@build.example.com");
    }

    /// Issue #1350's review: a CLI deck with no `id` answers to one derived
    /// from its name, so `remote remove prod` then `remote add prod` for a
    /// different host hands the old deck's id to the new one. A save made
    /// against the window's older list — an update, a removal, or both next to
    /// an unrelated add — must leave the new deck alone and publish nothing.
    #[test]
    fn a_stale_save_leaves_a_deck_re_added_under_the_same_name_alone() {
        let (_dir, path) = registry(CLI_ROW);
        let loaded = load_rows(&path).unwrap();
        let old = loaded[0].clone();
        assert_eq!(old.id.as_str(), "n-prod");

        deck_list::remove(&path, DeckRef::Name("prod")).unwrap();
        deck_list::add(
            &path,
            RemoteEntry {
                name: "prod".to_string(),
                kind: "ssh".to_string(),
                host: "ops@replacement.example".to_string(),
                port: 22,
                key: None,
                version: "0.43.0".to_string(),
                added_at: "2026-09-02T00:00:00Z".to_string(),
                upgraded_at: None,
                last_connected: None,
                id: None,
                user: None,
                jump_host: None,
                socket: None,
            },
        )
        .unwrap();
        let replaced = std::fs::read_to_string(&path).unwrap();

        let mut moved = old.clone();
        moved.jump = Some(HostAlias::parse("bastion").unwrap());
        moved.port = SshPort::parse(2200).unwrap();
        for stale in [
            vec![DeckEdit::Update {
                before: old.clone(),
                after: moved,
            }],
            vec![DeckEdit::Remove(old.clone())],
            vec![
                DeckEdit::Add(row("fedcba9876543210", "new.example")),
                DeckEdit::Remove(old.clone()),
            ],
        ] {
            let result = apply(&path, &stale);
            assert!(matches!(result, Err(ApplyError::Conflict)), "{result:?}");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                replaced,
                "the stale save changed the file: {stale:?}"
            );
        }
        let rows = load_rows(&path).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].host.as_str(), "replacement.example");
    }

    /// The same check reads a deck whose address another writer moved as a
    /// conflict: an edit of it made against the old address is not applied.
    #[test]
    fn an_update_against_an_address_moved_elsewhere_is_a_conflict() {
        let (_dir, path) = registry(CLI_ROW);
        let before = load_rows(&path).unwrap().remove(0);
        deck_list::update(&path, DeckRef::Name("prod"), |entry| entry.port = 2200).unwrap();
        let moved = std::fs::read_to_string(&path).unwrap();
        let mut after = before.clone();
        after.jump = Some(HostAlias::parse("bastion").unwrap());
        assert!(matches!(
            apply(&path, &[DeckEdit::Update { before, after }]),
            Err(ApplyError::Conflict)
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), moved);
    }

    /// An update that itself changes the address is matched on the address it
    /// was made against, and lands.
    #[test]
    fn an_address_edit_matches_on_the_base_address() {
        let (_dir, path) = registry(CLI_ROW);
        let before = load_rows(&path).unwrap().remove(0);
        let mut after = before.clone();
        after.host = Hostname::parse("Moved.Example").unwrap();
        after.user = Some(SshUser::parse("ops").unwrap());
        after.port = SshPort::DEFAULT;
        apply(
            &path,
            &[DeckEdit::Update {
                before,
                after: after.clone(),
            }],
        )
        .unwrap();
        assert_eq!(load_rows(&path).unwrap(), [after]);
    }

    /// Issue #1350's review: one save's deck edits are one transaction. The
    /// second edit here is refused — another writer hand-edited the deck's host
    /// (into one ssh would misread), so the row is no longer at the address the
    /// update was made against — and the first edit, an add, must not be
    /// published either.
    #[test]
    fn a_batch_whose_later_edit_fails_publishes_none_of_it() {
        let (_dir, path) = registry(CLI_ROW);
        let before = load_rows(&path).unwrap().remove(0);
        let hand_edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("dev@build.example.com", "dev@bad host");
        std::fs::write(&path, &hand_edited).unwrap();

        let mut after = before.clone();
        after.user = Some(SshUser::parse("ops").unwrap());
        let result = apply(
            &path,
            &[
                DeckEdit::Add(row("fedcba9876543210", "new.example")),
                DeckEdit::Update { before, after },
            ],
        );
        assert!(matches!(result, Err(ApplyError::Conflict)), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            hand_edited,
            "the add before the failing edit was published"
        );
    }

    #[test]
    fn a_batch_lands_every_edit_in_one_write() {
        let (_dir, path) = registry(CLI_ROW);
        let prod = load_rows(&path).unwrap().remove(0);
        let mut moved = prod.clone();
        moved.port = SshPort::parse(2200).unwrap();
        apply(
            &path,
            &[
                DeckEdit::Add(row("aaaa", "a.example")),
                DeckEdit::Add(row("bbbb", "b.example")),
                DeckEdit::Update {
                    before: prod,
                    after: moved,
                },
                DeckEdit::Remove(row("aaaa", "a.example")),
            ],
        )
        .unwrap();
        let rows = load_rows(&path).unwrap();
        let ids: Vec<_> = rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["n-prod", "bbbb"]);
        assert_eq!(rows[0].port.get(), 2200);
    }

    #[test]
    fn edits_name_exactly_what_changed() {
        let a = row("aaaa", "a.example");
        let b = row("bbbb", "b.example");
        let mut b2 = b.clone();
        b2.port = SshPort::parse(2022).unwrap();
        let c = row("cccc", "c.example");
        assert_eq!(edits(&[a.clone(), b.clone()], &[a.clone(), b.clone()]), []);
        assert_eq!(
            edits(&[a.clone(), b.clone()], &[b2.clone(), c.clone()]),
            [
                DeckEdit::Update {
                    before: b,
                    after: b2
                },
                DeckEdit::Add(c),
                DeckEdit::Remove(a),
            ]
        );
    }

    #[test]
    fn removing_a_deck_drops_only_that_row() {
        let (_dir, path) = registry(CLI_ROW);
        apply(&path, &[DeckEdit::Add(row("abcd", "other.example"))]).unwrap();
        let prod = load_rows(&path).unwrap().remove(0);
        apply(&path, &[DeckEdit::Remove(prod)]).unwrap();
        let ids: Vec<_> = load_rows(&path)
            .unwrap()
            .into_iter()
            .map(|row| row.id.as_str().to_string())
            .collect();
        assert_eq!(ids, ["abcd"]);
    }

    #[test]
    fn migration_merges_a_deck_present_in_both_lists_by_address() {
        let (_dir, path) = registry(CLI_ROW);
        // Same host (in another case), same login, same port as the CLI row.
        let legacy = [
            full_row("0123456789abcdef", "Build.Example.com"),
            row("1111222233334444", "fresh.example"),
        ];
        let remap = migrate(&path, &legacy).unwrap();
        assert_eq!(
            remap,
            [],
            "the CLI row had no id, so it takes the desktop's"
        );

        let entries = RemotesFile::load(&path).unwrap().remotes;
        assert_eq!(entries.len(), 2, "no duplicate: {entries:?}");
        assert_eq!(entries[0].name, "prod");
        assert_eq!(entries[0].id.as_deref(), Some("0123456789abcdef"));
        assert_eq!(entries[0].jump_host.as_deref(), Some("bastion"));
        assert_eq!(entries[0].version, "0.40.0", "the CLI's fields stand");
        assert_eq!(entries[1].name, "fresh.example");
        assert_eq!(entries[1].version, deck_list::UNMANAGED_VERSION);

        // Idempotent: a second run (a crash before desktop.toml was rewritten)
        // adds nothing and changes nothing.
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(migrate(&path, &legacy).unwrap(), []);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
    }

    /// Issue #1350's review: a CLI row `prod` with no id answers to the
    /// derived id `n-prod`, so a legacy row `n-prod` at the same address is
    /// the same deck — and the fields only the legacy row carried must still
    /// reach the registry, since `desktop.toml` loses them next. The registry
    /// row's own values stand, and a retry writes nothing.
    #[test]
    fn migration_into_a_row_answering_to_the_legacy_id_keeps_legacy_only_fields() {
        let (_dir, path) = registry(&format!("{CLI_ROW}key = \"~/.ssh/cli_key\"\n"));
        let legacy = [RemoteEndpointSettings {
            identity: Some(KeyPath::parse("~/.ssh/legacy_key").unwrap()),
            jump: Some(HostAlias::parse("bastion").unwrap()),
            port: SshPort::parse(2222).unwrap(),
            user: Some(SshUser::parse("dev").unwrap()),
            ..row("n-prod", "build.example.com")
        }];
        assert_eq!(migrate(&path, &legacy).unwrap(), []);

        let entries = RemotesFile::load(&path).unwrap().remotes;
        assert_eq!(entries.len(), 1, "no duplicate: {entries:?}");
        assert_eq!(entries[0].jump_host.as_deref(), Some("bastion"));
        assert_eq!(
            entries[0].key.as_deref(),
            Some("~/.ssh/cli_key"),
            "the registry row's own value is never overwritten"
        );
        assert_eq!(entries[0].socket, None);
        assert_eq!(deck_list::deck_id(&entries[0]), "n-prod");

        let after_first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(migrate(&path, &legacy).unwrap(), []);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            after_first,
            "the retry wrote"
        );
    }

    #[test]
    fn migration_remaps_a_deck_whose_registry_row_already_has_an_id() {
        let (_dir, path) = registry(&format!("{CLI_ROW}id = \"cli-own-id\"\n"));
        let legacy = [full_row("0123456789abcdef", "build.example.com")];
        let remap = migrate(&path, &legacy).unwrap();
        assert_eq!(
            remap,
            [(
                EndpointId::parse("0123456789abcdef").unwrap(),
                EndpointId::parse("cli-own-id").unwrap()
            )]
        );
        assert_eq!(RemotesFile::load(&path).unwrap().remotes.len(), 1);
        assert_eq!(migrate(&path, &legacy).unwrap(), remap, "idempotent");
    }

    /// Issue #1350's review: a registry row that merely shares a legacy deck's
    /// id — a different address — is not that deck. The legacy deck used to be
    /// skipped, then deleted from `desktop.toml`, so it was in neither list and
    /// a selection of it pointed at the unrelated deck.
    #[test]
    fn migration_keeps_a_legacy_deck_whose_id_an_unrelated_deck_holds() {
        let (_dir, path) = registry(
            "[[remotes]]\nname = \"other\"\ntype = \"ssh\"\nhost = \"other.example\"\n\
             port = 22\nversion = \"0.40.0\"\nadded_at = \"x\"\nid = \"0123456789abcdef\"\n",
        );
        let legacy = [full_row("0123456789abcdef", "legacy.example")];
        let remap = migrate(&path, &legacy).unwrap();
        let fresh = EndpointId::parse("0123456789abcdef-2").unwrap();
        assert_eq!(
            remap,
            [(
                EndpointId::parse("0123456789abcdef").unwrap(),
                fresh.clone()
            )]
        );

        let rows = load_rows(&path).unwrap();
        assert_eq!(rows.len(), 2, "both decks are kept: {rows:?}");
        assert_eq!(rows[0].host.as_str(), "other.example");
        assert_eq!(rows[0].id.as_str(), "0123456789abcdef");
        assert_eq!(rows[1].host.as_str(), "legacy.example");
        assert_eq!(rows[1].id, fresh);
        assert_eq!(
            rows[1].jump.as_ref().map(HostAlias::as_str),
            Some("bastion")
        );

        // Idempotent: the retry after a crash before desktop.toml was rewritten
        // adds nothing, writes nothing, and re-points the selection the same way.
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(migrate(&path, &legacy).unwrap(), remap);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
    }

    /// The same collision, where the legacy deck's address matches a CLI row
    /// with no id: it takes a fresh id rather than the one the unrelated deck
    /// holds, so the list never has two rows with one id.
    #[test]
    fn migration_merging_into_a_cli_row_does_not_reuse_a_colliding_id() {
        let (_dir, path) = registry(&format!(
            "{CLI_ROW}\n[[remotes]]\nname = \"other\"\ntype = \"ssh\"\n\
             host = \"other.example\"\nport = 22\nversion = \"0.40.0\"\nadded_at = \"x\"\n\
             id = \"0123456789abcdef\"\n"
        ));
        let legacy = [full_row("0123456789abcdef", "build.example.com")];
        let remap = migrate(&path, &legacy).unwrap();
        let fresh = EndpointId::parse("0123456789abcdef-2").unwrap();
        assert_eq!(
            remap,
            [(
                EndpointId::parse("0123456789abcdef").unwrap(),
                fresh.clone()
            )]
        );
        let ids: Vec<_> = load_rows(&path)
            .unwrap()
            .into_iter()
            .map(|row| row.id)
            .collect();
        assert_eq!(ids, [fresh, EndpointId::parse("0123456789abcdef").unwrap()]);
        let after_first = std::fs::read_to_string(&path).unwrap();
        assert_eq!(migrate(&path, &legacy).unwrap(), remap, "idempotent");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
    }

    #[test]
    fn a_fresh_id_fits_and_is_unused() {
        let taken = |ids: &[&str]| -> Vec<RemoteEntry> {
            ids.iter()
                .map(|id| RemoteEntry {
                    name: format!("n{id}"),
                    kind: "ssh".to_string(),
                    host: "h".to_string(),
                    port: 22,
                    key: None,
                    version: "1".to_string(),
                    added_at: "x".to_string(),
                    upgraded_at: None,
                    last_connected: None,
                    id: Some(id.to_string()),
                    user: None,
                    jump_host: None,
                    socket: None,
                })
                .collect()
        };
        let base = EndpointId::parse("abc").unwrap();
        assert_eq!(fresh_id(&base, &taken(&["abc"])).as_str(), "abc-2");
        assert_eq!(fresh_id(&base, &taken(&["abc", "abc-2"])).as_str(), "abc-3");
        let long = EndpointId::parse(&"a".repeat(64)).unwrap();
        let id = fresh_id(&long, &taken(&[long.as_str()]));
        assert_eq!(id.as_str().len(), 64);
        assert!(id.as_str().ends_with("-2"), "{id:?}");
    }

    #[test]
    fn migration_does_not_merge_decks_that_differ_in_login_or_port() {
        let (_dir, path) = registry(CLI_ROW);
        let other_user = RemoteEndpointSettings {
            user: Some(SshUser::parse("ops").unwrap()),
            ..full_row("aaaa", "build.example.com")
        };
        let other_port = RemoteEndpointSettings {
            port: SshPort::DEFAULT,
            ..full_row("bbbb", "build.example.com")
        };
        migrate(&path, &[other_user, other_port]).unwrap();
        let names: Vec<_> = RemotesFile::load(&path)
            .unwrap()
            .remotes
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            names,
            ["prod", "build.example.com", "dev-build.example.com"]
        );
    }

    /// The library's id rule and the desktop's `EndpointId::parse` must agree,
    /// or a derived id could be one the desktop refuses.
    #[test]
    fn the_shared_id_rule_agrees_with_endpoint_id() {
        let long = "a".repeat(65);
        for raw in [
            "abc",
            "A-b_9",
            "",
            "local",
            "LOCAL",
            "all",
            "All",
            "has space",
            "a.b",
            "é",
            &long,
            "0123456789abcdef",
        ] {
            assert_eq!(
                deck_list::is_usable_deck_id(raw),
                EndpointId::parse(raw).is_ok(),
                "{raw:?}"
            );
        }
    }
}
