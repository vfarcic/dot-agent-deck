//! PRD #1666 part 3 (Q35-18; R6 §9, R5 §7.1): the files an agent reads its
//! key bindings from, and whether the deck can establish that Ctrl+G still
//! runs the agent's external-editor action.
//!
//! A probe sends Ctrl+G, so the deck may send it only while the binding the
//! agent loaded is the default one. This module reads the sources and says
//! [`Hold`] when it can't establish that; it never makes the deck send a
//! rebound key. Nothing calls it yet: Q35-19's admission and Q35-20's
//! settlement are its readers.
//!
//! **A source's identity** ([`Identity`]) is `(dev, ino, size, mtime_ns,
//! ctime_ns)` of the opened file and a SHA-256 of its contents, read from one
//! open descriptor whose `fstat` must not move across the read
//! ([`read_source`]); a symlink is followed and its target's identity is
//! kept. A missing file is a read too ([`SourceRead::Absent`]), so a file
//! created where none was, or removed, differs from what was read before.
//! `ctime` can't be set back by a user, so a file changed and then restored
//! differs as well.
//!
//! **Claude Code** ([`ClaudeKeymap`], R6 §9.1) reads `keybindings.json`. The
//! deck records it at spawn and reads it again before each admission and at
//! each settlement ([`ClaudeKeymap::check`]); any difference from the spawn read holds the
//! rest of the generation ([`Hold::Changed`], CL-9t), even if a later read
//! matches again. With an unchanged file, P-KB1 (the release loads its
//! bindings before its first ready frame, M49b) is what makes the spawn
//! read the binding Claude loaded, so a release whose record doesn't verify
//! it holds ([`Hold::Unmeasured`]); every record leaves it absent until M49b
//! is measured. The spawn read's contents hold too when they bind Ctrl+G or
//! the external-editor action, or can't be read or parsed (CL-9).
//!
//! **Codex** ([`CodexSources`], R6 §9.2, R5 §7.1) reads `$CODEX_HOME/config.toml`,
//! the selected profile's `$CODEX_HOME/<name>.config.toml`, the trusted
//! project files for the pane's cwd, and each `-c key=value`. The deck reads
//! them before the spawn and again after the first ready frame; any
//! difference holds the generation. Otherwise the sources are merged in the
//! release record's precedence ([`KeymapSource`] order, a later source
//! winning key by key) and the effective `tui.keymap` is checked: a probe
//! is allowed only when `open_external_editor` is Ctrl+G (by default or
//! explicitly) and no other action is on Ctrl+G (CX-10b). A source present
//! (a file that exists, a selected profile, a `-c`) that the record doesn't
//! list has no known precedence and holds.

use crate::doorbell_activation::{EditorProbe, KeymapSource, Measurement};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The most bytes read from one source; a larger file is unreadable.
pub const SOURCE_BYTES: u64 = 1 << 20;

/// Claude Code's external-editor action.
pub const CLAUDE_EDITOR_ACTION: &str = "chat:externalEditor";

/// Codex's external-editor action.
pub const CODEX_EDITOR_ACTION: &str = "open_external_editor";

/// What a source file was when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    /// SHA-256 of the contents.
    pub hash: [u8; 32],
}

/// Why a source couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unreadable {
    /// Not a regular file (a directory, a FIFO, a device).
    NotRegular,
    /// Larger than [`SOURCE_BYTES`].
    TooLarge,
    /// Its `fstat` moved while it was read.
    Unstable,
    /// The open or the read failed.
    Io(std::io::ErrorKind),
    /// The platform has no file identity this module reads.
    Platform,
}

/// One read of a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRead {
    Absent,
    Present {
        identity: Identity,
        contents: Vec<u8>,
    },
    Unreadable(Unreadable),
}

/// Why the deck can't establish that Ctrl+G runs the agent's external
/// editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// The release's record doesn't verify P-KB1 (M49b).
    Unmeasured,
    /// A source changed since the read it is compared with (CL-9t,
    /// CX-10b); for Claude, for the rest of the generation.
    Changed,
    /// A source can't be read.
    Unreadable(Unreadable),
    /// A source can't be parsed.
    Unparsable,
    /// Ctrl+G is bound to another action, or the external-editor action to
    /// another key.
    Rebound,
    /// The keymap table holds something this module doesn't read as a
    /// binding (CX-10b's unknown key).
    UnknownKey,
    /// A source the release's record doesn't list, so its precedence isn't
    /// known.
    Unpinned,
}

/// Reads `path`, following a symlink: absent, its identity and contents, or
/// why it can't be read. One descriptor is opened (non-blocking, so a FIFO
/// can't hang it) and judged by `fstat` before and after the read; a file
/// that moved in between, or whose contents aren't the size `fstat` gave, is
/// [`Unreadable::Unstable`]. It does file I/O: call
/// it with no lock held.
pub fn read_source(path: &Path) -> SourceRead {
    #[cfg(unix)]
    {
        read_unix(path)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        SourceRead::Unreadable(Unreadable::Platform)
    }
}

#[cfg(unix)]
fn read_unix(path: &Path) -> SourceRead {
    use sha2::{Digest, Sha256};
    use std::io::{ErrorKind, Read};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == ErrorKind::NotFound => return SourceRead::Absent,
        Err(e) => return SourceRead::Unreadable(Unreadable::Io(e.kind())),
    };
    let stat = |file: &std::fs::File| {
        file.metadata().map(|m| {
            (
                m.file_type().is_file(),
                m.dev(),
                m.ino(),
                m.size(),
                i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
            )
        })
    };
    let before = match stat(&file) {
        Ok(before) => before,
        Err(e) => return SourceRead::Unreadable(Unreadable::Io(e.kind())),
    };
    let (regular, dev, ino, size, mtime_ns, ctime_ns) = before;
    if !regular {
        return SourceRead::Unreadable(Unreadable::NotRegular);
    }
    if size > SOURCE_BYTES {
        return SourceRead::Unreadable(Unreadable::TooLarge);
    }
    // One byte past the size `fstat` gave, so a file that grew is seen to.
    let mut contents = Vec::new();
    if let Err(e) = (&file).take(size + 1).read_to_end(&mut contents) {
        return SourceRead::Unreadable(Unreadable::Io(e.kind()));
    }
    match stat(&file) {
        Ok(after) if after == before && contents.len() as u64 == size => {}
        Ok(_) => return SourceRead::Unreadable(Unreadable::Unstable),
        Err(e) => return SourceRead::Unreadable(Unreadable::Io(e.kind())),
    }
    SourceRead::Present {
        identity: Identity {
            dev,
            ino,
            size,
            mtime_ns,
            ctime_ns,
            hash: Sha256::digest(&contents).into(),
        },
        contents,
    }
}

/// Where Claude Code reads `keybindings.json`: `$CLAUDE_CONFIG_DIR` when
/// set and non-empty, else `$HOME/.claude` (M39 measured the second), from
/// the agent's environment `env`. `None` when neither is known.
pub fn claude_keybindings_path(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    let dir = match set("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(set("HOME")?).join(".claude"),
    };
    Some(dir.join("keybindings.json"))
}

/// Claude Code's `keybindings.json` for one generation (R6 §9.1): the read
/// taken at spawn, and whether a later read has differed from it.
#[derive(Debug)]
pub struct ClaudeKeymap {
    path: PathBuf,
    at_spawn: SourceRead,
    loaded_at_start: bool,
    changed: AtomicBool,
}

impl ClaudeKeymap {
    /// Reads `path` at spawn, for a release whose record is `probe`.
    pub fn at_spawn(path: PathBuf, probe: &EditorProbe<'_>) -> ClaudeKeymap {
        let at_spawn = read_source(&path);
        ClaudeKeymap {
            path,
            at_spawn,
            loaded_at_start: matches!(probe.keymap_loaded_at_start, Measurement::Verified(())),
            changed: AtomicBool::new(false),
        }
    }

    /// The read taken at spawn.
    pub fn spawn_read(&self) -> &SourceRead {
        &self.at_spawn
    }

    /// Reads the file again, before an admission or at a settlement, and
    /// says whether the probe may go on. A read that differs from the spawn read holds this one and every
    /// later one ([`Hold::Changed`]), whatever they read. Otherwise P-KB1
    /// must be verified, and the spawn read must bind neither Ctrl+G nor
    /// [`CLAUDE_EDITOR_ACTION`]. It does file I/O: call it with no lock
    /// held.
    pub fn check(&self) -> Result<(), Hold> {
        if read_source(&self.path) != self.at_spawn {
            self.changed.store(true, Ordering::SeqCst);
        }
        if self.changed.load(Ordering::SeqCst) {
            return Err(Hold::Changed);
        }
        if !self.loaded_at_start {
            return Err(Hold::Unmeasured);
        }
        match &self.at_spawn {
            SourceRead::Absent => Ok(()),
            SourceRead::Unreadable(why) => Err(Hold::Unreadable(*why)),
            SourceRead::Present { contents, .. } => claude_bindings(contents),
        }
    }
}

/// Whether a `keybindings.json` leaves Ctrl+G on the external editor: it
/// mustn't name [`CLAUDE_EDITOR_ACTION`] or have a key with Ctrl+G in any
/// chord step, anywhere in the document. The format's layout isn't
/// measured beyond M39 (a `Chat` context that unbinds Ctrl+G and binds the
/// action elsewhere), so the check reads every key and string rather than
/// one layout, and an explicit default binding holds too.
fn claude_bindings(contents: &[u8]) -> Result<(), Hold> {
    fn walk(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(s) => s == CLAUDE_EDITOR_ACTION,
            serde_json::Value::Array(items) => items.iter().any(walk),
            serde_json::Value::Object(map) => map
                .iter()
                .any(|(key, value)| key.split_whitespace().any(is_ctrl_g) || walk(value)),
            _ => false,
        }
    }
    let doc: serde_json::Value = serde_json::from_slice(contents).map_err(|_| Hold::Unparsable)?;
    if walk(&doc) {
        Err(Hold::Rebound)
    } else {
        Ok(())
    }
}

/// Whether a key's spelling is Ctrl+G, in the spellings the agents' keymap
/// files are known to accept and their common variants.
pub fn is_ctrl_g(key: &str) -> bool {
    let key: String = key
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    matches!(
        key.as_str(),
        "ctrl+g" | "ctrl-g" | "control+g" | "control-g" | "c-g" | "<c-g>" | "^g"
    )
}

/// The sources one Codex launch reads its key bindings from (R6 §9.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexSources {
    /// `$CODEX_HOME/config.toml`.
    pub config: PathBuf,
    /// The selected profile's `$CODEX_HOME/<name>.config.toml`.
    pub profile: Option<PathBuf>,
    /// The trusted project files for the pane's cwd, lowest precedence
    /// first.
    pub projects: Vec<PathBuf>,
    /// Each `-c key=value`, in command-line order.
    pub overrides: Vec<String>,
}

impl CodexSources {
    /// The sources of a launch with `args` (the arguments after the
    /// program), `codex_home` and the trusted `projects`. `-c`/`--config`
    /// and `-p`/`--profile` are read in their separate, `=` and attached
    /// short forms; a flag missing its value, or one that isn't UTF-8, is
    /// [`Hold::Unparsable`].
    pub fn from_launch(
        codex_home: &Path,
        args: &[OsString],
        projects: Vec<PathBuf>,
    ) -> Result<CodexSources, Hold> {
        let mut profile = None;
        let mut overrides = Vec::new();
        // Longest first, so `-c=k=v` is the value `k=v`.
        const PREFIXES: [(&str, Flag); 6] = [
            ("--config=", Flag::Config),
            ("--profile=", Flag::Profile),
            ("-c=", Flag::Config),
            ("-p=", Flag::Profile),
            ("-c", Flag::Config),
            ("-p", Flag::Profile),
        ];
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let Some(arg) = arg.to_str() else {
                let lossy = arg.to_string_lossy();
                if PREFIXES.iter().any(|(prefix, _)| lossy.starts_with(prefix)) {
                    return Err(Hold::Unparsable);
                }
                continue;
            };
            let (kind, attached) = match arg {
                "-c" | "--config" => (Flag::Config, None),
                "-p" | "--profile" => (Flag::Profile, None),
                "--" => break,
                _ => match PREFIXES.iter().find_map(|(prefix, kind)| {
                    arg.strip_prefix(prefix).map(|rest| (*kind, Some(rest)))
                }) {
                    Some(found) => found,
                    None => continue,
                },
            };
            let value = match attached {
                Some(value) => value,
                None => args
                    .next()
                    .and_then(|v| v.to_str())
                    .ok_or(Hold::Unparsable)?,
            };
            match kind {
                Flag::Config => overrides.push(value.to_string()),
                Flag::Profile => profile = Some(value),
            }
        }
        let profile = match profile {
            Some("") => return Err(Hold::Unparsable),
            Some(name) => Some(codex_home.join(format!("{name}.config.toml"))),
            None => None,
        };
        Ok(CodexSources {
            config: codex_home.join("config.toml"),
            profile,
            projects,
            overrides,
        })
    }

    /// Reads every file source now. It does file I/O: call it with no lock
    /// held.
    pub fn read(&self) -> CodexRead {
        CodexRead {
            config: read_source(&self.config),
            profile: self.profile.as_deref().map(read_source),
            projects: self.projects.iter().map(|p| read_source(p)).collect(),
            overrides: self.overrides.clone(),
        }
    }
}

#[derive(Clone, Copy)]
enum Flag {
    Config,
    Profile,
}

/// One read of a launch's sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRead {
    pub config: SourceRead,
    pub profile: Option<SourceRead>,
    pub projects: Vec<SourceRead>,
    pub overrides: Vec<String>,
}

/// Whether Codex launched with these sources keeps Ctrl+G on the external
/// editor (R5 §7.1, R6 §9.2): `before` read before the spawn, `after` after
/// the first ready frame, merged in `precedence` (the release record's
/// [`EditorProbe::keymap_sources`], lowest first).
pub fn codex_keymap(
    before: &CodexRead,
    after: &CodexRead,
    precedence: &[KeymapSource],
) -> Result<(), Hold> {
    if before != after {
        return Err(Hold::Changed);
    }
    let present = [
        (
            KeymapSource::CodexConfig,
            before.config != SourceRead::Absent,
        ),
        (KeymapSource::CodexProfile, before.profile.is_some()),
        (
            KeymapSource::CodexProject,
            before
                .projects
                .iter()
                .any(|read| *read != SourceRead::Absent),
        ),
        (KeymapSource::CodexOverride, !before.overrides.is_empty()),
    ];
    if present
        .iter()
        .any(|(kind, present)| *present && !precedence.contains(kind))
    {
        return Err(Hold::Unpinned);
    }
    let mut merged = toml::Table::new();
    for kind in precedence {
        match kind {
            KeymapSource::CodexConfig => layer(&mut merged, &before.config)?,
            KeymapSource::CodexProfile => {
                if let Some(read) = &before.profile {
                    layer(&mut merged, read)?;
                }
            }
            KeymapSource::CodexProject => {
                for read in &before.projects {
                    layer(&mut merged, read)?;
                }
            }
            KeymapSource::CodexOverride => {
                for value in &before.overrides {
                    merge(&mut merged, override_table(value)?);
                }
            }
            KeymapSource::ClaudeKeybindings => return Err(Hold::Unpinned),
        }
    }
    codex_bindings(&merged)
}

fn layer(merged: &mut toml::Table, read: &SourceRead) -> Result<(), Hold> {
    match read {
        SourceRead::Absent => Ok(()),
        SourceRead::Unreadable(why) => Err(Hold::Unreadable(*why)),
        SourceRead::Present { contents, .. } => {
            let text = std::str::from_utf8(contents).map_err(|_| Hold::Unparsable)?;
            let table: toml::Table = text.parse().map_err(|_| Hold::Unparsable)?;
            merge(merged, table);
            Ok(())
        }
    }
}

/// `-c key=value` as the table it sets: the key a dotted path, the value
/// TOML, or a string when it doesn't parse as TOML.
fn override_table(arg: &str) -> Result<toml::Table, Hold> {
    let (key, value) = arg.split_once('=').ok_or(Hold::Unparsable)?;
    let path: Vec<&str> = key.trim().split('.').map(str::trim).collect();
    if path.iter().any(|p| p.is_empty()) {
        return Err(Hold::Unparsable);
    }
    let value = format!("v = {}", value.trim())
        .parse::<toml::Table>()
        .ok()
        .and_then(|mut t| t.remove("v"))
        .unwrap_or_else(|| toml::Value::String(value.trim().to_string()));
    let mut table = toml::Table::new();
    let (first, rest) = path.split_first().expect("non-empty");
    let value = rest.iter().rev().fold(value, |inner, segment| {
        let mut table = toml::Table::new();
        table.insert((*segment).to_string(), inner);
        toml::Value::Table(table)
    });
    table.insert((*first).to_string(), value);
    Ok(table)
}

/// Lays `over` on `base`: tables merge key by key, anything else replaces.
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(base)), toml::Value::Table(over)) => merge(base, over),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// The effective `tui.keymap`: contexts, each a table of action to a key or
/// a list of keys. Where [`CODEX_EDITOR_ACTION`] is bound, Ctrl+G must be
/// one of its keys (another key beside it moves nothing off Ctrl+G), and no
/// other action may be on Ctrl+G.
fn codex_bindings(merged: &toml::Table) -> Result<(), Hold> {
    let Some(tui) = merged.get("tui") else {
        return Ok(());
    };
    let tui = tui.as_table().ok_or(Hold::UnknownKey)?;
    let Some(keymap) = tui.get("keymap") else {
        return Ok(());
    };
    let keymap = keymap.as_table().ok_or(Hold::UnknownKey)?;
    for context in keymap.values() {
        let context = context.as_table().ok_or(Hold::UnknownKey)?;
        for (action, keys) in context {
            let keys: Vec<&str> = match keys {
                toml::Value::String(key) => vec![key.as_str()],
                toml::Value::Array(keys) => keys
                    .iter()
                    .map(|k| k.as_str().ok_or(Hold::UnknownKey))
                    .collect::<Result<_, _>>()?,
                _ => return Err(Hold::UnknownKey),
            };
            let editor = action == CODEX_EDITOR_ACTION;
            if keys.iter().any(|k| is_ctrl_g(k)) != editor {
                return Err(Hold::Rebound);
            }
        }
    }
    Ok(())
}
