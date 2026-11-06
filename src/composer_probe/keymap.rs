//! PRD #1666 part 3 (Q35-18; R6 §9, R5 §7.1): the files an agent reads its
//! key bindings from, and whether the deck can establish that Ctrl+G still
//! runs the agent's external-editor action.
//!
//! A probe sends Ctrl+G, so the deck may send it only while the binding the
//! agent loaded is the default one. This module reads the sources and says
//! [`Hold`] when it can't establish that; it never makes the deck send a
//! rebound key. Its readers are the daemon's: the spawn path's read before the
//! child exists, the observer's evidence when the permission settles, and
//! each probe's admission and settlement (`super::observe`).
//!
//! **A source's identity** ([`Identity`]) is `(dev, ino, size, mtime_ns,
//! ctime_ns)` of the opened file and a SHA-256 of its contents, read from one
//! open descriptor whose `fstat` must not move across the read
//! ([`read_source`]); a symlink is followed and its target's identity is
//! kept. A missing file is a read too ([`SourceRead::Absent`]), so a file
//! created where none was, or removed, differs from what was read before. A
//! file changed and then restored is seen when an identity field or its bytes
//! differ from the earlier read; the timestamps have the filesystem's
//! granularity, so the tuple isn't a count of edits, and an edit undone
//! within one tick with the same bytes isn't seen.
//!
//! The reads are bounded in bytes, not in time ([`read_source`]): a source on
//! a stalled filesystem can block the caller, so callers read with no lock
//! held, and nothing here is a deadline.
//!
//! **Claude Code** ([`ClaudeKeymap`], R6 §9.1) reads `keybindings.json`. The
//! deck records it before the spawn ([`KeymapBaseline`]) and reads it again
//! before each admission and at each settlement ([`ClaudeKeymap::check`]); any difference from the spawn read holds the
//! rest of the generation ([`Hold::Changed`], CL-9t), even if a later read
//! matches again. With an unchanged file, P-KB1 (the release loads its
//! bindings before its first ready frame, M49b) is what makes the spawn
//! read the binding Claude loaded, so a release whose record doesn't verify
//! it holds ([`Hold::Unmeasured`]); every record leaves it absent until M49b
//! is measured. The spawn read's contents hold too when they bind Ctrl+G or
//! the external-editor action, or can't be read or parsed (CL-9), and an
//! object that repeats a member name counts as unparsable. Where
//! Claude looks is measured only for `$HOME/.claude` (M39), so a
//! `CLAUDE_CONFIG_DIR` in the agent's environment holds
//! ([`claude_keybindings_path`]).
//!
//! **Codex** ([`CodexSources`], R6 §9.2, R5 §7.1) reads `$CODEX_HOME/config.toml`,
//! the selected profile's `$CODEX_HOME/<name>.config.toml`, the trusted
//! project files for the pane's cwd, and each `-c key=value`. The deck reads
//! them before the spawn ([`CodexBaseline`]) and again at each check, each
//! read compared with the last one accepted ([`CodexReads`]); any difference
//! holds, except a rewrite of `config.toml` that only steps Codex's own
//! start-up bookkeeping forward ([`CODEX_BOOKKEEPING`], [`codex_unchanged`]),
//! and a comparison that holds as changed holds the rest of the generation,
//! as Claude's does. A bound agent that launched with other sources than the spawn's
//! holds too (the deck wrapper's own `-c` pair left out,
//! [`CodexBaseline::bound`]). For the project files
//! the deck reads every `.codex/config.toml` Codex's root and trust rules
//! could pick ([`codex_project_files`]) and holds when one of them has a
//! keymap ([`codex_projects_guard`]). Otherwise the sources are merged in the
//! release record's precedence ([`KeymapSource`] order, a later source
//! winning key by key) and the effective `tui.keymap` is checked: a probe
//! is allowed only when `open_external_editor` is Ctrl+G (by default or
//! explicitly) and no other action is on Ctrl+G (CX-10b). Only the
//! contexts, actions and key spellings both pinned releases accept are read
//! ([`CODEX_KEYMAP`], [`codex_key`]); anything else in `tui.keymap` holds
//! ([`Hold::UnknownKey`]). A source present (a file that exists, a selected
//! profile, a `-c`) that the record doesn't list has no known precedence and
//! holds.

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
    /// A source can't be parsed, or (Claude's) an object in it names one
    /// member twice.
    Unparsable,
    /// Ctrl+G is bound to another action, or the external-editor action to
    /// another key.
    Rebound,
    /// The keymap table holds a context, action or key spelling outside what
    /// the pinned releases accept, or a value that isn't a binding (CX-10b's
    /// unknown key).
    UnknownKey,
    /// A source the release's record doesn't list, so its precedence isn't
    /// known.
    Unpinned,
    /// The editor the agent's Ctrl+G would open, as the agent's own
    /// environment selects it, isn't exactly the deck's helper
    /// ([`editor_is_helper`], the revcxact review's HIGH-1), or a file the
    /// agent copies variables from into its environment as it starts names
    /// the editor or can't be judged ([`claude_env_guard`],
    /// [`codex_dotenv_guard`], the revcxfix review's MEDIUM-A and
    /// MEDIUM-B): a probe would open another editor in the person's pane.
    Editor,
    /// Codex's home lies inside a folder Codex's sandbox may write to (the
    /// cwd, other than `<cwd>/.codex` itself, `/tmp` or `$TMPDIR`), or one
    /// of those can't be resolved ([`codex_home_checkable`]): Codex would
    /// write the box's file somewhere the deck doesn't read.
    CodexHome,
}

/// Reads `path`, following a symlink: absent, its identity and contents, or
/// why it can't be read. One descriptor is opened (non-blocking, so a FIFO
/// with no writer can't hang it) and judged by `fstat` before and after the
/// read; a file that moved in between, or whose contents aren't the size
/// `fstat` gave, is [`Unreadable::Unstable`].
///
/// It reads at most [`SOURCE_BYTES`] and one more, but nothing bounds how
/// long that takes: the path lookup, the open of a device (rejected only
/// after it opens), and a regular file on a stalled FUSE or network
/// filesystem can each block. Call it with no lock held, and don't treat it
/// as deadline-bounded.
pub fn read_source(path: &Path) -> SourceRead {
    read_source_within(path, SOURCE_BYTES)
}

/// [`read_source`] with its own bound: a file larger than `max` bytes is
/// [`Unreadable::TooLarge`].
pub fn read_source_within(path: &Path, max: u64) -> SourceRead {
    #[cfg(unix)]
    {
        read_unix(path, max)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, max);
        SourceRead::Unreadable(Unreadable::Platform)
    }
}

#[cfg(unix)]
fn read_unix(path: &Path, max: u64) -> SourceRead {
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
    if size > max {
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

/// Where Claude Code reads `keybindings.json`, from the agent's environment
/// `env`: `$HOME/.claude/keybindings.json`, the one location M39 measured.
/// [`Hold::Unmeasured`] when `CLAUDE_CONFIG_DIR` is present, empty included,
/// because which source Claude then reads isn't measured and the deck doesn't
/// guess it; and when `HOME` is unset, empty or relative, because where
/// Claude then looks isn't either.
pub fn claude_keybindings_path(env: impl Fn(&str) -> Option<OsString>) -> Result<PathBuf, Hold> {
    if env("CLAUDE_CONFIG_DIR").is_some() {
        return Err(Hold::Unmeasured);
    }
    let home = PathBuf::from(env("HOME").ok_or(Hold::Unmeasured)?);
    if !home.is_absolute() {
        return Err(Hold::Unmeasured);
    }
    Ok(home.join(".claude").join("keybindings.json"))
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
    /// Reads `path` now as the spawn read, for a release whose record is
    /// `probe`.
    pub fn at_spawn(path: PathBuf, probe: &EditorProbe<'_>) -> ClaudeKeymap {
        let at_spawn = read_source(&path);
        ClaudeKeymap::from_spawn_read(path, at_spawn, probe)
    }

    /// `path` as it was read before the spawn, `at_spawn` (the generation's
    /// [`KeymapBaseline`], A-20d-1b), for a release whose record is `probe`.
    pub fn from_spawn_read(
        path: PathBuf,
        at_spawn: SourceRead,
        probe: &EditorProbe<'_>,
    ) -> ClaudeKeymap {
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

/// Whether a `keybindings.json` leaves Ctrl+G on the external editor: no
/// string in the document, an object's member name or a value at any depth,
/// may name [`CLAUDE_EDITOR_ACTION`] or spell Ctrl+G, either in one of its
/// strokes ([`claude_strokes`]) or anywhere in it once whitespace is removed
/// ([`claude_mentions`]). The format's layout isn't measured beyond M39
/// (a `Chat` context that unbinds Ctrl+G and binds the action elsewhere), so
/// the check reads every string the same way rather than trusting one
/// layout, and an explicit default binding holds too.
///
/// An object that names one member twice, at any depth and compared after
/// its escapes are decoded (`"a"` and `"\u0061"`), is unparsable: which of
/// the two Claude reads isn't measured, and a parse into
/// [`serde_json::Value`] keeps only the last, so an earlier member's subtree
/// would never reach the check. The scan reads the raw document instead
/// ([`ClaudeScan`]).
fn claude_bindings(contents: &[u8]) -> Result<(), Hold> {
    let ClaudeScan(rebound) = serde_json::from_slice(contents).map_err(|_| Hold::Unparsable)?;
    if rebound { Err(Hold::Rebound) } else { Ok(()) }
}

/// One JSON value of a Claude `keybindings.json`, read straight from the
/// document: whether any string in it, a member name or a value at any
/// depth, names [`CLAUDE_EDITOR_ACTION`] or spells Ctrl+G. Deserializing it
/// fails when an object repeats a member name.
struct ClaudeScan(bool);

impl<'de> serde::Deserialize<'de> for ClaudeScan {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ClaudeScanVisitor)
    }
}

struct ClaudeScanVisitor;

impl ClaudeScanVisitor {
    fn holds(s: &str) -> bool {
        claude_mentions(s) || claude_strokes(s).iter().any(|k| is_ctrl_g(k))
    }
}

impl<'de> serde::de::Visitor<'de> for ClaudeScanVisitor {
    type Value = ClaudeScan;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _: bool) -> Result<ClaudeScan, E> {
        Ok(ClaudeScan(false))
    }

    fn visit_i64<E>(self, _: i64) -> Result<ClaudeScan, E> {
        Ok(ClaudeScan(false))
    }

    fn visit_u64<E>(self, _: u64) -> Result<ClaudeScan, E> {
        Ok(ClaudeScan(false))
    }

    fn visit_f64<E>(self, _: f64) -> Result<ClaudeScan, E> {
        Ok(ClaudeScan(false))
    }

    fn visit_unit<E>(self) -> Result<ClaudeScan, E> {
        Ok(ClaudeScan(false))
    }

    fn visit_str<E>(self, s: &str) -> Result<ClaudeScan, E> {
        Ok(ClaudeScan(Self::holds(s)))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<ClaudeScan, A::Error> {
        let mut rebound = false;
        while let Some(ClaudeScan(item)) = seq.next_element()? {
            rebound |= item;
        }
        Ok(ClaudeScan(rebound))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<ClaudeScan, A::Error> {
        let mut names = std::collections::HashSet::new();
        let mut rebound = false;
        while let Some(name) = map.next_key::<String>()? {
            rebound |= Self::holds(&name);
            if !names.insert(name) {
                return Err(serde::de::Error::custom("a member name repeats"));
            }
            let ClaudeScan(value) = map.next_value()?;
            rebound |= value;
        }
        Ok(ClaudeScan(rebound))
    }
}

/// Whether `s`, lower-cased and with all its whitespace removed, contains
/// [`CLAUDE_EDITOR_ACTION`] or any of [`CTRL_G`]'s spellings. Removing the
/// whitespace keeps the spellings [`is_ctrl_g`] reads (`c trl+g`, `< c-g >`)
/// that splitting a string into strokes would break apart, and a
/// containment test keeps a stroke that joining strokes would extend
/// (`ctrl+g -`). That over-holds: a string that only happens to contain one,
/// such as `abc-g` or `ctrl+gx`, holds as rebound too. That's accepted:
/// holding is the conservative answer, and every Claude release holds on
/// P-KB1 today anyway. The stroke check still runs beside it, for a stroke
/// whose words something other than whitespace separates (`ctrl + (g)`).
fn claude_mentions(s: &str) -> bool {
    let s = squeezed(s);
    s.contains(&CLAUDE_EDITOR_ACTION.to_lowercase()) || CTRL_G.iter().any(|g| s.contains(g))
}

/// `s` lower-cased, with all its whitespace removed.
fn squeezed(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The key strokes a string could spell: split at anything that can't be
/// part of a spelling [`is_ctrl_g`] reads, then a word that ends in `+`,
/// `-` or `^` joined to the next, and a word that starts with `+` or `-` to
/// the one before, so `ctrl + g` is one stroke and `ctrl+x ctrl+g` two.
fn claude_strokes(s: &str) -> Vec<String> {
    let mut strokes: Vec<String> = Vec::new();
    let mut joining = false;
    let words = s
        .split(|c: char| !(c.is_alphanumeric() || "+-^<>".contains(c)))
        .filter(|word| !word.is_empty());
    for word in words {
        match strokes.last_mut() {
            Some(last) if joining || word.starts_with(['+', '-']) => last.push_str(word),
            _ => strokes.push(word.to_string()),
        }
        joining = word.ends_with(['+', '-', '^']);
    }
    strokes
}

/// Whether a stroke's spelling is Ctrl+G for Claude Code's
/// `keybindings.json`, in any case and with spaces. The spelling the
/// measurements used is `ctrl+g` (M39's and M49b's file for Claude Code
/// 2.1.310: `{"bindings": [{"context": "Chat", "bindings": {"ctrl+g": null,
/// "ctrl+e": "chat:externalEditor"}}]}`). Claude's key grammar isn't
/// pinned, so the others are variants recognised so that a file written with
/// them holds, not spellings Claude is known to accept; a spelling Claude
/// accepts that isn't here isn't seen. Codex has its own, pinned grammar
/// ([`codex_key`]).
pub fn is_ctrl_g(key: &str) -> bool {
    CTRL_G.contains(&squeezed(key).as_str())
}

/// The spellings [`is_ctrl_g`] reads as Ctrl+G, lower-cased and without
/// whitespace.
const CTRL_G: &[&str] = &[
    "ctrl+g",
    "ctrl-g",
    "control+g",
    "control-g",
    "c-g",
    "<c-g>",
    "^g",
];

/// The most bytes read from one file whose `env` Claude Code applies
/// ([`claude_env_sources`]): Claude's own global config, `~/.claude.json`,
/// keeps per-project state and grows past [`SOURCE_BYTES`] on a busy
/// machine, and a larger one holds as [`Unreadable::TooLarge`].
pub const CLAUDE_ENV_BYTES: u64 = 16 << 20;

/// The most entries of Claude Code's managed drop-in directory read; more
/// hold as [`Unreadable::TooLarge`].
pub const CLAUDE_DROP_INS: usize = 64;

/// One place Claude Code may copy an `env` map from into its own
/// environment as it starts ([`claude_env_sources`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeEnvSource {
    /// A settings or config file whose top-level `env` member Claude applies.
    File(PathBuf),
    /// A directory of `*.json` settings files (the managed drop-ins).
    DropIns(PathBuf),
}

/// What one read of a [`ClaudeEnvSource`] found, as far as Claude's
/// environment goes: the `env` member alone, so a rewrite that leaves it as
/// it was (Claude rewrites `~/.claude.json` at every start, and writes
/// `.claude/settings.local.json` when a permission is granted) reads the
/// same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvRead {
    /// No file, or a document with no `env` member; for a directory, that it
    /// was listed (its files follow as their own reads).
    NoEnv,
    /// The document's `env` member, as parsed.
    Env(serde_json::Value),
    /// The file or directory exists but can't be read.
    Unreadable(Unreadable),
    /// The document isn't a JSON object once its comments are removed, or
    /// names `env` twice.
    Unparsable,
}

impl EnvRead {
    /// Whether a later read may be a write caught half done (a torn or
    /// moving file) rather than what the file now says.
    fn transient(&self) -> bool {
        matches!(
            self,
            EnvRead::Unparsable | EnvRead::Unreadable(Unreadable::Unstable)
        )
    }
}

/// One read of every [`ClaudeEnvSource`] of a launch, in source order, a
/// drop-in directory's files after its own entry.
pub type ClaudeEnvRead = Vec<(PathBuf, EnvRead)>;

/// The files Claude Code copies an `env` map from into its own environment
/// as it starts, for an agent whose environment is `env`, started in `cwd`
/// (the revcxfix review's MEDIUM-A). Read from the pinned 2.1.318 binary's
/// strings, and confirmed by running it in a sandbox HOME (a `VISUAL` in the
/// user, project, local and global-config files each opened that editor on
/// Ctrl+G, over an inherited `VISUAL`): the settings sources it applies in
/// turn (`userSettings`, `projectSettings`, `localSettings`, `flagSettings`,
/// `policySettings`) and its legacy global config's `env`.
///
/// - user settings: `$HOME/.claude/settings.json`, and
///   `cowork_settings.json` beside it, which Claude reads instead under a
///   switch of its own;
/// - global config: `$HOME/.claude.json` and the `-custom-oauth`,
///   `-staging-oauth` and `-local-oauth` names Claude gives it under other
///   endpoints, and the legacy `$HOME/.claude/.config.json`;
/// - project settings: `<cwd>/.claude/settings.json`;
/// - local settings: `.claude/settings.local.json` in `cwd` and in the
///   nearest ancestor holding a `.git` (Claude reads the git root's, and
///   the cwd's as a legacy location);
/// - managed settings: `/etc/claude-code/managed-settings.json` and the
///   `*.json` files of `/etc/claude-code/managed-settings.d`, and the
///   server-managed settings Claude caches in
///   `$HOME/.claude/remote-settings.json`, a plain settings document (seen
///   on 2.1.319, which rewrote it at each start for an account whose
///   organisation sends settings).
///
/// The flag source is a launch argument ([`claude_args_move_settings`]).
/// [`Hold::Unmeasured`] as [`claude_keybindings_path`] holds (a
/// `CLAUDE_CONFIG_DIR`, or no absolute `HOME`), and for a relative `cwd`.
pub fn claude_env_sources(
    env: impl Fn(&str) -> Option<OsString>,
    cwd: &Path,
) -> Result<Vec<ClaudeEnvSource>, Hold> {
    let keybindings = claude_keybindings_path(&env)?;
    if !cwd.is_absolute() {
        return Err(Hold::Unmeasured);
    }
    let config = keybindings
        .parent()
        .expect("keybindings.json is under the config directory");
    let home = config.parent().expect("the config directory is under HOME");
    let mut sources = vec![
        ClaudeEnvSource::File(config.join("settings.json")),
        ClaudeEnvSource::File(config.join("cowork_settings.json")),
        ClaudeEnvSource::File(config.join(".config.json")),
    ];
    for suffix in ["", "-custom-oauth", "-staging-oauth", "-local-oauth"] {
        sources.push(ClaudeEnvSource::File(
            home.join(format!(".claude{suffix}.json")),
        ));
    }
    sources.push(ClaudeEnvSource::File(cwd.join(".claude/settings.json")));
    sources.push(ClaudeEnvSource::File(
        cwd.join(".claude/settings.local.json"),
    ));
    if let Some(root) = cwd.ancestors().find(|dir| dir.join(".git").exists())
        && root != cwd
    {
        sources.push(ClaudeEnvSource::File(
            root.join(".claude/settings.local.json"),
        ));
    }
    let managed = Path::new("/etc/claude-code");
    sources.push(ClaudeEnvSource::File(managed.join("managed-settings.json")));
    sources.push(ClaudeEnvSource::DropIns(managed.join("managed-settings.d")));
    sources.push(ClaudeEnvSource::File(config.join("remote-settings.json")));
    Ok(sources)
}

/// Whether a Claude Code launch's arguments (after the program) name a
/// settings source the deck doesn't read: `--settings` (a file or inline
/// JSON whose `env` Claude applies) or `--project-config-root` (which moves
/// the project's settings), in their separate and `=` forms, anywhere.
pub fn claude_args_move_settings(args: &[OsString]) -> bool {
    args.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        ["--settings", "--project-config-root"]
            .iter()
            .any(|flag| arg == *flag || arg.starts_with(&format!("{flag}=")))
    })
}

/// Reads every source now, once. File I/O: no lock held.
pub fn read_claude_env(sources: &[ClaudeEnvSource]) -> ClaudeEnvRead {
    let mut reads = Vec::new();
    for source in sources {
        match source {
            ClaudeEnvSource::File(path) => reads.push((path.clone(), claude_env_file(path))),
            ClaudeEnvSource::DropIns(dir) => match std::fs::read_dir(dir) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    reads.push((dir.clone(), EnvRead::NoEnv));
                }
                Err(e) => reads.push((dir.clone(), EnvRead::Unreadable(Unreadable::Io(e.kind())))),
                Ok(entries) => {
                    let mut files = Vec::new();
                    let mut failed = None;
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                let path = entry.path();
                                if path
                                    .extension()
                                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                                {
                                    files.push(path);
                                }
                            }
                            Err(e) => failed = Some(EnvRead::Unreadable(Unreadable::Io(e.kind()))),
                        }
                    }
                    if files.len() > CLAUDE_DROP_INS {
                        failed = Some(EnvRead::Unreadable(Unreadable::TooLarge));
                    }
                    if let Some(failed) = failed {
                        reads.push((dir.clone(), failed));
                        continue;
                    }
                    files.sort();
                    reads.push((dir.clone(), EnvRead::NoEnv));
                    for path in files {
                        let read = claude_env_file(&path);
                        reads.push((path, read));
                    }
                }
            },
        }
    }
    reads
}

/// [`read_claude_env`] read again, up to twice more, 10 ms apart, while any
/// file reads as a write caught half done: for the reads whose result
/// stands for the whole generation (before the spawn, and at the
/// settlement), since another Claude may be rewriting `~/.claude.json` as
/// the deck reads it. File I/O and sleeps: no lock held, off the executor.
pub fn read_claude_env_settled(sources: &[ClaudeEnvSource]) -> ClaudeEnvRead {
    let mut read = read_claude_env(sources);
    for _ in 0..2 {
        if !read.iter().any(|(_, r)| r.transient()) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        read = read_claude_env(sources);
    }
    read
}

fn claude_env_file(path: &Path) -> EnvRead {
    match read_source_within(path, CLAUDE_ENV_BYTES) {
        SourceRead::Absent => EnvRead::NoEnv,
        SourceRead::Unreadable(why) => EnvRead::Unreadable(why),
        SourceRead::Present { contents, .. } => claude_env_member(&contents),
    }
}

/// The top-level `env` member of a Claude settings or config document,
/// read after its `//` and `/* */` comments are removed (Claude's settings
/// files are JSONC): only that member is parsed, every other one skipped.
/// A document that isn't a JSON object, or names `env` twice (which one
/// Claude keeps isn't measured), is [`EnvRead::Unparsable`].
pub fn claude_env_member(contents: &[u8]) -> EnvRead {
    let cleaned = without_json_comments(contents);
    match serde_json::from_slice::<EnvMember>(&cleaned) {
        Ok(EnvMember(Some(env))) => EnvRead::Env(env),
        Ok(EnvMember(None)) => EnvRead::NoEnv,
        Err(_) => EnvRead::Unparsable,
    }
}

/// A document's `env` member, or none; fails for a document that isn't an
/// object or that names `env` twice.
struct EnvMember(Option<serde_json::Value>);

impl<'de> serde::Deserialize<'de> for EnvMember {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = EnvMember;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<EnvMember, A::Error> {
                let mut env = None;
                while let Some(name) = map.next_key::<String>()? {
                    if name == "env" {
                        if env.is_some() {
                            return Err(serde::de::Error::custom("`env` repeats"));
                        }
                        env = Some(map.next_value::<serde_json::Value>()?);
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(EnvMember(env))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

/// `contents` with its `//` line and `/* */` block comments turned into
/// blanks (newlines kept), outside strings.
fn without_json_comments(contents: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(contents.len());
    let mut i = 0;
    let mut in_string = false;
    while i < contents.len() {
        let c = contents[i];
        let next = contents.get(i + 1).copied();
        if in_string {
            out.push(c);
            if c == b'\\'
                && let Some(next) = next
            {
                out.push(next);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_string = false;
            }
            i += 1;
        } else if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
        } else if c == b'/' && next == Some(b'/') {
            while i < contents.len() && contents[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
        } else if c == b'/' && next == Some(b'*') {
            out.extend_from_slice(b"  ");
            i += 2;
            while i < contents.len() && !(contents[i] == b'*' && contents.get(i + 1) == Some(&b'/'))
            {
                out.push(if contents[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < contents.len() {
                out.extend_from_slice(b"  ");
                i += 2;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Whether a read of Claude Code's `env` sources leaves its editor to the
/// environment it was started with: every file absent, or without an `env`
/// member, or with an `env` object none of whose names is, in any case,
/// `VISUAL` or `EDITOR`. A name there would replace the helper the deck put
/// in Claude's environment (Claude applies these over what it inherited),
/// so it holds as [`Hold::Editor`]; so does anything the deck can't judge:
/// a file that exists but can't be read or parsed, an `env` that isn't an
/// object, and a managed drop-in directory that can't be listed.
pub fn claude_env_guard(read: &ClaudeEnvRead) -> Result<(), Hold> {
    for (_, env) in read {
        match env {
            EnvRead::NoEnv => {}
            EnvRead::Env(serde_json::Value::Object(names)) => {
                if names.keys().any(|name| {
                    EDITOR_ENV
                        .iter()
                        .any(|editor| name.eq_ignore_ascii_case(editor))
                }) {
                    return Err(Hold::Editor);
                }
            }
            EnvRead::Env(_) | EnvRead::Unreadable(_) | EnvRead::Unparsable => {
                return Err(Hold::Editor);
            }
        }
    }
    Ok(())
}

/// Claude Code's `env` sources as the spawn path read them, before the child
/// existed (A-20d-1b's rule for `keybindings.json`, applied to them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeEnvBaseline {
    pub sources: Vec<ClaudeEnvSource>,
    pub read: ClaudeEnvRead,
}

impl ClaudeEnvBaseline {
    /// The sources for the spawn's environment `env` and working directory
    /// `cwd`, read now ([`read_claude_env_settled`]). File I/O: no lock
    /// held.
    pub fn at_spawn(
        env: impl Fn(&str) -> Option<OsString>,
        cwd: &Path,
    ) -> Result<ClaudeEnvBaseline, Hold> {
        let sources = claude_env_sources(env, cwd)?;
        let read = read_claude_env_settled(&sources);
        Ok(ClaudeEnvBaseline { sources, read })
    }

    /// The settlement's judgement for the bound agent, with its own
    /// environment `env`, arguments `args` (after the program; `None` when
    /// they couldn't be read) and working directory `cwd`: the same sources
    /// as the spawn's, else [`Hold::Changed`]; no settings source on its
    /// arguments, else [`Hold::Editor`]; a read now that is the spawn's,
    /// else [`Hold::Changed`]; and the spawn's read passing
    /// [`claude_env_guard`]. File I/O and [`read_claude_env_settled`]'s
    /// sleeps: no lock held.
    pub fn bound(
        &self,
        env: impl Fn(&str) -> Option<OsString>,
        args: Option<&[OsString]>,
        cwd: Option<&Path>,
    ) -> Result<(), Hold> {
        let (Some(args), Some(cwd)) = (args, cwd) else {
            return Err(Hold::Unmeasured);
        };
        if claude_env_sources(env, cwd)? != self.sources {
            return Err(Hold::Changed);
        }
        if claude_args_move_settings(args) {
            return Err(Hold::Editor);
        }
        if read_claude_env_settled(&self.sources) != self.read {
            return Err(Hold::Changed);
        }
        claude_env_guard(&self.read)
    }
}

/// Claude Code's `env` sources for one generation after its settlement: the
/// spawn's read, which [`ClaudeEnv::check`] compares each later read with.
#[derive(Debug)]
pub struct ClaudeEnv {
    sources: Vec<ClaudeEnvSource>,
    at_spawn: ClaudeEnvRead,
    changed: AtomicBool,
}

impl ClaudeEnv {
    /// A generation's `env` sources from its spawn baseline, once
    /// [`ClaudeEnvBaseline::bound`] passed.
    pub fn new(baseline: &ClaudeEnvBaseline) -> ClaudeEnv {
        ClaudeEnv {
            sources: baseline.sources.clone(),
            at_spawn: baseline.read.clone(),
            changed: AtomicBool::new(false),
        }
    }

    /// Reads the sources again, before an admission or at a settlement. A
    /// read that differs from the spawn's holds this check and every later
    /// one ([`Hold::Changed`]), as [`ClaudeKeymap::check`] does, except a
    /// file that reads as a write caught half done, which holds this check
    /// alone ([`Hold::Unparsable`]) and is read again at the next. It does
    /// file I/O: call it with no lock held.
    pub fn check(&self) -> Result<(), Hold> {
        if self.changed.load(Ordering::SeqCst) {
            return Err(Hold::Changed);
        }
        let now = read_claude_env(&self.sources);
        if now == self.at_spawn {
            return claude_env_guard(&self.at_spawn);
        }
        if now.len() == self.at_spawn.len()
            && now
                .iter()
                .zip(&self.at_spawn)
                .all(|((path, read), (was_path, was))| {
                    path == was_path && (read == was || read.transient())
                })
        {
            return Err(Hold::Unparsable);
        }
        self.changed.store(true, Ordering::SeqCst);
        Err(Hold::Changed)
    }
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
    /// `$CODEX_HOME/.env`, which Codex loads into its own environment as it
    /// starts ([`codex_dotenv_guard`]): no key binding, but it can name the
    /// editor.
    pub dotenv: PathBuf,
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
            dotenv: codex_home.join(".env"),
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
            dotenv: read_source(&self.dotenv),
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
    pub dotenv: SourceRead,
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
    codex_unchanged(before, after)?;
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

/// Whether `after` is the same launch's sources as `before` (CX-10b), else
/// [`Hold::Changed`]. Every source compares by its whole read (identity
/// and bytes, [`read_source`]), except that `$CODEX_HOME/config.toml` may
/// also differ by Codex's own bookkeeping ([`codex_config_unchanged`]).
pub fn codex_unchanged(before: &CodexRead, after: &CodexRead) -> Result<(), Hold> {
    if codex_config_unchanged(&before.config, &after.config)
        && before.profile == after.profile
        && before.projects == after.projects
        && before.overrides == after.overrides
        && before.dotenv == after.dotenv
    {
        Ok(())
    } else {
        Err(Hold::Changed)
    }
}

/// The keys Codex writes into its own `$CODEX_HOME/config.toml` as it
/// starts, under `[tui]`, each with the only shape it writes: the one-time
/// screen-reader probe's marker, a boolean (`tui/src/screen_reader.rs`,
/// once per home), and the startup tooltip's shown counts, a table of
/// model slug to count (`tui/src/app/startup_prompts.rs`, raised on each of
/// a home's first starts up to `MODEL_AVAILABILITY_NUX_MAX_SHOW_COUNT`, 4).
/// A `git grep` for either name in both pinned trees (`rust-v0.160.0`
/// `a956835`, `rust-v0.164.0` `6aaac69`; checkouts outside this repository,
/// not vendored here) found it only in the settings structs and the config
/// schema, the config editor, that probe, that tooltip, a sample binary and
/// tests: no hit in the keymap or the key-handling code.
pub const CODEX_BOOKKEEPING: [&str; 2] = ["screen_reader_detection_done", "model_availability_nux"];

/// The highest tooltip count Codex writes: it raises a model's count only
/// while it is below `MODEL_AVAILABILITY_NUX_MAX_SHOW_COUNT`, 4, in both
/// pinned releases (`tui/src/app/startup_prompts.rs`, `rust-v0.160.0` :242,
/// `rust-v0.164.0` :246), and the cxkey measurement's counts stopped at 4.
pub const CODEX_NUX_MAX_COUNT: i64 = 4;

/// Whether `config.toml` read as `after` loads what it did as `before`:
/// the same read, or (cxkey) a rewrite whose only difference is a step
/// forward in Codex's own bookkeeping ([`CODEX_BOOKKEEPING`],
/// [`codex_bookkeeping_forward`]). Both are parsed; everything but those
/// keys, in those shapes, must be equal, so any other key, a bookkeeping
/// key of another shape, an unreadable or unparsable read, a rewrite that
/// leaves the bookkeeping as it was (a hand edit undone, a touch) and one
/// that moves it in a way Codex doesn't all differ. An absent file loads
/// as an empty one.
fn codex_config_unchanged(before: &SourceRead, after: &SourceRead) -> bool {
    if before == after {
        return true;
    }
    match (codex_config_split(before), codex_config_split(after)) {
        (Some((before, kept_before)), Some((after, kept_after))) => {
            before == after && codex_bookkeeping_forward(&kept_before, &kept_after)
        }
        _ => false,
    }
}

/// Whether the bookkeeping `after` is one Codex could have written over
/// `before` (the revcxkey review's M1), as both pinned releases write it:
/// the screen-reader marker set to `true` where it was absent or `false`
/// (`tui/src/screen_reader.rs`, which writes only `true`), and tooltip counts
/// that each stay or rise by exactly 1 to at most [`CODEX_NUX_MAX_COUNT`],
/// an absent count reading as 0 (`tui/src/app/startup_prompts.rs`, which
/// raises one model's count by 1 per start and rewrites the others as they
/// were), so a new model's count is 1, and at most one count moves.
/// Something must move, and no key may go: a marker or a count removed, a
/// marker cleared, a count lowered, raised by more than 1 or past the
/// maximum, a new one other than 1, or two counts moved in one rewrite is
/// not Codex's write.
fn codex_bookkeeping_forward(before: &toml::Table, after: &toml::Table) -> bool {
    let marker = |kept: &toml::Table| {
        kept.get(CODEX_BOOKKEEPING[0])
            .and_then(toml::Value::as_bool)
    };
    let (marker_was, marker_now) = (marker(before), marker(after));
    let marker_ok = marker_was == marker_now
        || (matches!(marker_was, None | Some(false)) && marker_now == Some(true));
    // [`codex_config_split`] keeps a table only when every count is an
    // integer, so `as_integer` below fails only for a key that isn't kept.
    let counts = |kept: &toml::Table| {
        kept.get(CODEX_BOOKKEEPING[1])
            .and_then(toml::Value::as_table)
            .cloned()
    };
    let (counts_was, counts_now) = (counts(before), counts(after));
    let table_kept = counts_was.is_none() || counts_now.is_some();
    let (was, now) = (
        counts_was.unwrap_or_default(),
        counts_now.unwrap_or_default(),
    );
    let counts_ok = was.keys().all(|model| now.contains_key(model))
        && now.iter().all(|(model, count)| {
            let Some(count) = count.as_integer() else {
                return false;
            };
            match was.get(model).map(toml::Value::as_integer) {
                None => count == 1,
                Some(Some(prior)) => {
                    count == prior || (count == prior + 1 && count <= CODEX_NUX_MAX_COUNT)
                }
                Some(None) => false,
            }
        });
    // At most one count moves (the revcxkey2 review's NIT-4): each tooltip
    // write raises one model's count (`prepare_startup_tooltip_override`,
    // both pinned trees). A comparison spanning two such writes that moved
    // different models' counts holds too, which fails closed.
    let counts_moved = now
        .iter()
        .filter(|(model, count)| was.get(*model) != Some(*count))
        .count();
    let moved = marker_was != marker_now || counts_moved > 0;
    marker_ok && table_kept && counts_ok && counts_moved <= 1 && moved
}

/// `config.toml` parsed, and the bookkeeping taken out of it: the rest and
/// the bookkeeping. `None` when it can't be read or parsed.
fn codex_config_split(read: &SourceRead) -> Option<(toml::Table, toml::Table)> {
    let mut table: toml::Table = match read {
        SourceRead::Absent => toml::Table::new(),
        SourceRead::Present { contents, .. } => std::str::from_utf8(contents).ok()?.parse().ok()?,
        SourceRead::Unreadable(_) => return None,
    };
    let mut kept = toml::Table::new();
    if let Some(toml::Value::Table(tui)) = table.get_mut("tui") {
        if matches!(tui.get(CODEX_BOOKKEEPING[0]), Some(toml::Value::Boolean(_))) {
            kept.insert(
                CODEX_BOOKKEEPING[0].into(),
                tui.remove(CODEX_BOOKKEEPING[0])?,
            );
        }
        if let Some(toml::Value::Table(counts)) = tui.get(CODEX_BOOKKEEPING[1])
            && counts
                .values()
                .all(|count| count.as_integer().is_some_and(|n| u32::try_from(n).is_ok()))
        {
            kept.insert(
                CODEX_BOOKKEEPING[1].into(),
                tui.remove(CODEX_BOOKKEEPING[1])?,
            );
        }
        if tui.is_empty() {
            table.remove("tui");
        }
    }
    Some((table, kept))
}

/// Where Codex keeps its home, from the agent's environment `env`:
/// `$CODEX_HOME`, else `$HOME/.codex`. `None` when the value chosen is
/// empty or relative (the revb21 review's U1-1b), as
/// [`claude_keybindings_path`] holds for Claude Code: Codex doesn't read
/// such a value as the deck would (an empty one falls back to another
/// home, and a relative one resolves against Codex's own directory), so
/// the files the deck would read needn't be the ones Codex loads.
/// Comparing the spawn's value with the bound agent's can't catch that,
/// since both sides carry the same string.
///
/// `None` too when the value chosen isn't UTF-8 (the revc21 review's
/// U1-1c): Codex reads `CODEX_HOME` through `std::env::var`, which
/// refuses such a value, and falls back to `$HOME/.codex` (as
/// `codex_hooks_manage::codex_home` records), so the path as written
/// isn't the one Codex loads. A non-UTF-8 `HOME` holds the same way:
/// how Codex reads one isn't recorded, and holding costs nothing a real
/// home needs.
pub fn codex_home(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let home = match env("CODEX_HOME") {
        Some(codex_home) => PathBuf::from(codex_home.into_string().ok()?),
        None => PathBuf::from(env("HOME")?.into_string().ok()?).join(".codex"),
    };
    home.is_absolute().then_some(home)
}

/// The project files Codex may read for a launch in `cwd` (A-20d-1b, R6
/// §9.2's trusted project file), lowest precedence first: `.codex/config.toml`
/// in `cwd` and in each of its ancestors, the root's first, leaving out the
/// one whose `.codex` is `codex_home` (Codex reads that as its own config).
///
/// It is a superset, on purpose. Both pinned releases read a project's
/// `.codex/config.toml` in each directory from the project root down to the
/// cwd, the root found by markers its config may set (`.git` by default),
/// and ignore one in a directory whose project isn't trusted (`rust-v0.160.0`
/// and `rust-v0.164.0`, `codex-rs/config/src/loader/mod.rs`'s
/// `discover_project_layers`, :1639 and :1838; the CODEX_HOME skip at :1684
/// and :1883). The deck resolves neither the root nor the trust, so it reads
/// every directory those rules could pick, and [`codex_projects_guard`]
/// holds wherever which of them Codex loads could matter.
///
/// The `codex_home` comparison is by the path as written: one that differs
/// from Codex's own only through a symlink is kept, which can only hold
/// more. [`Hold::Unmeasured`] for a relative `cwd`: where Codex then looks
/// isn't known.
pub fn codex_project_files(cwd: &Path, codex_home: &Path) -> Result<Vec<PathBuf>, Hold> {
    if !cwd.is_absolute() {
        return Err(Hold::Unmeasured);
    }
    let mut files: Vec<PathBuf> = cwd
        .ancestors()
        .filter(|dir| dir.join(".codex") != codex_home)
        .map(|dir| dir.join(".codex").join("config.toml"))
        .collect();
    files.reverse();
    Ok(files)
}

/// Whether a Codex launch's arguments move its working directory (`-C`,
/// `--cd`, in their separate, `=` and attached forms; nothing after `--`):
/// its project files are then read from a directory the deck doesn't
/// follow.
pub fn codex_moves_cwd(args: &[OsString]) -> bool {
    for arg in args {
        let arg = arg.to_string_lossy();
        if arg == "--" {
            return false;
        }
        if arg == "--cd" || arg.starts_with("--cd=") || arg.starts_with("-C") {
            return true;
        }
    }
    false
}

/// The project-file half of a Codex read (A-20d-1b): a project file that
/// can't be read or parsed holds as such, and one whose table has a
/// `tui.keymap` holds as [`Hold::Unpinned`], since whether Codex loads it
/// depends on the project root and trust the deck doesn't resolve
/// ([`codex_project_files`]). With no keymap in any of them, the project
/// layer can't move a binding whichever of them Codex loads, so merging
/// them all ([`codex_keymap`]) gives the same effective `tui.keymap` as
/// Codex's own layers, though it includes files Codex doesn't load. Absent
/// files allow.
pub fn codex_projects_guard(read: &CodexRead) -> Result<(), Hold> {
    for project in &read.projects {
        match project {
            SourceRead::Absent => {}
            SourceRead::Unreadable(why) => return Err(Hold::Unreadable(*why)),
            SourceRead::Present { contents, .. } => {
                let text = std::str::from_utf8(contents).map_err(|_| Hold::Unparsable)?;
                let table: toml::Table = text.parse().map_err(|_| Hold::Unparsable)?;
                if table
                    .get("tui")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|tui| tui.contains_key("keymap"))
                {
                    return Err(Hold::Unpinned);
                }
            }
        }
    }
    Ok(())
}

/// A Codex launch's sources and their read, taken before its spawn
/// (A-20d-1b, R5 §7.1: "the deck reads them before the spawn").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexBaseline {
    pub sources: CodexSources,
    pub read: CodexRead,
}

impl CodexBaseline {
    /// The sources of a launch with `args` (after the program) in `cwd`,
    /// with `codex_home`, read now. File I/O: no lock held. A launch that
    /// moves its cwd holds as [`Hold::Unpinned`].
    pub fn at_spawn(
        codex_home: &Path,
        args: &[OsString],
        cwd: &Path,
    ) -> Result<CodexBaseline, Hold> {
        let sources = codex_launch(codex_home, args, cwd)?;
        let read = sources.read();
        Ok(CodexBaseline { sources, read })
    }

    /// The baseline against the launch the bound agent actually made, with
    /// `args` in `cwd`: the same sources, or [`Hold::Changed`] (a launcher
    /// that passed other flags, or started Codex in another directory).
    ///
    /// The bound launch may also be the baseline's with the deck's telemetry
    /// shim's own `otel` overrides in front ([`without_shim_otel`], the
    /// revcxkey review's M3): the baseline reads the deck's launch line,
    /// which never passes through the shim, so only this side can carry
    /// them, and they are left out only when what remains is the baseline's
    /// launch.
    pub fn bound(&self, codex_home: &Path, args: &[OsString], cwd: &Path) -> Result<(), Hold> {
        let args = without_embedded_pair(args);
        if codex_sources(codex_home, args, cwd)? == self.sources {
            return Ok(());
        }
        match without_shim_otel(args) {
            Some(rest) if codex_sources(codex_home, rest, cwd)? == self.sources => Ok(()),
            _ => Err(Hold::Changed),
        }
    }
}

/// A Codex generation's key-binding sources over its life (A-20d-1b,
/// cxkey): the sources the bound agent launched with, and the last read of
/// them that was accepted, first the one before the spawn. Each check reads
/// them again and compares with that last read ([`codex_unchanged`]). An
/// accepted read becomes the last, so Codex's own bookkeeping write is
/// accepted once, and a later rewrite that leaves the bookkeeping as it was
/// (a hand edit undone, which Codex may have reloaded meanwhile) still holds
/// as changed.
///
/// At most [`CODEX_REWRITES_PER_GENERATION`] bookkeeping rewrites are
/// accepted per generation, each a step forward from the last accepted read
/// ([`codex_bookkeeping_forward`]); a further one holds.
///
/// Which way a read lies from the last accepted one is decided by content
/// (the revcxkey2 review's LOW-1): Codex's bookkeeping only moves forward
/// ([`codex_bookkeeping_forward`] requires a move, so no two different reads
/// are each a step forward of the other). A read that is the same as the
/// last accepted one, or a step forward of it, is adopted; one that lags
/// behind it, a
/// read that was in flight when a later one was adopted (the revcxkey
/// review's M2: the daemon's two callers, admission and the observer's
/// re-read, read outside the lock and may take it in either order), is
/// accepted but never adopted, so the last accepted read can't wind back. A
/// read may lag only when it began before the last adoption
/// ([`CodexReads::start_read`]); one that began after it and still lags
/// found the file itself stepped back, and holds.
///
/// The first comparison that fails latches (the revcxkey review's H1): it is
/// returned by that check and every later one of the generation, whatever
/// they read, as [`ClaudeKeymap::check`] holds. A held read is never adopted.
/// Without the latch an edit Codex reloaded could be undone after Codex's own
/// bookkeeping write landed over it, and the undone file, whose bookkeeping
/// then differs from the last accepted read, would be accepted while Codex
/// still ran the edit's binding.
#[derive(Debug)]
pub struct CodexReads {
    sources: CodexSources,
    state: std::sync::Mutex<CodexReadsState>,
}

#[derive(Debug)]
struct CodexReadsState {
    /// The last read adopted, first the one before the spawn.
    last: CodexRead,
    /// The first comparison that failed, returned by every later check.
    held: Option<Hold>,
    /// How many bookkeeping rewrites this generation has accepted, which is
    /// how many times `last` has changed.
    rewrites: u8,
}

/// The most bookkeeping rewrites of `config.toml` one Codex generation
/// accepts (the revcxkey review's M1): the cxkey measurement saw at most
/// two per start, the screen-reader marker and one tooltip count on a
/// home's first start, one count on each of the next three. A further
/// rewrite holds as changed, which can only refuse more.
pub const CODEX_REWRITES_PER_GENERATION: u8 = 2;

impl CodexReads {
    /// A generation's reads from its spawn baseline.
    pub fn new(baseline: &CodexBaseline) -> CodexReads {
        CodexReads {
            sources: baseline.sources.clone(),
            state: std::sync::Mutex::new(CodexReadsState {
                last: baseline.read.clone(),
                held: None,
                rewrites: 0,
            }),
        }
    }

    /// Reads the sources now and checks them against the last accepted read
    /// (or the latched hold), the project files ([`codex_projects_guard`])
    /// and the effective keymap in `precedence` ([`codex_keymap`]):
    /// [`CodexReads::read`] then [`CodexReads::settle`]. The file I/O comes
    /// before its own lock, which guards only the comparison: call it with
    /// no lock held.
    pub fn check(&self, precedence: &[KeymapSource]) -> Result<(), Hold> {
        self.settle(self.read(), precedence)
    }

    /// Reads the sources now: [`CodexReads::start_read`] then
    /// [`CodexReads::finish_read`]. File I/O: no lock held.
    pub fn read(&self) -> CodexTicket {
        self.finish_read(self.start_read())
    }

    /// The half of [`CodexReads::read`] before the file I/O: notes how many
    /// rewrites had been adopted when the read began, so a read that lags
    /// behind the last accepted one can be told from a file that stepped
    /// back ([`CodexReads::settle`]). It takes the comparison's lock only to
    /// copy that count.
    pub fn start_read(&self) -> CodexReadStart {
        CodexReadStart {
            adopted: self.lock().rewrites,
        }
    }

    /// The half of [`CodexReads::read`] that reads the files. File I/O: no
    /// lock held.
    pub fn finish_read(&self, start: CodexReadStart) -> CodexTicket {
        CodexTicket {
            adopted: start.adopted,
            read: self.sources.read(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CodexReadsState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Checks `ticket`'s read (the half of [`CodexReads::check`] after the
    /// I/O). A read that is the same as the last accepted one, or a step
    /// forward of it ([`codex_unchanged`]), is accepted and adopted, a step counting
    /// against [`CODEX_REWRITES_PER_GENERATION`]. Else, a read that began
    /// before a later one was adopted is accepted without being adopted when
    /// the last accepted read is the same as it or a step forward of it in
    /// Codex's bookkeeping alone ([`codex_unchanged`] with the two swapped).
    /// Anything else, an edit included, holds and latches.
    pub fn settle(&self, ticket: CodexTicket, precedence: &[KeymapSource]) -> Result<(), Hold> {
        let CodexTicket { adopted, read: now } = ticket;
        {
            let mut state = self.lock();
            if let Some(hold) = state.held {
                return Err(hold);
            }
            let compared = match codex_unchanged(&state.last, &now) {
                Ok(()) if state.last == now => Ok(()),
                Ok(()) if state.rewrites < CODEX_REWRITES_PER_GENERATION => {
                    state.rewrites += 1;
                    state.last = now.clone();
                    Ok(())
                }
                Ok(()) => Err(Hold::Changed),
                Err(hold) => {
                    if adopted < state.rewrites && codex_unchanged(&now, &state.last).is_ok() {
                        Ok(())
                    } else {
                        Err(hold)
                    }
                }
            };
            if let Err(hold) = compared {
                state.held = Some(hold);
                return Err(hold);
            }
        }
        // A-20d-1b: the project files are a superset of what Codex loads; a
        // keymap in any of them holds.
        codex_projects_guard(&now)?;
        codex_keymap(&now, &now, precedence)
    }
}

/// One read of a Codex generation's sources ([`CodexReads::read`]), with how
/// many rewrites had been adopted when it began.
#[derive(Debug)]
pub struct CodexTicket {
    adopted: u8,
    read: CodexRead,
}

/// A read of a Codex generation's sources that has begun and not yet read
/// the files ([`CodexReads::start_read`]).
#[derive(Debug)]
pub struct CodexReadStart {
    adopted: u8,
}

/// [`CodexSources::from_launch`] with the project files for `cwd`, for
/// `args` less the deck's own embedded-mode pair ([`without_embedded_pair`]).
/// The spawn's baseline and the bound agent's launch both come through here,
/// so the pair is left out on both sides (A-20d-13).
fn codex_launch(codex_home: &Path, args: &[OsString], cwd: &Path) -> Result<CodexSources, Hold> {
    codex_sources(codex_home, without_embedded_pair(args), cwd)
}

/// [`CodexSources::from_launch`] with the project files for `cwd`, for
/// `args` as given.
fn codex_sources(codex_home: &Path, args: &[OsString], cwd: &Path) -> Result<CodexSources, Hold> {
    if codex_moves_cwd(args) {
        return Err(Hold::Unpinned);
    }
    CodexSources::from_launch(codex_home, args, codex_project_files(cwd, codex_home)?)
}

/// The keys the deck's telemetry shim sets on a Codex's command line, in
/// the order it writes them (`crate::telemetry::launch::codex_args`).
const SHIM_OTEL_KEYS: [&str; 2] = ["otel.trace_exporter", "otel.metrics_exporter"];

/// `args` (after the deck's embedded-mode pair) without the telemetry
/// shim's leading `otel` overrides, or `None` when they don't open with one
/// (the revcxkey review's M3). With an OTLP endpoint configured, the daemon
/// puts its shim first on the `PATH` of the agents it starts with a
/// telemetry run, when their base environment has a `PATH`
/// (`crate::telemetry::exporter`), and the shim runs a Codex called by name
/// with `-c otel.trace_exporter=…` and `-c otel.metrics_exporter=…` after
/// the pair and before the user's own arguments, only when
/// `crate::telemetry::launch::codex_args`'s conditions hold, either of them
/// left out when its signal has no endpoint or a protocol other than HTTP
/// (`crate::telemetry::shim`).
///
/// Only that position and shape is the shim's: `-c` (not another spelling)
/// and the key, in that order, each at most once, whose value is exactly
/// `{otlp-http={endpoint=<string>,protocol="binary"|"json"}}`; and the shim
/// adds nothing when the user's own arguments override `otel`
/// (`codex_overrides_otel`), so overrides followed by one are not left out.
/// An `otel` override sets no key binding, so leaving out one the user
/// wrote in the shim's shape at the front can't hide a rebinding.
fn without_shim_otel(args: &[OsString]) -> Option<&[OsString]> {
    let mut rest = args;
    for key in SHIM_OTEL_KEYS {
        if let [flag, value, after @ ..] = rest
            && flag == "-c"
            && value
                .to_str()
                .is_some_and(|value| shim_otel_value(key, value))
        {
            rest = after;
        }
    }
    (rest.len() < args.len() && !crate::telemetry::launch::codex_overrides_otel(rest))
        .then_some(rest)
}

/// Whether `arg` is `key=` followed by the exporter table the telemetry
/// shim writes: `{otlp-http={endpoint=<string>,protocol="binary"|"json"}}`
/// with no key or table after it. The value is parsed as the document
/// `v = <value>`, and that document must hold `v` alone (the revcxkey2
/// review's NIT-1): a value that goes on past the table, with a newline and
/// another key, is not the shim's. What TOML ignores after the table (a
/// comment, blanks, a newline) is accepted, which can carry no key binding
/// (the revcxkey3 review's NIT-A).
fn shim_otel_value(key: &str, arg: &str) -> bool {
    let Some(value) = arg
        .strip_prefix(key)
        .and_then(|rest| rest.strip_prefix('='))
    else {
        return false;
    };
    let Ok(mut table) = format!("v = {value}").parse::<toml::Table>() else {
        return false;
    };
    let Some(toml::Value::Table(exporter)) = table.remove("v") else {
        return false;
    };
    if !table.is_empty() {
        return false;
    }
    let Some(toml::Value::Table(http)) = exporter.get("otlp-http") else {
        return false;
    };
    exporter.len() == 1
        && http.len() == 2
        && http.get("endpoint").is_some_and(toml::Value::is_str)
        && http
            .get("protocol")
            .and_then(toml::Value::as_str)
            .is_some_and(|p| p == "binary" || p == "json")
}

/// `args` (after the program) without #2002's embedded-mode pair
/// (`crate::wrap::CODEX_EMBEDDED_ARGS`) when they open with it, else
/// unchanged. Below the deck's `wrap` a direct Codex's argv opens with the
/// pair while the launch line the baseline reads may not carry it (the
/// installation adds it only with the editor's variables), so keeping it
/// would hold every wrapped Codex as changed.
///
/// Only one leading copy of exactly those two words is stripped. The wrapper
/// (`wrap::launch_args`) and the installation (`Installation::command`) put
/// the pair first, and only when the arguments don't already open with it,
/// and the telemetry shim keeps it first, so a launch carries at most one
/// deck pair and it is at the front, where its `-c` can't be another
/// option's value. The same words anywhere else, a second leading copy,
/// another spelling (`--config`, `-c=`) or another value were written by the
/// user and stay sources. A user's own leading copy is indistinguishable
/// from the deck's and is stripped too: it sets no key binding.
fn without_embedded_pair(args: &[OsString]) -> &[OsString] {
    if crate::wrap::opens_with_codex_embedded_args(args) {
        &args[crate::wrap::CODEX_EMBEDDED_ARGS.len()..]
    } else {
        args
    }
}

/// A generation's key-binding sources read before its spawn (A-20d-1b): what
/// each later read is compared with, so a change made after the spawn and
/// before the deck first reads them is seen, rather than becoming the
/// baseline. One per agent the generation may turn out to be: a wrapper's
/// agent is learned only after its spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapBaseline {
    /// Claude Code's `keybindings.json` and its read.
    pub claude: Result<(PathBuf, SourceRead), Hold>,
    /// The files Claude Code copies an `env` map from, and their read (the
    /// revcxfix review's MEDIUM-A).
    pub claude_env: Result<ClaudeEnvBaseline, Hold>,
    pub codex: Result<CodexBaseline, Hold>,
}

impl KeymapBaseline {
    /// Read now, before the spawn, for the agents named: the spawn's
    /// environment `env` (its own variables over the daemon's), its
    /// arguments `args` (after the program) and its working directory
    /// `cwd`. An agent not named is [`Hold::Unmeasured`]. File I/O: no lock
    /// held.
    pub fn at_spawn(
        claude: bool,
        codex: bool,
        env: impl Fn(&str) -> Option<OsString>,
        args: &[OsString],
        cwd: &Path,
    ) -> KeymapBaseline {
        KeymapBaseline {
            claude: if claude {
                claude_keybindings_path(&env).map(|path| {
                    let read = read_source(&path);
                    (path, read)
                })
            } else {
                Err(Hold::Unmeasured)
            },
            claude_env: if claude {
                ClaudeEnvBaseline::at_spawn(&env, cwd)
            } else {
                Err(Hold::Unmeasured)
            },
            codex: if codex {
                codex_home(&env)
                    .ok_or(Hold::Unmeasured)
                    .and_then(|home| CodexBaseline::at_spawn(&home, args, cwd))
            } else {
                Err(Hold::Unmeasured)
            },
        }
    }
}

/// The variables that pick an agent's key-binding files (the reva21
/// review's U1-1): Claude Code's `HOME` and `CLAUDE_CONFIG_DIR`, Codex's
/// `HOME` and `CODEX_HOME`.
pub const KEYMAP_ENV: [&str; 3] = ["HOME", "CLAUDE_CONFIG_DIR", "CODEX_HOME"];

/// The variables an agent picks its external editor from
/// ([`editor_is_helper`]).
pub const EDITOR_ENV: [&str; 2] = ["VISUAL", "EDITOR"];

/// A lookup over a process's own environment `environ`, as
/// `/proc/<pid>/environ` lists it, for the variables in [`KEYMAP_ENV`] and
/// [`EDITOR_ENV`]: the first value a name has, as `getenv` answers. A name
/// in either listed twice with different values holds as
/// [`Hold::Unmeasured`]: which one the agent's runtime reads isn't
/// measured.
pub fn environ_lookup(
    environ: &[(OsString, OsString)],
) -> Result<impl Fn(&str) -> Option<OsString> + '_, Hold> {
    for name in KEYMAP_ENV.into_iter().chain(EDITOR_ENV) {
        let mut values = environ.iter().filter(|(k, _)| k == name).map(|(_, v)| v);
        if let Some(first) = values.next()
            && values.any(|v| v != first)
        {
            return Err(Hold::Unmeasured);
        }
    }
    Ok(move |name: &str| {
        environ
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    })
}

/// Whether the editor a Codex's Ctrl+G opens, selected from its own
/// environment `env` ([`environ_lookup`]) as Codex selects it, is exactly
/// `helper` (the revcxact review's HIGH-1). The deck puts its helper in
/// `VISUAL` and `EDITOR` only in the spawn's environment, and a shell's
/// start-up file (zsh's `.zshenv` and fish's `config.fish` run for a `-c`
/// command too, which the deck's Codex line is) or a launcher
/// (`VISUAL=nvim codex`, `env VISUAL=… codex`, a wrapper script) can give
/// Codex another. A probe would then open that editor in the person's pane,
/// and a Codex check's held input is released only by the helper's return,
/// so the pane would stay held until the agent is replaced.
///
/// Codex's selection (`EditorSelection::Codex`, M34, the 0.164.0
/// evidence's `editor-probe/README.md`): a set `VISUAL`, else a set
/// `EDITOR`; a set but empty one is an error, not a fall-through, and with
/// neither set Codex opens nothing. The value is split in shell words as
/// Codex splits it ([`super::helper::split_shell_words`]), and the words
/// must be exactly one, byte for byte the helper's path. Anything else,
/// an error or an unsplittable value included, holds as [`Hold::Editor`].
///
/// It reads the environment the agent was started with: a value the agent
/// sets in its own process afterwards isn't seen.
#[cfg(target_os = "linux")]
pub fn codex_editor_is_helper(
    env: impl Fn(&str) -> Option<OsString>,
    helper: &Path,
) -> Result<(), Hold> {
    let value = env("VISUAL")
        .or_else(|| env("EDITOR"))
        .ok_or(Hold::Editor)?;
    if value.is_empty() {
        return Err(Hold::Editor);
    }
    match super::helper::split_shell_words(&value).as_deref() {
        Some([only]) if only.as_os_str() == helper.as_os_str() => Ok(()),
        _ => Err(Hold::Editor),
    }
}

/// Whether `$CODEX_HOME/.env`, as read, leaves Codex's editor to the
/// environment it was started with (the revcxfix review's MEDIUM-B). Codex
/// 0.164.0 loads that file as it starts and sets its variables over the
/// ones it inherited: run in a sandbox HOME with `VISUAL=<a recorder>` in
/// it, Ctrl+G ran the recorder, also over an inherited `VISUAL` and in the
/// `export` and quoted forms, and with `EDITOR=` alone; with no `.env` it
/// drew its "Cannot open external editor" line. So an absent file passes,
/// one that can't be read holds, and one that names the editor's variables
/// holds as [`Hold::Editor`]. The test is a raw scan rather than a parse,
/// so that every quoting, `export` and spacing form holds: any word of
/// letters, digits and `_` that is `VISUAL` or `EDITOR` in any case, on any
/// line, a comment's included, holds.
pub fn codex_dotenv_guard(read: &SourceRead) -> Result<(), Hold> {
    match read {
        SourceRead::Absent => Ok(()),
        SourceRead::Unreadable(_) => Err(Hold::Editor),
        SourceRead::Present { contents, .. } => {
            let names_editor = contents
                .split(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                .any(|word| {
                    EDITOR_ENV
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name.as_bytes()))
                });
            if names_editor {
                Err(Hold::Editor)
            } else {
                Ok(())
            }
        }
    }
}

/// Whether a Codex whose home is `home` (`$CODEX_HOME`, else `~/.codex`, as
/// the bound native's own environment names it: [`codex_home`]), started in
/// `cwd` with `tmpdir` its `TMPDIR`, can be checked at all (the user's
/// ruling Q227b on the revcxact review's STOP-1 follow-up). Codex 0.164.0
/// hands its editor a file in the first of `$CODEX_HOME`, `~/.codex` and
/// `<cwd>/.codex` its own sandbox can't write, and the deck reads only
/// `$CODEX_HOME/editor`. Under `workspace-write` the cwd, `/tmp` and
/// `$TMPDIR` are writable, a project's own top-level `.codex` is kept
/// read-only, so a home under any of them (other than `<cwd>/.codex`
/// itself) sends the file elsewhere: the helper is refused the file's
/// shape, exits 1, and Codex draws `Failed to open editor` at each check.
/// This holds such a generation before any check, so no key is typed and
/// no helper runs.
///
/// Paths are compared resolved (`canonicalize`, symlinks followed): a home
/// reached through a symlink is judged where it lands. Any of them that
/// can't be resolved, `TMPDIR` set but empty or relative included, holds.
/// It accepts no new path: a home outside all three is still read only as
/// `$CODEX_HOME/editor` by the export rule. What it doesn't see: Codex's
/// `writable_roots`, `--add-dir` and sandbox mode (a home it holds can be
/// one Codex's sandbox wouldn't write, under `read-only` or
/// `danger-full-access`), which can only hold more or leave the case as it
/// was.
pub fn codex_home_checkable(
    home: &Path,
    cwd: &Path,
    tmpdir: Option<&std::ffi::OsStr>,
) -> Result<(), Hold> {
    let resolve = |path: &Path| std::fs::canonicalize(path).map_err(|_| Hold::CodexHome);
    let home = resolve(home)?;
    let cwd = resolve(cwd)?;
    if home.starts_with(&cwd) && home != cwd.join(".codex") {
        return Err(Hold::CodexHome);
    }
    let mut temporary = vec![resolve(Path::new("/tmp"))?];
    if let Some(tmpdir) = tmpdir {
        let tmpdir = Path::new(tmpdir);
        if !tmpdir.is_absolute() {
            return Err(Hold::CodexHome);
        }
        temporary.push(resolve(tmpdir)?);
    }
    if temporary.iter().any(|dir| home.starts_with(dir)) {
        return Err(Hold::CodexHome);
    }
    Ok(())
}

/// [`codex_editor_is_helper`] for Claude Code, with its selection
/// (`EditorSelection::ClaudeCode`, M22, M23): a non-empty `VISUAL`, else a
/// non-empty `EDITOR`, else the first of `code`, `vi` and `nano` on `PATH`,
/// which is never the helper; the value split on literal spaces with empty
/// words dropped. Exactly one word, byte for byte the helper's path, or
/// [`Hold::Editor`].
///
/// A Claude Code pane whose command has arguments runs under `$SHELL -c`
/// as a Codex line does, so a shell start-up file reaches it the same way.
/// Where the person's typing is held behind an unmatched Claude probe, the
/// lapse ([`super::query_ledger::T_TRIGGER`]) releases nothing (it only
/// lets a retry replace a probe that holds nothing), so that pane is held
/// until the agent restarts too. Claude Code also copies the `env` maps of
/// its settings files into its own environment when it starts, which this
/// environment read doesn't see: [`claude_env_guard`] holds those.
#[cfg(target_os = "linux")]
pub fn claude_editor_is_helper(
    env: impl Fn(&str) -> Option<OsString>,
    helper: &Path,
) -> Result<(), Hold> {
    let value = env("VISUAL")
        .filter(|v| !v.is_empty())
        .or_else(|| env("EDITOR").filter(|v| !v.is_empty()))
        .ok_or(Hold::Editor)?;
    match super::helper::split_spaces(&value).as_slice() {
        [only] if only.as_os_str() == helper.as_os_str() => Ok(()),
        _ => Err(Hold::Editor),
    }
}

/// The editor check for an agent whose release's record selects its
/// editor by `selection`: [`claude_editor_is_helper`] or
/// [`codex_editor_is_helper`].
#[cfg(target_os = "linux")]
pub fn editor_is_helper(
    selection: crate::doorbell_activation::EditorSelection,
    env: impl Fn(&str) -> Option<OsString>,
    helper: &Path,
) -> Result<(), Hold> {
    match selection {
        crate::doorbell_activation::EditorSelection::ClaudeCode => {
            claude_editor_is_helper(env, helper)
        }
        crate::doorbell_activation::EditorSelection::Codex => codex_editor_is_helper(env, helper),
    }
}

impl KeymapBaseline {
    /// Claude Code's spawn read, when the bound agent's own environment `env`
    /// still names the file it was read from; [`Hold::Changed`] when it
    /// names another, and the path's own hold when it names none.
    pub fn claude_bound(
        &self,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<(&Path, &SourceRead), Hold> {
        let (path, read) = self.claude.as_ref().map_err(|hold| *hold)?;
        match claude_keybindings_path(env) {
            Ok(now) if now == *path => Ok((path, read)),
            Ok(_) => Err(Hold::Changed),
            Err(hold) => Err(hold),
        }
    }

    /// Claude Code's `env` sources, judged for the bound agent
    /// ([`ClaudeEnvBaseline::bound`]).
    pub fn claude_env_bound(
        &self,
        env: impl Fn(&str) -> Option<OsString>,
        args: Option<&[OsString]>,
        cwd: Option<&Path>,
    ) -> Result<&ClaudeEnvBaseline, Hold> {
        let baseline = self.claude_env.as_ref().map_err(|hold| *hold)?;
        baseline.bound(env, args, cwd).map(|()| baseline)
    }

    /// Codex's spawn baseline, when the bound agent launched with the same
    /// sources ([`CodexBaseline::bound`]): its arguments `args` after the
    /// program, its working directory `cwd` and the home its own
    /// environment `env` names ([`environ_lookup`]). No
    /// home, no `cwd`, or arguments that couldn't be read (`None`: an
    /// unreadable or over-long command line, which may carry a `-c`
    /// override the deck never saw) hold as unmeasured.
    pub fn codex_bound(
        &self,
        env: impl Fn(&str) -> Option<OsString>,
        args: Option<&[OsString]>,
        cwd: Option<&Path>,
    ) -> Result<&CodexBaseline, Hold> {
        let codex = self.codex.as_ref().map_err(|hold| *hold)?;
        let (Some(home), Some(args), Some(cwd)) = (codex_home(env), args, cwd) else {
            return Err(Hold::Unmeasured);
        };
        codex.bound(&home, args, cwd).map(|()| codex)
    }
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

/// The `tui.keymap` contexts and their actions that both pinned Codex
/// releases accept: `TuiKeymap` and its context structs in
/// `codex-rs/config/src/tui_keymap.rs` at `rust-v0.160.0` (`a956835d`) and
/// `rust-v0.164.0` (`6aaac694`), quoted with their line numbers from the
/// same pinned checkouts as the tester's source bundles, in
/// `.dot-agent-deck/sources/codex-keymap.md`. Each context cites the
/// `TuiKeymap` field and the struct, each action its field, as
/// `<0.160 line>, <0.164 line>`. A name only one release has (0.164's
/// `global.leader`, `chat.open_presets`, `agents.fork` and
/// `agents.toggle_pin`) isn't listed, so it holds.
pub const CODEX_KEYMAP: &[(&str, &[&str])] = &[
    // `global`: field of `TuiKeymap` at 504, 543; `TuiGlobalKeymap` at 94, 135
    (
        "global",
        &[
            "open_agents",              // 96, 139
            "open_transcript",          // 98, 141
            "find_transcript",          // 100, 143
            "focus_activity",           // 102, 145
            "open_external_editor",     // 104, 147
            "copy",                     // 106, 149
            "clear_terminal",           // 108, 151
            "submit",                   // 110, 153
            "queue",                    // 112, 155
            "toggle_shortcuts",         // 114, 157
            "toggle_vim_mode",          // 116, 159
            "toggle_fast_mode",         // 118, 161
            "toggle_raw_output",        // 120, 163
            "toggle_side_conversation", // 122, 165
        ],
    ),
    // `chat`: field of `TuiKeymap` at 506, 545; `TuiChatKeymap` at 129, 171
    (
        "chat",
        &[
            "toggle_voice",              // 131, 175
            "toggle_voice_mute",         // 133, 177
            "interrupt_turn",            // 135, 179
            "decrease_reasoning_effort", // 137, 181
            "increase_reasoning_effort", // 139, 183
            "previous_permission_mode",  // 141, 185
            "next_permission_mode",      // 143, 187
            "edit_queued_message",       // 145, 189
            "prompt_stack_back",         // 147, 191
            "skip_question",             // 149, 193
        ],
    ),
    // `composer`: field of `TuiKeymap` at 508, 547; `TuiComposerKeymap` at 156, 199
    (
        "composer",
        &[
            "submit",                  // 158, 201
            "queue",                   // 160, 203
            "toggle_shortcuts",        // 162, 205
            "history_search_previous", // 164, 207
            "history_search_next",     // 166, 209
        ],
    ),
    // `editor`: field of `TuiKeymap` at 510, 549; `TuiEditorKeymap` at 173, 215
    (
        "editor",
        &[
            "insert_newline",       // 175, 217
            "move_left",            // 177, 219
            "move_right",           // 179, 221
            "move_up",              // 181, 223
            "move_down",            // 183, 225
            "move_word_left",       // 185, 227
            "move_word_right",      // 187, 229
            "move_line_start",      // 189, 231
            "move_line_end",        // 191, 233
            "delete_backward",      // 193, 235
            "delete_forward",       // 195, 237
            "delete_backward_word", // 197, 239
            "delete_forward_word",  // 199, 241
            "kill_line_start",      // 201, 243
            "kill_whole_line",      // 203, 245
            "kill_line_end",        // 205, 247
            "yank",                 // 207, 249
        ],
    ),
    // `vim_normal`: field of `TuiKeymap` at 512, 551; `TuiVimNormalKeymap` at 217, 259
    (
        "vim_normal",
        &[
            "enter_insert",          // 219, 261
            "append_after_cursor",   // 221, 263
            "append_line_end",       // 223, 265
            "insert_line_start",     // 225, 267
            "open_line_below",       // 227, 269
            "open_line_above",       // 229, 271
            "enter_replace_mode",    // 231, 273
            "move_left",             // 233, 275
            "move_right",            // 235, 277
            "move_up",               // 237, 279
            "move_down",             // 239, 281
            "move_word_forward",     // 241, 283
            "move_word_backward",    // 243, 285
            "move_word_end",         // 245, 287
            "move_line_start",       // 247, 289
            "move_line_end",         // 249, 291
            "find_forward",          // 251, 293
            "find_backward",         // 253, 295
            "till_forward",          // 255, 297
            "till_backward",         // 257, 299
            "jump_top",              // 259, 301
            "jump_bottom",           // 261, 303
            "delete_char",           // 263, 305
            "replace_char",          // 265, 307
            "repeat_last_change",    // 267, 309
            "substitute_char",       // 269, 311
            "delete_to_line_end",    // 271, 313
            "change_to_line_end",    // 273, 315
            "yank_line",             // 275, 317
            "paste_after",           // 277, 319
            "start_delete_operator", // 279, 321
            "start_yank_operator",   // 281, 323
            "start_change_operator", // 283, 325
            "undo",                  // 285, 327
            "redo",                  // 287, 329
            "cancel_operator",       // 289, 331
        ],
    ),
    // `vim_operator`: field of `TuiKeymap` at 514, 553; `TuiVimOperatorKeymap` at 300, 342
    (
        "vim_operator",
        &[
            "delete_line",               // 302, 344
            "yank_line",                 // 304, 346
            "motion_left",               // 306, 348
            "motion_right",              // 308, 350
            "motion_up",                 // 310, 352
            "motion_down",               // 312, 354
            "motion_word_forward",       // 314, 356
            "motion_word_backward",      // 316, 358
            "motion_word_end",           // 318, 360
            "motion_line_start",         // 320, 362
            "motion_line_end",           // 322, 364
            "motion_find_forward",       // 324, 366
            "motion_find_backward",      // 326, 368
            "motion_till_forward",       // 328, 370
            "motion_till_backward",      // 330, 372
            "motion_jump_top",           // 332, 374
            "motion_jump_bottom",        // 334, 376
            "select_inner_text_object",  // 336, 378
            "select_around_text_object", // 338, 380
            "cancel",                    // 340, 382
        ],
    ),
    // `vim_search`: field of `TuiKeymap` at 516, 555; `TuiVimSearchKeymap` at 347, 388
    (
        "vim_search",
        &[
            "forward",  // 349, 390
            "backward", // 351, 392
            "next",     // 353, 394
            "previous", // 355, 396
        ],
    ),
    // `vim_text_object`: field of `TuiKeymap` at 518, 557; `TuiVimTextObjectKeymap` at 362, 402
    (
        "vim_text_object",
        &[
            "word",         // 364, 404
            "big_word",     // 366, 406
            "parentheses",  // 368, 408
            "brackets",     // 370, 410
            "braces",       // 372, 412
            "double_quote", // 374, 414
            "single_quote", // 376, 416
            "backtick",     // 378, 418
            "cancel",       // 380, 420
        ],
    ),
    // `pager`: field of `TuiKeymap` at 520, 559; `TuiPagerKeymap` at 387, 426
    (
        "pager",
        &[
            "scroll_up",        // 389, 428
            "scroll_down",      // 391, 430
            "page_up",          // 393, 432
            "page_down",        // 395, 434
            "half_page_up",     // 397, 436
            "half_page_down",   // 399, 438
            "jump_top",         // 401, 440
            "jump_bottom",      // 403, 442
            "close",            // 405, 444
            "close_transcript", // 407, 446
            "find",             // 409, 448
        ],
    ),
    // `list`: field of `TuiKeymap` at 522, 561; `TuiListKeymap` at 416, 454
    (
        "list",
        &[
            "move_up",     // 418, 456
            "move_down",   // 420, 458
            "move_left",   // 422, 460
            "move_right",  // 424, 462
            "page_up",     // 426, 464
            "page_down",   // 428, 466
            "jump_top",    // 430, 468
            "jump_bottom", // 432, 470
            "accept",      // 434, 472
            "cancel",      // 436, 474
        ],
    ),
    // `agents`: field of `TuiKeymap` at 524, 563; `TuiAgentsKeymap` at 443, 480
    (
        "agents",
        &[
            "resume",          // 445, 482
            "search",          // 447, 484
            "new_task",        // 449, 486
            "new_worktree",    // 451, 488
            "rename",          // 453, 492
            "stop",            // 455, 494
            "archive",         // 457, 496
            "delete",          // 459, 498
            "hide",            // 461, 500
            "toggle_grouping", // 463, 504
        ],
    ),
    // `approval`: field of `TuiKeymap` at 526, 565; `TuiApprovalKeymap` at 470, 510
    (
        "approval",
        &[
            "open_fullscreen",     // 472, 512
            "open_thread",         // 474, 514
            "approve",             // 476, 516
            "approve_for_session", // 478, 518
            "approve_for_prefix",  // 480, 520
            "deny",                // 482, 522
            "decline",             // 484, 524
            "cancel",              // 486, 526
        ],
    ),
];

/// One Codex key spec in canonical form, one string per stroke, or `None`
/// where the grammar both pinned releases accept refuses it: 0.160's
/// `normalize_keybinding_spec`, `normalize_keybinding_stroke` and
/// `normalize_key_name` (`tui_keymap.rs:539`, `:559`, `:640`; 0.164 at
/// `:578`, `:609`, `:700` accepts all of it and adds `super`/`cmd`/`command`
/// and `leader`, which this refuses). At most two strokes split at
/// whitespace; a stroke is lowercased and split at `-`, empty segments
/// dropped; `ctrl`/`control`, `alt`/`option` and `shift`, each at most once,
/// come before the key; the key is a printable ASCII character other than
/// `-`, a named key (aliases `escape`, `return`, `spacebar`,
/// `pgup`/`pageup`, `pgdn`/`pagedown`, `del`), or `f1`–`f24`. The canonical
/// stroke orders its modifiers `ctrl-alt-shift-`. Codex matches a key event
/// to a binding by `(key, modifiers)` equality after `normalize_key_parts`
/// (`codex-rs/tui/src/key_hint.rs:125,148` at 0.160, `:132,166` at 0.164),
/// so the deck's Ctrl+G (`0x07`, read as `g` with Control) is the stroke
/// `ctrl-g` and no other.
pub fn codex_key(raw: &str) -> Option<Vec<String>> {
    let strokes: Vec<&str> = raw.split_whitespace().collect();
    if strokes.is_empty() || strokes.len() > 2 {
        return None;
    }
    strokes.into_iter().map(codex_stroke).collect()
}

fn codex_stroke(raw: &str) -> Option<String> {
    const MODIFIERS: [&str; 3] = ["ctrl", "alt", "shift"];
    let lower = raw.trim().to_ascii_lowercase();
    let mut held = [false; 3];
    let mut key = Vec::new();
    for segment in lower.split('-').filter(|s| !s.is_empty()) {
        let modifier = match segment {
            "ctrl" | "control" => Some(0),
            "alt" | "option" => Some(1),
            "shift" => Some(2),
            _ => None,
        };
        match modifier {
            Some(_) if !key.is_empty() => return None,
            Some(i) if held[i] => return None,
            Some(i) => held[i] = true,
            None => key.push(segment),
        }
    }
    let key = key.join("-");
    let key = match key.as_str() {
        "escape" => "esc",
        "return" => "enter",
        "spacebar" => "space",
        "pgup" | "pageup" => "page-up",
        "pgdn" | "pagedown" => "page-down",
        "del" => "delete",
        "enter" | "tab" | "backspace" | "esc" | "delete" | "up" | "down" | "left" | "right"
        | "home" | "end" | "page-up" | "page-down" | "space" | "minus" => key.as_str(),
        one if one.len() == 1 && one.as_bytes()[0].is_ascii_graphic() && one != "-" => one,
        f if f
            .strip_prefix('f')
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| (1..=24).contains(&n)) =>
        {
            f
        }
        _ => return None,
    };
    let mut stroke: Vec<&str> = MODIFIERS
        .iter()
        .zip(held)
        .filter_map(|(name, on)| on.then_some(*name))
        .collect();
    stroke.push(key);
    Some(stroke.join("-"))
}

/// The effective `tui.keymap`: contexts, each a table of action to a key or
/// a list of keys, every name in [`CODEX_KEYMAP`] and every key in
/// [`codex_key`]'s grammar, else [`Hold::UnknownKey`]. A key with Ctrl+G in
/// any stroke must be the single stroke `ctrl-g` on [`CODEX_EDITOR_ACTION`];
/// where that action is bound, `ctrl-g` must be one of its keys (another key
/// beside it moves nothing off Ctrl+G).
fn codex_bindings(merged: &toml::Table) -> Result<(), Hold> {
    let Some(tui) = merged.get("tui") else {
        return Ok(());
    };
    let tui = tui.as_table().ok_or(Hold::UnknownKey)?;
    let Some(keymap) = tui.get("keymap") else {
        return Ok(());
    };
    let keymap = keymap.as_table().ok_or(Hold::UnknownKey)?;
    for (name, context) in keymap {
        let known = CODEX_KEYMAP
            .iter()
            .find_map(|(known, actions)| (known == name).then_some(*actions))
            .ok_or(Hold::UnknownKey)?;
        let context = context.as_table().ok_or(Hold::UnknownKey)?;
        for (action, keys) in context {
            if !known.contains(&action.as_str()) {
                return Err(Hold::UnknownKey);
            }
            let keys: Vec<&str> = match keys {
                toml::Value::String(key) => vec![key.as_str()],
                toml::Value::Array(keys) => keys
                    .iter()
                    .map(|k| k.as_str().ok_or(Hold::UnknownKey))
                    .collect::<Result<_, _>>()?,
                _ => return Err(Hold::UnknownKey),
            };
            let keys: Vec<Vec<String>> = keys
                .into_iter()
                .map(|k| codex_key(k).ok_or(Hold::UnknownKey))
                .collect::<Result<_, _>>()?;
            let editor = action == CODEX_EDITOR_ACTION;
            let alone = |k: &Vec<String>| k.len() == 1 && k[0] == "ctrl-g";
            let anywhere = |k: &Vec<String>| k.iter().any(|stroke| stroke == "ctrl-g");
            if keys.iter().any(|k| anywhere(k) && !(editor && alone(k)))
                || (editor && !keys.iter().any(alone))
            {
                return Err(Hold::Rebound);
            }
        }
    }
    Ok(())
}
