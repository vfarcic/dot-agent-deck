//! Binary-only reader for the daemon's work journal (PRD #1889), as the
//! tests see it from outside: `<state_dir>/work-journal/*.jsonl`, one JSON
//! object per `\n`-terminated line. No production journal types — the file
//! format is the contract.
#![allow(dead_code)] // Each target uses a different subset of this fixture.

use crate::common;
use serde_json::Value;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a lifecycle event may take to reach the journal.
pub fn wait() -> Duration {
    common::load_scaled(Duration::from_secs(15))
}

/// The journal directory under `state_dir`.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("work-journal")
}

/// The state directory a [`common::DaemonProc`] was started with, unless the
/// test overrode `DOT_AGENT_DECK_STATE_DIR`.
pub fn default_state_dir(daemon: &common::DaemonProc) -> PathBuf {
    daemon
        .home
        .parent()
        .expect("the daemon HOME has a parent")
        .join("state")
}

/// Every `*.jsonl` file in the journal directory, sorted by name.
pub fn files(state_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir(state_dir))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
}

/// The per-run segments (`run-<run>.jsonl`, and once it rotates
/// `run-<run>.<n>.jsonl`), sorted by name.
pub fn segments(state_dir: &Path) -> Vec<PathBuf> {
    files(state_dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run-"))
        })
        .collect()
}

/// A segment name's `(<run>, <n>)`: `run-<run>.jsonl` is segment 0 of
/// `<run>`, a rotated `run-<run>.<n>.jsonl` is segment `n` of the same run
/// (run ids contain no `.`).
pub fn segment_name(path: &Path) -> (String, u32) {
    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("run-"))
        .and_then(|name| name.strip_suffix(".jsonl"))
        .unwrap_or_else(|| panic!("{} is not a run segment", path.display()));
    match stem.split_once('.') {
        None => (stem.to_string(), 0),
        Some((run, n)) => (
            run.to_string(),
            n.parse()
                .unwrap_or_else(|_| panic!("{} has no segment number", path.display())),
        ),
    }
}

/// The `<run>` a segment belongs to, rotated or not.
pub fn segment_run(path: &Path) -> String {
    segment_name(path).0
}

/// The distinct runs that have at least one segment, sorted.
pub fn runs(state_dir: &Path) -> Vec<String> {
    let mut runs: Vec<_> = segments(state_dir).iter().map(|s| segment_run(s)).collect();
    runs.sort();
    runs.dedup();
    runs
}

/// The `opened` lines of `work` and of each ancestor up its `parent` chain,
/// nearest first. Stops at a missing `opened` or a cycle.
pub fn ancestry<'a>(lines: &'a [Value], work: &str) -> Vec<&'a Value> {
    let mut chain: Vec<&Value> = Vec::new();
    let mut next = Some(work.to_string());
    while let Some(work) = next.take() {
        if chain.iter().any(|o| o["work"] == work.as_str()) {
            break;
        }
        let Some(opened) = find(lines, "opened", |o| o["work"] == work.as_str()) else {
            break;
        };
        chain.push(opened);
        next = opened["parent"].as_str().map(str::to_string);
    }
    chain
}

/// The complete (`\n`-terminated), parseable lines of one file. A torn tail
/// and unparseable lines are left out.
pub fn file_lines(path: &Path) -> Vec<Value> {
    let bytes = std::fs::read(path).unwrap_or_default();
    let complete = match bytes.iter().rposition(|b| *b == b'\n') {
        Some(end) => &bytes[..=end],
        None => &[][..],
    };
    complete
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .filter(Value::is_object)
        .collect()
}

/// Every journal line across every file, a line seen twice (a dead segment
/// and the compacted file carrying it) counted once by its `(run, seq)`.
pub fn lines(state_dir: &Path) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    files(state_dir)
        .iter()
        .flat_map(|path| file_lines(path))
        .filter(|line| match (line["run"].as_str(), line["seq"].as_u64()) {
            (Some(run), Some(seq)) => seen.insert((run.to_string(), seq)),
            _ => true,
        })
        .collect()
}

/// The journal directory's contents, for a failure message.
pub fn dump(state_dir: &Path) -> String {
    let files = files(state_dir);
    if files.is_empty() {
        let listing: Vec<_> = std::fs::read_dir(state_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        return format!(
            "no journal file under {} (state dir holds {listing:?})",
            dir(state_dir).display()
        );
    }
    files
        .iter()
        .map(|path| {
            format!(
                "== {} ==\n{}",
                path.display(),
                String::from_utf8_lossy(&std::fs::read(path).unwrap_or_default())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait until `find` returns `Some` over the journal's lines, and return it;
/// panic naming `what` and the journal's contents otherwise.
pub fn wait_for<T>(state_dir: &Path, what: &str, find: impl Fn(&[Value]) -> Option<T>) -> T {
    let found = RefCell::new(None);
    let budget = wait();
    common::wait_until(budget, || {
        *found.borrow_mut() = find(&lines(state_dir));
        found.borrow().is_some()
    });
    found.into_inner().unwrap_or_else(|| {
        panic!(
            "the work journal never recorded {what} within {budget:?}.\n{}",
            dump(state_dir)
        )
    })
}

/// The first line of `kind` that `matches` accepts.
pub fn find<'a>(
    lines: &'a [Value],
    kind: &str,
    matches: impl Fn(&Value) -> bool,
) -> Option<&'a Value> {
    lines
        .iter()
        .find(|line| line["kind"] == kind && matches(line))
}

/// Every line of `kind` that `matches` accepts.
pub fn all<'a>(lines: &'a [Value], kind: &str, matches: impl Fn(&Value) -> bool) -> Vec<&'a Value> {
    lines
        .iter()
        .filter(|line| line["kind"] == kind && matches(line))
        .collect()
}

/// The `attempt_started` line for agent `agent` (its registry id).
pub fn attempt_of<'a>(lines: &'a [Value], agent: &str) -> Option<&'a Value> {
    find(lines, "attempt_started", |line| {
        line["agent"].as_u64().map(|n| n.to_string()).as_deref() == Some(agent)
    })
}

/// The spellings the journal's `workspace` may give `path` under the
/// redaction rule: the home directory itself written as `~`, a path under
/// `home` as `~/<rest>`, any other as-is, of `path` and of its canonical form.
pub fn workspace_spellings(home: &Path, path: &Path) -> Vec<String> {
    let mut forms = Vec::new();
    let homes = [
        home.to_path_buf(),
        std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf()),
    ];
    let paths = [
        path.to_path_buf(),
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
    ];
    for path in &paths {
        let under_home = homes.iter().find_map(|home| path.strip_prefix(home).ok());
        let form = match under_home {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        };
        if !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

/// Whether `line`'s `workspace` names `path` (see [`workspace_spellings`]).
pub fn workspace_is(line: &Value, home: &Path, path: &Path) -> bool {
    line["workspace"].as_str().is_some_and(|ws| {
        workspace_spellings(home, path)
            .iter()
            .any(|form| form == ws)
    })
}

/// The lines of `dead_bytes` (a dead segment as it was left) that a
/// compactor must carry byte for byte: every complete, `\n`-terminated line.
/// A torn tail is not one of them (a compactor may drop it).
pub fn complete_lines(dead_bytes: &[u8]) -> Vec<Vec<u8>> {
    let complete = match dead_bytes.iter().rposition(|b| *b == b'\n') {
        Some(end) => &dead_bytes[..=end],
        None => &[][..],
    };
    complete
        .split_inclusive(|b| *b == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

/// Every `compacted-*.jsonl` file's bytes, concatenated.
pub fn compacted_bytes(state_dir: &Path) -> Vec<u8> {
    files(state_dir)
        .iter()
        .filter(|f| {
            f.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("compacted-"))
        })
        .flat_map(|f| std::fs::read(f).unwrap_or_default())
        .collect()
}

/// Whether `haystack` holds `line` as one whole line: at its start or right
/// after a `\n`, ending in `line`'s own `\n`.
pub fn holds_line(haystack: &[u8], line: &[u8]) -> bool {
    haystack
        .windows(line.len())
        .enumerate()
        .any(|(at, w)| w == line && (at == 0 || haystack[at - 1] == b'\n'))
}

/// A one-commit git repository at `dir`, for a dispatch to cut a worktree
/// from. Whatever the caller wrote into `dir` first is committed with it.
pub fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("create the fixture repository");
    std::fs::write(dir.join("README.md"), "work journal fixture\n").expect("write README");
    for args in [
        &["-c", "init.defaultBranch=main", "init", "-q"][..],
        &["add", "-A"][..],
        &["commit", "-qm", "fixture baseline"][..],
    ] {
        let out = common::fixture_git(dir, dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed: {out:?}");
    }
}
