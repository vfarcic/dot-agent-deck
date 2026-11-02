//! PRD #1666 part 3 (Q35-18; R6 §9, R5 §7.1): the files an agent reads its
//! key bindings from (`src/composer_probe/keymap.rs`), over temporary
//! directories only: Claude Code's `keybindings.json` read at spawn and again
//! before an admission and at a settlement, with its creation, change and
//! removal in between; and Codex's pinned sources, their precedence and the
//! read before the spawn against the one after the first ready frame.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use dot_agent_deck::composer_probe::keymap::{
    ClaudeKeymap, CodexSources, Hold, SOURCE_BYTES, SourceRead, Unreadable,
    claude_keybindings_path, codex_key, codex_keymap, is_ctrl_g, read_source,
};
use dot_agent_deck::doorbell_activation::{
    EditorProbe, EditorSelection, ExportShape, KeymapSource, Measurement, PasteToggles,
};
use spec::spec;

#[path = "../src/test_temp.rs"]
mod test_temp;

/// Past the coarse clock's tick, so a rewrite's timestamps move.
const TICK: Duration = Duration::from_millis(30);

const CODEX_ORDER: &[KeymapSource] = &[
    KeymapSource::CodexConfig,
    KeymapSource::CodexProfile,
    KeymapSource::CodexProject,
    KeymapSource::CodexOverride,
];

fn probe(loaded: Measurement<()>) -> EditorProbe<'static> {
    EditorProbe {
        export: ExportShape::ClaudeTmpdir,
        selection: EditorSelection::ClaudeCode,
        paste_toggles: PasteToggles {
            disables: 1,
            enables: 1,
        },
        keymap_sources: &[KeymapSource::ClaudeKeybindings],
        keymap_loaded_at_start: loaded,
        inert_candidates: &[],
        pasted_bel_is_text: Measurement::Absent,
    }
}

fn verified() -> EditorProbe<'static> {
    probe(Measurement::Verified(()))
}

/// A binding that leaves Ctrl+G alone.
const UNRELATED: &str = r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+e":"chat:submit"}}]}"#;

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

fn keybindings(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("keybindings.json")
}

/// Scenario: A `keybindings.json` with a binding that leaves Ctrl+G alone is read at spawn for a release whose record verifies P-KB1, then read again before an admission and at a settlement with nothing changed: both allow the probe. The control writes different bindings between the admission and the settlement: the settlement holds with "changed".
#[spec("inbox/probe/014")]
#[test]
fn probe_014_an_unchanged_file_allows_at_admission_and_settlement() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    write(&path, UNRELATED);
    let keymap = ClaudeKeymap::at_spawn(path.clone(), &verified());
    assert!(matches!(keymap.spawn_read(), SourceRead::Present { .. }));
    assert_eq!(keymap.check(), Ok(()), "admission");
    assert_eq!(keymap.check(), Ok(()), "settlement");

    let control = ClaudeKeymap::at_spawn(path.clone(), &verified());
    assert_eq!(control.check(), Ok(()), "admission");
    write(&path, r#"{"bindings":[]}"#);
    assert_eq!(control.check(), Err(Hold::Changed), "settlement");
}

/// Scenario: The file read at spawn is changed before the admission: the admission holds with "changed", and after the original bytes are written back the settlement and a later admission still hold, since a change holds the rest of the generation. The control is a new generation spawned on the restored file, which allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_change_since_spawn_holds_the_rest_of_the_generation() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    write(&path, UNRELATED);
    let keymap = ClaudeKeymap::at_spawn(path.clone(), &verified());
    write(&path, r#"{"bindings":[{"context":"Chat","bindings":{}}]}"#);
    assert_eq!(keymap.check(), Err(Hold::Changed), "admission");
    std::thread::sleep(TICK);
    write(&path, UNRELATED);
    assert_eq!(keymap.check(), Err(Hold::Changed), "settlement");
    assert_eq!(keymap.check(), Err(Hold::Changed), "a later admission");

    let next = ClaudeKeymap::at_spawn(path, &verified());
    assert_eq!(next.check(), Ok(()), "a new generation reads anew");
}

/// Scenario: With no `keybindings.json` at spawn the admission allows the probe, then the file is created before the settlement, which holds with "changed"; with one at spawn that is removed before the admission, the admission holds. The controls keep the file absent, and present, throughout: both allow the probe at the admission and the settlement.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_creation_and_removal_between_reads_hold() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    let created = ClaudeKeymap::at_spawn(path.clone(), &verified());
    assert_eq!(created.spawn_read(), &SourceRead::Absent);
    assert_eq!(created.check(), Ok(()), "admission, still absent");
    write(&path, UNRELATED);
    assert_eq!(
        created.check(),
        Err(Hold::Changed),
        "created before the settlement"
    );

    let removed = ClaudeKeymap::at_spawn(path.clone(), &verified());
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        removed.check(),
        Err(Hold::Changed),
        "removed before the admission"
    );

    let absent = ClaudeKeymap::at_spawn(path.clone(), &verified());
    assert_eq!(absent.check(), Ok(()));
    assert_eq!(absent.check(), Ok(()));
    write(&path, UNRELATED);
    let present = ClaudeKeymap::at_spawn(path, &verified());
    assert_eq!(present.check(), Ok(()));
    assert_eq!(present.check(), Ok(()));
}

/// Scenario: The file is rewritten with the very bytes read at spawn, after the clock moved: its contents hash the same but its times don't, so the admission holds with "changed". The file is also replaced by a new file with the same bytes under the same name, which holds too. The control reads the untouched file again and allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_the_same_bytes_written_again_still_hold() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    write(&path, UNRELATED);
    let rewritten = ClaudeKeymap::at_spawn(path.clone(), &verified());
    std::thread::sleep(TICK);
    write(&path, UNRELATED);
    assert_eq!(rewritten.check(), Err(Hold::Changed));

    let replaced = ClaudeKeymap::at_spawn(path.clone(), &verified());
    let other = dir.path().join("other.json");
    write(&other, UNRELATED);
    std::fs::rename(&other, &path).unwrap();
    assert_eq!(replaced.check(), Err(Hold::Changed), "a new inode");

    let fresh = ClaudeKeymap::at_spawn(path, &verified());
    assert_eq!(fresh.check(), Ok(()));
}

/// Scenario: A release whose record leaves P-KB1 absent, as every record does until M49b is measured, holds with "unmeasured" on an unchanged file and on no file at all. The control is the same file with P-KB1 verified, which allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_an_unmeasured_p_kb1_holds() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    let none = ClaudeKeymap::at_spawn(path.clone(), &probe(Measurement::Absent));
    assert_eq!(none.check(), Err(Hold::Unmeasured));
    write(&path, UNRELATED);
    let unmeasured = ClaudeKeymap::at_spawn(path.clone(), &probe(Measurement::Absent));
    assert_eq!(unmeasured.check(), Err(Hold::Unmeasured));
    let control = ClaudeKeymap::at_spawn(path, &verified());
    assert_eq!(control.check(), Ok(()));
}

/// Scenario: Unchanged `keybindings.json` files that unbind Ctrl+G, bind the external-editor action to another key, put Ctrl+G inside a chord, or bind it explicitly each hold with "rebound"; a file that isn't JSON holds as unparsable, a directory, a FIFO and a file over the size limit as unreadable. The control binds other keys only and allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_claude_rebinding_or_an_unreadable_file_holds() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    for (contents, want) in [
        (
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+g":null}}]}"#,
            Err(Hold::Rebound),
        ),
        (
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+e":"chat:externalEditor"}}]}"#,
            Err(Hold::Rebound),
        ),
        (
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+x ctrl+g":"chat:submit"}}]}"#,
            Err(Hold::Rebound),
        ),
        (
            r#"{"bindings":[{"context":"Chat","bindings":{"Ctrl+G":"chat:externalEditor"}}]}"#,
            Err(Hold::Rebound),
        ),
        ("{not json", Err(Hold::Unparsable)),
        (UNRELATED, Ok(())),
    ] {
        write(&path, contents);
        let keymap = ClaudeKeymap::at_spawn(path.clone(), &verified());
        assert_eq!(keymap.check(), want, "{contents}");
    }

    let as_dir = dir.path().join("as-dir");
    std::fs::create_dir(&as_dir).unwrap();
    let keymap = ClaudeKeymap::at_spawn(as_dir, &verified());
    assert_eq!(
        keymap.check(),
        Err(Hold::Unreadable(Unreadable::NotRegular))
    );

    let fifo = dir.path().join("fifo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert_eq!(
        read_source(&fifo),
        SourceRead::Unreadable(Unreadable::NotRegular),
        "a FIFO with no writer doesn't hang the read"
    );

    let big = dir.path().join("big.json");
    std::fs::write(&big, vec![b' '; SOURCE_BYTES as usize + 1]).unwrap();
    assert_eq!(
        read_source(&big),
        SourceRead::Unreadable(Unreadable::TooLarge)
    );
    std::fs::write(&big, vec![b' '; SOURCE_BYTES as usize]).unwrap();
    assert!(matches!(read_source(&big), SourceRead::Present { .. }));
}

/// Scenario: `keybindings.json` is a symlink to a file elsewhere: it is read through the link, its identity the target's, and allows the probe while nothing changes. Pointing the link at another file with the same bytes holds with "changed". The control leaves the link alone and allows the probe at both reads.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_symlinked_file_is_read_through_and_retargeting_holds() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    let first = dir.path().join("first.json");
    let second = dir.path().join("second.json");
    write(&first, UNRELATED);
    write(&second, UNRELATED);
    std::os::unix::fs::symlink(&first, &path).unwrap();
    assert_eq!(
        read_source(&path),
        read_source(&first),
        "the target's identity"
    );

    let control = ClaudeKeymap::at_spawn(path.clone(), &verified());
    assert_eq!(control.check(), Ok(()));
    assert_eq!(control.check(), Ok(()));

    let retargeted = ClaudeKeymap::at_spawn(path.clone(), &verified());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&second, &path).unwrap();
    assert_eq!(retargeted.check(), Err(Hold::Changed));
}

/// Scenario: Claude's `keybindings.json` is looked for under `HOME/.claude` when `CLAUDE_CONFIG_DIR` isn't in the agent's environment. With `CLAUDE_CONFIG_DIR` set, or set but empty, the lookup holds as unmeasured instead of following it, and so does a `HOME` that is unset, empty or relative.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_where_claude_reads_its_bindings() {
    let env = |vars: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    };
    assert_eq!(
        claude_keybindings_path(env(&[("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "/c")])),
        Err(Hold::Unmeasured)
    );
    assert_eq!(
        claude_keybindings_path(env(&[("HOME", "/h"), ("CLAUDE_CONFIG_DIR", "")])),
        Err(Hold::Unmeasured),
        "present but empty"
    );
    assert_eq!(
        claude_keybindings_path(env(&[("HOME", "/h")])),
        Ok(PathBuf::from("/h/.claude/keybindings.json"))
    );
    for home in [&[][..], &[("HOME", "")], &[("HOME", "h")]] {
        assert_eq!(
            claude_keybindings_path(env(home)),
            Err(Hold::Unmeasured),
            "{home:?}"
        );
    }
}

/// Scenario: Ctrl+G is recognised in the spellings the keymap files use, in either case and with spaces, and nothing else is: not Ctrl+H, not plain G, not Alt+G.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_ctrl_g_in_its_spellings() {
    for key in [
        "ctrl+g",
        "Ctrl+G",
        "ctrl-g",
        "control+g",
        "C-g",
        "<c-g>",
        "^G",
        "ctrl + g",
    ] {
        assert!(is_ctrl_g(key), "{key}");
    }
    for key in ["ctrl+h", "g", "alt+g", "ctrl+shift+g", "f6", ""] {
        assert!(!is_ctrl_g(key), "{key}");
    }
}

fn args(list: &[&str]) -> Vec<OsString> {
    list.iter().map(OsString::from).collect()
}

/// Scenario: Codex's launch arguments name its sources: `-c`, `--config`, `--config=` and the attached `-ck=v` and `-c=k=v` forms are overrides in command-line order, `-p`/`--profile` selects `CODEX_HOME/<name>.config.toml` (the last one wins), and nothing after `--` is read. A flag with no value, or an empty profile name, holds as unparsable. The control, with no such flag, has the config file only.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_sources_from_its_launch() {
    let home = Path::new("/ch");
    let sources = CodexSources::from_launch(
        home,
        &args(&[
            "-c",
            "a=1",
            "--config",
            "b=2",
            "--config=c=3",
            "-cd=4",
            "-c=e=5",
            "-p",
            "one",
            "--profile=two",
            "-m",
            "o3",
            "--",
            "-c",
            "z=9",
        ]),
        vec![PathBuf::from("/p/.codex/config.toml")],
    )
    .unwrap();
    assert_eq!(sources.config, PathBuf::from("/ch/config.toml"));
    assert_eq!(sources.profile, Some(PathBuf::from("/ch/two.config.toml")));
    assert_eq!(sources.overrides, ["a=1", "b=2", "c=3", "d=4", "e=5"]);
    assert_eq!(
        sources.projects,
        vec![PathBuf::from("/p/.codex/config.toml")]
    );

    assert_eq!(
        CodexSources::from_launch(home, &args(&["-c"]), vec![]),
        Err(Hold::Unparsable)
    );
    assert_eq!(
        CodexSources::from_launch(home, &args(&["--profile="]), vec![]),
        Err(Hold::Unparsable)
    );
    let control = CodexSources::from_launch(home, &args(&["-m", "o3"]), vec![]).unwrap();
    assert_eq!(control.profile, None);
    assert!(control.overrides.is_empty());
}

/// A Codex home and project in a temporary directory.
struct Codex {
    dir: tempfile::TempDir,
}

impl Codex {
    fn new() -> Codex {
        let codex = Codex {
            dir: test_temp::tempdir().unwrap(),
        };
        std::fs::create_dir_all(codex.home()).unwrap();
        std::fs::create_dir_all(codex.dir.path().join("project/.codex")).unwrap();
        codex
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn config(&self) -> PathBuf {
        self.home().join("config.toml")
    }

    fn profile(&self) -> PathBuf {
        self.home().join("work.config.toml")
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project/.codex/config.toml")
    }

    fn sources(&self, launch: &[&str]) -> CodexSources {
        CodexSources::from_launch(&self.home(), &args(launch), vec![self.project()]).unwrap()
    }

    /// Reads twice with nothing between and resolves in `order`.
    fn resolve(&self, launch: &[&str], order: &[KeymapSource]) -> Result<(), Hold> {
        let sources = self.sources(launch);
        codex_keymap(&sources.read(), &sources.read(), order)
    }
}

fn editor_on(key: &str) -> String {
    format!("[tui.keymap.global]\nopen_external_editor = \"{key}\"\n")
}

/// Scenario: Codex's sources merge in the release record's order, a later source winning: Ctrl+G moved to F6 in `config.toml` and back by `-c` allows the probe, the reverse holds as rebound; the profile overrides `config.toml`, and the project file overrides the profile. Resolving the same files in the reversed order flips each outcome, so the order is the record's data. A profile with no keymap, a `-c` that doesn't touch it, and no sources at all each allow the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_sources_merge_in_the_records_precedence() {
    let reversed: Vec<KeymapSource> = CODEX_ORDER.iter().rev().copied().collect();
    let restore = "-ctui.keymap.global.open_external_editor=ctrl-g";
    let move_off = "-ctui.keymap.global.open_external_editor=f6";

    let codex = Codex::new();
    assert_eq!(codex.resolve(&[], CODEX_ORDER), Ok(()), "no sources");
    assert_eq!(codex.resolve(&["-c", "model=\"o3\""], CODEX_ORDER), Ok(()));

    write(&codex.config(), &editor_on("f6"));
    assert_eq!(codex.resolve(&[], CODEX_ORDER), Err(Hold::Rebound));
    assert_eq!(codex.resolve(&[restore], CODEX_ORDER), Ok(()), "-c wins");
    assert_eq!(codex.resolve(&[restore], &reversed), Err(Hold::Rebound));

    write(&codex.config(), &editor_on("ctrl-g"));
    assert_eq!(codex.resolve(&[move_off], CODEX_ORDER), Err(Hold::Rebound));
    assert_eq!(codex.resolve(&[move_off], &reversed), Ok(()));

    write(&codex.config(), &editor_on("f6"));
    write(&codex.profile(), &editor_on("ctrl-g"));
    assert_eq!(
        codex.resolve(&["-p", "work"], CODEX_ORDER),
        Ok(()),
        "the profile over the config"
    );
    assert_eq!(
        codex.resolve(&["-p", "work"], &reversed),
        Err(Hold::Rebound)
    );

    write(&codex.profile(), &editor_on("f6"));
    write(&codex.project(), &editor_on("ctrl-g"));
    assert_eq!(
        codex.resolve(&["-p", "work"], CODEX_ORDER),
        Ok(()),
        "the project over the profile"
    );
    assert_eq!(
        codex.resolve(&["-p", "work"], &reversed),
        Err(Hold::Rebound)
    );

    write(&codex.config(), "model = \"o3\"\n");
    write(&codex.profile(), "model = \"o4\"\n");
    write(&codex.project(), "");
    assert_eq!(codex.resolve(&["-p", "work"], CODEX_ORDER), Ok(()));
}

/// Scenario: The effective keymap holds as rebound when another action is on Ctrl+G or the external editor isn't on it, as unknown when the keymap, a context or a binding isn't the shape a binding has, as unparsable when a file or a `-c` can't be read as TOML, as unreadable when a file can't be read, and as unpinned when a source the record doesn't list is present. The controls bind the editor to Ctrl+G explicitly, alone or with another key, and other actions elsewhere, and allow the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_bindings_that_hold() {
    let codex = Codex::new();
    for (config, want) in [
        (
            "[tui.keymap.global]\ncopy = \"ctrl-g\"\n".to_string(),
            Err(Hold::Rebound),
        ),
        (
            "[tui.keymap.editor]\ninsert_newline = [\"f2\", \"ctrl-g\"]\n".to_string(),
            Err(Hold::Rebound),
        ),
        (
            "[tui.keymap.global]\nopen_external_editor = []\n".to_string(),
            Err(Hold::Rebound),
        ),
        (editor_on("f6"), Err(Hold::Rebound)),
        (
            "[tui]\nkeymap = \"f6\"\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        ("tui = 1\n".to_string(), Err(Hold::UnknownKey)),
        (
            "[tui.keymap]\nquit = \"ctrl+q\"\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        (
            "[tui.keymap.global]\ncopy = 7\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        ("[tui.keymap\n".to_string(), Err(Hold::Unparsable)),
        (editor_on("ctrl-g"), Ok(())),
        (
            "[tui.keymap.global]\nopen_external_editor = [\"ctrl-g\", \"f6\"]\ncopy = \"ctrl-o\"\n"
                .to_string(),
            Ok(()),
        ),
    ] {
        write(&codex.config(), &config);
        assert_eq!(codex.resolve(&[], CODEX_ORDER), want, "{config}");
    }

    write(&codex.config(), "");
    assert_eq!(
        codex.resolve(
            &["-c", "tui.keymap={global={open_external_editor=\"f6\"}}"],
            CODEX_ORDER
        ),
        Err(Hold::Rebound),
        "an inline table"
    );
    assert_eq!(
        codex.resolve(&["-c", "no-equals-sign"], CODEX_ORDER),
        Err(Hold::Unparsable)
    );
    assert_eq!(
        codex.resolve(&["-c", "tui..keymap=1"], CODEX_ORDER),
        Err(Hold::Unparsable)
    );
    std::fs::write(codex.config(), [0xff, 0xfe]).unwrap();
    assert_eq!(
        codex.resolve(&[], CODEX_ORDER),
        Err(Hold::Unparsable),
        "not UTF-8"
    );
    std::fs::remove_file(codex.config()).unwrap();
    std::fs::create_dir(codex.config()).unwrap();
    assert_eq!(
        codex.resolve(&[], CODEX_ORDER),
        Err(Hold::Unreadable(Unreadable::NotRegular))
    );
    std::fs::remove_dir(codex.config()).unwrap();

    let no_profile = [
        KeymapSource::CodexConfig,
        KeymapSource::CodexProject,
        KeymapSource::CodexOverride,
    ];
    assert_eq!(
        codex.resolve(&["-p", "work"], &no_profile),
        Err(Hold::Unpinned)
    );
    assert_eq!(
        codex.resolve(&[], &no_profile),
        Ok(()),
        "no profile selected"
    );
}

/// Scenario: A Codex keymap with a context, an action or a key spelling outside what both pinned releases accept holds as unknown: an unknown context (also an empty one), an unknown action beside known ones, a name only the newer release has, and spellings the grammar refuses (`+` instead of `-`, a modifier after the key, three strokes, `f25`, `cmd-`, `leader`). Ctrl+G in another spelling the grammar accepts, on another action or in a chord, holds as rebound. The controls bind known actions in known contexts with accepted spellings, leave a known context empty, and spell the editor's Ctrl+G with Codex's aliases; each allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_names_and_spellings_outside_the_pinned_schema_hold() {
    let codex = Codex::new();
    let copy_on = |key: &str| format!("[tui.keymap.global]\ncopy = \"{key}\"\n");
    let mut cases = vec![
        (
            "[tui.keymap.unmeasured_context]\nunmeasured_action = \"f6\"\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        (
            "[tui.keymap.unmeasured_context]\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        (
            "[tui.keymap.global]\ncopy = \"ctrl-o\"\nnot_a_measured_action = \"f6\"\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        (
            "[tui.keymap.global]\nleader = \"ctrl-x\"\n".to_string(),
            Err(Hold::UnknownKey),
        ),
        (editor_on("ctrl+g"), Err(Hold::UnknownKey)),
        (
            "[tui.keymap.global]\nopen_external_editor = [\"ctrl-g\", \"unmeasured-key\"]\n"
                .to_string(),
            Err(Hold::UnknownKey),
        ),
    ];
    for spelling in [
        "unmeasured-key",
        "ctrl+o",
        "o-ctrl",
        "ctrl-ctrl-o",
        "ctrl-x ctrl-y ctrl-z",
        "f25",
        "cmd-o",
        "leader c",
        "",
        " ",
    ] {
        cases.push((copy_on(spelling), Err(Hold::UnknownKey)));
    }
    for spelling in ["Control-G", "CTRL--g", "ctrl-x ctrl-g", "ctrl-g x"] {
        cases.push((copy_on(spelling), Err(Hold::Rebound)));
    }
    cases.push((editor_on("ctrl-g x"), Err(Hold::Rebound)));
    cases.extend([
        (
            "[tui.keymap.global]\ncopy = \"ctrl-o\"\n[tui.keymap.pager]\nclose = [\"q\", \"escape\"]\n[tui.keymap.list]\npage_up = \"alt-pgup\"\n[tui.keymap.editor]\n"
                .to_string(),
            Ok(()),
        ),
        (copy_on("f24"), Ok(())),
        (copy_on("ctrl-shift-g"), Ok(())),
        (copy_on("ctrl-x g"), Ok(())),
        (editor_on("Control-G"), Ok(())),
        (editor_on(" CTRL--g "), Ok(())),
    ]);
    for (config, want) in cases {
        write(&codex.config(), &config);
        assert_eq!(codex.resolve(&[], CODEX_ORDER), want, "{config}");
    }
}

/// Scenario: Codex key spellings read in the grammar both pinned releases accept: each alias becomes its canonical key, each named key, printable character and function key up to F24 is kept, modifiers in any order and spelling come out as `ctrl-alt-shift-`, and a two-stroke chord stays two strokes. Spellings the grammar refuses read as nothing.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_key_spellings_in_the_pinned_grammar() {
    for (raw, want) in [
        ("escape", "esc"),
        ("Return", "enter"),
        ("spacebar", "space"),
        ("pgup", "page-up"),
        ("PageUp", "page-up"),
        ("pgdn", "page-down"),
        ("pagedown", "page-down"),
        ("del", "delete"),
        ("shift-option-control-x", "ctrl-alt-shift-x"),
        ("Control-G", "ctrl-g"),
        ("f1", "f1"),
        ("F24", "f24"),
        ("?", "?"),
    ] {
        assert_eq!(codex_key(raw), Some(vec![want.to_string()]), "{raw}");
    }
    for named in [
        "enter",
        "tab",
        "backspace",
        "esc",
        "delete",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "page-up",
        "page-down",
        "space",
        "minus",
    ] {
        assert_eq!(
            codex_key(&format!("alt-{named}")),
            Some(vec![format!("alt-{named}")]),
            "{named}"
        );
    }
    assert_eq!(
        codex_key("ctrl-x  ctrl-g"),
        Some(vec!["ctrl-x".to_string(), "ctrl-g".to_string()])
    );
    for refused in [
        "",
        "-",
        "ctrl",
        "ctrl+g",
        "g-ctrl",
        "alt-alt-g",
        "f0",
        "f25",
        "super-g",
        "cmd-g",
        "leader g",
        "a b c",
        "ctrl-é",
        "insert",
    ] {
        assert_eq!(codex_key(refused), None, "{refused}");
    }
}

/// Scenario: Claude's `keybindings.json` is read as strings wherever they sit: Ctrl+G as a value rather than a member name, the external-editor action as a member name rather than a value, either one deep in nested arrays and objects, and Ctrl+G spelled with spaces, in a chord or alone, each hold as rebound. The control, a document of unrelated bindings that also uses spaced spellings and strings that merely contain a `g`, allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_claude_scan_reads_every_string() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    for (contents, want) in [
        (
            r#"{"bindings":[{"key":"ctrl+g","action":"chat:submit"}]}"#,
            Err(Hold::Rebound),
        ),
        (r#"{"chat:externalEditor":null}"#, Err(Hold::Rebound)),
        (
            r#"{"a":[{"b":[{"c":"chat:externalEditor"}]}]}"#,
            Err(Hold::Rebound),
        ),
        (r#"{"a":[{"b":[{"^G":1}]}]}"#, Err(Hold::Rebound)),
        (
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl + g":"chat:submit"}}]}"#,
            Err(Hold::Rebound),
        ),
        (
            r#"{"bindings":[{"key":"ctrl + x  ctrl + g","action":"chat:submit"}]}"#,
            Err(Hold::Rebound),
        ),
        (
            r#"{"bindings":[{"key":"Control - G"}]}"#,
            Err(Hold::Rebound),
        ),
        (
            r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+e":"chat:submit","ctrl + x ctrl + h":"chat:cancel"}}],"note":"go, gauge"}"#,
            Ok(()),
        ),
    ] {
        write(&path, contents);
        let keymap = ClaudeKeymap::at_spawn(path.clone(), &verified());
        assert_eq!(keymap.check(), want, "{contents}");
    }
}

/// Scenario: Claude's `keybindings.json` spells Ctrl+G in ways splitting a string into strokes would lose — whitespace inside a word (`c trl+g`, `ct rl+g`, `< c-g >`) or a stroke followed by a word starting with `-` (`ctrl+g -`) — in a member name and in a value; each holds as rebound, as do a stroke split by something other than whitespace (`ctrl + (g)`, alone or after another stroke) and a string that only happens to contain a spelling (`ctrl+gx`). The control, a document of unrelated bindings and near-miss words, allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_claude_spellings_strokes_would_split_hold() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    for spelling in [
        "c trl+g",
        "ct rl+g",
        "< c-g >",
        "C ontrol - G",
        "ctrl+g -",
        "ctrl+x ctrl+g -",
        "ctrl + (g)",
        "ctrl+x, ctrl + (g)",
        "ctrl+gx",
    ] {
        for contents in [
            serde_json::json!({"bindings": [{"context": "Chat", "bindings": {spelling: null}}]}),
            serde_json::json!({"bindings": [{"key": spelling, "action": "chat:submit"}]}),
        ] {
            write(&path, &contents.to_string());
            let keymap = ClaudeKeymap::at_spawn(path.clone(), &verified());
            assert_eq!(keymap.check(), Err(Hold::Rebound), "{contents}");
        }
    }

    write(
        &path,
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+h":"chat:submit","c-h":null,"^h":"chat:cancel","<c-x> -":"chat:killAgents"}}],"note":"go, gauge, chat: external, ctrl+ h g"}"#,
    );
    let control = ClaudeKeymap::at_spawn(path, &verified());
    assert_eq!(control.check(), Ok(()));
}

/// Scenario: Claude's `keybindings.json` names one object member twice: a `bindings` whose first copy binds Ctrl+G and whose second is empty, a `key` of `ctrl+g` then `ctrl+h`, an `action` of the external editor then another, a name written once plainly and once with a `\u` escape (`ctrl+g` and `ctrl+\u0067`, and `note` and `n\u006fte` with no Ctrl+G anywhere), and a repeat nested in a context's bindings or deep in arrays; each holds as unparsable, including the ones whose strings are otherwise unrelated. The control uses the same name in sibling objects, at different depths and in another case, a name escaped once (`n\u006fte`), beside booleans, numbers and null, and allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_claude_member_named_twice_holds() {
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    for contents in [
        r#"{"bindings":{"ctrl+g":null},"bindings":{}}"#,
        r#"{"key":"ctrl+g","key":"ctrl+h"}"#,
        r#"{"action":"chat:externalEditor","action":"chat:submit"}"#,
        r#"{"ctrl+g":null,"ctrl+g":null}"#,
        r#"{"note":"go","note":"gauge"}"#,
        r#"{"ctrl+g":null,"ctrl+\u0067":null}"#,
        r#"{"note":"go","n\u006fte":"gauge"}"#,
        r#"{"note":1,"\u006eote":2}"#,
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+e":"chat:submit","ctrl+e":"chat:cancel"}}]}"#,
        r#"{"a":[[{"b":{"c":1,"c":1}}]]}"#,
    ] {
        write(&path, contents);
        let keymap = ClaudeKeymap::at_spawn(path.clone(), &verified());
        assert_eq!(keymap.check(), Err(Hold::Unparsable), "{contents}");
    }

    write(
        &path,
        r#"{"bindings":[{"context":"Chat","bindings":{"ctrl+e":"chat:submit"}},{"context":"Global","bindings":{"ctrl+e":"chat:submit","Ctrl+E":null}}],"context":"top","n\u006fte":1,"n":[true,false,-1,2,0.5,null]}"#,
    );
    let control = ClaudeKeymap::at_spawn(path, &verified());
    assert_eq!(control.check(), Ok(()));
}

/// Scenario: Codex's sources are read before the spawn and again after the first ready frame: a file changed between the two, created where none was, or removed holds with "changed", whatever either read binds. The control reads the same files twice and allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_codex_source_changed_between_its_reads_holds() {
    let codex = Codex::new();
    write(&codex.config(), "model = \"o3\"\n");
    let sources = codex.sources(&["-p", "work"]);

    let before = sources.read();
    assert_eq!(
        codex_keymap(&before, &sources.read(), CODEX_ORDER),
        Ok(()),
        "unchanged"
    );

    write(&codex.config(), "model = \"o4\"\n");
    assert_eq!(
        codex_keymap(&before, &sources.read(), CODEX_ORDER),
        Err(Hold::Changed),
        "changed"
    );

    let before = sources.read();
    write(&codex.profile(), "model = \"o4\"\n");
    assert_eq!(
        codex_keymap(&before, &sources.read(), CODEX_ORDER),
        Err(Hold::Changed),
        "the profile created"
    );

    let before = sources.read();
    std::fs::remove_file(codex.profile()).unwrap();
    assert_eq!(
        codex_keymap(&before, &sources.read(), CODEX_ORDER),
        Err(Hold::Changed),
        "the profile removed"
    );

    let before = sources.read();
    write(&codex.project(), &editor_on("ctrl-g"));
    assert_eq!(
        codex_keymap(&before, &sources.read(), CODEX_ORDER),
        Err(Hold::Changed),
        "a project file created"
    );
}

/// Scenario: A source file with a set modification time reads as its device, inode, size, both times to the nanosecond and the SHA-256 of its bytes, as the file's own metadata gives them. A path through a regular file reads as unreadable rather than absent, and on Linux a `/proc` file, whose contents aren't the size its metadata gives, reads as unstable. The control, a missing file, reads as absent.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_source_reads_as_its_identity() {
    use sha2::{Digest, Sha256};
    let dir = test_temp::tempdir().unwrap();
    let path = keybindings(&dir);
    write(&path, UNRELATED);
    let at = std::time::UNIX_EPOCH + Duration::new(1_234_567_890, 123_456_789);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(at)
        .unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let SourceRead::Present { identity, contents } = read_source(&path) else {
        panic!("present");
    };
    assert_eq!(contents, UNRELATED.as_bytes());
    assert_eq!(identity.dev, meta.dev());
    assert_eq!(identity.ino, meta.ino());
    assert_eq!(identity.size, UNRELATED.len() as u64);
    assert_eq!(identity.mtime_ns, 1_234_567_890_123_456_789);
    assert_eq!(
        identity.ctime_ns,
        i128::from(meta.ctime()) * 1_000_000_000 + i128::from(meta.ctime_nsec())
    );
    let hash: [u8; 32] = Sha256::digest(UNRELATED.as_bytes()).into();
    assert_eq!(identity.hash, hash);

    assert_eq!(
        read_source(&path.join("below-a-file")),
        SourceRead::Unreadable(Unreadable::Io(std::io::ErrorKind::NotADirectory))
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        read_source(Path::new("/proc/self/status")),
        SourceRead::Unreadable(Unreadable::Unstable)
    );
    assert_eq!(read_source(&dir.path().join("missing")), SourceRead::Absent);
}

/// Scenario: Each Codex source kind is resolved against a precedence that leaves it out: `config.toml`, a project file and a `-c` each hold as unpinned once present, and allow the probe while absent (no file, no `-c`). The control resolves the same sources against the full order and allows the probe.
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_present_source_the_record_doesnt_list_holds() {
    let without = |left_out: KeymapSource| -> Vec<KeymapSource> {
        CODEX_ORDER
            .iter()
            .copied()
            .filter(|kind| *kind != left_out)
            .collect()
    };
    let codex = Codex::new();
    let no_config = without(KeymapSource::CodexConfig);
    let no_project = without(KeymapSource::CodexProject);
    let no_override = without(KeymapSource::CodexOverride);
    assert_eq!(codex.resolve(&[], &no_config), Ok(()), "no config file");
    assert_eq!(codex.resolve(&[], &no_project), Ok(()), "no project file");
    assert_eq!(codex.resolve(&[], &no_override), Ok(()), "no -c");

    write(&codex.config(), "model = \"o3\"\n");
    assert_eq!(codex.resolve(&[], &no_config), Err(Hold::Unpinned));
    write(&codex.project(), "model = \"o3\"\n");
    assert_eq!(codex.resolve(&[], &no_project), Err(Hold::Unpinned));
    assert_eq!(
        codex.resolve(&["-c", "model=\"o3\""], &no_override),
        Err(Hold::Unpinned)
    );
    assert_eq!(
        codex.resolve(&["-c", "model=\"o3\""], CODEX_ORDER),
        Ok(()),
        "the full order"
    );
}

/// Scenario: List the project files a Codex launch in a nested cwd may read, with Codex's home inside one of its ancestors and with it elsewhere, and from a relative cwd. Every ancestor's `.codex/config.toml` is listed, the root's first and the cwd's last, except the one that is Codex's own home; a relative cwd holds as unmeasured (A-20d-1b).
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_project_files_are_every_ancestors_but_its_home() {
    use dot_agent_deck::composer_probe::keymap::codex_project_files;
    let files = codex_project_files(Path::new("/w/repo/sub"), Path::new("/w/.codex")).unwrap();
    assert_eq!(
        files,
        vec![
            PathBuf::from("/.codex/config.toml"),
            PathBuf::from("/w/repo/.codex/config.toml"),
            PathBuf::from("/w/repo/sub/.codex/config.toml"),
        ],
        "the home's own `.codex` is Codex's config, not a project's"
    );
    let all = codex_project_files(Path::new("/w/repo"), Path::new("/elsewhere/.codex")).unwrap();
    assert_eq!(all.len(), 3, "/, /w and /w/repo: {all:?}");
    assert_eq!(
        codex_project_files(Path::new("repo"), Path::new("/h/.codex")),
        Err(Hold::Unmeasured)
    );
}

/// Scenario: Read a Codex launch whose project files sit in the cwd and in an ancestor, through the project guard. With no keymap in any of them the probe is allowed, even where the files would rebind if read; a keymap in either one, binding the editor away or even to Ctrl+G, holds as unpinned, since whether Codex loads it depends on the root and trust the deck doesn't resolve; a project file that isn't TOML holds as unparsable and a directory in its place as unreadable (A-20d-1b).
#[spec("inbox/probe/014")]
#[test]
fn probe_014_a_keymap_in_any_codex_project_file_holds() {
    use dot_agent_deck::composer_probe::keymap::{CodexBaseline, codex_projects_guard};
    let dir = test_temp::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("repo/sub");
    std::fs::create_dir_all(cwd.join(".codex")).unwrap();
    std::fs::create_dir_all(dir.path().join("repo/.codex")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let guard = || codex_projects_guard(&CodexBaseline::at_spawn(&home, &[], &cwd).unwrap().read);

    assert_eq!(guard(), Ok(()), "no project file at all");
    write(&cwd.join(".codex/config.toml"), "model = \"o3\"\n");
    write(
        &dir.path().join("repo/.codex/config.toml"),
        "[tui]\nalt_screen = false\n",
    );
    assert_eq!(guard(), Ok(()), "project files without a keymap");
    for (file, contents) in [
        (cwd.join(".codex/config.toml"), editor_on("f6")),
        (
            dir.path().join("repo/.codex/config.toml"),
            editor_on("ctrl-g"),
        ),
    ] {
        let before = std::fs::read(&file).unwrap();
        std::fs::write(&file, contents).unwrap();
        assert_eq!(guard(), Err(Hold::Unpinned), "{}", file.display());
        std::fs::write(&file, before).unwrap();
    }
    write(&cwd.join(".codex/config.toml"), "not = [toml");
    assert_eq!(guard(), Err(Hold::Unparsable));
    std::fs::remove_file(cwd.join(".codex/config.toml")).unwrap();
    std::fs::create_dir(cwd.join(".codex/config.toml")).unwrap();
    assert!(matches!(guard(), Err(Hold::Unreadable(_))));
}

/// Scenario: Take a Codex launch's baseline before its spawn, then compare it with the launch the bound agent made. The same flags in the same cwd match; another `-c`, another profile or another cwd hold as changed; `-C`/`--cd` in any form, before `--`, holds as unpinned at the spawn and at the binding. A project file changed between the spawn's read and a later read holds as changed (A-20d-1b).
#[spec("inbox/probe/014")]
#[test]
fn probe_014_codex_launches_are_compared_with_the_spawns_baseline() {
    use dot_agent_deck::composer_probe::keymap::{CodexBaseline, codex_moves_cwd};
    let dir = test_temp::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("repo");
    let other = dir.path().join("other");
    for d in [&home, &cwd.join(".codex"), &other] {
        std::fs::create_dir_all(d).unwrap();
    }
    let launch = args(&["-c", "model=\"o3\"", "-p", "work"]);
    let baseline = CodexBaseline::at_spawn(&home, &launch, &cwd).unwrap();
    assert_eq!(baseline.bound(&home, &launch, &cwd), Ok(()));
    assert_eq!(
        baseline.bound(&home, &args(&["-c", "model=\"o4\"", "-p", "work"]), &cwd),
        Err(Hold::Changed)
    );
    assert_eq!(
        baseline.bound(&home, &args(&["-c", "model=\"o3\""]), &cwd),
        Err(Hold::Changed)
    );
    assert_eq!(baseline.bound(&home, &launch, &other), Err(Hold::Changed));
    for moved in [&["-C", "/x"][..], &["-C/x"], &["--cd", "/x"], &["--cd=/x"]] {
        assert!(codex_moves_cwd(&args(moved)), "{moved:?}");
        assert_eq!(
            CodexBaseline::at_spawn(&home, &args(moved), &cwd),
            Err(Hold::Unpinned)
        );
        assert_eq!(
            baseline.bound(&home, &args(moved), &cwd),
            Err(Hold::Unpinned)
        );
    }
    assert!(!codex_moves_cwd(&args(&["--", "-C", "/x"])));
    assert!(!codex_moves_cwd(&args(&["-c", "k=v"])));

    write(&cwd.join(".codex/config.toml"), "model = \"o3\"\n");
    let fresh = CodexBaseline::at_spawn(&home, &launch, &cwd).unwrap();
    std::thread::sleep(TICK);
    write(&cwd.join(".codex/config.toml"), "model = \"o4\"\n");
    assert_eq!(
        codex_keymap(&fresh.read, &fresh.sources.read(), CODEX_ORDER),
        Err(Hold::Changed),
        "a project file changed since the spawn"
    );
}

/// Scenario: Take a generation's key-binding baseline before a Codex spawn, then compare it with what the deck read of the bound agent's launch. The same flags, cwd and home match; a bound agent whose arguments couldn't be read (unreadable, or a command line over the deck's bound) holds as unmeasured rather than counting as no arguments, so an override the deck never saw can't pass (the reva21 review's U2-2).
#[spec("inbox/probe/014")]
#[test]
fn probe_014_an_unread_bound_codex_argv_holds() {
    use dot_agent_deck::composer_probe::keymap::KeymapBaseline;
    let dir = test_temp::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("repo");
    for d in [&home, &cwd] {
        std::fs::create_dir_all(d).unwrap();
    }
    let env = |name: &str| (name == "HOME").then(|| home.clone().into_os_string());
    let baseline = KeymapBaseline::at_spawn(false, true, env, &[], &cwd);
    assert!(baseline.codex_bound(env, Some(&[]), Some(&cwd)).is_ok());
    assert_eq!(
        baseline.codex_bound(env, None, Some(&cwd)).err(),
        Some(Hold::Unmeasured),
        "an unread argv is not an empty one"
    );
}

/// Scenario: Take a generation's key-binding baseline before its spawn with the spawn's `HOME`, then compare it with the bound agent's own environment, as a command that sets its own variables (`CODEX_HOME=/x codex`, `HOME=/y claude`) leaves it. The same `HOME` matches for both agents, and so does a `CODEX_HOME` naming the home the spawn's `HOME` already gave Codex (the deck's wrapper pins it so); another `HOME` or `CODEX_HOME` holds as changed, and a `CLAUDE_CONFIG_DIR` as unmeasured. An environment that lists one of these names twice with different values holds as unmeasured (the reva21 review's U1-1).
#[spec("inbox/probe/014")]
#[test]
fn probe_014_the_bound_agents_environment_is_compared_with_the_spawns() {
    use dot_agent_deck::composer_probe::keymap::{KeymapBaseline, environ_lookup};
    let dir = test_temp::tempdir().unwrap();
    let home = dir.path().join("home");
    let cwd = dir.path().join("repo");
    for d in [&home, &cwd] {
        std::fs::create_dir_all(d).unwrap();
    }
    let spawn = |name: &str| (name == "HOME").then(|| home.clone().into_os_string());
    let baseline = KeymapBaseline::at_spawn(true, true, spawn, &[], &cwd);
    let environ = |pairs: &[(&str, &Path)]| -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), v.as_os_str().to_os_string()))
            .collect()
    };
    let claude = |pairs: &[(&str, &Path)]| {
        let environ = environ(pairs);
        let env = environ_lookup(&environ).unwrap();
        baseline.claude_bound(&env).map(|_| ())
    };
    let codex = |pairs: &[(&str, &Path)]| {
        let environ = environ(pairs);
        let env = environ_lookup(&environ).unwrap();
        baseline
            .codex_bound(&env, Some(&[]), Some(&cwd))
            .map(|_| ())
    };
    let other = dir.path().join("other");
    assert_eq!(claude(&[("HOME", &home)]), Ok(()));
    assert_eq!(codex(&[("HOME", &home)]), Ok(()));
    assert_eq!(
        codex(&[("HOME", &home), ("CODEX_HOME", &home.join(".codex"))]),
        Ok(()),
        "the wrapper's pin names the same home"
    );
    assert_eq!(claude(&[("HOME", &other)]), Err(Hold::Changed));
    assert_eq!(codex(&[("HOME", &other)]), Err(Hold::Changed));
    assert_eq!(
        codex(&[("HOME", &home), ("CODEX_HOME", &other)]),
        Err(Hold::Changed)
    );
    assert_eq!(
        claude(&[("HOME", &home), ("CLAUDE_CONFIG_DIR", &other)]),
        Err(Hold::Unmeasured)
    );
    assert_eq!(claude(&[]), Err(Hold::Unmeasured), "no HOME");

    let twice = environ(&[("HOME", &home), ("HOME", &other)]);
    assert_eq!(environ_lookup(&twice).err(), Some(Hold::Unmeasured));
    let same = environ(&[("HOME", &home), ("HOME", &home)]);
    assert_eq!(
        environ_lookup(&same).unwrap()("HOME"),
        Some(home.clone().into_os_string())
    );
}
