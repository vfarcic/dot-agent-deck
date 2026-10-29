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
//! deck records it at spawn and reads it again before each admission and at
//! each settlement ([`ClaudeKeymap::check`]); any difference from the spawn read holds the
//! rest of the generation ([`Hold::Changed`], CL-9t), even if a later read
//! matches again. With an unchanged file, P-KB1 (the release loads its
//! bindings before its first ready frame, M49b) is what makes the spawn
//! read the binding Claude loaded, so a release whose record doesn't verify
//! it holds ([`Hold::Unmeasured`]); every record leaves it absent until M49b
//! is measured. The spawn read's contents hold too when they bind Ctrl+G or
//! the external-editor action, or can't be read or parsed (CL-9). Where
//! Claude looks is measured only for `$HOME/.claude` (M39), so a
//! `CLAUDE_CONFIG_DIR` in the agent's environment holds
//! ([`claude_keybindings_path`]).
//!
//! **Codex** ([`CodexSources`], R6 §9.2, R5 §7.1) reads `$CODEX_HOME/config.toml`,
//! the selected profile's `$CODEX_HOME/<name>.config.toml`, the trusted
//! project files for the pane's cwd, and each `-c key=value`. The deck reads
//! them before the spawn and again after the first ready frame; any
//! difference holds the generation. Otherwise the sources are merged in the
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
    /// A source can't be parsed.
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

/// Whether a `keybindings.json` leaves Ctrl+G on the external editor: no
/// string in the document, an object's member name or a value at any depth,
/// may name [`CLAUDE_EDITOR_ACTION`] or spell Ctrl+G, either in one of its
/// strokes ([`claude_strokes`]) or anywhere in it once whitespace is removed
/// ([`claude_mentions`]). The format's layout isn't measured beyond M39
/// (a `Chat` context that unbinds Ctrl+G and binds the action elsewhere), so
/// the check reads every string the same way rather than trusting one
/// layout, and an explicit default binding holds too.
fn claude_bindings(contents: &[u8]) -> Result<(), Hold> {
    fn holds(s: &str) -> bool {
        claude_mentions(s) || claude_strokes(s).iter().any(|k| is_ctrl_g(k))
    }
    fn walk(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::String(s) => holds(s),
            serde_json::Value::Array(items) => items.iter().any(walk),
            serde_json::Value::Object(map) => {
                map.iter().any(|(key, value)| holds(key) || walk(value))
            }
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
