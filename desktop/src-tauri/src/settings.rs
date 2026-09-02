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
//! file and an unknown enum value all yield defaults — exactly as
//! `DashboardConfig::load()` does. A settings file is not worth failing an app
//! launch over, so the failure is logged and never propagated.
//!
//! **Writing is atomic and owner-only.** A temp file in the same directory,
//! then a rename; mode 0o600 on Unix. Modelled on
//! `dot_agent_deck::schedule_cli::write_atomic`.
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

/// Load the settings document. Never fails — see the module docs.
pub fn load() -> DesktopSettings {
    load_from(&settings_path())
}

/// [`load`] against an explicit path, so tests never depend on process-global
/// environment state.
pub fn load_from(path: &Path) -> DesktopSettings {
    match std::fs::read_to_string(path) {
        Ok(contents) => match toml::from_str(&contents) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!(
                    "Invalid desktop settings at {}: {error}; using defaults",
                    path.display()
                );
                DesktopSettings::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DesktopSettings::default(),
        Err(error) => {
            eprintln!(
                "Failed to read desktop settings at {}: {error}; using defaults",
                path.display()
            );
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
pub fn save_to(path: &Path, settings: &DesktopSettings) -> Result<(), SettingsWriteError> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    fsperm::create_owner_only_dir(parent)
        .map_err(|error| write_error("could not create the directory for", path, error))?;

    let contents = toml::to_string_pretty(settings)
        .map_err(|error| write_error("could not serialize", path, error))?;

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

/// Exclusively create a fresh temp file next to `dest`, redrawing the name on
/// collision. Returns the open file and its path.
fn create_temp(parent: &Path, dest: &Path) -> Result<(std::fs::File, PathBuf), SettingsWriteError> {
    static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut last = None;
    for _ in 0..TEMP_NAME_ATTEMPTS {
        let tmp = parent.join(format!(
            ".{SETTINGS_FILE_NAME}.tmp.{}.{}",
            std::process::id(),
            WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        fsperm::set_create_mode_owner_only(&mut options);
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

    #[test]
    fn a_failed_publish_reports_an_error_and_removes_its_temp_file() {
        let dir = tempdir();
        // A directory at the destination makes the rename fail for every user,
        // privileged or not — it is a type error, not a permission one.
        let path = dir.path().join(SETTINGS_FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"x").unwrap();

        let error = save_to(&path, &dark()).unwrap_err();
        assert!(
            error.detail().contains("could not write"),
            "unexpected error: {error}"
        );
        assert_eq!(
            entries(dir.path()),
            vec![SETTINGS_FILE_NAME.to_string()],
            "a failed publish must not leave a temp file behind"
        );
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

    /// The pinned shape of the default document, in the idiom of
    /// `dto::agent_mapping_is_frontend_stable`.
    ///
    /// This is deliberate friction. A new field shows up here as a diff, which
    /// forces the ownership question — "does this setting describe the client
    /// itself, or does it describe the work?" — to be answered in review rather
    /// than discovered by the feature that inherits it.
    #[test]
    fn default_document_shape_is_pinned() {
        let rendered = toml::to_string_pretty(&DesktopSettings::default()).unwrap();
        assert_eq!(rendered, "version = 1\n\n[appearance]\nmode = \"system\"\n");

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
