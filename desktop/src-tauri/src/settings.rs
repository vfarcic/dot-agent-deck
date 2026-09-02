//! The desktop app's own settings document (PRD #803, M2).
//!
//! This is the *client-owned* half of the boundary rule the PRD writes down:
//! the desktop app gets everything else from the daemon, and the only thing it
//! owns is its own settings. Nothing in this module crosses the TUI↔daemon
//! protocol — the document is read and written entirely inside the desktop
//! process, and the daemon cannot observe it.
//!
//! # Where it lives
//!
//! [`config_dir`]`().join("desktop.toml")` — a **sibling** of the TUI's
//! `config.toml` and `keybindings.toml`, never a section inside them.
//! `DashboardConfig::save()` serialises its struct (`src/config.rs`), so a
//! `[desktop]` table the TUI does not know about would be silently deleted on
//! the next TUI write; the split is what keeps the two schemas from coupling.
//! [`SETTINGS_PATH_ENV`] overrides the whole path, mirroring the
//! `DOT_AGENT_DECK_CONFIG` convention and giving tests a seam.
//!
//! # Failure behaviour
//!
//! **Loading never fails.** A missing file, an unparseable file, an unreadable
//! file, an unusable path and an unknown enum value all yield defaults —
//! exactly as `DashboardConfig::load()` does. A settings file is not worth
//! failing an app launch over, so the failure is logged and never propagated.
//!
//! **The path is vetted and the read is bounded.** [`read_document`] requires
//! an absolute path with a file name whose target is absent or a regular file,
//! and reads at most [`MAX_SETTINGS_BYTES`]. That is not a privilege boundary —
//! anyone who can set [`SETTINGS_PATH_ENV`] can already run code as this user —
//! it is there because an app that hangs forever on a FIFO or dies on
//! `/dev/zero` is a miserable thing to debug.
//!
//! **Writing is atomic and owner-only.** A temp file in the same directory
//! under an unpredictable name, then a rename; mode 0o600 on Unix, a protected
//! DACL on Windows. Modelled on `dot_agent_deck::schedule_cli::write_atomic`.
//! [`save_to`] records what that deliberately does *not* defend against.
//!
//! **Writing also preserves what it does not understand.** The save merges the
//! serialised struct into the document already on disk rather than replacing
//! it, so an older build cannot delete a section a newer one wrote. See
//! [`merged_document`] — this is the one place where `#[serde(default)]`
//! genuinely does not give what it looks like it gives.
//!
//! # Adding a setting
//!
//! Add a field to your feature's section struct, or add a new section struct
//! and one line to [`DesktopSettings`]. `#[serde(default)]` gives it a default
//! and there is deliberately no `deny_unknown_fields`, so a field written by a
//! newer build survives an older build reading the file.
//!
//! Two rules constrain what may go in:
//!
//! 1. **A secret never goes in this document, and never in `localStorage`.**
//!    The document may hold a non-secret *reference* — which backend holds the
//!    key, or a boolean saying one is stored — and nothing more. A real
//!    credential belongs behind the `SecretStore` seam (PRD #803 M5), whose
//!    intended implementation is the OS keychain. The guard test
//!    [`tests::no_settings_key_may_look_like_a_secret`] fails the build rather
//!    than leaving this to a reviewer.
//! 2. **Field names stay `snake_case`, and single-word where it is natural.**
//!    The same struct is serialised to TOML (which a user hand-edits, and where
//!    `snake_case` is this repo's convention) *and* to JSON for the webview
//!    (where the desktop DTOs use `camelCase`). Every name today is one word,
//!    so the two agree byte for byte. The first genuinely multi-word field is
//!    the point at which a separate webview DTO has to be introduced — not a
//!    `rename_all` on this struct, which would make the TOML read badly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dot_agent_deck::platform::fsperm;
use dot_agent_deck::platform::paths::config_dir;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Overrides the whole settings path, mirroring `DOT_AGENT_DECK_CONFIG`
/// (`src/config.rs`). Also the seam every test uses instead of the real
/// config directory.
pub const SETTINGS_PATH_ENV: &str = "DOT_AGENT_DECK_DESKTOP_CONFIG";

/// The document's file name inside [`config_dir`].
const SETTINGS_FILE_NAME: &str = "desktop.toml";

/// Schema version carried by every document this build writes.
///
/// Open Question 4 in PRD #803: cheap insurance, and impossible to add
/// retroactively without a heuristic for "documents written before the field
/// existed". Nothing reads it yet — a future migration will.
pub const SETTINGS_VERSION: u32 = 1;

/// How the app picks its light/dark palette.
///
/// The *storage* is #803's; what the choice does to the UI is PRD #743's.
/// Serialised as a lowercase string in both TOML and JSON, because that is what
/// reads well in a hand-edited config file.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AppearanceMode {
    /// Follow the operating system's own light/dark preference.
    #[default]
    System,
    Light,
    Dark,
}

impl AppearanceMode {
    /// The exact token written to TOML and JSON. Pinned by
    /// [`tests::default_document_shape_is_pinned`].
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a stored token, falling back to the default.
    ///
    /// An unknown value is **not** an error: a document written by a newer
    /// build may name a mode this one has never heard of, and losing the whole
    /// document over one unreadable field would be the opposite of the
    /// unknown-key tolerance the rest of the schema is built for.
    fn from_str_lossy(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "light" => Self::Light,
            "dark" => Self::Dark,
            _ => Self::default(),
        }
    }
}

impl Serialize for AppearanceMode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AppearanceMode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // A derived unit-variant `Deserialize` would reject an unknown token,
        // and serde's `#[serde(other)]` is not available on an externally
        // tagged enum, so the fallback is spelled out here.
        Ok(Self::from_str_lossy(&String::deserialize(deserializer)?))
    }
}

/// The `[appearance]` section — PRD #743's tenant, stored here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceSettings {
    pub mode: AppearanceMode,
}

/// The whole settings document.
///
/// Deliberately carries exactly one section. A container that grows opinions
/// about its contents blocks its dependents, so #741's endpoints and #802's
/// voice backends each add their own section when they land — they are not
/// pre-created here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopSettings {
    pub version: u32,
    pub appearance: AppearanceSettings,
}

impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings::default(),
        }
    }
}

/// The document plus where it lives, which is what the settings surface renders.
///
/// A separate struct rather than a field on [`DesktopSettings`], because the
/// path is **not** part of the document: it is where the document is, it is
/// never written to TOML, and putting it on the struct would put it in the file
/// and into [`tests::default_document_shape_is_pinned`]'s pinned shape.
///
/// # Why this path reaches the webview when error paths deliberately do not
///
/// [`SettingsWriteError`] splits itself precisely so a `/home/<user>/…` path
/// never crosses the bridge, and that is not in tension with this. A path
/// leaking out of an *error* is incidental detail the user did not ask for; the
/// location of their own settings file is the answer to "where did that go?",
/// which PRD #803 makes a visible footer line specifically so it is answerable
/// without documentation. Same string, opposite intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DesktopSettingsSnapshot {
    pub settings: DesktopSettings,
    /// Absolute, as [`settings_path`] resolved it — including when
    /// [`SETTINGS_PATH_ENV`] pointed somewhere else, since the footer's job is
    /// to name the file this process will actually write.
    pub path: String,
}

/// [`load_from`] against [`settings_path`], plus that path, for the settings
/// surface. This is how the app loads its settings.
pub fn load_snapshot() -> DesktopSettingsSnapshot {
    let path = settings_path();
    DesktopSettingsSnapshot {
        settings: load_from(&path),
        path: path.display().to_string(),
    }
}

/// A settings write failure, split so a filesystem path never reaches the
/// webview.
///
/// Connection errors are already sanitised before they cross the bridge
/// (`dto::safe_message`); an error naming `/home/<user>/...` deserves the same
/// treatment. [`Self::detail`] is for the app's own log and carries the path;
/// [`Self::public`] is what the webview renders and never does. `io::Error`'s
/// own `Display` carries no path — `std` does not add that context — so the
/// public half stays specific ("Permission denied") without leaking anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsWriteError {
    detail: String,
    public: String,
}

impl SettingsWriteError {
    /// The operator-facing message, including the path. Log this.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The webview-facing message. Contains no path.
    pub fn public(&self) -> &str {
        &self.public
    }
}

impl std::fmt::Display for SettingsWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for SettingsWriteError {}

fn write_error(what: &str, path: &Path, cause: impl std::fmt::Display) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!("{what} {}: {cause}", path.display()),
        public: format!("{what} the desktop settings file: {cause}"),
    }
}

/// A path that cannot be a settings document at all, split the same way
/// [`write_error`] is so the reason crosses the bridge and the path does not.
fn path_error(reason: &str, path: &Path) -> SettingsWriteError {
    SettingsWriteError {
        detail: format!(
            "unusable desktop settings path {}: {reason}",
            path.display()
        ),
        public: format!("the desktop settings path is unusable: {reason}"),
    }
}

/// The largest settings document this build will read.
///
/// `desktop.toml` is a hand-edited preferences file: today's default document
/// is 40 bytes, and #741's endpoint list plus #802's model configuration are
/// kilobytes at the very outside. 256 KiB leaves about four orders of magnitude
/// of headroom over anything the schema can plausibly grow into, while turning
/// "the app read a multi-gigabyte file into memory because a path pointed at
/// one" into a named error instead of an out-of-memory kill.
pub const MAX_SETTINGS_BYTES: u64 = 256 * 1024;

/// Vet `path` as the settings document and read it, bounded.
///
/// `Ok(None)` is the ordinary first-run case: nothing is there yet. `Err` means
/// the path cannot be a settings document at all — [`load_from`] logs it and
/// falls back to defaults, and [`save_to`] refuses rather than writing over
/// whatever is actually at that name.
///
/// # This is about misconfiguration, not privilege
///
/// [`SETTINGS_PATH_ENV`] used to accept any non-empty string, and both load and
/// save then went straight to an unbounded `read_to_string`. Anyone who can set
/// this process's environment can already run code as this user, so none of
/// this is a privilege boundary. It is here because the *misconfiguration*
/// failures are miserable to debug: a FIFO blocks the app forever with no
/// message at all, `/dev/zero` exhausts memory, a large regular file is read
/// whole, and a relative path resolves against whatever directory the app
/// happened to be launched from — even though [`DesktopSettingsSnapshot`]
/// documents the path it reports as absolute.
///
/// So the target must be absolute, must have a file name, and must be either
/// **absent** or a **regular file**. The check is `symlink_metadata`, which does
/// not follow a link at the final component, so a symlink is rejected as a
/// symlink rather than quietly resolved to something else. Then at most
/// [`MAX_SETTINGS_BYTES`] are read.
///
/// One residual is accepted: a target swapped between the check and the open —
/// a regular file replaced by a FIFO in that window — still blocks. Closing it
/// means an `openat`-anchored read, which is the same complexity [`save_to`]
/// declines for the same reason, and it is written down there rather than
/// repeated here.
fn read_document(path: &Path) -> Result<Option<String>, SettingsWriteError> {
    if !path.is_absolute() {
        return Err(path_error("it is not an absolute path", path));
    }
    if path.file_name().is_none() {
        return Err(path_error("it names no file", path));
    }

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            // `io::Error`'s own `Display` carries no path — `std` does not add
            // that context — so the cause is safe to put in the public half.
            return Err(path_error(
                &format!("it cannot be inspected: {error}"),
                path,
            ));
        }
    };
    if let Some(kind) = unusable_kind(&meta) {
        return Err(path_error(
            &format!("it is {kind}, and the settings document must be a regular file"),
            path,
        ));
    }
    if meta.len() > MAX_SETTINGS_BYTES {
        return Err(oversized(path));
    }

    read_bounded(path)
}

fn oversized(path: &Path) -> SettingsWriteError {
    path_error(
        &format!("it is larger than the {MAX_SETTINGS_BYTES}-byte settings limit"),
        path,
    )
}

/// What `meta` describes, when it is not a plain regular file. `None` means it
/// is one.
fn unusable_kind(meta: &std::fs::Metadata) -> Option<&'static str> {
    let kind = meta.file_type();
    if kind.is_symlink() {
        return Some("a symlink");
    }
    #[cfg(windows)]
    {
        // `is_symlink` covers a symlink reparse point but not a junction or a
        // mount point, and following one of those lands somewhere the user
        // never named.
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Some("a reparse point");
        }
    }
    if kind.is_dir() {
        return Some("a directory");
    }
    if kind.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt as _;
        if kind.is_fifo() {
            return Some("a FIFO");
        }
        if kind.is_socket() {
            return Some("a socket");
        }
        if kind.is_block_device() {
            return Some("a block device");
        }
        if kind.is_char_device() {
            return Some("a character device");
        }
    }
    Some("of an unrecognised type")
}

/// Read an already-vetted regular file, refusing anything past
/// [`MAX_SETTINGS_BYTES`].
///
/// The bound is re-applied to the bytes actually read, not just to the size the
/// vet saw: the file can grow — or be replaced by a larger one — between the
/// two, and a limit that only consults `stat` would not be a limit.
fn read_bounded(path: &Path) -> Result<Option<String>, SettingsWriteError> {
    use std::io::Read as _;
    let file = std::fs::File::open(path)
        .map_err(|error| path_error(&format!("it cannot be read: {error}"), path))?;
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| path_error(&format!("it cannot be read: {error}"), path))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(oversized(path));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| path_error("it is not valid UTF-8", path))
}

/// The resolved path of the settings document.
///
/// [`SETTINGS_PATH_ENV`] wins when it is set and non-empty. An empty value is
/// treated as "unset": it can only arrive from a caller that meant to clear the
/// override, and honouring it would resolve to a path with no file name.
pub fn settings_path() -> PathBuf {
    match std::env::var(SETTINGS_PATH_ENV) {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => config_dir().join(SETTINGS_FILE_NAME),
    }
}

/// Load the settings document from `path`. Never fails — see the module docs.
///
/// Every caller passes a path: the app goes through [`load_snapshot`], which
/// needs the resolved path anyway to show the user where their settings live,
/// and every test passes one explicitly so none of them depends on
/// process-global environment state.
pub fn load_from(path: &Path) -> DesktopSettings {
    match read_document(path) {
        Ok(None) => DesktopSettings::default(),
        Ok(Some(contents)) => match toml::from_str(&contents) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!(
                    "Invalid desktop settings at {}: {error}; using defaults",
                    path.display()
                );
                DesktopSettings::default()
            }
        },
        Err(error) => {
            // The detail names the path and this is the app's own log, which is
            // the half of the split that is allowed to.
            eprintln!("{}; using defaults", error.detail());
            DesktopSettings::default()
        }
    }
}

/// Persist the settings document atomically and owner-only.
pub fn save(settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    save_to(&settings_path(), settings)
}

/// How many temp names [`save_to`] draws before giving up. A leftover temp file
/// from a crashed run would otherwise make every later save fail with
/// `AlreadyExists`, since the publish deliberately uses `create_new` and never
/// unlinks whatever holds a name it wanted.
const TEMP_NAME_ATTEMPTS: usize = 8;

/// [`save`] against an explicit path.
///
/// Writes a temp file in the **same directory** as `path`, then renames over
/// `path`. Rename within one directory is atomic on POSIX, so no reader ever
/// observes a partially written document and a failed write leaves the previous
/// one exactly where it was. The temp file is created with `create_new`
/// (`O_CREAT|O_EXCL`, which cannot follow a symlink someone planted at that
/// name) at owner-only mode, so the document is never briefly world-readable.
///
/// The path is vetted first — see [`read_document`] — so a save never creates a
/// directory for, or writes over, something that is not a settings document.
///
/// # What this deliberately does not defend against
///
/// An audit of this path recommended two further steps: rejecting a symlinked
/// or non-user-owned **parent** directory, and anchoring both the create and
/// the publish to one verified directory handle (`openat`/`renameat`-style) so
/// no name is resolved twice. Both are declined on purpose, and the reasoning
/// is here so the next auditor finds it rather than re-deriving it.
///
/// The destination is the **per-user config directory**. Any actor who can win
/// the temp-swap race in it can already write `desktop.toml` directly, so the
/// race buys an attacker nothing they do not already have — and
/// directory-handle anchoring is real work and real complexity to spend on a
/// preferences file. If this document ever holds something security-relevant —
/// a daemon endpoint under #741, a secret *reference* under #802 — that
/// calculus changes and the anchoring should be revisited.
///
/// Two residual **reliability** properties come with that, both accepted, both
/// worth knowing before either is reported as a bug:
///
/// - abrupt process death between [`create_temp`] and the rename leaves an
///   owner-only `.desktop.toml.tmp.*` file behind. Nothing reads it, and the
///   next save simply draws a different name — which is what
///   [`TEMP_NAME_ATTEMPTS`] is for;
/// - the parent directory is **not** fsync'd after the rename. The write is
///   therefore atomic to every live observer but not fully crash-durable: a
///   power loss immediately after a save can leave the previous document in
///   place.
pub fn save_to(path: &Path, settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    // Before anything is created: a rejected path must not leave a directory
    // behind, and an unreadable or over-limit document must not be replaced.
    let existing = read_document(path)?;

    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fsperm::create_owner_only_dir(parent)
        .map_err(|error| write_error("could not create the directory for", path, error))?;

    let contents = merged_document(path, existing.as_deref(), settings)?;

    let (mut file, tmp) = create_temp(parent, path)?;
    let published = (|| {
        // On Unix `create_new` + the owner-only creation mode already produced
        // 0o600; this re-asserts it, and on Windows it is where the DACL is
        // applied at all. Same defence in depth as `schedule_cli::write_atomic`.
        fsperm::set_file_owner_only(&file)?;
        use std::io::Write as _;
        file.write_all(contents.as_bytes())?;
        file.sync_all()
    })();
    drop(file);

    if let Err(error) = published.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_error("could not write", path, error));
    }
    Ok(())
}

/// Serialise `settings` over whatever the document at `path` already holds,
/// preserving every table and field this build does not know about.
///
/// **`#[serde(default)]` without `deny_unknown_fields` means *ignore*, not
/// *retain*.** It covers reading — an older build loads a newer build's
/// `[voice]` section without error — but the unknown table is dropped on the
/// way in, so serialising the struct straight out would delete it on the next
/// save. That is exactly the `DashboardConfig::save()` failure mode PRD #803
/// rejects sharing `config.toml` for, reproduced in our own file, and it would
/// break the container's central promise: that a feature can add a section and
/// trust an older build not to eat it.
///
/// `existing` is what [`save_to`] just read off disk, **not** what the frontend
/// loaded at startup: re-reading at save time is what lets a section another
/// process wrote between this app's read and its write survive as well. The
/// cost is one small read per save, on a file the app writes only when a user
/// changes a setting.
///
/// An unparseable or non-table document is treated as empty and therefore
/// replaced. Nothing can be preserved out of bytes that are not TOML, and
/// refusing the save instead would leave a user whose file got corrupted unable
/// to change a setting from inside the app — the same call [`load_from`]
/// already makes in the other direction.
///
/// **"Unparseable" means unparseable by *this* build**, which is a wider set
/// than "corrupt". A document a *newer* build wrote in a syntax this one does
/// not accept lands on the same path and is replaced, taking that build's
/// sections with it — the one case where the unknown-section preservation this
/// function exists for does not apply. Nothing in the schema can produce such a
/// document today (it is TOML written by `toml::to_string_pretty` either way),
/// so this is a property to know rather than a hazard to design around; it
/// becomes one the moment the format itself changes.
fn merged_document(
    path: &Path,
    existing: Option<&str>,
    settings: &DesktopSettings,
) -> Result<String, SettingsWriteError> {
    // `toml::from_str`, not `str::parse` — `Value`'s `FromStr` parses a single
    // TOML *value* expression, so a whole document fails it on the first key.
    let mut document = existing
        .and_then(|contents| toml::from_str::<toml::Table>(contents).ok())
        .unwrap_or_default();
    let owned = toml::Table::try_from(settings)
        .map_err(|error| write_error("could not serialize", path, error))?;
    merge_tables(&mut document, owned);
    toml::to_string_pretty(&document)
        .map_err(|error| write_error("could not serialize", path, error))
}

/// Deep-merge `incoming` into `base`: two tables merge key by key, anything
/// else replaces outright.
///
/// So a field the struct owns always wins over whatever the file held — the
/// struct is the authority on its own schema — while a key only the file has is
/// left exactly as it was, down to a field nested inside a section this build
/// *does* know.
///
/// An existing entry is edited **in place** rather than removed and
/// re-inserted, so every key keeps its position even if some dependency turns
/// on toml's `preserve_order` feature and the table stops being sorted.
fn merge_tables(base: &mut toml::Table, incoming: toml::Table) {
    for (key, value) in incoming {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge_tables(existing, incoming);
            }
            (Some(existing), value) => *existing = value,
            (None, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Exclusively create a fresh temp file next to `dest`, redrawing the name on
/// collision. Returns the open file and its path.
///
/// The suffix is **random** rather than the old `<pid>.<counter>`, which any
/// other process could compute. `create_new` (`O_CREAT|O_EXCL`) already means a
/// planted name costs a failed save rather than a write through someone else's
/// symlink, so the guessable form was a nuisance rather than a hole — but an
/// unpredictable name costs one hash and removes the question.
fn create_temp(parent: &Path, dest: &Path) -> Result<(std::fs::File, PathBuf), SettingsWriteError> {
    let mut last = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp = parent.join(format!(
            ".{SETTINGS_FILE_NAME}.tmp.{:016x}",
            unpredictable_suffix()
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        fsperm::set_create_mode_owner_only(&mut options);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // Deny every other principal an open handle for the lifetime of
            // ours, so nobody can be holding one across the DACL tightening
            // `set_file_owner_only` performs on the handle a moment later. The
            // rename happens after the handle is dropped, so this costs
            // nothing.
            options.share_mode(0);
        }
        match options.open(&tmp) {
            Ok(file) => return Ok((file, tmp)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => last = Some(error),
            Err(error) => {
                return Err(write_error(
                    "could not create a temp file beside",
                    dest,
                    error,
                ));
            }
        }
    }
    let cause = last
        .map(|error| error.to_string())
        .unwrap_or_else(|| "no temp name was attempted".to_string());
    Err(write_error(
        "could not create a temp file beside",
        dest,
        cause,
    ))
}

/// A temp-name suffix an outside observer cannot predict, with no new
/// dependency.
///
/// `RandomState` is seeded from the operating system, so hashing a
/// monotonically increasing counter and the current time under it gives a value
/// that is unique within the process and unguessable outside it. This names a
/// scratch file for a few milliseconds; it is not, and must not be used as,
/// a source of cryptographic randomness.
fn unpredictable_suffix() -> u64 {
    use std::hash::{BuildHasher as _, Hash as _, Hasher as _};

    static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    WRITE_COUNTER
        .fetch_add(1, Ordering::Relaxed)
        .hash(&mut hasher);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `settings_path` is the only thing here that reads the environment, and
    /// the environment is process-global while `cargo test` runs a module's
    /// tests as threads in one process. Every other test drives [`load_from`]
    /// and [`save_to`] with an explicit path instead, so this lock only ever
    /// serialises the one test below against itself.
    static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("settings tempdir")
    }

    fn dark() -> DesktopSettings {
        DesktopSettings {
            version: SETTINGS_VERSION,
            appearance: AppearanceSettings {
                mode: AppearanceMode::Dark,
            },
        }
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Names of every file directly inside `dir`, sorted.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_saved_document_round_trips() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        assert_eq!(load_from(&path), dark());
        // And the file a user would open reads the way the PRD promises.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[appearance]"), "unexpected document: {raw}");
        assert!(
            raw.contains("mode = \"dark\""),
            "unexpected document: {raw}"
        );
    }

    #[test]
    fn an_absent_file_yields_defaults() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        assert!(!path.exists());
        assert_eq!(load_from(&path), DesktopSettings::default());
        assert_eq!(
            DesktopSettings::default().appearance.mode,
            AppearanceMode::System
        );
    }

    #[test]
    fn loading_never_fails_on_a_malformed_or_wrongly_typed_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);

        for contents in [
            // Not TOML at all.
            "this is not = = toml [[[",
            // Valid TOML, wrong types throughout.
            "version = \"one\"\n[appearance]\nmode = 3\n",
            // Valid TOML, a scalar where a table belongs.
            "version = 1\nappearance = \"dark\"\n",
            // Empty.
            "",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                load_from(&path),
                DesktopSettings::default(),
                "document should have fallen back to defaults: {contents:?}"
            );
        }
    }

    #[test]
    fn an_unknown_appearance_value_falls_back_without_losing_the_document() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 9\n[appearance]\nmode = \"solarized\"\n").unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::System);
        // The rest of the document survives: only the unreadable field is
        // replaced, and the load is not downgraded to a whole-file default.
        assert_eq!(loaded.version, 9);
    }

    #[cfg(unix)]
    #[test]
    fn loading_never_fails_on_an_unreadable_file() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        set_mode(&path, 0o000);
        if std::fs::read_to_string(&path).is_ok() {
            set_mode(&path, 0o600);
            eprintln!(
                "SKIP: this process can read a 0o000 file (running privileged), so an \
                 unreadable document cannot be constructed here"
            );
            return;
        }
        assert_eq!(load_from(&path), DesktopSettings::default());
        set_mode(&path, 0o600);
    }

    #[test]
    fn unknown_sections_and_fields_do_not_break_a_load() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\
             future_toplevel = true\n\n\
             [appearance]\n\
             mode = \"light\"\n\
             future_field = \"whatever a newer build wrote\"\n\n\
             [voice]\n\
             backend = \"whisper\"\n",
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        assert_eq!(loaded.version, 1);
    }

    /// The property the merge exists for: an older build saving must not eat a
    /// newer build's section. Without it `#[serde(default)]` drops `[voice]` on
    /// the way in and the next save writes the struct straight over it.
    ///
    /// The unknown section is pinned **byte for byte** here, which is the
    /// strongest form of the guarantee and the one PRD #803 states. Note what
    /// that does and does not mean — see
    /// [`tests::an_unknown_section_keeps_its_data_but_not_its_formatting`].
    #[test]
    fn an_unknown_section_survives_a_load_modify_save_round_trip() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        let voice = "[voice]\nbackend = \"whisper\"\nlocal = true\nretries = 3\n";
        std::fs::write(
            &path,
            format!("version = 1\n\n[appearance]\nmode = \"light\"\n\n{voice}"),
        )
        .unwrap();

        let mut loaded = load_from(&path);
        assert_eq!(loaded.appearance.mode, AppearanceMode::Light);
        loaded.appearance.mode = AppearanceMode::Dark;
        save_to(&path, &loaded).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(voice),
            "the [voice] section did not survive byte for byte: {raw}"
        );
        // And the change the user actually made is on disk.
        assert_eq!(load_from(&path).appearance.mode, AppearanceMode::Dark);
    }

    /// The limit of "byte for byte", pinned so it is a known property rather
    /// than a surprise.
    ///
    /// The merge round-trips through `toml::Table`, which models *data*, so a
    /// save re-renders the whole document in the serializer's own canonical
    /// form. No unknown **data** is ever lost — every key, value and type comes
    /// back — but a comment is dropped and an inline array is re-flowed across
    /// lines. Preserving those needs a format-preserving parser (`toml_edit`),
    /// a dependency this does not carry.
    ///
    /// The practical consequence is worth knowing before someone reports it as
    /// a bug: a user who hand-annotates `desktop.toml` loses the annotations
    /// the next time the app writes a setting.
    #[test]
    fn an_unknown_section_keeps_its_data_but_not_its_formatting() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n\
             # which speech-to-text backend #802 picked\n\
             [voice]\n\
             backend = \"whisper\"\n\
             stages = [\"stt\", \"intent\"]\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();

        // The data is all there, with its types intact.
        let reparsed = toml::from_str::<toml::Table>(&raw).unwrap();
        let voice = reparsed["voice"].as_table().unwrap();
        assert_eq!(voice["backend"].as_str(), Some("whisper"));
        assert_eq!(
            voice["stages"].as_array().unwrap().len(),
            2,
            "unexpected document: {raw}"
        );

        // The formatting is not: the comment is gone and the array is re-flowed.
        assert!(!raw.contains("# which"), "comments survived: {raw}");
        assert!(
            !raw.contains("[\"stt\", \"intent\"]"),
            "the inline array survived: {raw}"
        );
    }

    /// The same property one level down: an unknown *field* inside a section
    /// this build does own. A section-granular merge would silently drop this.
    #[test]
    fn an_unknown_field_inside_a_known_section_survives_a_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 1\n\n[appearance]\nmode = \"light\"\nterminal = \"follow\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("terminal = \"follow\""),
            "the unknown appearance field was dropped: {raw}"
        );
    }

    /// The other half of the merge: preserving unknown keys must not make the
    /// file authoritative over the struct for a field the struct owns.
    #[test]
    fn a_known_field_the_struct_owns_wins_over_the_file() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(
            &path,
            "version = 99\n\n[appearance]\nmode = \"light\"\n\n[voice]\nbackend = \"whisper\"\n",
        )
        .unwrap();

        save_to(&path, &dark()).unwrap();

        let reloaded = load_from(&path);
        assert_eq!(reloaded.appearance.mode, AppearanceMode::Dark);
        assert_eq!(reloaded.version, SETTINGS_VERSION);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("99"), "the stale version survived: {raw}");
        assert!(raw.contains("[voice]"), "the merge lost [voice]: {raw}");
    }

    /// A corrupt document cannot be merged into, and refusing the save would
    /// lock the user out of their own settings from inside the app. It is
    /// replaced instead — the same call `load_from` makes in the other
    /// direction.
    #[test]
    fn an_unparseable_document_is_replaced_rather_than_failing_the_save() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "this is not [ valid toml\n").unwrap();

        save_to(&path, &dark()).unwrap();

        assert_eq!(load_from(&path), dark());
    }

    #[test]
    fn the_path_resolves_to_a_sibling_of_the_tui_config_and_honours_the_override() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // SAFETY: this is the only test that touches the environment, it holds
        // ENV_TEST_LOCK for the whole mutation, and it restores the prior value
        // before releasing it. Nothing here touches the filesystem, so the
        // developer's real ~/.config/dot-agent-deck/desktop.toml is never read
        // or written even while the override is unset.
        let prior = std::env::var(SETTINGS_PATH_ENV).ok();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };
        let default_path = settings_path();

        unsafe { std::env::set_var(SETTINGS_PATH_ENV, "/tmp/somewhere/else.toml") };
        let overridden = settings_path();

        // An empty override is treated as unset rather than as an empty path.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, "") };
        let empty_override = settings_path();

        unsafe {
            match prior {
                Some(value) => std::env::set_var(SETTINGS_PATH_ENV, value),
                None => std::env::remove_var(SETTINGS_PATH_ENV),
            }
        }

        assert_eq!(default_path, config_dir().join("desktop.toml"));
        assert_eq!(
            default_path.parent(),
            config_dir().join("config.toml").parent(),
            "the document must be a sibling of the TUI's config.toml, not a section inside it"
        );
        assert_eq!(overridden, Path::new("/tmp/somewhere/else.toml"));
        assert_eq!(empty_override, default_path);
    }

    /// The settings surface shows where the document lives, so the snapshot has
    /// to name the path this process would actually write — including under the
    /// env override, which is the only way a test or a packaged build ends up
    /// somewhere other than the config directory.
    #[test]
    fn the_snapshot_carries_the_path_the_process_would_write() {
        let _guard = ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempdir();
        let path = dir.path().join("elsewhere.toml");
        save_to(&path, &dark()).unwrap();

        // SAFETY: the lock above serialises every test that touches this var.
        unsafe { std::env::set_var(SETTINGS_PATH_ENV, &path) };
        let snapshot = load_snapshot();
        unsafe { std::env::remove_var(SETTINGS_PATH_ENV) };

        assert_eq!(snapshot.settings, dark());
        assert_eq!(snapshot.path, path.display().to_string());

        // The path rides alongside the document rather than inside it, so the
        // pinned document shape is untouched by this.
        let json = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(json["settings"]["appearance"]["mode"], "dark");
        assert_eq!(json["path"], path.display().to_string());
        assert!(
            json["settings"].get("path").is_none(),
            "the path must not have leaked into the document: {json}"
        );
    }

    #[test]
    fn a_successful_save_leaves_no_temp_file_behind() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(entries(dir.path()), vec![SETTINGS_FILE_NAME.to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_save_publishes_by_rename_rather_than_writing_the_document_in_place() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &DesktopSettings::default()).unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();

        // A writer that opened the destination and truncated it would expose a
        // partially written document; one that renames a finished temp file
        // over it cannot. The inode changing is that difference, observably.
        save_to(&path, &dark()).unwrap();
        let after = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(
            before, after,
            "the document must be replaced by rename, never truncated in place"
        );
        assert_eq!(load_from(&path), dark());
    }

    /// A directory at the destination used to be caught by the rename failing.
    /// It is now caught before anything is created at all, which is the point
    /// of vetting the path — but the property that mattered is the same one:
    /// a refused save leaves nothing behind.
    #[test]
    fn a_directory_at_the_destination_is_refused_before_anything_is_written() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        assert!(
            error.detail().contains("a directory"),
            "unexpected error: {error}"
        );
        assert_eq!(
            entries(dir.path()),
            vec![SETTINGS_FILE_NAME.to_string()],
            "a refused save must not leave a temp file behind"
        );
        // And the thing that was really there is untouched.
        assert_eq!(entries(&path), vec!["occupied".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_save_leaves_the_existing_document_intact() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        set_mode(dir.path(), 0o500);
        let probe = dir.path().join(".probe");
        if std::fs::File::create(&probe).is_ok() {
            let _ = std::fs::remove_file(&probe);
            set_mode(dir.path(), 0o700);
            eprintln!(
                "SKIP: this process can write into a 0o500 directory (running privileged), \
                 so a failing save cannot be constructed here"
            );
            return;
        }

        let error = save_to(&path, &DesktopSettings::default()).unwrap_err();
        set_mode(dir.path(), 0o700);

        assert!(!error.detail().is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a failed save must leave the previous document byte for byte"
        );
        assert_eq!(load_from(&path), dark());
    }

    #[cfg(unix)]
    #[test]
    fn a_saved_document_is_owner_only() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &dark()).unwrap();
        assert_eq!(
            mode_of(&path),
            0o600,
            "a deck-created desktop.toml must be owner-only"
        );

        // A rewrite re-asserts it rather than inheriting whatever the previous
        // file happened to carry.
        set_mode(&path, 0o644);
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn a_write_error_never_names_a_filesystem_path() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        let directory = dir.path().to_string_lossy().into_owned();
        assert!(
            error.detail().contains(&directory),
            "the logged detail should name the path: {}",
            error.detail()
        );
        assert!(
            !error.public().contains(&directory),
            "the webview-facing message must not leak a path: {}",
            error.public()
        );
        assert!(
            !error.public().contains(SETTINGS_FILE_NAME),
            "the webview-facing message must not leak a file name: {}",
            error.public()
        );
    }

    /// Every way a settings path can be refused, and the two properties that
    /// have to hold for all of them: **load never fails** (it logs and falls
    /// back to defaults) and **save refuses with a path-free public message**.
    ///
    /// Each kind is built rather than asserted about, because the whole value
    /// of the guard is that it recognises the real thing. The Unix-only kinds
    /// live in the test below.
    #[test]
    fn every_portable_path_rejection_is_refused_without_leaking_the_path() {
        let dir = tempdir();
        let a_directory = dir.path().join("as-a-directory");
        std::fs::create_dir(&a_directory).unwrap();

        let oversized = dir.path().join("oversized.toml");
        std::fs::write(&oversized, "#".repeat(MAX_SETTINGS_BYTES as usize + 1)).unwrap();
        assert!(std::fs::metadata(&oversized).unwrap().len() > MAX_SETTINGS_BYTES);

        // A root with no final component. `/` is not absolute on Windows, so
        // the two platforms need different spellings of the same idea.
        #[cfg(unix)]
        let no_file_name = PathBuf::from("/");
        #[cfg(windows)]
        let no_file_name = PathBuf::from(r"C:\");

        let cases: Vec<(&str, PathBuf, &str)> = vec![
            (
                "a relative path",
                PathBuf::from("desktop.toml"),
                "not an absolute path",
            ),
            ("a path with no file name", no_file_name, "names no file"),
            ("a directory", a_directory, "a directory"),
            ("an over-limit document", oversized, "settings limit"),
        ];

        for (what, path, expected) in cases {
            assert_reject(what, &path, expected);
        }
    }

    /// Both halves of a rejected path, for one kind: the load falls back to
    /// defaults, and the save refuses with the reason in the log detail and no
    /// path in the public message.
    fn assert_reject(what: &str, path: &Path, expected: &str) {
        assert_eq!(
            load_from(path),
            DesktopSettings::default(),
            "loading from {what} must fall back to defaults"
        );

        let error = match save_to(path, &dark()) {
            Err(error) => error,
            Ok(()) => panic!("saving to {what} must be refused"),
        };
        assert!(
            error.detail().contains(expected),
            "{what}: unexpected error: {error}"
        );
        assert!(
            !error.public().contains(&path.display().to_string()),
            "{what}: the public message leaked the path: {}",
            error.public()
        );
    }

    /// The kinds that only exist on Unix, and the two that motivated the guard
    /// in the first place: a **FIFO** blocked the app forever with no message,
    /// and a **character device** like `/dev/zero` exhausted memory. Neither is
    /// opened at all now — the rejection is on `lstat`, so this test cannot
    /// hang even if the guard regresses to following the final component.
    #[cfg(unix)]
    #[test]
    fn every_unix_only_path_rejection_is_refused_without_leaking_the_path() {
        let dir = tempdir();

        // A symlink at the final component, pointing at a perfectly good
        // document. The target must come back untouched: rejecting a symlink is
        // only meaningful if nothing was written through it.
        let target = dir.path().join("target.toml");
        save_to(&target, &DesktopSettings::default()).unwrap();
        let before = std::fs::read_to_string(&target).unwrap();
        let link = dir.path().join("as-a-symlink.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_reject("a symlink", &link, "a symlink");
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            before,
            "nothing may be written through a rejected symlink"
        );

        // A socket, which `std` can bind without help.
        let socket_path = dir.path().join("as-a-socket");
        let _socket = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        assert_reject("a socket", &socket_path, "a socket");

        // A FIFO, which it cannot.
        let fifo = dir.path().join("as-a-fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c_path` is a NUL-terminated path inside this test's own temp
        // directory and outlives the call; `mkfifo` reads it and returns.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            made,
            0,
            "mkfifo failed: {}",
            std::io::Error::last_os_error()
        );
        assert_reject("a FIFO", &fifo, "a FIFO");

        // And a character device, if this machine has the usual one.
        let zero = Path::new("/dev/zero");
        if zero.exists() {
            assert_reject("a character device", zero, "a character device");
        } else {
            eprintln!("SKIP: no /dev/zero on this machine");
        }
    }

    /// The limit is a limit: exactly [`MAX_SETTINGS_BYTES`] loads, one byte
    /// more is refused.
    ///
    /// [`read_bounded`] re-applies the bound to the bytes actually read, which
    /// covers a document that grows between the `stat` and the read. That race
    /// is not constructible deterministically, so it is asserted by the code
    /// rather than by a test — but the boundary itself is pinned here, and it
    /// is the boundary a hand-edited file can actually reach.
    #[test]
    fn the_byte_limit_admits_a_document_at_the_limit_and_refuses_one_past_it() {
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::write(&path, "version = 1\n").unwrap();
        assert_eq!(load_from(&path).version, 1);

        // Exactly at the limit is fine; one byte over is not.
        let mut at_limit = "version = 1\n".to_string();
        at_limit.push_str(&"#".repeat(MAX_SETTINGS_BYTES as usize - at_limit.len()));
        assert_eq!(at_limit.len() as u64, MAX_SETTINGS_BYTES);
        std::fs::write(&path, &at_limit).unwrap();
        assert_eq!(
            load_from(&path).version,
            1,
            "a document at the limit must load"
        );

        std::fs::write(&path, format!("{at_limit}#")).unwrap();
        assert_reject("an over-limit document", &path, "settings limit");
    }

    /// The temp name is unpredictable rather than `<pid>.<counter>`.
    ///
    /// A guessable name is one another process can plant first. `create_new`
    /// means the cost of that is a failed save rather than a write through
    /// someone else's symlink, so this is closing a nuisance rather than a
    /// hole — but the nuisance is free to close.
    #[test]
    fn temp_names_are_unpredictable_rather_than_the_pid_and_a_counter() {
        let dir = tempdir();
        let dest = dir.path().join(SETTINGS_FILE_NAME);

        let mut names = std::collections::BTreeSet::new();
        for _ in 0..16 {
            let (file, tmp) = create_temp(dir.path(), &dest).unwrap();
            drop(file);
            let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                !name.contains(&std::process::id().to_string()),
                "the temp name still carries the pid: {name}"
            );
            assert!(
                name.starts_with(&format!(".{SETTINGS_FILE_NAME}.tmp.")),
                "unexpected temp name: {name}"
            );
            names.insert(name);
            std::fs::remove_file(&tmp).unwrap();
        }
        assert_eq!(names.len(), 16, "temp names repeated: {names:?}");
    }

    /// The pinned shape of the default document, in the idiom of
    /// `dto::agent_mapping_is_frontend_stable`.
    ///
    /// This is deliberate friction. A new field shows up here as a diff, which
    /// forces the ownership question — "does this setting describe the client
    /// itself, or does it describe the work?" — to be answered in review rather
    /// than discovered by the feature that inherits it.
    #[test]
    fn default_document_shape_is_pinned() {
        const FRESH: &str = "version = 1\n\n[appearance]\nmode = \"system\"\n";
        let rendered = toml::to_string_pretty(&DesktopSettings::default()).unwrap();
        assert_eq!(rendered, FRESH);

        // The same bytes must come out of `save_to`, which no longer serializes
        // the struct directly — it merges the struct into the document already
        // on disk. Against **no** existing document that merge has to be a
        // no-op, or this pin would describe a shape the app never writes.
        let dir = tempdir();
        let path = dir.path().join(SETTINGS_FILE_NAME);
        save_to(&path, &DesktopSettings::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), FRESH);

        // The same struct crosses the Tauri IPC, so its JSON shape is the
        // frontend's contract and is pinned with it.
        let json = serde_json::to_value(DesktopSettings::default()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "version": 1, "appearance": { "mode": "system" } })
        );
        for mode in [
            AppearanceMode::System,
            AppearanceMode::Light,
            AppearanceMode::Dark,
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
            assert_eq!(AppearanceMode::from_str_lossy(mode.as_str()), mode);
        }
    }

    /// Substrings that make a key name look like it holds a credential.
    const SECRETISH: [&str; 5] = ["key", "token", "secret", "password", "credential"];

    /// Key names that legitimately contain one of [`SECRETISH`] because they
    /// are a *reference to* a credential rather than the credential itself —
    /// the one carve-out PRD #803 allows. Keep this list short, and add to it
    /// only with a reviewer who has read the rule below.
    const SECRETISH_ALLOWED: [&str; 2] = [
        // "which backend holds the key" — a name, not a secret.
        "secret_backend",
        // "a boolean saying one is stored" — presence, not the value.
        "has_api_key",
    ];

    const SECRET_RULE: &str = "\
PRD #803 sets one hard rule about credentials: a secret NEVER goes in \
desktop.toml and NEVER in localStorage. This document is world-visible to \
anyone with the user's disk, is synced by whatever backs up ~/.config, and is \
handed to the webview verbatim.\n\n\
The document may hold a non-secret REFERENCE — which backend holds the key, or \
a boolean saying one is stored — and nothing more. A real credential belongs \
behind the SecretStore seam (PRD #803 M5): store/load/delete keyed by a stable \
identifier, with the OS keychain as the intended implementation.\n\n\
If the name that tripped this really is a reference and not a credential, add \
it to SECRETISH_ALLOWED with a comment saying which of those two forms it is.";

    /// Every key name in a serialised document, including nested tables.
    fn key_names(value: &toml::Value, into: &mut Vec<String>) {
        if let toml::Value::Table(table) = value {
            for (key, nested) in table {
                into.push(key.clone());
                key_names(nested, into);
            }
        }
    }

    fn secretish_keys(value: &toml::Value) -> Vec<String> {
        let mut names = Vec::new();
        key_names(value, &mut names);
        names.retain(|name| {
            let lowered = name.to_ascii_lowercase();
            SECRETISH.iter().any(|pattern| lowered.contains(pattern))
                && !SECRETISH_ALLOWED.contains(&lowered.as_str())
        });
        names
    }

    #[test]
    fn no_settings_key_may_look_like_a_secret() {
        let document = toml::Value::try_from(DesktopSettings::default()).unwrap();
        let offenders = secretish_keys(&document);
        assert!(
            offenders.is_empty(),
            "the desktop settings document has key(s) that look like credentials: {}\n\n{SECRET_RULE}",
            offenders.join(", ")
        );
    }

    #[test]
    fn the_secret_guard_catches_a_credential_shaped_key() {
        // The guard's own logic, proven rather than assumed — an empty result
        // on the real document is only meaningful if a bad key would be caught.
        let bad = toml::from_str::<toml::Value>(
            "version = 1\n\n[voice]\napi_key = \"sk-live-nope\"\n\n[voice.remote]\nauth_token = \"t\"\n",
        )
        .unwrap();
        let mut offenders = secretish_keys(&bad);
        offenders.sort();
        assert_eq!(offenders, ["api_key", "auth_token"]);

        let referenced = toml::from_str::<toml::Value>(
            "[voice]\nsecret_backend = \"keychain\"\nhas_api_key = true\n",
        )
        .unwrap();
        assert!(secretish_keys(&referenced).is_empty());
    }
}
