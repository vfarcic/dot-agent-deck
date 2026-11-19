//! Issue #2130: Codex's `/cd` and `/worktree` move the directory its session
//! works in without a `chdir`, and Codex then rebuilds its keymap from the new
//! directory's project files. Before the deck's check presses Ctrl+G, it reads
//! the directory Codex's status line shows on the frame the check is admitted
//! on and compares it with the directory the generation was bound in
//! (`src/composer_probe/keymap.rs`'s `codex_session_cwd` and
//! `CodexReads::session`, `src/doorbell_screen.rs`'s `codex_status_line`).
//!
//! The frames are real Codex 0.164.0 output (`tests/fixtures/codex-inbox/
//! release-0.164.0/session-cwd/`, recorded by `tests/support/
//! codex_editor_measure.py`'s `cwd` and `worktree` modes); the rest is pure.

use std::path::{Path, PathBuf};

use dot_agent_deck::composer_probe::keymap::{
    CodexBaseline, CodexReads, CodexSession, Hold, codex_cwd_display, codex_session_cwd,
};
use dot_agent_deck::doorbell_activation::KeymapSource;
use dot_agent_deck::doorbell_screen::{Grid, codex_status_line};
use spec::spec;

#[path = "../src/test_temp.rs"]
mod test_temp;

const FIXTURES: &str = "tests/fixtures/codex-inbox/release-0.164.0/session-cwd";

/// The recording's sandbox root, which every captured path starts with.
fn summary() -> serde_json::Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join("summary.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The screen at the end of a fixture's bytes.
fn frame(name: &str, rows: u16, cols: u16) -> Grid {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURES)
        .join(name);
    Grid::parse(&std::fs::read(path).unwrap(), rows, cols)
}

fn root(recording: &str) -> PathBuf {
    PathBuf::from(summary()[recording]["root"].as_str().unwrap())
}

/// Scenario: Read Codex 0.164.0's real screens: started in a folder outside its home, after `/cd` into another folder, into a folder under its home, into a long path an 80-column pane cuts short, back to where it started, and after `/worktree`. The deck reads the status line under the input box on each, finds the folder Codex started in on the first and the last `/cd` back, another folder after each move, and holds when the line is cut short.
#[spec("inbox/probe/103")]
#[test]
fn probe_103_the_status_line_on_codexs_measured_screens() {
    let cwd_root = root("cwd");
    let home = cwd_root.join("home");
    let work = cwd_root.join("work");
    let cases = [
        (
            "work-24x80-start.bin",
            (24, 80),
            format!("nollm-model default · {}", work.display()),
            Ok(()),
        ),
        (
            "work-24x80-cd-other.bin",
            (24, 80),
            format!("nollm-model default · {}", cwd_root.join("other").display()),
            Err(Hold::Changed),
        ),
        (
            "work-24x80-cd-home-proj.bin",
            (24, 80),
            "nollm-model default · ~/proj".to_string(),
            Err(Hold::Changed),
        ),
        (
            "work-24x80-cd-back.bin",
            (24, 80),
            format!("nollm-model default · {}", work.display()),
            Ok(()),
        ),
        (
            "plain-50x200-start.bin",
            (50, 200),
            "nollm-model default · ~/plain".to_string(),
            Err(Hold::Changed),
        ),
    ];
    for (name, (rows, cols), line, outcome) in cases {
        let grid = frame(name, rows, cols);
        let read = codex_status_line(&grid);
        assert_eq!(read.as_deref(), Some(line.as_str()), "{name}");
        assert_eq!(
            codex_session_cwd(read.as_deref(), &work, Some(&home), false),
            outcome,
            "{name}, bound in work"
        );
    }
    // The launch that started in `~/plain` reads as its own folder.
    let plain = frame("plain-50x200-start.bin", 50, 200);
    assert_eq!(
        codex_session_cwd(
            codex_status_line(&plain).as_deref(),
            &home.join("plain"),
            Some(&home),
            false
        ),
        Ok(()),
        "~/plain, bound in ~/plain"
    );
    // With no HOME the deck can't tell which folder `~` is.
    assert_eq!(
        codex_session_cwd(
            codex_status_line(&plain).as_deref(),
            &home.join("plain"),
            None,
            false
        ),
        Err(Hold::Footer),
        "~/plain with no HOME"
    );

    // An 80-column pane cuts the long path short: held, not latched.
    let long = frame("work-24x80-cd-long.bin", 24, 80);
    let read = codex_status_line(&long).expect("the long path's line");
    assert!(read.ends_with('…'), "{read}");
    assert_eq!(
        codex_session_cwd(Some(&read), &work, Some(&home), false),
        Err(Hold::Footer)
    );

    // `/worktree`, continuing the conversation in the new checkout.
    let wt_root = root("worktree");
    let wt = frame("worktree-50x200.bin", 50, 200);
    let read = codex_status_line(&wt).expect("the worktree's line");
    assert!(
        read.starts_with("nollm-model default · ~/.codex/worktrees/"),
        "{read}"
    );
    assert_eq!(
        codex_session_cwd(
            Some(&read),
            &wt_root.join("work"),
            Some(&wt_root.join("home")),
            false
        ),
        Err(Hold::Changed)
    );
}

/// Scenario: A screen that isn't Codex's ready input box, a status line with its first two columns drawn, and one holding a character two columns wide each give the deck no status line to read.
#[spec("inbox/probe/103")]
#[test]
fn probe_103_no_status_line_is_read_off_codexs_ready_screen() {
    let ready = frame("work-24x80-start.bin", 24, 80);
    assert!(codex_status_line(&ready).is_some());
    // No ready screen: the composer's footer hint gone.
    let blank = Grid::parse(b"\x1b[2J\x1b[H  just text\r\n", 24, 80);
    assert_eq!(codex_status_line(&blank), None);
    // The line's indent drawn over.
    let mut indented = ready.clone();
    let row = indented.last_drawn_row().unwrap() - 1;
    indented.cells[row as usize][0].text = "x".into();
    assert_eq!(codex_status_line(&indented), None);
    // A wide character: its second cell reads as nothing.
    let mut wide = ready.clone();
    wide.cells[row as usize][10].text = "漢".into();
    assert_eq!(codex_status_line(&wide), None);
}

/// Scenario: Compare status lines with the folder Codex was started in, as Codex spells it: `~` for its home and below it, the absolute path elsewhere. A line naming that folder passes, with or without a thread name after it; one naming another folder is a move; a line cut short, one with two folders or none, a `~` with no home to resolve it, no line at all, and a start folder the deck can't spell all hold without being taken as a move.
#[spec("inbox/probe/103")]
#[test]
fn probe_103_codexs_status_line_against_the_start_folder() {
    let home = Path::new("/home/me");
    let proj = Path::new("/home/me/code/proj");
    let outside = Path::new("/srv/proj");
    assert_eq!(codex_cwd_display(home, Some(home)).as_deref(), Some("~"));
    assert_eq!(
        codex_cwd_display(proj, Some(home)).as_deref(),
        Some("~/code/proj")
    );
    // A component-wise prefix, as Codex's `strip_prefix`: `/home/meant`
    // isn't below `/home/me`, and a trailing slash on HOME changes nothing.
    assert_eq!(
        codex_cwd_display(Path::new("/home/meant"), Some(home)).as_deref(),
        Some("/home/meant")
    );
    assert_eq!(
        codex_cwd_display(proj, Some(Path::new("/home/me/"))).as_deref(),
        Some("~/code/proj")
    );
    assert_eq!(
        codex_cwd_display(outside, Some(home)).as_deref(),
        Some("/srv/proj")
    );
    assert_eq!(
        codex_cwd_display(outside, None).as_deref(),
        Some("/srv/proj")
    );
    assert_eq!(codex_cwd_display(Path::new("/a · b"), None), None);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = Path::new(std::ffi::OsStr::from_bytes(b"/srv/\xff"));
        assert_eq!(codex_cwd_display(odd, None), None);
        assert_eq!(
            codex_session_cwd(Some("m low · /srv/\u{fffd}"), odd, None, false),
            Err(Hold::Footer)
        );
    }

    let check = |line: &str| codex_session_cwd(Some(line), proj, Some(home), false);
    assert_eq!(check("gpt-5.5 medium · ~/code/proj"), Ok(()));
    assert_eq!(
        check("gpt-5.5 medium · ~/code/proj · fix the tests"),
        Ok(())
    );
    assert_eq!(check("gpt-5.5 medium · ~/code/proj · fix th…"), Ok(()));
    assert_eq!(check("~/code/proj · gpt-5.5 medium"), Ok(()));
    assert_eq!(check("gpt-5.5 medium · ~/code/other"), Err(Hold::Changed));
    assert_eq!(check("gpt-5.5 medium · /srv/proj"), Err(Hold::Changed));
    assert_eq!(check("gpt-5.5 medium · ~"), Err(Hold::Changed));
    assert_eq!(
        check("gpt-5.5 medium · ~/code/proj/sub · thread"),
        Err(Hold::Changed)
    );
    assert_eq!(check("gpt-5.5 medium · ~/code/pr…"), Err(Hold::Footer));
    assert_eq!(check("gpt-5.5 medium · ~/code/proj…"), Err(Hold::Footer));
    assert_eq!(check("gpt-5.5 medium"), Err(Hold::Footer));
    assert_eq!(check("gpt-5.5 medium · main"), Err(Hold::Footer));
    assert_eq!(
        check("gpt-5.5 medium · ~/code/proj · /a thread"),
        Err(Hold::Footer),
        "two items read as a folder"
    );
    assert_eq!(
        check("gpt-5.5 medium · /tmp/x · ~/code/proj"),
        Err(Hold::Footer),
        "a folder holding the separator"
    );
    assert_eq!(
        check("gpt-5.5 medium ·  ~/code/proj"),
        Err(Hold::Footer),
        "another separator"
    );
    assert_eq!(
        codex_session_cwd(None, proj, Some(home), false),
        Err(Hold::Footer)
    );
    // No HOME: an absolute path compares, a `~` holds.
    assert_eq!(
        codex_session_cwd(Some("m low · /srv/proj"), outside, None, false),
        Ok(())
    );
    assert_eq!(
        codex_session_cwd(Some("m low · /srv/other"), outside, None, false),
        Err(Hold::Changed)
    );
    assert_eq!(
        codex_session_cwd(Some("m low · ~/proj"), outside, None, false),
        Err(Hold::Footer)
    );
    // A folder named like the start folder, the separator and more beside
    // it (the Greptile review's P1): Codex in `~/code/proj · copy` draws
    // the same line as a thread named `copy`, so anything after the folder
    // holds while such a folder exists, and the folder alone still passes.
    let beside = |line: &str| codex_session_cwd(Some(line), proj, Some(home), true);
    assert_eq!(beside("gpt-5.5 medium · ~/code/proj"), Ok(()));
    assert_eq!(
        beside("gpt-5.5 medium · ~/code/proj · copy"),
        Err(Hold::Footer)
    );
    assert_eq!(
        beside("gpt-5.5 medium · ~/code/proj · co…"),
        Err(Hold::Footer)
    );
    assert_eq!(beside("gpt-5.5 medium · ~/code/other"), Err(Hold::Changed));
}

/// Scenario: Look beside a start folder for a folder named like it followed by ` · `, which Codex's status line would show as the start folder and a thread name. Only such a folder counts, the start folder's own contents and look-alikes without the separator don't, and a folder holding `/` is looked for in `/`; a parent that can't be read counts as holding one.
#[spec("inbox/probe/103")]
#[test]
fn probe_103_folders_named_like_the_start_folder_and_more_are_found() {
    use dot_agent_deck::composer_probe::keymap::codex_separator_siblings;
    let dir = test_temp::tempdir().unwrap();
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(proj.join("proj · inside")).unwrap();
    std::fs::create_dir(dir.path().join("proj copy")).unwrap();
    std::fs::create_dir(dir.path().join("projx · y")).unwrap();
    std::fs::write(dir.path().join("proj ·"), "").unwrap();
    assert!(!codex_separator_siblings(&proj), "none beside it");
    std::fs::create_dir(dir.path().join("proj · copy")).unwrap();
    assert!(codex_separator_siblings(&proj), "proj · copy");
    assert!(
        codex_separator_siblings(&dir.path().join("missing").join("proj")),
        "an unreadable parent"
    );
    // `/`: what matters is an entry of `/` starting with the separator,
    // which a host's root doesn't hold.
    assert!(!codex_separator_siblings(std::path::Path::new("/")));
}

/// Scenario: Follow one Codex generation's checks as the deck makes them. Its status line naming its own folder passes; a line the deck can't read holds that check alone; a line naming another folder holds, and from then on every check of the generation holds as changed, its own folder shown again and its settings files unchanged included. A generation the deck has no folder for holds every check, and a new generation on its own folder passes.
#[spec("inbox/probe/103")]
#[test]
fn probe_103_a_moved_codex_session_holds_for_the_rest_of_the_generation() {
    const ORDER: &[KeymapSource] = &[
        KeymapSource::CodexConfig,
        KeymapSource::CodexProfile,
        KeymapSource::CodexProject,
        KeymapSource::CodexOverride,
    ];
    let dir = test_temp::tempdir().unwrap();
    let home = dir.path().join("home");
    let codex_home = home.join(".codex");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&codex_home).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(codex_home.join("config.toml"), "model = \"m\"\n").unwrap();
    let baseline = CodexBaseline::at_spawn(&codex_home, &[], &work).unwrap();
    let session = CodexSession {
        cwd: work.clone(),
        home: Some(home.clone()),
    };
    let own = format!("m low · {}", work.display());
    let other = format!("m low · {}", dir.path().join("other").display());

    let reads = CodexReads::with_session(&baseline, Some(session.clone()));
    assert_eq!(reads.check(ORDER), Ok(()));
    assert_eq!(reads.session(Some(&own)), Ok(()));
    assert_eq!(reads.session(None), Err(Hold::Footer), "no line");
    assert_eq!(
        reads.session(Some("m low · /x…")),
        Err(Hold::Footer),
        "cut short"
    );
    assert_eq!(reads.session(Some(&own)), Ok(()), "not latched");
    assert_eq!(reads.session(Some(&other)), Err(Hold::Changed), "moved");
    assert_eq!(reads.session(Some(&own)), Err(Hold::Changed), "moved back");
    assert_eq!(reads.check(ORDER), Err(Hold::Changed), "the sources too");

    let unbound = CodexReads::new(&baseline);
    assert_eq!(unbound.check(ORDER), Ok(()));
    assert_eq!(unbound.session(Some(&own)), Err(Hold::Footer));

    let next = CodexReads::with_session(&baseline, Some(session.clone()));
    assert_eq!(next.session(Some(&own)), Ok(()), "a new generation");

    // A thread name after the folder passes only once a check has looked
    // beside the folder and found nothing named like it and ` · `; each
    // check looks again.
    let named = format!("{own} · copy");
    let reads = CodexReads::with_session(&baseline, Some(session));
    assert_eq!(reads.session(Some(&named)), Err(Hold::Footer), "not looked");
    assert_eq!(reads.check(ORDER), Ok(()));
    assert_eq!(reads.session(Some(&named)), Ok(()), "a thread name");
    std::fs::create_dir(dir.path().join("work · copy")).unwrap();
    assert_eq!(reads.session(Some(&named)), Ok(()), "until the next check");
    assert_eq!(reads.check(ORDER), Ok(()));
    assert_eq!(
        reads.session(Some(&named)),
        Err(Hold::Footer),
        "the folder beside it"
    );
    assert_eq!(reads.session(Some(&own)), Ok(()), "the folder alone");
}
