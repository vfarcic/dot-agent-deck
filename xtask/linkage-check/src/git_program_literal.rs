//! Check 13 (issue #1181): no `git` program literal in the root crate's
//! PRODUCTION sources, outside the one module that neutralizes the ambient git
//! environment.
//!
//! ## What it is for
//!
//! `GIT_DIR`, `GIT_WORK_TREE` and their siblings outrank BOTH `-C <dir>` and a
//! command's `current_dir`. A `git` spawned without clearing them acts on
//! whatever repository the ambient variable names, and reports success. Issue
//! #1181 measured all three shapes on the exact commands this crate runs: a
//! `worktree add` landing its branch and its directory in the wrong
//! repository, a `worktree list --porcelain` enumerating the wrong
//! repository's worktrees — the probe a removal decision is made from — and a
//! `worktree remove` DELETING a directory out of the wrong repository.
//!
//! `src/git_env.rs` clears them, and every production `git` is built by one of
//! its constructors. This rule is what keeps that true. It is a **compile-time
//! tripwire for the next call site**, which is the thing a runtime test cannot
//! be: `worktree_reclaim`'s
//! `ambient_location::reclaim_and_dispatch_git_ignore_the_ambient_location_env`
//! proves the constructors neutralize and proves the call sites that exist go
//! through them, but a fourteenth call site added next year is covered by
//! neither assertion. Both are wanted, and they catch different things.
//!
//! ## Why the literal, rather than `Command::new`
//!
//! Because the call shapes vary and the text does not. The sites this rule was
//! written for spell their invocation four ways — `std::process::Command::new`,
//! `tokio::process::Command::new`, `run_status("git", …)` and
//! `run_capture_args("git", …)` — and one of them, `dispatch.rs`'s branch
//! delete, splits the call across lines so that `grep 'run_status("git"'` does
//! not find it. An earlier hand enumeration of this class missed exactly that
//! site. Every one of them, however, names the program with the same five
//! characters, and the sanctioned constructors name it with none: they take no
//! program argument at all, and `git_env` holds the single `GIT` const. So
//! "the literal must not appear" is both the simplest rule and the one with no
//! shape to slip through.
//!
//! ## Scope, and what is NOT covered
//!
//! Every `.rs` under `src/`, **production code only** — anything gated on
//! `test` by a `#[cfg]` is exempt however it is spelled, because a test
//! fixture that builds its own repositories is a different question (issue
//! #1121 covers it, and `git_env::fixture_git` is what it uses).
//! [`production_violations`] documents exactly which `cfg` forms exempt and
//! why the set is narrow. `src/git_env.rs` is exempt because it is the
//! neutralizer.
//!
//! Not covered, so it is not mistaken for covered: `desktop/src-tauri/src/`,
//! `xtask/` and `tests/`. A wrapper that takes the program name from a
//! variable also walks straight through this, exactly as PRD #819's boundary
//! check says of its own subject — this is a regression tripwire, not a proof.

use std::path::{Path, PathBuf};

/// Repo-relative root of the sources this rule covers.
const SRC: &str = "src";

/// The one file allowed to name the program, because it is the file that
/// switches the ambient location environment off. Its absence is a failure
/// rather than a vacuous pass: if it is gone or renamed, the exemption is
/// stale and so is every claim made about it.
const NEUTRALIZER: &str = "src/git_env.rs";

/// The rule sentence, quoted in every failure so the reader is not left to
/// infer what was violated from a line number.
pub const GIT_LITERAL_RULE: &str = "a `git` program literal in production code. An ambient `GIT_DIR` outranks both \
     `-C <dir>` and `current_dir`, so a `git` spawned without clearing it acts on whatever \
     repository that variable names — measured creating, enumerating and DELETING worktrees in \
     the wrong repository (issue #1181). Build the command with `git_env::git_at` (sync) or \
     `git_env::git_async` (tokio), or call `issue_dispatch_run::run_git_status` / \
     `run_git_capture`";

/// Run the check against `root` (the repository root). Returns one string per
/// violation, plus one for a structural problem that would otherwise empty the
/// rule.
pub fn run(root: &Path) -> Vec<String> {
    let src = root.join(SRC);
    if !src.is_dir() {
        return vec![format!(
            "{SRC}/ not found under {} — this rule scanned nothing",
            root.display()
        )];
    }
    if !root.join(NEUTRALIZER).is_file() {
        return vec![format!(
            "{NEUTRALIZER} not found — the module this rule exempts, and whose constructors it \
             points every other module at, is gone or renamed. Fix the rule rather than deleting \
             it: without that module there is nothing clearing the ambient git location \
             environment (issue #1181)"
        )];
    }

    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    files.sort();
    if files.is_empty() {
        return vec![format!(
            "{SRC}/ holds no .rs files — this rule scanned nothing"
        )];
    }

    let mut out = Vec::new();
    for file in files {
        let display = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if display == NEUTRALIZER {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&file) else {
            out.push(format!("{display}: could not be read as UTF-8"));
            continue;
        };
        for line in production_violations(&text) {
            out.push(format!("{display}:{line}: {GIT_LITERAL_RULE}"));
        }
    }
    out
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The 1-indexed lines of `text`'s PRODUCTION code that hold a `git` string
/// literal.
///
/// "Production" is decided **structurally**, by `#[cfg]`, not by a module name.
/// An item gated on `test` is not compiled into a production build, so its
/// whole body is exempt however it is spelled: `mod tests`, `daemon.rs`'s
/// `mod orphan_watchdog_tests`, `platform/proc/mod.rs`'s `pub(crate) mod
/// test_child`, a bare `#[cfg(test)]` block inside a production function
/// (`issue_dispatch_run::devbox_program` has one), or a braceless item such as
/// `#[cfg(test)] mod test_temp;`. An earlier version of this rule matched only
/// a top-level `#[cfg(test)]` immediately followed by `mod tests`, which
/// scanned the first two of those as production and would have failed the
/// build on a test-only `git` fixture added to either.
///
/// Exemption is **narrow on purpose**, because the two errors are not
/// symmetric: scanning something that did not need scanning is a build failure
/// a human looks at, while exempting something that did is the silent hole
/// this rule exists to close. So only `cfg(test)` and `cfg(all(test, …))`
/// exempt. `cfg(any(test, debug_assertions))` — which `build_id.rs` and
/// `daemon_protocol.rs` use — does NOT, because such an item IS compiled into
/// a debug production build; neither does `cfg(not(test))`, which is
/// production-only. Widening this set is a deliberate act.
pub fn production_violations(text: &str) -> Vec<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    // Brace depth, the depth a `cfg(test)` item's body opened at, and whether
    // a `cfg(test)` attribute has been read but its item not yet entered (the
    // attribute itself suppresses, so `#[cfg(test)] const X: &str = "git";`
    // is exempt as well as `#[cfg(test)] fn f() { … }`).
    let mut depth = 0usize;
    let mut suppress_at: Option<usize> = None;
    let mut pending = false;
    // `#![cfg(test)]` gates the whole file from wherever it appears.
    let mut file_suppressed = false;
    let at = |i: usize| chars.get(i).copied();
    let suppressed = |file_suppressed: bool, suppress_at: Option<usize>, pending: bool| {
        file_suppressed || suppress_at.is_some() || pending
    };

    while i < chars.len() {
        match chars[i] {
            '\n' => {
                line += 1;
                i += 1;
            }
            '/' if at(i + 1) == Some('/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if at(i + 1) == Some('*') => {
                let mut nest = 1usize;
                i += 2;
                while i < chars.len() && nest > 0 {
                    if chars[i] == '\n' {
                        line += 1;
                        i += 1;
                    } else if chars[i] == '/' && at(i + 1) == Some('*') {
                        nest += 1;
                        i += 2;
                    } else if chars[i] == '*' && at(i + 1) == Some('/') {
                        nest -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            '#' if matches!(at(i + 1), Some('[') | Some('!')) => {
                let inner = at(i + 1) == Some('!');
                let mut j = i + 1;
                if inner {
                    j += 1;
                }
                if at(j) != Some('[') {
                    i += 1;
                    continue;
                }
                let mut nest = 0usize;
                let mut body = String::new();
                while j < chars.len() {
                    match chars[j] {
                        // Stepped over rather than read: a `]` inside an
                        // attribute's own string (`#[doc = "a ] b"]`) would
                        // otherwise end the attribute early and leave the
                        // scan resuming mid-attribute.
                        '"' => {
                            j += 1;
                            while j < chars.len() {
                                match chars[j] {
                                    '\\' => j += 2,
                                    '"' => break,
                                    '\n' => {
                                        line += 1;
                                        j += 1;
                                    }
                                    _ => j += 1,
                                }
                            }
                        }
                        '[' => {
                            nest += 1;
                            if nest > 1 {
                                body.push('[');
                            }
                        }
                        ']' => {
                            nest -= 1;
                            if nest == 0 {
                                j += 1;
                                break;
                            }
                            body.push(']');
                        }
                        '\n' => {
                            line += 1;
                            body.push(' ');
                        }
                        c => body.push(c),
                    }
                    j += 1;
                }
                if is_test_gate(&body) {
                    if inner {
                        file_suppressed = true;
                    } else {
                        pending = true;
                    }
                }
                i = j;
            }
            '{' => {
                if pending {
                    suppress_at = Some(depth);
                    pending = false;
                }
                depth += 1;
                i += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if suppress_at == Some(depth) {
                    suppress_at = None;
                }
                i += 1;
            }
            ';' => {
                // A braceless item ends here: `#[cfg(test)] mod test_temp;`,
                // `#[cfg(test)] use …;`, `#[cfg(test)] const X: &str = "git";`.
                pending = false;
                i += 1;
            }
            '\'' => {
                // `'\n'` (escaped char), `'a'` (plain char), or `'a` (lifetime).
                if at(i + 1) == Some('\\') {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                } else if at(i + 2) == Some('\'') {
                    i += 3;
                } else {
                    i += 1;
                }
            }
            'r' | 'b' if raw_string_hashes(&chars, i).is_some() => {
                let (start, hashes) = raw_string_hashes(&chars, i).expect("just matched");
                let start_line = line;
                let mut content = String::new();
                i = start;
                loop {
                    if i >= chars.len() {
                        break;
                    }
                    if chars[i] == '"' && (1..=hashes).all(|h| at(i + h) == Some('#')) {
                        i += hashes + 1;
                        break;
                    }
                    if chars[i] == '\n' {
                        line += 1;
                    }
                    content.push(chars[i]);
                    i += 1;
                }
                if content == "git" && !suppressed(file_suppressed, suppress_at, pending) {
                    out.push(start_line);
                }
            }
            '"' => {
                let start_line = line;
                let mut content = String::new();
                i += 1;
                while i < chars.len() {
                    match chars[i] {
                        '\\' => {
                            if at(i + 1) == Some('\n') {
                                line += 1;
                            }
                            content.push('\\');
                            if let Some(c) = at(i + 1) {
                                content.push(c);
                            }
                            i += 2;
                        }
                        '"' => {
                            i += 1;
                            break;
                        }
                        '\n' => {
                            line += 1;
                            content.push('\n');
                            i += 1;
                        }
                        c => {
                            content.push(c);
                            i += 1;
                        }
                    }
                }
                if content == "git" && !suppressed(file_suppressed, suppress_at, pending) {
                    out.push(start_line);
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether an attribute body (the text inside `#[…]`, newlines flattened)
/// gates its item on `test` in a way that keeps it out of every production
/// build. See [`production_violations`] for why this is deliberately narrow.
fn is_test_gate(body: &str) -> bool {
    let body: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if body == "cfg(test)" {
        return true;
    }
    body.starts_with("cfg(all(")
        && !body.contains("any(")
        && !body.contains("not(")
        && has_token(&body, "test")
}

/// `needle` appearing in `haystack` bounded by non-identifier characters, so
/// `test` does not match inside `debug_assertions` or `latest`.
fn has_token(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    haystack.match_indices(needle).any(|(at, _)| {
        let before_ok = at == 0 || !ident(bytes[at - 1]);
        let end = at + needle.len();
        let after_ok = end == bytes.len() || !ident(bytes[end]);
        before_ok && after_ok
    })
}

/// If a raw string starts at `i` (`r"`, `r#"`, `br##"`, …), the index just past
/// its opening quote and the number of hashes. `None` when `i` is an ordinary
/// identifier character — `for` ends in `r`, and `bar"` would otherwise read as
/// one.
fn raw_string_hashes(chars: &[char], i: usize) -> Option<(usize, usize)> {
    if i > 0 {
        let prev = chars[i - 1];
        if prev.is_alphanumeric() || prev == '_' {
            return None;
        }
    }
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hash_start = j;
    while chars.get(j) == Some(&'#') {
        j += 1;
    }
    if chars.get(j) == Some(&'"') {
        Some((j + 1, j - hash_start))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_command_new_is_a_violation() {
        let src = "fn f() {\n    Command::new(\"git\").arg(\"status\");\n}\n";
        assert_eq!(production_violations(src), vec![2]);
    }

    #[test]
    fn a_program_argument_split_across_lines_is_still_found() {
        // `dispatch.rs`'s branch delete is written this way, which is why the
        // rule matches the literal rather than the call shape: the hand
        // enumeration that preceded this rule missed exactly this site.
        let src = "fn f() {\n    run_status(\n        \"git\",\n        &[\"branch\", \"-D\", b],\n    );\n}\n";
        assert_eq!(production_violations(src), vec![3]);
    }

    #[test]
    fn the_sanctioned_constructors_are_clean() {
        let src = "fn f() {\n    git_at(dir).args([\"worktree\", \"remove\"]);\n    run_git_status(&[\"-C\", d, \"branch\", \"-D\", b]);\n}\n";
        assert!(production_violations(src).is_empty());
    }

    #[test]
    fn a_longer_string_beginning_with_git_is_not_a_match() {
        // `"github.com"` and `"git_dir"` are not program names, and a rule
        // that drowns in them gets abandoned — which buys exactly as much as
        // not having one.
        let src = "fn f() {\n    let h = \"github.com\";\n    let k = \"git_dir\";\n    let m = \"failed to spawn `git worktree list`\";\n}\n";
        assert!(production_violations(src).is_empty());
    }

    #[test]
    fn the_test_half_is_exempt() {
        let src = "fn f() {}\n#[cfg(test)]\nmod tests {\n    fn fixture() {\n        Command::new(\"git\");\n    }\n}\n";
        assert!(production_violations(src).is_empty());
    }

    #[test]
    fn a_test_module_not_called_tests_is_exempt_too() {
        // `daemon.rs`'s `mod orphan_watchdog_tests` and
        // `platform/proc/mod.rs`'s `pub(crate) mod test_child`. Matching the
        // module NAME rather than the `#[cfg]` scanned both as production.
        for decl in ["mod orphan_watchdog_tests", "pub(crate) mod test_child"] {
            let src =
                format!("fn f() {{}}\n#[cfg(test)]\n{decl} {{\n    Command::new(\"git\");\n}}\n");
            assert!(
                production_violations(&src).is_empty(),
                "`{decl}` is gated on test and must be exempt"
            );
        }
    }

    #[test]
    fn a_test_gated_item_is_exempt_whatever_shape_it_has() {
        // A bare block inside a production fn, a braceless item, and a
        // platform-narrowed test module.
        let block = "fn p() {\n    #[cfg(test)]\n    {\n        Command::new(\"git\");\n    }\n}\n";
        let braceless = "#[cfg(test)]\nconst P: &str = \"git\";\n";
        let narrowed = "#[cfg(all(test, unix))]\nmod tests {\n    Command::new(\"git\");\n}\n";
        for src in [block, braceless, narrowed] {
            assert!(production_violations(src).is_empty(), "exempt: {src:?}");
        }
    }

    #[test]
    fn production_after_a_test_gated_item_is_still_scanned() {
        // The failure mode of a boundary-to-end-of-file rule: everything
        // below the first `#[cfg(test)]` stops being checked.
        let src = "#[cfg(test)]\nmod tests {\n    Command::new(\"git\");\n}\nfn q() {\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![6]);

        // `issue_dispatch_run::devbox_program`'s shape specifically.
        let devbox = "fn p() -> String {\n    #[cfg(test)]\n    {\n        return o();\n    }\n    \"devbox\".to_string()\n}\nfn q() {\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(devbox), vec![9]);
    }

    #[test]
    fn a_cfg_that_still_reaches_a_production_build_is_not_exempt() {
        // `cfg(any(test, debug_assertions))` — `build_id.rs` and
        // `daemon_protocol.rs` both use it — compiles in a debug production
        // build, and `cfg(not(test))` is production-only. Exempting either
        // would be the silent hole this rule exists to close.
        let any = "#[cfg(any(test, debug_assertions))]\nfn f() {\n    Command::new(\"git\");\n}\n";
        let not = "#[cfg(not(test))]\nfn f() {\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(any), vec![3]);
        assert_eq!(production_violations(not), vec![3]);
    }

    #[test]
    fn a_bracket_inside_an_attribute_string_does_not_end_the_attribute_early() {
        let src =
            "#[doc = \"see [1] below\"]\npub struct S;\nfn f() {\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![4]);
    }

    #[test]
    fn a_non_cfg_attribute_does_not_exempt_anything() {
        let src = "#[derive(Debug)]\nstruct S;\nfn f() {\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![4]);
    }

    #[test]
    fn has_token_does_not_match_inside_an_identifier() {
        assert!(has_token("cfg(all(test,unix))", "test"));
        assert!(!has_token("cfg(all(latest,unix))", "test"));
        assert!(!has_token("cfg(any(test,debug_assertions))", "nope"));
    }

    #[test]
    fn a_literal_inside_a_comment_is_not_a_violation() {
        // This file's own rule sentence is such a comment.
        let src = "// Build it with `git_at`, never Command::new(\"git\").\n/// Doc: Command::new(\"git\") is what this replaces.\n/* block Command::new(\"git\") */\nfn f() {}\n";
        assert!(production_violations(src).is_empty());
    }

    #[test]
    fn a_quote_inside_a_char_literal_does_not_desynchronize_the_scan() {
        // Without char-literal handling the `'\"'` opens a string that
        // swallows the real violation two lines down.
        let src = "fn f() {\n    let q = '\"';\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![3]);
    }

    #[test]
    fn a_lifetime_is_not_read_as_a_char_literal() {
        let src = "fn f<'a>(s: &'a str) {\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![2]);
    }

    #[test]
    fn a_raw_string_is_matched_and_does_not_desynchronize_the_scan() {
        // `r#"a"b"#` holds a quote that would open a string for a scanner
        // that did not recognize raw strings, swallowing both violations.
        let src = "fn f() {\n    let re = r#\"a\"b\"#;\n    Command::new(r#\"git\"#);\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![3, 4]);
    }

    #[test]
    fn a_multi_line_string_does_not_desynchronize_the_scan() {
        let src = "fn f() {\n    let s = \"one\ntwo\";\n    Command::new(\"git\");\n}\n";
        assert_eq!(production_violations(src), vec![4]);
    }

    /// The live checkout, which is what the rule is actually for. A rule whose
    /// only coverage is synthetic strings states nothing about this tree.
    #[test]
    fn the_checkout_is_clean() {
        let root = repo_root();
        let found = run(&root);
        assert!(
            found.is_empty(),
            "the production sources must name no `git` program literal outside \
             {NEUTRALIZER}:\n{}",
            found.join("\n")
        );
    }

    /// …and that it is clean is not because it scanned nothing. Measured
    /// rather than assumed: the rule must have a boundary to find in the
    /// modules issue #1181 was about, and the files must be there to read.
    #[test]
    fn the_checkout_scan_is_not_vacuous() {
        let root = repo_root();
        for named in [
            "src/git_env.rs",
            "src/dispatch.rs",
            "src/issue_dispatch_run.rs",
            "src/worktree_reclaim.rs",
            "src/worktree_owner.rs",
        ] {
            assert!(
                root.join(named).is_file(),
                "{named} is gone — this rule's subject moved and the rule did not"
            );
        }
        // The exemption is load-bearing: without it the neutralizer itself is
        // the first violation, so a scan that reports nothing for `git_env.rs`
        // is a scan that read it.
        let neutralizer =
            std::fs::read_to_string(root.join(NEUTRALIZER)).expect("read the neutralizer");
        assert!(
            !production_violations(&neutralizer).is_empty(),
            "{NEUTRALIZER} must itself name the program, or the exemption is \
             covering nothing and the scanner is not finding literals at all"
        );
    }

    fn repo_root() -> PathBuf {
        // `xtask/linkage-check/` -> repository root.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("the crate is two levels below the repository root")
            .to_path_buf()
    }
}
