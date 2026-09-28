//! Issue #1350 — the one deck list both clients share.
//!
//! `~/.config/dot-agent-deck/remotes.toml` has been the CLI's registry since
//! PRD #76 (`remote add`, `connect`, `remote upgrade`), and since #1350 it is
//! the desktop app's deck list as well. Both clients read and write it through
//! this module, which is why it lives in the root crate (CLAUDE.md rule 18):
//! the desktop depends on this package by path, so a second copy of the file
//! format would be a second thing to keep in step.
//!
//! # Three properties every writer here keeps
//!
//! 1. **Every edit is made against a fresh read.** [`add`], [`update`] and
//!    [`remove`] each read the file at call time, apply exactly one change and
//!    write atomically (temp file + rename, [`write_atomic`]). A long-running
//!    writer — the desktop, which stays open for days while the user runs
//!    `remote add` in a terminal — therefore never writes back a list it loaded
//!    earlier, which is the stale-copy clobber #828 fixed inside `desktop.toml`.
//!    Edits from one process are also serialised by an in-process lock, so two
//!    desktop windows saving at once cannot interleave their read and write.
//!    There is no cross-process lock: a user-driven list does not need one.
//! 2. **Keys this build does not know survive a re-save.** The file is edited
//!    as a [`toml_edit::DocumentMut`], and an entry is updated key by key, so a
//!    field a newer build added — and every comment — is left where it was. An
//!    **older** CLI does not do this: its `RemotesFile::save` re-serialises the
//!    whole file, so `remote add` from a build before #1350 drops `id`, `user`,
//!    `jump_host` and `socket` from every row. That loss is accepted and
//!    documented (`docs/remote-environments.md`); it is why a row without an
//!    `id` still has one, derived by [`deck_id`].
//! 3. **What is written still parses under the old, strict shape.** A build
//!    before #1350 requires `type`, `version` and `added_at` on every row and
//!    fails to parse the *whole* file without them, so every row written here
//!    carries all three — a deck the desktop added carries
//!    [`UNMANAGED_VERSION`] as its version.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;

use crate::remote::{RemoteConfigError, RemoteEntry, RemotesFile};

/// The `version` a deck carries when no `dot-agent-deck` binary was installed
/// on it by `remote add` — today, every deck added from the desktop app.
///
/// A row must have *some* `version` (property 3 above), and what an older CLI
/// does with the value decides which one is safe. Measured against the code
/// rather than assumed: `remote list` prints it verbatim in its VERSION column,
/// `remote upgrade` overwrites it without reading it, and `connect` and
/// `remote doctor` never read it at all — they ask the remote binary for its
/// version over ssh. So any string parses and nothing parses it as a version.
///
/// It is deliberately **not** version-shaped. `0.0.0` would read in `remote
/// list` as a real, ancient install, and an empty string as a broken row;
/// `unmanaged` says what is true — the CLI did not install this deck's binary —
/// and `remote upgrade <name>` replaces it with a real version the first time
/// the CLI does.
pub const UNMANAGED_VERSION: &str = "unmanaged";

/// The longest deck name [`validate_deck_name`] accepts.
pub const MAX_DECK_NAME_BYTES: usize = 64;

/// The longest id [`deck_id`] will use as written — the desktop's
/// `MAX_ENDPOINT_ID_BYTES`.
pub const MAX_DECK_ID_BYTES: usize = 64;

/// The two words a deck id may not be, because the desktop's deck selection
/// uses them to mean the local deck and every deck at once.
const RESERVED_IDS: [&str; 2] = ["local", "all"];

/// The `added_at` / `upgraded_at` spelling: RFC 3339, UTC — what `remote add`
/// has always written. A function so a client of this module (the desktop,
/// which has no `chrono` of its own) writes the same spelling.
pub fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339()
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Why a name is not a valid deck name.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DeckNameError {
    #[error("a deck name cannot be empty")]
    Empty,
    #[error("a deck name is at most {MAX_DECK_NAME_BYTES} bytes; got {len}")]
    TooLong { len: usize },
    #[error("a deck name must start with an ASCII letter or digit")]
    BadStart,
    #[error(
        "a deck name is ASCII letters, digits, '.', '-' and '_'; found byte 0x{byte:02x} at offset {offset}"
    )]
    ForbiddenByte { offset: usize, byte: u8 },
}

/// Whether `name` is a valid name for a **new** deck.
///
/// One function for both clients (issue #1350): `remote add` checks the name
/// the user typed, and the desktop's derived names pass through it too. The
/// rule is a slug — ASCII letters, digits, `.`, `-` and `_`, starting with a
/// letter or digit, at most [`MAX_DECK_NAME_BYTES`] — because the name is typed
/// as `connect <name>` in a shell and rendered in the desktop, and a slug is
/// safe in both without quoting or bidi handling.
///
/// **Applied on write only.** A registry written before this rule may hold a
/// name it refuses — `remote add` accepted any string — and such a file must
/// keep loading; nothing on the read path calls this.
pub fn validate_deck_name(name: &str) -> Result<(), DeckNameError> {
    if name.is_empty() {
        return Err(DeckNameError::Empty);
    }
    if name.len() > MAX_DECK_NAME_BYTES {
        return Err(DeckNameError::TooLong { len: name.len() });
    }
    if let Some((offset, byte)) = name
        .bytes()
        .enumerate()
        .find(|(_, byte)| !is_name_byte(*byte))
    {
        return Err(DeckNameError::ForbiddenByte { offset, byte });
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() {
        return Err(DeckNameError::BadStart);
    }
    Ok(())
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
}

/// A valid, unused name for a deck at `host` (and `user`), for a client that
/// does not ask the user for one — the desktop, which deliberately takes no
/// free-form label.
///
/// The host, lowercased, with every byte [`validate_deck_name`] refuses turned
/// into `-`. If that is taken and there is a user, `user-host`. If that is
/// taken too, a numeric suffix: `host-2`, `host-3`, … The result always passes
/// [`validate_deck_name`] and is never one `taken` reports.
pub fn derive_deck_name(host: &str, user: Option<&str>, taken: impl Fn(&str) -> bool) -> String {
    let base = slug(host).unwrap_or_else(|| "deck".to_string());
    let mut candidates = vec![base.clone()];
    if let Some(with_user) = user.and_then(|user| slug(&format!("{user}-{host}"))) {
        candidates.push(with_user);
    }
    if let Some(free) = candidates.into_iter().find(|name| !taken(name)) {
        return free;
    }
    (2u64..)
        .map(|n| {
            let suffix = format!("-{n}");
            let keep = MAX_DECK_NAME_BYTES - suffix.len();
            let stem = base[..base.len().min(keep)].trim_end_matches(['-', '.', '_']);
            format!("{stem}{suffix}")
        })
        .find(|name| !taken(name))
        .expect("an unbounded suffix search always finds a free name")
}

/// `raw` lowercased and reduced to deck-name bytes, or `None` when nothing of
/// it survives.
fn slug(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii() && is_name_byte(lower as u8) {
            out.push(lower);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    let bounded = &trimmed[..trimmed.len().min(MAX_DECK_NAME_BYTES)];
    let bounded = bounded.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    (!bounded.is_empty()).then(|| bounded.to_string())
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Whether `raw` can be used as a deck id as written: the charset and bound of
/// the desktop's `EndpointId::parse` — ASCII letters, digits, `-` and `_`, at
/// most [`MAX_DECK_ID_BYTES`], and neither reserved word in any case. The
/// desktop crate pins that the two agree.
pub fn is_usable_deck_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= MAX_DECK_ID_BYTES
        && raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && !RESERVED_IDS
            .iter()
            .any(|reserved| raw.eq_ignore_ascii_case(reserved))
}

/// The stable identity of a row: its `id` when it has a usable one, otherwise
/// one derived from its `name`.
///
/// A row without an `id` is ordinary rather than broken: every row `remote
/// add` wrote before #1350 has none, and an older CLI that re-saves the file
/// strips the ones the desktop wrote. The derived id is a pure function of the
/// name, which the file keeps unique, so it is the same on every read and
/// never needs writing back. `n-<name>` when the name fits the id charset, and
/// `h-<16 hex digits>` of a stable hash of it otherwise; the prefixes keep both
/// forms apart from each other, from the desktop's minted 16-hex ids and from
/// the reserved words.
pub fn deck_id(entry: &RemoteEntry) -> String {
    if let Some(id) = entry.id.as_deref()
        && is_usable_deck_id(id)
    {
        return id.to_string();
    }
    derived_id(&entry.name)
}

fn derived_id(name: &str) -> String {
    let readable = format!("n-{name}");
    if is_usable_deck_id(&readable) {
        readable
    } else {
        format!("h-{:016x}", fnv1a(name.as_bytes()))
    }
}

/// FNV-1a, 64-bit. Chosen over `DefaultHasher` because a derived id is stored
/// (as the desktop's selection) and must not change between Rust releases.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The login name and bare host a row reaches.
///
/// `host` carries `[user@]host`, the way `remote add` stored what the user
/// typed; an explicit `user` field, when present, wins over the one in `host`.
/// Split on the first `@`, exactly as `SshTarget::parse` does, so every reader
/// agrees on which part is the host.
pub fn login_and_host(entry: &RemoteEntry) -> (Option<&str>, &str) {
    let (from_host, host) = match entry.host.split_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, entry.host.as_str()),
    };
    (entry.user.as_deref().or(from_host), host)
}

/// The `host` and `user` fields to store for `host` and `user`.
///
/// The user is folded into `host` as `user@host` whenever that round-trips,
/// because that is the one spelling an older CLI's `connect` understands — it
/// has never heard of a `user` field and would log in as the local user. Only a
/// login containing `@` itself (a Kerberos-style `user@realm`) goes in the
/// separate `user` field, since folding it would split at the wrong `@`.
pub fn host_fields(host: &str, user: Option<&str>) -> (String, Option<String>) {
    match user {
        Some(user) if user.contains('@') => (host.to_string(), Some(user.to_string())),
        Some(user) => (format!("{user}@{host}"), None),
        None => (host.to_string(), None),
    }
}

/// What two rows are compared on when deciding they are the same deck: the
/// bare host (lowercased, since DNS names are case-insensitive), the login and
/// the port. Used by the desktop's one-time migration (#1350) so a deck present
/// in both lists is not added twice.
pub fn address_key(entry: &RemoteEntry) -> (String, Option<String>, u16) {
    let (user, host) = login_and_host(entry);
    (
        host.to_ascii_lowercase(),
        user.map(str::to_string),
        entry.port,
    )
}

/// Which row an edit is aimed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckRef<'a> {
    /// The row with this `name` — the CLI's handle (`connect <name>`).
    Name(&'a str),
    /// The row whose [`deck_id`] is this — the desktop's handle.
    Id(&'a str),
}

impl DeckRef<'_> {
    pub fn matches(&self, entry: &RemoteEntry) -> bool {
        match self {
            Self::Name(name) => entry.name == *name,
            Self::Id(id) => deck_id(entry) == *id,
        }
    }
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

/// Why [`add`] refused to add a row.
#[derive(Debug, Error)]
pub enum AddDeckError {
    #[error("A remote named '{name}' already exists.")]
    DuplicateName { name: String },
    #[error("A deck with id '{id}' already exists.")]
    DuplicateId { id: String },
    #[error("Invalid remote name: {0}.")]
    InvalidName(#[from] DeckNameError),
    #[error(transparent)]
    Config(#[from] RemoteConfigError),
}

/// How `remote add` reports a refusal from [`add`]. `DuplicateId` cannot reach
/// it — `remote add` writes no `id` — but is mapped rather than assumed away.
impl From<AddDeckError> for crate::remote::RemoteAddError {
    fn from(error: AddDeckError) -> Self {
        use crate::remote::RemoteAddError;
        match error {
            AddDeckError::DuplicateName { name } => RemoteAddError::DuplicateName { name },
            AddDeckError::InvalidName(reason) => RemoteAddError::InvalidName(reason),
            AddDeckError::Config(error) => RemoteAddError::Registry(error),
            AddDeckError::DuplicateId { id } => {
                RemoteAddError::Registry(RemoteConfigError::Unwritable {
                    path: "remotes.toml".to_string(),
                    reason: format!("a deck with id '{id}' already exists"),
                })
            }
        }
    }
}

/// Serialises edits within one process, so two desktop windows saving at the
/// same moment cannot both read before either writes.
static EDIT_LOCK: Mutex<()> = Mutex::new(());

/// The registry as a format-preserving document, handed to an [`edit`]
/// closure. Rows are addressed by index into [`Self::entries`].
pub struct DeckDocument {
    doc: toml_edit::DocumentMut,
    path: String,
}

impl DeckDocument {
    fn open(path: &Path, contents: Option<&str>) -> Result<Self, RemoteConfigError> {
        let display = path.display().to_string();
        let mut doc = match contents {
            Some(contents) => {
                // The strict read first: an edit never lands on a file this
                // build cannot load, since the load would then have refused it.
                toml::from_str::<RemotesFile>(contents).map_err(|source| {
                    RemoteConfigError::Parse {
                        path: display.clone(),
                        source,
                    }
                })?;
                contents
                    .parse::<toml_edit::DocumentMut>()
                    .map_err(|error| unwritable(&display, &error.to_string()))?
            }
            None => toml_edit::DocumentMut::new(),
        };
        // `remotes = []` is what `RemotesFile::save` writes for an empty list,
        // and `remotes = [{ … }]` is a legal hand spelling: both become
        // `[[remotes]]` so rows can be edited one at a time.
        if let Some(item) = doc.get_mut("remotes")
            && !item.is_array_of_tables()
        {
            let taken = std::mem::take(item);
            let empty = taken.as_array().is_some_and(toml_edit::Array::is_empty);
            *item = match taken.into_array_of_tables() {
                Ok(rows) => toml_edit::Item::ArrayOfTables(rows),
                // `into_array_of_tables` refuses an empty array, which is
                // exactly the spelling `RemotesFile::save` uses for no rows.
                Err(_) if empty => toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
                Err(_) => {
                    return Err(unwritable(&display, "`remotes` is not a list of tables"));
                }
            };
        }
        Ok(Self { doc, path: display })
    }

    fn rows(&self) -> Option<&toml_edit::ArrayOfTables> {
        self.doc.get("remotes")?.as_array_of_tables()
    }

    fn rows_mut(&mut self) -> &mut toml_edit::ArrayOfTables {
        let item = self
            .doc
            .entry("remotes")
            .or_insert_with(|| toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
        item.as_array_of_tables_mut()
            .expect("`open` leaves `remotes` absent or a list of tables")
    }

    /// Every row, as this build reads it, in file order.
    pub fn entries(&self) -> Result<Vec<RemoteEntry>, RemoteConfigError> {
        toml::from_str::<RemotesFile>(&self.doc.to_string())
            .map(|file| file.remotes)
            .map_err(|source| RemoteConfigError::Parse {
                path: self.path.clone(),
                source,
            })
    }

    /// Append `entry` as a new row.
    pub fn push(&mut self, entry: &RemoteEntry) -> Result<(), RemoteConfigError> {
        let table = entry_table(entry, &self.path)?;
        self.rows_mut().push(table);
        Ok(())
    }

    /// Make row `index` say what `entry` says, key by key.
    ///
    /// A key whose value is unchanged keeps its own bytes, a key `entry` no
    /// longer carries (an optional field set to `None`) is removed, and a key
    /// this build does not know is not touched at all — which is property 2 in
    /// the module docs. Which keys this build knows is read off its own
    /// serialisation of the row rather than listed by hand, so a field added
    /// to [`RemoteEntry`] later is covered without an edit here.
    pub fn replace(&mut self, index: usize, entry: &RemoteEntry) -> Result<(), RemoteConfigError> {
        let path = self.path.clone();
        let before = self
            .entries()?
            .into_iter()
            .nth(index)
            .ok_or_else(|| unwritable(&path, "no such row"))?;
        let old = entry_table(&before, &path)?;
        let new = entry_table(entry, &path)?;
        let row = self
            .rows_mut()
            .get_mut(index)
            .ok_or_else(|| unwritable(&path, "no such row"))?;
        for (key, value) in new.iter() {
            let unchanged = old.get(key).map(ToString::to_string) == Some(value.to_string());
            if !unchanged || !row.contains_key(key) {
                row.insert(key, value.clone());
            }
        }
        for (key, _) in old.iter() {
            if !new.contains_key(key) {
                row.remove(key);
            }
        }
        Ok(())
    }

    /// Remove row `index`.
    pub fn remove(&mut self, index: usize) {
        let rows = self.rows_mut();
        if index < rows.len() {
            rows.remove(index);
        }
    }
}

/// `entry` rendered by this build, as a table.
fn entry_table(entry: &RemoteEntry, path: &str) -> Result<toml_edit::Table, RemoteConfigError> {
    let text = toml::to_string(entry)?;
    let doc = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| unwritable(path, &error.to_string()))?;
    Ok(doc.as_table().clone())
}

fn unwritable(path: &str, reason: &str) -> RemoteConfigError {
    RemoteConfigError::Unwritable {
        path: path.to_string(),
        reason: reason.to_string(),
    }
}

/// Read `path` fresh, let `f` change the document, and write the result
/// atomically — or, if `f` changed nothing, write nothing.
///
/// The building block [`add`], [`update`] and [`remove`] are made of, and what
/// a caller with a batch uses (the desktop's one-time migration). The result is
/// checked to parse as a registry before it is published, so an edit can never
/// leave a file the next load would refuse. A missing file is an empty
/// registry, and is created only if `f` adds something.
pub fn edit<T, E>(path: &Path, f: impl FnOnce(&mut DeckDocument) -> Result<T, E>) -> Result<T, E>
where
    E: From<RemoteConfigError>,
{
    let _guard = EDIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let original = match std::fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => {
            return Err(RemoteConfigError::Io {
                path: path.display().to_string(),
                source,
            }
            .into());
        }
    };
    let mut document = DeckDocument::open(path, original.as_deref())?;
    let value = f(&mut document)?;
    let rendered = document.doc.to_string();
    let unchanged = match &original {
        Some(original) => *original == rendered,
        None => document.rows().is_none_or(|rows| rows.is_empty()),
    };
    if unchanged {
        return Ok(value);
    }
    toml::from_str::<RemotesFile>(&rendered).map_err(|_| {
        unwritable(
            &path.display().to_string(),
            "the edit would leave the file unreadable, so nothing was written",
        )
    })?;
    write_atomic(path, &rendered)?;
    Ok(value)
}

/// Append `entry`, refusing a name or [`deck_id`] already in the file.
///
/// The name is checked with [`validate_deck_name`] — this is the write path, so
/// this is where the slug rule applies — and the duplicate checks run against
/// the file as it is **now**, not as the caller last saw it.
pub fn add(path: &Path, entry: RemoteEntry) -> Result<RemoteEntry, AddDeckError> {
    validate_deck_name(&entry.name)?;
    edit(path, |document| {
        let existing = document.entries()?;
        if existing.iter().any(|row| row.name == entry.name) {
            return Err(AddDeckError::DuplicateName {
                name: entry.name.clone(),
            });
        }
        let id = deck_id(&entry);
        if existing.iter().any(|row| deck_id(row) == id) {
            return Err(AddDeckError::DuplicateId { id });
        }
        document.push(&entry)?;
        Ok(entry)
    })
}

/// Apply `f` to the row `which` names, as the file holds it now, and return the
/// updated row — or `None`, writing nothing, when no row matches.
pub fn update(
    path: &Path,
    which: DeckRef<'_>,
    f: impl FnOnce(&mut RemoteEntry),
) -> Result<Option<RemoteEntry>, RemoteConfigError> {
    edit(path, |document| {
        let entries = document.entries()?;
        let Some(index) = entries.iter().position(|row| which.matches(row)) else {
            return Ok(None);
        };
        let mut entry = entries[index].clone();
        f(&mut entry);
        if entry != entries[index] {
            document.replace(index, &entry)?;
        }
        Ok(Some(entry))
    })
}

/// Remove the row `which` names and return it — or `None`, writing nothing,
/// when no row matches.
pub fn remove(path: &Path, which: DeckRef<'_>) -> Result<Option<RemoteEntry>, RemoteConfigError> {
    edit(path, |document| {
        let entries = document.entries()?;
        let Some(index) = entries.iter().position(|row| which.matches(row)) else {
            return Ok(None);
        };
        document.remove(index);
        Ok(Some(entries[index].clone()))
    })
}

/// Distinguishes temp files written by one process, which may save more than
/// once at a time (the desktop, from several windows).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically replace the file at `path` with `contents`. Creates the parent
/// directory if missing. Writes via a sibling temp file with mode 0o600, then
/// `rename(2)`s it into place — so a partial write or a crash mid-save can
/// never leave a half-written `remotes.toml` for the next run to choke on, and
/// the final file is owner-only (0o600) regardless of the user's umask.
pub fn write_atomic(path: &Path, contents: &str) -> Result<(), RemoteConfigError> {
    use std::io::Write;

    // PRD #163 auditor: create the parent through the fsperm seam, not plain
    // `create_dir_all`, so the *directory* is owner-only too — the same call
    // `schedules.toml` already makes. The per-file DACL/mode protects the
    // contents; this protects the metadata (which remotes exist, by filename)
    // when `DOT_AGENT_DECK_REMOTES_DIR` points somewhere shared. Create-only,
    // so an existing directory is never surprise-tightened (PRD #127 S2).
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    crate::platform::fsperm::create_owner_only_dir(parent).map_err(|source| {
        RemoteConfigError::Io {
            path: parent.display().to_string(),
            source,
        }
    })?;

    // Sibling temp file: same directory as the final path so `rename` is
    // atomic on POSIX filesystems. The pid and a per-process counter keep
    // concurrent saves — from several processes, or several windows of one —
    // off each other's temp file.
    let file_name = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "remotes.toml".to_string());
    let tmp_path = parent.join(format!(
        "{file_name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    // PRD #42 M1: owner-only (0o600) creation mode comes from the platform
    // seam — `.mode(0o600)` on Unix; on Windows (#163) the DACL cannot be
    // supplied at create time, so the seam instead puts `WRITE_DAC` on the
    // handle, which is what lets the `set_file_owner_only` call below apply it.
    let mut open_opts = std::fs::OpenOptions::new();
    open_opts.create(true).write(true).truncate(true);
    crate::platform::fsperm::set_create_mode_owner_only(&mut open_opts);
    let mut tmp_file = open_opts
        .open(&tmp_path)
        .map_err(|source| RemoteConfigError::Io {
            path: tmp_path.display().to_string(),
            source,
        })?;

    // Owner-only permissions BEFORE the first content byte (PRD #163 M4).
    //
    // Two reasons this runs here rather than after the write. (1) Defense in
    // depth on Unix: a stale temp file from a crashed previous save would not
    // have had `OpenOptions::mode()` re-applied, so the bits have to be set
    // explicitly. (2) On Windows this call is not a re-assert but the *only*
    // place the protected current-user-only DACL is applied —
    // `std::fs::OpenOptions` has no `SECURITY_ATTRIBUTES` hook, so
    // `set_create_mode_owner_only` can only pre-authorize this call by putting
    // `WRITE_DAC` on the handle. Doing that after `write_all` would leave the
    // file readable under the parent directory's inherited ACL for the length
    // of the write; doing it now means only an empty file is ever exposed.
    if let Err(source) = crate::platform::fsperm::set_file_owner_only(&tmp_file) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(RemoteConfigError::Io {
            path: tmp_path.display().to_string(),
            source,
        });
    }

    if let Err(source) = tmp_file.write_all(contents.as_bytes()) {
        // Best-effort cleanup; ignore secondary errors.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(RemoteConfigError::Io {
            path: tmp_path.display().to_string(),
            source,
        });
    }
    drop(tmp_file);

    std::fs::rename(&tmp_path, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp_path);
        RemoteConfigError::Io {
            path: path.display().to_string(),
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, host: &str) -> RemoteEntry {
        RemoteEntry {
            name: name.to_string(),
            kind: "ssh".to_string(),
            host: host.to_string(),
            port: 22,
            key: None,
            version: "0.40.0".to_string(),
            added_at: "2026-01-01T00:00:00+00:00".to_string(),
            upgraded_at: None,
            last_connected: None,
            id: None,
            user: None,
            jump_host: None,
            socket: None,
        }
    }

    fn desktop_entry(name: &str) -> RemoteEntry {
        RemoteEntry {
            key: Some("~/.ssh/id_ed25519".to_string()),
            version: UNMANAGED_VERSION.to_string(),
            id: Some("0123456789abcdef".to_string()),
            user: Some("dev@REALM".to_string()),
            jump_host: Some("bastion".to_string()),
            socket: Some("/run/user/1000/dot-agent-deck-attach.sock".to_string()),
            ..entry(name, "build.example.com")
        }
    }

    /// The registry shape a build before #1350 reads — copied, not imported,
    /// so it stays what those builds actually compiled. Strict in exactly the
    /// way they were: `type`, `version` and `added_at` required, unknown keys
    /// ignored.
    mod pre_1350 {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        pub struct RemoteEntry {
            pub name: String,
            #[serde(rename = "type")]
            pub kind: String,
            pub host: String,
            pub port: u16,
            #[serde(default)]
            pub key: Option<String>,
            pub version: String,
            pub added_at: String,
            #[serde(default)]
            pub upgraded_at: Option<String>,
            #[serde(default)]
            pub last_connected: Option<String>,
        }

        #[derive(Debug, Deserialize)]
        pub struct RemotesFile {
            #[serde(default)]
            pub remotes: Vec<RemoteEntry>,
        }
    }

    fn registry(dir: &tempfile::TempDir, contents: &str) -> std::path::PathBuf {
        let path = dir.path().join("remotes.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_row_this_build_writes_parses_under_the_pre_1350_shape() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("cli-box", "me@cli.example.com")).unwrap();
        add(&path, desktop_entry("desktop-box")).unwrap();

        let old: pre_1350::RemotesFile =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).expect("old CLI parses");
        assert_eq!(old.remotes.len(), 2);
        assert_eq!(old.remotes[1].kind, "ssh");
        assert_eq!(old.remotes[1].version, UNMANAGED_VERSION);
        assert_eq!(old.remotes[1].key.as_deref(), Some("~/.ssh/id_ed25519"));
    }

    #[test]
    fn keys_this_build_does_not_know_survive_an_update_and_a_remove() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "# my decks\n\
             future = \"top-level\"\n\n\
             [[remotes]]\n\
             name = \"a\"\n\
             type = \"ssh\"\n\
             host = \"a.example\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n\
             id = \"deck-a\"\n\
             jump_host = \"bastion\"\n\
             colour = \"green\" # a field from a newer build\n\n\
             [[remotes]]\n\
             name = \"b\"\n\
             type = \"ssh\"\n\
             host = \"b.example\"\n\
             port = 22\n\
             version = \"0.40.0\"\n\
             added_at = \"2026-01-01T00:00:00Z\"\n",
        );

        update(&path, DeckRef::Name("a"), |row| {
            row.version = "0.41.0".to_string();
        })
        .unwrap()
        .expect("row a exists");
        remove(&path, DeckRef::Name("b"))
            .unwrap()
            .expect("row b exists");

        let written = std::fs::read_to_string(&path).unwrap();
        for kept in [
            "# my decks",
            "future = \"top-level\"",
            "id = \"deck-a\"",
            "jump_host = \"bastion\"",
            "colour = \"green\" # a field from a newer build",
            "version = \"0.41.0\"",
        ] {
            assert!(written.contains(kept), "{kept:?} missing from:\n{written}");
        }
        assert!(!written.contains("b.example"), "{written}");
    }

    #[test]
    fn an_update_that_clears_an_optional_field_removes_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, desktop_entry("d")).unwrap();
        update(&path, DeckRef::Id("0123456789abcdef"), |row| {
            row.jump_host = None;
        })
        .unwrap()
        .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("jump_host"), "{written}");
        assert!(written.contains("socket ="), "{written}");
    }

    #[test]
    fn an_edit_lands_on_the_file_as_it_is_now_not_as_a_caller_last_saw_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, entry("first", "first.example")).unwrap();
        // A caller that loaded the list here…
        let stale = RemotesFile::load(&path).unwrap();
        // …while another writer adds a row…
        add(&path, entry("second", "second.example")).unwrap();
        // …then edits the row it knows about.
        update(&path, DeckRef::Name(&stale.remotes[0].name), |row| {
            row.port = 2222;
        })
        .unwrap()
        .unwrap();

        let now = RemotesFile::load(&path).unwrap();
        let names: Vec<_> = now.remotes.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["first", "second"]);
        assert_eq!(now.remotes[0].port, 2222);
    }

    #[test]
    fn the_empty_list_the_old_save_writes_can_be_added_to() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(&dir, "remotes = []\n");
        add(&path, entry("a", "a.example")).unwrap();
        assert_eq!(RemotesFile::load(&path).unwrap().remotes.len(), 1);
    }

    #[test]
    fn an_inline_list_of_rows_is_edited_rather_than_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "remotes = [{ name = \"a\", type = \"ssh\", host = \"a.example\", port = 22, \
             version = \"0.40.0\", added_at = \"2026-01-01T00:00:00Z\" }]\n",
        );
        add(&path, entry("b", "b.example")).unwrap();
        let names: Vec<_> = RemotesFile::load(&path)
            .unwrap()
            .remotes
            .into_iter()
            .map(|row| row.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn an_edit_refuses_a_file_this_build_cannot_read_and_leaves_it_alone() {
        let dir = tempfile::tempdir().unwrap();
        let original = "[[remotes]]\nname = \"a\"\n";
        let path = registry(&dir, original);
        assert!(matches!(
            add(&path, entry("b", "b.example")),
            Err(AddDeckError::Config(RemoteConfigError::Parse { .. }))
        ));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn an_edit_that_changes_nothing_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let original = "# hand-written\n[[remotes]]\nname = \"a\"\ntype = \"ssh\"\nhost = \"a\"\nport = 22\nversion = \"1.0.0\"\nadded_at = \"x\"\n";
        let path = registry(&dir, original);
        assert!(update(&path, DeckRef::Name("a"), |_| {}).unwrap().is_some());
        assert!(
            update(&path, DeckRef::Name("missing"), |_| {})
                .unwrap()
                .is_none()
        );
        assert!(remove(&path, DeckRef::Name("missing")).unwrap().is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        // And a no-op against a missing file does not create one.
        let absent = dir.path().join("absent.toml");
        assert!(remove(&absent, DeckRef::Name("a")).unwrap().is_none());
        assert!(!absent.exists());
    }

    #[test]
    fn add_refuses_a_duplicate_name_or_id_and_an_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remotes.toml");
        add(&path, desktop_entry("d")).unwrap();
        assert!(matches!(
            add(&path, entry("d", "other")),
            Err(AddDeckError::DuplicateName { .. })
        ));
        assert!(matches!(
            add(&path, desktop_entry("e")),
            Err(AddDeckError::DuplicateId { .. })
        ));
        assert!(matches!(
            add(&path, entry("has space", "h")),
            Err(AddDeckError::InvalidName(DeckNameError::ForbiddenByte {
                offset: 3,
                ..
            }))
        ));
    }

    #[test]
    fn existing_non_slug_names_still_load_and_can_be_edited_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = registry(
            &dir,
            "[[remotes]]\nname = \"My Box (prod)\"\ntype = \"ssh\"\nhost = \"h\"\nport = 22\n\
             version = \"0.30.0\"\nadded_at = \"2025-01-01T00:00:00Z\"\n",
        );
        let loaded = RemotesFile::load(&path).unwrap();
        assert_eq!(loaded.remotes[0].name, "My Box (prod)");
        let id = deck_id(&loaded.remotes[0]);
        assert!(is_usable_deck_id(&id), "{id}");
        update(&path, DeckRef::Id(&id), |row| row.port = 2200)
            .unwrap()
            .unwrap();
        assert_eq!(RemotesFile::load(&path).unwrap().remotes[0].port, 2200);
        remove(&path, DeckRef::Name("My Box (prod)"))
            .unwrap()
            .unwrap();
        assert!(RemotesFile::load(&path).unwrap().remotes.is_empty());
    }

    #[test]
    fn deck_names_are_slugs() {
        for good in [
            "prod",
            "Prod-1",
            "build.example.com",
            "a_b",
            "9",
            &"x".repeat(64),
        ] {
            assert_eq!(validate_deck_name(good), Ok(()), "{good}");
        }
        assert_eq!(validate_deck_name(""), Err(DeckNameError::Empty));
        assert_eq!(
            validate_deck_name(&"x".repeat(65)),
            Err(DeckNameError::TooLong { len: 65 })
        );
        assert_eq!(validate_deck_name("-x"), Err(DeckNameError::BadStart));
        assert_eq!(validate_deck_name(".x"), Err(DeckNameError::BadStart));
        for bad in ["a b", "a/b", "a@b", "é", "a\u{202e}b", "a;b", "a\nb"] {
            assert!(
                matches!(
                    validate_deck_name(bad),
                    Err(DeckNameError::ForbiddenByte { .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_derived_name_is_valid_and_unique() {
        let none = |_: &str| false;
        assert_eq!(
            derive_deck_name("Build.Example.com", None, none),
            "build.example.com"
        );
        assert_eq!(derive_deck_name("[2001:db8::1]", None, none), "2001-db8-1");
        assert_eq!(derive_deck_name("@@@", None, none), "deck");

        let taken = ["box", "dev-box", "box-2"];
        let is_taken = |name: &str| taken.contains(&name);
        assert_eq!(derive_deck_name("box", None, is_taken), "box-3");
        assert_eq!(derive_deck_name("box", Some("ops"), is_taken), "ops-box");
        assert_eq!(derive_deck_name("box", Some("dev"), is_taken), "box-3");

        let long = "h".repeat(80);
        let full = "h".repeat(64);
        let derived = derive_deck_name(&long, None, |name| name == full);
        assert_eq!(validate_deck_name(&derived), Ok(()));
        assert!(derived.ends_with("-2"), "{derived}");
        for host in ["a.b", "UPPER", "x--y", "ünï.côdé", "-lead", "trail-"] {
            let name = derive_deck_name(host, Some("u@realm"), none);
            assert_eq!(validate_deck_name(&name), Ok(()), "{host} → {name}");
        }
    }

    #[test]
    fn a_row_without_an_id_has_a_stable_derived_one() {
        let row = entry("prod", "h");
        assert_eq!(deck_id(&row), "n-prod");
        let odd = entry("My Box", "h");
        let id = deck_id(&odd);
        assert!(id.starts_with("h-") && id.len() == 18, "{id}");
        assert_eq!(deck_id(&odd), id, "deterministic");
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325, "FNV-1a offset basis");

        // A usable id is used as written; a reserved or malformed one is not.
        let with = |id: &str| RemoteEntry {
            id: Some(id.to_string()),
            ..entry("prod", "h")
        };
        assert_eq!(deck_id(&with("abc123")), "abc123");
        assert_eq!(deck_id(&with("LOCAL")), "n-prod");
        assert_eq!(deck_id(&with("all")), "n-prod");
        assert_eq!(deck_id(&with("has space")), "n-prod");
    }

    #[test]
    fn the_login_comes_from_the_user_field_or_the_host() {
        let folded = entry("a", "me@h.example");
        assert_eq!(login_and_host(&folded), (Some("me"), "h.example"));
        let explicit = RemoteEntry {
            user: Some("me@REALM".to_string()),
            ..entry("a", "h.example")
        };
        assert_eq!(login_and_host(&explicit), (Some("me@REALM"), "h.example"));
        assert_eq!(host_fields("h", Some("me")), ("me@h".to_string(), None));
        assert_eq!(
            host_fields("h", Some("me@REALM")),
            ("h".to_string(), Some("me@REALM".to_string()))
        );
        assert_eq!(host_fields("h", None), ("h".to_string(), None));
        assert_eq!(
            address_key(&entry("a", "me@H.Example")),
            ("h.example".to_string(), Some("me".to_string()), 22)
        );
    }
}
