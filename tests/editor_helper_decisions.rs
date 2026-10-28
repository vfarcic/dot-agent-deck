//! PRD #1666 part 3 (Q35-12): the editor helper's pure decisions
//! (`src/composer_probe/helper.rs`): how a value is split, which editor each
//! agent kind chooses on the user's route and on a tool's, the export shapes,
//! the branch, the armed path's return table, the marker's name and the
//! editor's environment. Hostile values are split literally and never
//! evaluated. No process runs here; `tests/e2e_editor_helper.rs` and
//! `tests/e2e_editor_helper_faults.rs` run the real helper.

#![cfg(target_os = "linux")]

use std::ffi::{OsStr, OsString};
use std::time::{Duration, Instant};

use dot_agent_deck::composer_probe::classifier::Class;
use dot_agent_deck::composer_probe::helper::{
    Arm, ArmedExit, Branch, Deadline, LEARNED_DEFAULT, NoEditor, Route, Seen, armed_exit, branch,
    editor_command, editor_env, export_shaped, marker_name, split_shell_words, split_spaces,
};
use dot_agent_deck::composer_probe::install::{ProbeAgent, SavedEditor};
use dot_agent_deck::composer_probe::instance::{HELPER_NAME, Kind};
use dot_agent_deck::event::{ComposerDecision, ComposerOutcome, ExportRead};
use spec::spec;

const CLAUDE: Kind = Kind::Agent(ProbeAgent::Claude);
const CODEX: Kind = Kind::Agent(ProbeAgent::Codex);

fn words(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn saved(visual: Option<&str>, editor: Option<&str>) -> SavedEditor {
    SavedEditor {
        visual: visual.map(OsString::from),
        editor: editor.map(OsString::from),
    }
}

fn nothing_on_path(_: &OsStr) -> bool {
    false
}

/// Scenario: Split editor values the way each agent does: Claude Code's on literal spaces, Codex's by shell words with single and double quotes and backslashes. Values holding `;`, `$(…)`, backticks, pipes and redirections come back as literal words, never evaluated, and an unterminated quote or a trailing backslash is no command at all.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_values_are_split_literally() {
    assert_eq!(
        split_spaces(OsStr::new("  ed  --wait ")),
        words(&["ed", "--wait"])
    );
    assert_eq!(
        split_spaces(OsStr::new("ed; rm -rf $(pwd) | tee >out")),
        words(&["ed;", "rm", "-rf", "$(pwd)", "|", "tee", ">out"])
    );
    assert_eq!(
        split_spaces(OsStr::new("'my ed' a\tb")),
        words(&["'my", "ed'", "a\tb"])
    );
    let shell = |s: &str| split_shell_words(OsStr::new(s));
    assert_eq!(
        shell("ed 'a b' \"c d\" e\\ f"),
        Some(words(&["ed", "a b", "c d", "e f"]))
    );
    assert_eq!(
        shell("ed ';' $(touch x) `id` | >out"),
        Some(words(&["ed", ";", "$(touch", "x)", "`id`", "|", ">out"]))
    );
    assert_eq!(
        shell(r#"ed "a\"b\\c\$d\e" 'x\y'"#),
        Some(words(&["ed", r#"a"b\c$d\e"#, r"x\y"]))
    );
    assert_eq!(shell("ed ''"), Some(words(&["ed", ""])));
    assert_eq!(shell(" \t\n"), Some(vec![]));
    assert_eq!(shell("ed 'open"), None);
    assert_eq!(shell("ed \"open"), None);
    assert_eq!(shell("ed \\"), None);
}

/// Scenario: Choose Claude Code's editor: a truthy `VISUAL`, else a truthy `EDITOR`, else the first of `code`, `vi` and `nano` installed. On the user's route exactly `code` becomes `code -w` and `subl` becomes `subl --wait`, a tool's route keeps the value as given, and a value that names the helper itself counts as unset.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_claude_code_chooses_as_claude_does() {
    let pick = |route, s: &SavedEditor, path: &[&str]| {
        let path: Vec<OsString> = words(path);
        editor_command(CLAUDE, route, s, |name| path.iter().any(|p| p == name))
    };
    let user = Route::User;
    assert_eq!(
        pick(user, &saved(Some("vim -u NONE"), Some("nano")), &[]),
        Ok(words(&["vim", "-u", "NONE"]))
    );
    assert_eq!(
        pick(user, &saved(Some(""), Some("nano")), &[]),
        Ok(words(&["nano"]))
    );
    assert_eq!(
        pick(user, &saved(None, None), &["vi", "nano"]),
        Ok(words(&["vi"]))
    );
    assert_eq!(
        pick(user, &saved(None, None), &["nano", "code"]),
        Ok(words(&["code", "-w"]))
    );
    assert_eq!(pick(user, &saved(None, None), &[]), Err(NoEditor::Unset));
    assert_eq!(
        pick(user, &saved(Some("subl"), None), &[]),
        Ok(words(&["subl", "--wait"]))
    );
    assert_eq!(
        pick(user, &saved(Some("code --new-window"), None), &[]),
        Ok(words(&["code", "--new-window"]))
    );
    assert_eq!(
        pick(Route::Tool, &saved(Some("code"), None), &[]),
        Ok(words(&["code"]))
    );
    assert_eq!(
        pick(user, &saved(Some("  "), None), &[]),
        Err(NoEditor::Unsplittable)
    );
    let helper = format!("/x/{HELPER_NAME} --flag");
    assert_eq!(
        pick(user, &saved(Some(&helper), Some("nano")), &[]),
        Ok(words(&["nano"]))
    );
    assert_eq!(
        pick(user, &saved(Some(&helper), Some(&helper)), &["vi"]),
        Ok(words(&["vi"]))
    );
}

/// Scenario: Choose Codex's editor: on the user's route a set `VISUAL` wins, an empty one is an error rather than a fallback, an unset one falls back to `EDITOR` with the same rule, neither set is an error, and one of blanks alone is no command; on a tool's route empty values are skipped. Values are shell words, and one naming the helper counts as unset.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_codex_chooses_as_codex_does() {
    let pick = |route, s: &SavedEditor| editor_command(CODEX, route, s, nothing_on_path);
    let user = Route::User;
    assert_eq!(
        pick(user, &saved(Some("'my ed' -n"), Some("nano"))),
        Ok(words(&["my ed", "-n"]))
    );
    assert_eq!(
        pick(user, &saved(Some(""), Some("nano"))),
        Err(NoEditor::EmptyVisual)
    );
    assert_eq!(pick(user, &saved(None, Some("nano"))), Ok(words(&["nano"])));
    assert_eq!(
        pick(user, &saved(None, Some(""))),
        Err(NoEditor::EmptyEditor)
    );
    assert_eq!(pick(user, &saved(None, None)), Err(NoEditor::Unset));
    assert_eq!(
        pick(user, &saved(Some("ed 'open"), None)),
        Err(NoEditor::Unsplittable)
    );
    assert_eq!(
        pick(user, &saved(Some(" \t"), Some("nano"))),
        Err(NoEditor::Unsplittable)
    );
    assert_eq!(
        pick(Route::Tool, &saved(Some(""), Some("nano"))),
        Ok(words(&["nano"]))
    );
    assert_eq!(
        pick(Route::Tool, &saved(Some(""), Some(""))),
        Err(NoEditor::Unset)
    );
    let helper = HELPER_NAME.to_string();
    assert_eq!(
        pick(user, &saved(Some(&helper), Some("nano"))),
        Ok(words(&["nano"]))
    );
}

/// Scenario: Choose a learned generation's editor (an agent not known yet): a non-empty `VISUAL`, else a non-empty `EDITOR`, else `vi`, in shell words, on either route; a value of blanks alone is no command.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_a_learned_generation_falls_back_to_vi() {
    for route in [Route::User, Route::Tool] {
        let pick = |s: &SavedEditor| editor_command(Kind::Learned, route, s, nothing_on_path);
        assert_eq!(pick(&saved(Some("ed -s"), None)), Ok(words(&["ed", "-s"])));
        assert_eq!(pick(&saved(Some(""), Some("nano"))), Ok(words(&["nano"])));
        assert_eq!(pick(&saved(None, None)), Ok(words(&[LEARNED_DEFAULT])));
        assert_eq!(pick(&saved(Some("  "), None)), Err(NoEditor::Unsplittable));
    }
}

/// Scenario: Recognise each agent's export shape in the invocation's last argument: Claude Code's under `<TMPDIR>/claude-<uid>/`, Codex's as `.tmp*.md` directly in `<CODEX_HOME>/editor/`, with `/tmp` and `~/.codex` when those are unset or empty. Another uid, a `..` or `.` on the way, a nested directory, a wrong name, no argument and a learned generation have no shape.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_export_shapes() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    };
    let shaped = |kind, args: &[&str], e: &'static [(&'static str, &'static str)]| {
        export_shaped(kind, &words(args), env(e), 1000)
    };
    assert!(shaped(CLAUDE, &["/tmp/claude-1000/a/prompt.md"], &[]));
    assert!(shaped(CLAUDE, &["+3", "/tmp/claude-1000/p.md"], &[]));
    assert!(shaped(
        CLAUDE,
        &["/run/t/claude-1000/p.md"],
        &[("TMPDIR", "/run/t/")]
    ));
    assert!(shaped(
        CLAUDE,
        &["/tmp/claude-1000/p.md"],
        &[("TMPDIR", "")]
    ));
    assert!(!shaped(CLAUDE, &["/tmp/claude-1001/p.md"], &[]));
    assert!(!shaped(CLAUDE, &["/tmp/claude-1000"], &[]));
    assert!(!shaped(CLAUDE, &["/tmp/claude-1000/../x.md"], &[]));
    assert!(!shaped(CLAUDE, &["/tmp/claude-1000/./x.md"], &[]));
    assert!(!shaped(CLAUDE, &["tmp/claude-1000/x.md"], &[]));
    assert!(!shaped(CLAUDE, &["/tmp/claude-1000/p.md", "x"], &[]));
    assert!(!shaped(CLAUDE, &[], &[]));
    let home = &[("CODEX_HOME", "/h/codex")];
    assert!(shaped(CODEX, &["/h/codex/editor/.tmpAbC123.md"], home));
    assert!(shaped(
        CODEX,
        &["/home/u/.codex/editor/.tmpx.md"],
        &[("HOME", "/home/u"), ("CODEX_HOME", "")]
    ));
    assert!(!shaped(CODEX, &["/h/codex/editor/sub/.tmpx.md"], home));
    assert!(!shaped(CODEX, &["/h/codex/editor/tmpx.md"], home));
    assert!(!shaped(CODEX, &["/h/codex/editor/.tmpx.txt"], home));
    assert!(!shaped(
        CODEX,
        &["/h/codex/editor/../editor/.tmpx.md"],
        home
    ));
    assert!(!shaped(CODEX, &["/h/codex/editor/.tmpx.md"], &[]));
    assert!(!shaped(Kind::Learned, &["/tmp/claude-1000/p.md"], &[]));
}

/// Scenario: Take the helper's branch for every arm and class: a tool's invocation runs the tool's editor whatever the arm; with the arm present every other invocation is armed and opens no editor; with it absent the agent's own invocation reports unarmed, and anything else opens the user's editor unreported only with the export shape, the tool's otherwise.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_the_branch_table() {
    let seen = Some(Seen { seg: 4, native: 9 });
    let absent = [Arm::Absent { seen }, Arm::Absent { seen: None }];
    let present = [Arm::Present { seen }, Arm::Present { seen: None }];
    for shaped in [false, true] {
        for arm in absent.into_iter().chain(present) {
            assert_eq!(branch(arm, Class::PositivelyTool, shaped), Branch::Tool);
        }
        for arm in present {
            for class in [Class::Owned, Class::Unknown] {
                assert_eq!(branch(arm, class, shaped), Branch::Armed);
            }
        }
        for arm in absent {
            assert_eq!(branch(arm, Class::Owned, shaped), Branch::Unarmed);
            assert_eq!(
                branch(arm, Class::Unknown, shaped),
                if shaped {
                    Branch::UnreportedUser
                } else {
                    Branch::UnreportedTool
                }
            );
        }
    }
    assert_eq!(Arm::Present { seen }.seen(), seen);
}

/// Scenario: Read the armed path's return table: Claude Code exits 0 on every outcome, Codex only on an attested empty read, a learned generation as Codex; the notice shows for a notice outcome, an uncertain or unarmed answer, an outcome a newer daemon names and a failed exchange, and not for a plain attested read.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_the_armed_return_table() {
    let mine = |read| Some(ComposerOutcome::Mine { read });
    let notice = |read| Some(ComposerOutcome::MineWithNotice { read });
    let quiet = |status| ArmedExit {
        notice: false,
        status,
    };
    let loud = |status| ArmedExit {
        notice: true,
        status,
    };
    for read in [
        ExportRead::Empty,
        ExportRead::NonEmpty,
        ExportRead::Unreadable,
    ] {
        assert_eq!(armed_exit(CLAUDE, mine(read)), quiet(0));
        assert_eq!(armed_exit(CLAUDE, notice(read)), loud(0));
        let codex = i32::from(read != ExportRead::Empty);
        assert_eq!(armed_exit(CODEX, mine(read)), quiet(codex));
        assert_eq!(armed_exit(CODEX, notice(read)), loud(codex));
        assert_eq!(armed_exit(Kind::Learned, mine(read)), quiet(codex));
    }
    assert_eq!(armed_exit(CODEX, mine(ExportRead::Unknown)), quiet(1));
    for failed in [
        None,
        Some(ComposerOutcome::Uncertain),
        Some(ComposerOutcome::Unarmed),
        Some(ComposerOutcome::Unknown),
    ] {
        assert_eq!(armed_exit(CLAUDE, failed), loud(0));
        assert_eq!(armed_exit(CODEX, failed), loud(1));
        assert_eq!(armed_exit(Kind::Learned, failed), loud(1));
    }
}

/// Scenario: Name markers for each decision, with the segment and native numbers when the arm read was consistent and without them otherwise; every name, with and without the daemon's `.seen`, fits the grammar the daemon parses, and none holds a path separator.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_marker_names_fit_their_grammar() {
    let seen = Some(Seen {
        seg: u64::MAX,
        native: 0,
    });
    assert_eq!(
        marker_name(12, 345, ComposerDecision::Armed, seen),
        format!("12-345-armed-{}-0", u64::MAX)
    );
    assert_eq!(
        marker_name(12, 345, ComposerDecision::Unarmed, None),
        "12-345-unarmed"
    );
    assert_eq!(
        marker_name(12, 345, ComposerDecision::Tool, None),
        "12-345-tool"
    );
    let fits = |name: &str| {
        let name = name.strip_suffix(".seen").unwrap_or(name);
        let parts: Vec<&str> = name.split('-').collect();
        let digits = |s: &str, max: usize| {
            !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_digit())
        };
        matches!(parts.len(), 3 | 5)
            && digits(parts[0], 10)
            && digits(parts[1], 20)
            && ["armed", "unarmed", "tool"].contains(&parts[2])
            && parts[3..].iter().all(|p| digits(p, 20))
    };
    for decision in [
        ComposerDecision::Armed,
        ComposerDecision::Unarmed,
        ComposerDecision::Tool,
    ] {
        for seen in [seen, None] {
            let name = marker_name(u32::MAX, u64::MAX, decision, seen);
            assert!(fits(&name) && fits(&format!("{name}.seen")), "{name}");
            assert!(!name.contains('/'));
        }
    }
}

/// Scenario: Build the editor's environment: every variable under the helper's reserved prefix is removed, `VISUAL` and `EDITOR` come back exactly as the user had them, set, set and empty, or unset, and everything else is kept as it was, values that aren't UTF-8 included.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_the_editor_gets_the_users_environment() {
    use std::os::unix::ffi::OsStringExt;
    let odd = OsString::from_vec(vec![0xff, b'x']);
    let vars = vec![
        ("VISUAL".into(), "/i/dot-agent-deck-editor".into()),
        ("EDITOR".into(), "/i/dot-agent-deck-editor".into()),
        ("DOT_AGENT_DECK_EDITOR_GEN".into(), "3".into()),
        ("DOT_AGENT_DECK_EDITOR_USER_VISUAL".into(), "vim".into()),
        ("DOT_AGENT_DECK_EDITOR_FUTURE".into(), "x".into()),
        ("DOT_AGENT_DECK_SOCKET".into(), "/s".into()),
        ("ODD".into(), odd.clone()),
    ];
    let env = editor_env(vars.clone(), &saved(Some("vim"), Some("")));
    let get = |env: &[(OsString, OsString)], name: &str| {
        env.iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(get(&env, "VISUAL"), words(&["vim"]));
    assert_eq!(get(&env, "EDITOR"), words(&[""]));
    assert_eq!(get(&env, "DOT_AGENT_DECK_SOCKET"), words(&["/s"]));
    assert_eq!(get(&env, "ODD"), vec![odd]);
    assert!(
        env.iter()
            .all(|(k, _)| !k.to_string_lossy().starts_with("DOT_AGENT_DECK_EDITOR_"))
    );
    let unset = editor_env(vars, &saved(None, None));
    assert!(get(&unset, "VISUAL").is_empty() && get(&unset, "EDITOR").is_empty());
}

/// Scenario: A post-editor deadline has time left until it passes and none from then on, so a step taken after it is skipped rather than called with no time.
#[spec("inbox/probe/013")]
#[test]
fn probe_013_a_passed_deadline_leaves_no_time() {
    let now = Instant::now();
    let ahead = Deadline::after(now, Duration::from_secs(60));
    assert!(
        ahead
            .remaining()
            .is_some_and(|left| left > Duration::from_secs(50))
    );
    let passed = Deadline::after(now, Duration::ZERO);
    assert_eq!(passed.remaining(), None);
    let soon = Deadline::after(Instant::now(), Duration::from_millis(20));
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(soon.remaining(), None);
}
