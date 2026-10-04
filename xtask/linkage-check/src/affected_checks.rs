//! `cargo xtask affected-checks` (issue #1575): which of CLAUDE.md rule 2's and
//! rule 5's gates a change actually needs.
//!
//! Rule 2 asks for `cargo fmt --check` and the e2e-featured clippy command
//! before a commit, and rule 5 for `cargo test-fast` per task. For a change to
//! Rust or to anything that feeds a build, those stay mandatory and this helper
//! says so. For a change that touches only text — a PRD, `CLAUDE.md`, a skill,
//! a workflow, a docs page — rustfmt and clippy have nothing to check, and most
//! of the fast tier never opens the file. But "text needs no tests" is wrong
//! here: a handful of fast-tier tests read text from the real checkout, and
//! `build.rs` compiles the published docs into the binary. So a text path maps
//! to the tests that read it rather than to nothing.
//!
//! **The mapping fails safe.** A path is narrowed only when it matches one of
//! [`TEXT_CLASSES`] and is not Rust or a build input; every other path —
//! including one nobody thought about — selects the full gates.
//!
//! **Every narrowed plan runs every xtask package's tests**
//! ([`XTASK_TESTS`]), not a per-class subset. Those crates are where most
//! real-tree readers live (`pin_lockstep`, `release_workflow_wiring`,
//! `skill_frontmatter`, `verify_pr_stream`, `contract_breaks`, the `xtask/site`
//! tests) and one of them, `xtask/site`'s redirect test, reads every tracked
//! file through `git ls-files`, so no text path is read by none of them. They
//! do not build the root package, which is where the time goes, so running all
//! of them costs little and leaves nothing in those crates to enumerate.
//!
//! What the per-class lists DO have to get right is the **root package** and
//! the **desktop crate**, which a narrowed plan builds only partly or not at
//! all. That is what
//! `every_checkout_read_in_the_root_package_and_the_desktop_crate_is_covered`
//! pins: it scans those two crates for string literals that name a checkout
//! path in a narrowed class and fails when the class's checks do not run the
//! test that reads it.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, ExitCode};

/// CLAUDE.md rule 2's two commands and rule 5's per-task tier, verbatim, as
/// argument vectors so `--run` needs no shell.
pub(crate) const FULL_GATES: &[&[&str]] = &[
    &["cargo", "fmt", "--check"],
    &[
        "cargo",
        "clippy",
        "--workspace",
        "--all-targets",
        "--features",
        "e2e,e2e-live",
        "--",
        "-D",
        "warnings",
    ],
    &["cargo", "test-fast"],
];

/// Every xtask package's tests, and no root package or desktop crate.
///
/// `--workspace --exclude` rather than a `-p` list, so a new `xtask/*` member
/// is included without anyone editing this line;
/// `the_workspace_has_no_member_this_helper_does_not_account_for` fails if a
/// member that is not an xtask appears.
pub(crate) const XTASK_TESTS: &[&str] = &[
    "cargo",
    "nextest",
    "run",
    "--workspace",
    "--exclude",
    "dot-agent-deck",
    "--exclude",
    "dot-agent-deck-desktop",
];

/// The root package's name, as `-p` takes it.
const ROOT_PACKAGE: &str = "dot-agent-deck";

/// How a [`TextClass`] matches a repo-relative, `/`-separated path.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Pattern {
    /// Everything under a directory; the prefix ends in `/`.
    Under(&'static str),
    /// One file.
    File(&'static str),
}

impl Pattern {
    fn matches(self, path: &str) -> bool {
        match self {
            Pattern::Under(prefix) => path.starts_with(prefix),
            Pattern::File(file) => path == file,
        }
    }
}

/// A root-package integration test (`tests/<name>.rs`) a class runs, with the
/// cargo features its `#![cfg(...)]` needs.
#[derive(Debug)]
pub(crate) struct RootTest {
    pub(crate) name: &'static str,
    pub(crate) features: &'static [&'static str],
}

/// A set of text paths, and the root-package tests that read them on top of
/// [`XTASK_TESTS`], which every narrowed plan runs.
#[derive(Debug)]
pub(crate) struct TextClass {
    pub(crate) name: &'static str,
    /// What reads these paths, printed beside each one.
    pub(crate) read_by: &'static str,
    pub(crate) patterns: &'static [Pattern],
    pub(crate) root_tests: &'static [RootTest],
    /// Root-package lib modules (`embedded_docs` for `src/embedded_docs.rs`)
    /// whose unit tests read these paths.
    pub(crate) root_lib_modules: &'static [&'static str],
}

/// The text paths a change can touch without selecting the full gates.
///
/// Anything not listed here selects them. Adding a class narrows the gates for
/// those paths, so it needs the same evidence as these: what in the root
/// package and the desktop crate reads them, which the guard test checks.
pub(crate) const TEXT_CLASSES: &[TextClass] = &[
    TextClass {
        name: "docs",
        read_by: "embedded into the binary by build.rs; read by the xtask/site tests, \
                  src/embedded_docs.rs's unit tests, tests/embedded_docs_boundary.rs and \
                  tests/e2e_cli_docs.rs",
        patterns: &[Pattern::Under("docs/")],
        root_tests: &[
            RootTest {
                name: "embedded_docs_boundary",
                features: &[],
            },
            RootTest {
                name: "e2e_cli_docs",
                features: &["e2e"],
            },
        ],
        root_lib_modules: &["embedded_docs"],
    },
    TextClass {
        name: "skills",
        read_by: "read by xtask/linkage-check (skill_frontmatter, verify_pr_stream, \
                  tag_release_cleanup)",
        patterns: &[Pattern::Under(".claude/skills/")],
        root_tests: &[],
        root_lib_modules: &[],
    },
    TextClass {
        name: "changelog fragments",
        read_by: "read by xtask/linkage-check's contract_breaks",
        patterns: &[Pattern::Under("changelog.d/")],
        root_tests: &[],
        root_lib_modules: &[],
    },
    TextClass {
        name: "GitHub configuration",
        read_by: "read by xtask/linkage-check (pin_lockstep, release_workflow_wiring, \
                  gh_aw_lock_consistency, pr_review_verdict, the issue-labeler tests)",
        patterns: &[Pattern::Under(".github/")],
        root_tests: &[],
        root_lib_modules: &[],
    },
    TextClass {
        name: "devbox manifest",
        read_by: "read by xtask/linkage-check's pin_lockstep and tests/dogfood_config.rs",
        patterns: &[Pattern::File("devbox.json")],
        root_tests: &[RootTest {
            name: "dogfood_config",
            features: &[],
        }],
        root_lib_modules: &[],
    },
    TextClass {
        name: "Renovate configuration",
        read_by: "read by xtask/linkage-check's renovate_lock_file_maintenance and \
                  release_workflow_wiring",
        patterns: &[Pattern::File("renovate.json")],
        root_tests: &[],
        root_lib_modules: &[],
    },
    TextClass {
        name: "prose",
        read_by: "read by xtask/site's redirect test, which reads every tracked file",
        patterns: &[
            Pattern::File("AGENTS.md"),
            Pattern::File("CHANGELOG.md"),
            Pattern::File("CLAUDE.md"),
            Pattern::File("CONTRIBUTING.md"),
            Pattern::File("MAINTAINERS.md"),
            Pattern::File("README.md"),
            Pattern::Under("prds/"),
        ],
        root_tests: &[],
        root_lib_modules: &[],
    },
];

/// Why a path selects the full gates, or which text class it is in.
#[derive(Debug)]
pub(crate) enum Verdict {
    Full(&'static str),
    Text(&'static TextClass),
}

/// Files that feed a build wherever they sit. Checked before [`TEXT_CLASSES`],
/// so a `Cargo.toml` or `build.rs` inside a text directory still selects the
/// full gates.
const BUILD_INPUT_FILE_NAMES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
    "rust-toolchain",
    "rust-toolchain.toml",
    "rustfmt.toml",
    ".rustfmt.toml",
    "clippy.toml",
    ".clippy.toml",
];

pub(crate) fn classify(path: &str) -> Verdict {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    if path.ends_with(".rs") {
        return Verdict::Full("Rust source");
    }
    if BUILD_INPUT_FILE_NAMES.contains(&file_name) || path.starts_with(".cargo/") {
        return Verdict::Full("build input");
    }
    for class in TEXT_CLASSES {
        if class.patterns.iter().any(|p| p.matches(path)) {
            return Verdict::Text(class);
        }
    }
    Verdict::Full("in no text mapping, so the full gates apply (fail safe)")
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// The commands a set of changed paths needs, in the order to run them.
///
/// Empty when nothing changed. The full gates when any path is not text.
/// Otherwise [`XTASK_TESTS`] plus, when a class names any, ONE root-package
/// command carrying every named test and lib module with the union of their
/// features, so two classes never build the root package twice.
pub(crate) fn plan(paths: &[String]) -> Vec<Vec<String>> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut tests = BTreeSet::new();
    let mut modules = BTreeSet::new();
    let mut features = BTreeSet::new();
    for path in paths {
        match classify(path) {
            Verdict::Full(_) => return FULL_GATES.iter().map(|c| argv(c)).collect(),
            Verdict::Text(class) => {
                for test in class.root_tests {
                    tests.insert(test.name);
                    features.extend(test.features.iter().copied());
                }
                modules.extend(class.root_lib_modules.iter().copied());
            }
        }
    }
    let mut out = vec![argv(XTASK_TESTS)];
    if tests.is_empty() && modules.is_empty() {
        return out;
    }
    let mut root = argv(&["cargo", "nextest", "run", "-p", ROOT_PACKAGE]);
    if !features.is_empty() {
        root.push("--features".to_string());
        root.push(features.into_iter().collect::<Vec<_>>().join(","));
    }
    if !modules.is_empty() {
        root.push("--lib".to_string());
    }
    for test in &tests {
        root.push("--test".to_string());
        root.push(test.to_string());
    }
    if !modules.is_empty() {
        // `--lib` runs every unit test in the crate unless filtered; keep the
        // named modules' and, when there are any, the integration tests'.
        let mut filter: Vec<String> = modules.iter().map(|m| format!("test(/^{m}::/)")).collect();
        if !tests.is_empty() {
            filter.insert(0, "kind(test)".to_string());
        }
        root.push("-E".to_string());
        root.push(filter.join(" | "));
    }
    out.push(root);
    out
}

/// One argument vector as a shell line: an argument with anything outside a
/// conservative safe set is single-quoted.
fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|arg| {
            let safe = !arg.is_empty()
                && arg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-./,=:+@".contains(c));
            if safe {
                arg.clone()
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`plan`] as shell lines, the form the report prints.
pub(crate) fn commands(paths: &[String]) -> Vec<String> {
    plan(paths).iter().map(|c| shell_line(c)).collect()
}

/// Text that goes inside a `#` comment of the report, with every control
/// character escaped. A file name may contain a newline, and printed raw it
/// would end the comment and put the rest of the name on a line of its own —
/// a command, to anyone who runs the report as a script.
fn comment_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() {
                c.escape_default().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// The report: every changed path with its verdict as a `#` comment, then the
/// commands, one per line — so the output is itself a shell script.
pub(crate) fn render(origin: &str, paths: &[String]) -> String {
    let origin = comment_text(origin);
    let mut out = String::new();
    if paths.is_empty() {
        out.push_str(&format!(
            "# affected-checks: no changes {origin}; nothing to run\n"
        ));
        return out;
    }
    out.push_str(&format!(
        "# affected-checks: {} changed path(s) {origin}\n",
        paths.len()
    ));
    let shown: Vec<String> = paths.iter().map(|p| comment_text(p)).collect();
    let width = shown.iter().map(|p| p.chars().count()).max().unwrap_or(0);
    let mut full = false;
    for (path, shown) in paths.iter().zip(&shown) {
        let why = match classify(path) {
            Verdict::Full(why) => {
                full = true;
                format!("full gates: {why}")
            }
            Verdict::Text(class) => format!("{}: {}", class.name, class.read_by),
        };
        out.push_str(&format!("#   {shown:<width$}  {why}\n"));
    }
    if full {
        out.push_str(
            "# Code or a build input changed: the full gates apply (CLAUDE.md rules 2 and 5).\n",
        );
    } else {
        out.push_str(
            "# Text only: rustfmt and clippy have no Rust to check, and these replace \
             `cargo test-fast` (CLAUDE.md rules 2 and 5).\n",
        );
    }
    out.push_str("# Plus the tests covering what you touched, any tier (CLAUDE.md rule 5).\n");
    for command in commands(paths) {
        out.push_str(&command);
        out.push('\n');
    }
    out
}

/// A `git ... -z` path list, as repo-relative `/`-separated strings.
pub(crate) fn parse_nul_list(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect()
}

/// A path as given on the command line, in the form [`classify`] reads.
///
/// Only a leading `./` is dropped. A backslash is left alone: on Unix it is a
/// legal file-name character, so turning it into `/` could move an unmapped
/// file into a mapped directory, and a Windows-style path left as typed maps to
/// nothing and so gets the full gates — the safe side.
fn normalize_arg(path: &str) -> String {
    let mut path = path;
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    path.to_string()
}

/// Git's repository-location variables. Each outranks `-C`, and a pre-commit
/// hook, `rebase --exec` or `bisect run` can set them, so left in place they
/// could make the diff describe another repository and narrow this one's gates.
/// The same list `repo_state`'s fixtures clear (issue #834).
pub(crate) const GIT_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
];

/// `git -C <root> <args>`, with [`GIT_LOCATION_VARS`] removed so `root` is the
/// repository it reads.
fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(args);
    for var in GIT_LOCATION_VARS {
        command.env_remove(var);
    }
    command
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = git_command(root, args)
        .output()
        .map_err(|e| format!("invoke git {}: {e}", args.join(" ")))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Every path that differs between the merge-base with `base` and the working
/// tree — committed, staged, unstaged, and untracked-but-not-ignored.
/// `--no-renames` so a rename reports its old path too: deleting a `.rs` file
/// is a code change even when its content reappears under `docs/`.
fn changed_paths(root: &Path, base: &str) -> Result<(String, Vec<String>), String> {
    let merge_base = git(root, &["merge-base", "HEAD", base])?;
    let merge_base = String::from_utf8_lossy(&merge_base).trim().to_string();
    if merge_base.is_empty() {
        return Err(format!("git merge-base HEAD {base} returned nothing"));
    }
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(parse_nul_list(&git(
        root,
        &["diff", "--name-only", "--no-renames", "-z", &merge_base],
    )?));
    paths.extend(parse_nul_list(&git(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?));
    Ok((merge_base, paths.into_iter().collect()))
}

const USAGE: &str = "usage: cargo xtask affected-checks [--run] [--base <ref>] [PATH...]";

/// Runs each command in order from `root`, stopping at the first that fails.
/// No shell is involved: every command is an argument vector.
fn execute(root: &Path, plan: &[Vec<String>]) -> ExitCode {
    for command in plan {
        eprintln!("affected-checks: running {}", shell_line(command));
        match Command::new(&command[0])
            .args(&command[1..])
            .current_dir(root)
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => {
                eprintln!(
                    "affected-checks: `{}` failed ({status})",
                    shell_line(command)
                );
                return ExitCode::FAILURE;
            }
            Err(e) => {
                eprintln!(
                    "affected-checks: cannot start `{}`: {e}",
                    shell_line(command)
                );
                return ExitCode::FAILURE;
            }
        }
    }
    eprintln!("affected-checks: all {} command(s) passed", plan.len());
    ExitCode::SUCCESS
}

pub(crate) fn run(root: &Path, args: &[String]) -> ExitCode {
    let mut base = "origin/main".to_string();
    let mut explicit: Vec<String> = Vec::new();
    let mut execute_plan = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                println!();
                println!(
                    "Prints the checks a change needs (issue #1575, CLAUDE.md rules 2 and 5)."
                );
                println!("With no PATH, the change is everything between the merge-base with");
                println!("<ref> (default origin/main) and the working tree, untracked files");
                println!("included; `--base HEAD` narrows it to what is not yet committed.");
                println!("Rust or a build input selects the full gates, and so does any path");
                println!("no text mapping covers. `--run` runs the checks after printing them,");
                println!("stopping at the first failure; without it nothing is run.");
                return ExitCode::SUCCESS;
            }
            "--run" => execute_plan = true,
            "--base" => match iter.next() {
                Some(value) => base = value.clone(),
                None => {
                    eprintln!("xtask affected-checks: --base needs a value");
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "--" => explicit.extend(iter.by_ref().map(|p| normalize_arg(p))),
            other if other.starts_with('-') => {
                eprintln!("xtask affected-checks: unknown argument {other:?}");
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
            path => explicit.push(normalize_arg(path)),
        }
    }

    let (origin, paths) = if explicit.is_empty() {
        match changed_paths(root, &base) {
            Ok((merge_base, paths)) => {
                let short = merge_base.get(..12).unwrap_or(&merge_base).to_string();
                (format!("against {base} (merge-base {short})"), paths)
            }
            Err(e) => {
                // Fail safe: a change that cannot be computed gets the full
                // gates, and the exit status says something went wrong.
                eprintln!("xtask affected-checks: {e}");
                println!(
                    "# affected-checks: could not compute the change, so the full gates apply"
                );
                for command in FULL_GATES {
                    println!("{}", shell_line(&argv(command)));
                }
                return ExitCode::FAILURE;
            }
        }
    } else {
        explicit.sort();
        explicit.dedup();
        ("as given".to_string(), explicit)
    };
    print!("{}", render(&origin, &paths));
    if execute_plan {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        return execute(root, &plan(&paths));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::str::FromStr;

    use proc_macro2::{Delimiter, TokenStream, TokenTree};

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| p.to_string()).collect()
    }

    fn full() -> Vec<String> {
        FULL_GATES.iter().map(|c| shell_line(&argv(c))).collect()
    }

    fn xtask() -> String {
        shell_line(&argv(XTASK_TESTS))
    }

    // --- the classification the issue asks for ---------------------------

    #[test]
    fn a_rust_change_runs_the_full_gates() {
        assert_eq!(commands(&paths(&["src/ui.rs"])), full());
        assert_eq!(
            commands(&paths(&["xtask/linkage-check/src/main.rs"])),
            full()
        );
    }

    #[test]
    fn a_build_input_runs_the_full_gates() {
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            ".cargo/config.toml",
            "xtask/site/Cargo.toml",
            "desktop/src-tauri/build.rs",
        ] {
            assert_eq!(commands(&paths(&[path])), full(), "{path}");
        }
    }

    #[test]
    fn rust_or_a_build_input_inside_a_text_directory_still_runs_the_full_gates() {
        for path in [
            "docs/example.rs",
            ".claude/skills/x/Cargo.toml",
            "prds/build.rs",
        ] {
            assert!(
                matches!(classify(path), Verdict::Full(_)),
                "{path} must not be narrowed"
            );
        }
    }

    #[test]
    fn a_docs_change_runs_the_xtask_site_and_embedded_docs_tests() {
        for path in [
            "docs/published.toml",
            "docs/quick-start.md",
            "docs/develop/e2e-lanes.md",
        ] {
            assert_eq!(
                commands(&paths(&[path])),
                vec![
                    xtask(),
                    "cargo nextest run -p dot-agent-deck --features e2e --lib \
                     --test e2e_cli_docs --test embedded_docs_boundary \
                     -E 'kind(test) | test(/^embedded_docs::/)'"
                        .to_string(),
                ],
                "{path}"
            );
        }
    }

    #[test]
    fn a_skill_change_runs_linkage_check_with_verify_pr_stream() {
        // `XTASK_TESTS` is `xtask/linkage-check`'s whole suite, which is where
        // `skill_frontmatter`, `verify_pr_stream` and `tag_release_cleanup` live.
        for path in [
            ".claude/skills/pr-create/SKILL.md",
            ".claude/skills/verify-pr/scan.sh",
        ] {
            assert_eq!(commands(&paths(&[path])), vec![xtask()]);
        }
    }

    #[test]
    fn a_workflow_change_runs_the_xtask_tests() {
        assert_eq!(
            commands(&paths(&[".github/workflows/ci.yml"])),
            vec![xtask()]
        );
        assert_eq!(commands(&paths(&["renovate.json"])), vec![xtask()]);
        assert_eq!(
            commands(&paths(&["devbox.json"])),
            vec![
                xtask(),
                "cargo nextest run -p dot-agent-deck --test dogfood_config".to_string(),
            ]
        );
    }

    #[test]
    fn a_prd_or_claude_md_change_runs_the_xtask_tests() {
        assert_eq!(
            commands(&paths(&[
                "prds/1575-x.md",
                "CLAUDE.md",
                "changelog.d/1.misc.md"
            ])),
            vec![xtask()]
        );
    }

    #[test]
    fn an_unknown_path_runs_the_full_gates() {
        for path in [
            "site/landing/index.html",
            "scripts/link-gate.sh",
            "tests/CATALOG.md",
            ".claude/settings.json",
            "docs",
            "something-new.txt",
        ] {
            assert_eq!(commands(&paths(&[path])), full(), "{path}");
        }
    }

    #[test]
    fn one_full_path_among_text_paths_runs_the_full_gates() {
        assert_eq!(
            commands(&paths(&["CLAUDE.md", "docs/x.md", "src/main.rs"])),
            full()
        );
    }

    #[test]
    fn classes_that_name_root_tests_share_one_root_command() {
        assert_eq!(
            commands(&paths(&["devbox.json", "docs/x.md"])),
            vec![
                xtask(),
                "cargo nextest run -p dot-agent-deck --features e2e --lib \
                 --test dogfood_config --test e2e_cli_docs --test embedded_docs_boundary \
                 -E 'kind(test) | test(/^embedded_docs::/)'"
                    .to_string(),
            ]
        );
    }

    #[test]
    fn no_change_runs_nothing() {
        assert!(commands(&[]).is_empty());
        assert!(render("against origin/main", &[]).contains("nothing to run"));
    }

    #[test]
    fn the_report_is_a_shell_script_whose_commands_are_the_plan() {
        let changed = paths(&["CLAUDE.md", "docs/x.md"]);
        let report = render("as given", &changed);
        let lines: Vec<&str> = report.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(lines, commands(&changed));
        assert!(report.contains("#   CLAUDE.md"));
    }

    #[test]
    fn git_path_lists_and_arguments_are_read_as_repo_relative_paths() {
        assert_eq!(
            parse_nul_list(b"docs/a b.md\0src/x.rs\0"),
            paths(&["docs/a b.md", "src/x.rs"])
        );
        assert!(parse_nul_list(b"").is_empty());
        assert_eq!(normalize_arg("./docs/x.md"), "docs/x.md");
        // A backslash is a file-name character on Unix, so it is kept, and the
        // unmapped root-level file it names gets the full gates.
        assert_eq!(normalize_arg("docs\\x.md"), "docs\\x.md");
        assert!(matches!(
            classify(&normalize_arg("docs\\x.md")),
            Verdict::Full(_)
        ));
    }

    #[test]
    fn the_full_gates_print_as_the_commands_claude_md_names() {
        assert_eq!(
            full(),
            vec![
                "cargo fmt --check",
                "cargo clippy --workspace --all-targets --features e2e,e2e-live -- -D warnings",
                "cargo test-fast",
            ]
        );
        assert_eq!(
            xtask(),
            "cargo nextest run --workspace --exclude dot-agent-deck --exclude dot-agent-deck-desktop"
        );
    }

    #[test]
    fn a_file_name_cannot_put_a_command_line_into_the_report() {
        // A changed path, and a `--base` value, each carrying a newline and a
        // command: printed raw, `touch pwned` would be a line of the script.
        let changed = paths(&["docs/x.md\ntouch pwned\n#", "CLAUDE.md"]);
        let report = render("against x\rtouch pwned2\n", &changed);
        let lines: Vec<&str> = report
            .split(['\n', '\r'])
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(lines, commands(&changed), "{report}");
        assert!(report.contains("docs/x.md\\ntouch pwned\\n#"), "{report}");
    }

    #[test]
    fn shell_lines_quote_what_a_shell_would_split_or_expand() {
        assert_eq!(
            shell_line(&argv(&["a", "-E", "kind(test) | x", "it's", ""])),
            "a -E 'kind(test) | x' 'it'\\''s' ''"
        );
    }

    #[test]
    fn git_runs_without_the_ambient_repository_location_variables() {
        let command = git_command(Path::new("/repo"), &["diff"]);
        let removed: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for var in GIT_LOCATION_VARS {
            assert!(removed.iter().any(|r| r == var), "{var} is not removed");
        }
        let args: Vec<String> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-C", "/repo", "diff"]);
    }

    // --- the guard: the mapping cannot silently go stale -----------------

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    /// The crates a narrowed plan does not run in full, as (manifest dir,
    /// package name). Every other workspace member is an xtask, which
    /// [`XTASK_TESTS`] runs whole.
    const PARTLY_RUN_CRATES: &[(&str, &str)] = &[
        ("", ROOT_PACKAGE),
        ("desktop/src-tauri", "dot-agent-deck-desktop"),
    ];

    /// A literal in a scanned crate that names a narrowed path but is not a
    /// read of THIS checkout, with the reason. Each entry must still match
    /// something, so a stale one fails rather than masking a future read.
    const NOT_CHECKOUT_READS: &[(&str, &str, &str)] = &[
        (
            "tests/assemble_changelog.rs",
            "changelog.d",
            "a scratch root the test builds; the real changelog.d is never read",
        ),
        (
            "tests/assemble_changelog.rs",
            "CHANGELOG.md",
            "a scratch root the test builds; the real CHANGELOG.md is never read",
        ),
        (
            "tests/common/mod.rs",
            ".claude",
            "the host's or a test HOME's ~/.claude, not the checkout's",
        ),
        (
            "tests/common/harness_unit_tests.rs",
            ".claude",
            "a test HOME's ~/.claude, not the checkout's",
        ),
        (
            "tests/harness_isolation.rs",
            ".claude",
            "a test HOME's ~/.claude, not the checkout's",
        ),
    ];

    /// What a scanned file's literal reads the checkout on behalf of.
    #[derive(Debug, PartialEq)]
    enum Reader {
        /// The root package's `build.rs`: every root-package command builds it.
        RootBuild,
        /// `tests/<name>.rs` in the root package.
        RootTest(String),
        /// A root-package lib module's unit tests, by module path.
        RootLib(String),
        /// Anything else in a partly-run crate — `src/` unit tests, a shared
        /// `tests/` module, the desktop crate — which no narrowed plan runs.
        NotRun(String),
    }

    fn reader_of(file: &str) -> Reader {
        if file == "build.rs" {
            return Reader::RootBuild;
        }
        if let Some(name) = file
            .strip_prefix("tests/")
            .and_then(|f| f.strip_suffix(".rs"))
            .filter(|n| !n.contains('/'))
        {
            return Reader::RootTest(name.to_string());
        }
        if let Some(module) = file
            .strip_prefix("src/")
            .and_then(|f| f.strip_suffix(".rs"))
            .map(|m| m.strip_suffix("/mod").unwrap_or(m))
            .filter(|m| *m != "lib" && *m != "main")
        {
            return Reader::RootLib(module.replace('/', "::"));
        }
        Reader::NotRun(file.to_string())
    }

    /// Whether `class`'s checks run what `reader` needs.
    fn covers(class: &TextClass, reader: &Reader) -> bool {
        match reader {
            Reader::RootBuild => !class.root_tests.is_empty(),
            Reader::RootTest(name) => class.root_tests.iter().any(|t| t.name == name),
            Reader::RootLib(module) => class.root_lib_modules.contains(&module.as_str()),
            Reader::NotRun(_) => false,
        }
    }

    /// A file reads the real checkout when it reaches it at all: through the
    /// manifest dir, a compile-time include, or git's file list. Literals in
    /// files without any of these cannot be checkout paths.
    fn reaches_the_checkout(source: &str) -> bool {
        [
            "CARGO_MANIFEST_DIR",
            "include_str!",
            "include_bytes!",
            "ls-files",
        ]
        .iter()
        .any(|anchor| source.contains(anchor))
    }

    /// Every string literal in `source`, doc comments excluded.
    fn string_literals(source: &str) -> Vec<String> {
        fn walk(stream: TokenStream, out: &mut Vec<String>) {
            for tree in stream {
                match tree {
                    TokenTree::Group(group) => {
                        let is_doc = group.delimiter() == Delimiter::Bracket
                            && matches!(
                                group.stream().into_iter().next(),
                                Some(TokenTree::Ident(ident)) if ident == "doc"
                            );
                        if !is_doc {
                            walk(group.stream(), out);
                        }
                    }
                    TokenTree::Literal(literal) => {
                        let text = literal.to_string();
                        // A string with no escape is the text between its
                        // quotes; syn is needed only for escapes and raw strings.
                        let plain = text
                            .strip_prefix('"')
                            .and_then(|t| t.strip_suffix('"'))
                            .filter(|t| !t.contains('\\'));
                        if let Some(value) = plain {
                            out.push(value.to_string());
                        } else if (text.starts_with('"') || text.starts_with('r'))
                            && let Ok(lit) = syn::parse_str::<syn::LitStr>(&text)
                        {
                            out.push(lit.value());
                        }
                    }
                    _ => {}
                }
            }
        }
        let stream = TokenStream::from_str(source).expect("a workspace source file tokenizes");
        let mut out = Vec::new();
        walk(stream, &mut out);
        out
    }

    /// The repo-relative path a literal in `file` names, or `None` when it
    /// cannot be one.
    ///
    /// `./` and `../` resolve against the file's directory (an `include_str!`);
    /// anything else against the crate's manifest dir (a
    /// `CARGO_MANIFEST_DIR` join, or a `concat!` that starts with `/`). Either
    /// reading may be the wrong one for a given literal; both over-report
    /// rather than under-report, which is the safe direction for a guard.
    ///
    /// A literal that resolves to the crate root itself is not a path — `"/"`
    /// and `"."` are separators far more often than reads — except `ls-files`,
    /// which lists the whole tree and so stands for every path.
    fn checkout_path(file: &str, crate_dir: &str, literal: &str) -> Option<String> {
        if literal == "ls-files" {
            return Some(String::new());
        }
        if literal.is_empty() || literal.chars().any(char::is_whitespace) {
            return None;
        }
        let (base, rest) = if literal.starts_with("./") || literal.starts_with("../") {
            (file.rsplit_once('/').map_or("", |(dir, _)| dir), literal)
        } else {
            (crate_dir, literal.trim_start_matches('/'))
        };
        let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
        for part in rest.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop()?;
                }
                part => parts.push(part),
            }
        }
        (!parts.is_empty()).then(|| parts.join("/"))
    }

    /// Whether `class` narrows `path` or anything under it — so a literal
    /// `"docs"` joined onto the manifest dir counts as reading every docs page.
    fn overlaps(class: &TextClass, path: &str) -> bool {
        let dir = format!("{path}/");
        class.patterns.iter().any(|pattern| match *pattern {
            Pattern::Under(prefix) => {
                path.is_empty() || path.starts_with(prefix) || prefix.starts_with(&dir)
            }
            Pattern::File(file) => path.is_empty() || path == file || file.starts_with(&dir),
        })
    }

    /// Every narrowed-path read in `sources` (repo-relative path → contents)
    /// that its class does not run, minus the allowlist; and the allowlist
    /// entries nothing matched.
    fn uncovered_reads(
        sources: &BTreeMap<String, String>,
        allow: &[(&str, &str, &str)],
    ) -> (Vec<String>, Vec<String>) {
        let mut findings = Vec::new();
        let mut used = BTreeSet::new();
        for (file, source) in sources {
            if !reaches_the_checkout(source) {
                continue;
            }
            let crate_dir = PARTLY_RUN_CRATES
                .iter()
                .map(|(dir, _)| *dir)
                .filter(|dir| dir.is_empty() || file.starts_with(&format!("{dir}/")))
                .max_by_key(|dir| dir.len())
                .unwrap_or("");
            let reader = reader_of(file);
            let mut seen = BTreeSet::new();
            for literal in string_literals(source) {
                let Some(path) = checkout_path(file, crate_dir, &literal) else {
                    continue;
                };
                for class in TEXT_CLASSES {
                    if !overlaps(class, &path) || covers(class, &reader) {
                        continue;
                    }
                    if let Some(entry) = allow
                        .iter()
                        .position(|(f, l, _)| *f == file && *l == literal)
                    {
                        used.insert(entry);
                        continue;
                    }
                    if seen.insert((literal.clone(), class.name)) {
                        findings.push(format!(
                            "{file}: literal {literal:?} names `{path}`, which the \"{}\" class \
                             narrows, but that class does not run {reader:?}",
                            class.name
                        ));
                    }
                }
            }
        }
        let stale = allow
            .iter()
            .enumerate()
            .filter(|(i, _)| !used.contains(i))
            .map(|(_, (f, l, _))| format!("{f}: {l:?}"))
            .collect();
        (findings, stale)
    }

    fn rust_sources_under(root: &Path, rel: &str, out: &mut BTreeMap<String, String>) {
        let path = root.join(rel);
        if path.is_file() {
            if rel.ends_with(".rs") {
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                out.insert(rel.to_string(), source);
            }
            return;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            return;
        };
        for entry in entries {
            let entry = entry.expect("readable directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "target" || name.starts_with('.') {
                continue;
            }
            rust_sources_under(root, &format!("{rel}/{name}"), out);
        }
    }

    #[test]
    fn every_checkout_read_in_the_root_package_and_the_desktop_crate_is_covered() {
        let root = repo_root();
        let mut sources = BTreeMap::new();
        for (dir, _) in PARTLY_RUN_CRATES {
            let prefix = if dir.is_empty() {
                String::new()
            } else {
                format!("{dir}/")
            };
            for rel in ["build.rs", "src", "tests"] {
                rust_sources_under(&root, &format!("{prefix}{rel}"), &mut sources);
            }
        }
        assert!(
            sources.contains_key("build.rs")
                && sources.contains_key("tests/embedded_docs_boundary.rs"),
            "the scan found the root package's sources"
        );
        let (findings, stale) = uncovered_reads(&sources, NOT_CHECKOUT_READS);
        assert!(
            findings.is_empty(),
            "a test reads a narrowed path its class does not run — add the test to the \
             class's `root_tests`, take the path out of the class, or (if the literal is \
             not a checkout read) allowlist it in NOT_CHECKOUT_READS with the reason:\n{}",
            findings.join("\n")
        );
        assert!(
            stale.is_empty(),
            "NOT_CHECKOUT_READS entries that no longer match anything — remove them:\n{}",
            stale.join("\n")
        );
    }

    #[test]
    fn the_guard_catches_a_planted_read_of_each_shape() {
        let planted: BTreeMap<String, String> = [
            (
                "tests/reads_changelog.rs",
                r#"fn f() { std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("changelog.d"); }"#,
            ),
            (
                "src/reads_claude_md.rs",
                r#"const X: &str = include_str!("../CLAUDE.md");"#,
            ),
            (
                "src/platform/reads_docs.rs",
                r#"fn f() { concat!(env!("CARGO_MANIFEST_DIR"), "/docs/published.toml"); }"#,
            ),
            (
                "tests/common/reads_skills.rs",
                r#"fn f() { concat!(env!("CARGO_MANIFEST_DIR"), "/.claude/skills/x/SKILL.md"); }"#,
            ),
            (
                "desktop/src-tauri/src/reads_docs.rs",
                r#"const X: &str = include_str!("../../../docs/published.toml");"#,
            ),
            (
                "tests/lists_the_tree.rs",
                r#"fn f() { git(env!("CARGO_MANIFEST_DIR"), &["ls-files", "-z", "."]); }"#,
            ),
        ]
        .into_iter()
        .map(|(f, s)| (f.to_string(), s.to_string()))
        .collect();
        let (findings, _) = uncovered_reads(&planted, &[]);
        for file in planted.keys() {
            assert!(
                findings.iter().any(|f| f.starts_with(&format!("{file}:"))),
                "{file} was not caught: {findings:#?}"
            );
        }
    }

    #[test]
    fn the_guard_accepts_covered_reads_and_ignores_what_is_not_a_checkout_path() {
        let fine: BTreeMap<String, String> = [
            // Covered: the docs class runs this test and builds the root package.
            (
                "tests/embedded_docs_boundary.rs",
                r#"fn f() { std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs"); }"#,
            ),
            (
                "build.rs",
                r#"fn f() { std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR")).join("docs"); }"#,
            ),
            (
                "src/embedded_docs.rs",
                r#"fn f() { concat!(env!("CARGO_MANIFEST_DIR"), "/docs/published.toml"); }"#,
            ),
            // Separators, not paths.
            (
                "tests/joins.rs",
                r#"fn f() { env!("CARGO_MANIFEST_DIR"); a.split("/"); b.join("."); }"#,
            ),
            // Not a checkout read: the desktop crate's `docs` is its own dir,
            // and a doc comment or a file that never reaches the checkout is
            // not a read at all.
            (
                "desktop/src-tauri/src/a.rs",
                r#"fn f() { std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs"); }"#,
            ),
            (
                "src/b.rs",
                "/// See `CLAUDE.md` and \"docs/x.md\".\nconst X: &str = include_str!(\"b.txt\");",
            ),
            ("src/c.rs", r#"fn f() { home.join(".claude"); }"#),
        ]
        .into_iter()
        .map(|(f, s)| (f.to_string(), s.to_string()))
        .collect();
        let (findings, _) = uncovered_reads(&fine, &[]);
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn every_root_test_a_class_names_exists_and_gets_the_features_it_is_gated_on() {
        let root = repo_root();
        for class in TEXT_CLASSES {
            for test in class.root_tests {
                let path = root.join("tests").join(format!("{}.rs", test.name));
                let source = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                    panic!(
                        "class \"{}\" names tests/{}.rs, which cannot be read: {e}",
                        class.name, test.name
                    )
                });
                let gate = source
                    .lines()
                    .find(|l| l.trim_start().starts_with("#![cfg("))
                    .unwrap_or("");
                let gated_on: BTreeSet<&str> = ["e2e", "e2e-live"]
                    .into_iter()
                    .filter(|f| gate.contains(&format!("feature = \"{f}\"")))
                    .collect();
                let given: BTreeSet<&str> = test.features.iter().copied().collect();
                assert_eq!(
                    given, gated_on,
                    "class \"{}\": tests/{}.rs is gated on {gated_on:?}",
                    class.name, test.name
                );
            }
        }
    }

    #[test]
    fn every_root_lib_module_a_class_names_exists_and_has_unit_tests() {
        let root = repo_root();
        for class in TEXT_CLASSES {
            for module in class.root_lib_modules {
                let rel = module.replace("::", "/");
                let source = [format!("src/{rel}.rs"), format!("src/{rel}/mod.rs")]
                    .iter()
                    .find_map(|f| std::fs::read_to_string(root.join(f)).ok())
                    .unwrap_or_else(|| {
                        panic!(
                            "class \"{}\" names lib module {module}, which has no source",
                            class.name
                        )
                    });
                assert!(
                    source.contains("#[cfg(test)]"),
                    "class \"{}\" names lib module {module}, which has no unit tests",
                    class.name
                );
            }
        }
    }

    #[test]
    fn the_workspace_has_no_member_this_helper_does_not_account_for() {
        let manifest = std::fs::read_to_string(repo_root().join("Cargo.toml"))
            .expect("read the workspace Cargo.toml");
        let document: toml_edit::DocumentMut = manifest.parse().expect("Cargo.toml parses");
        let members: Vec<String> = document["workspace"]["members"]
            .as_array()
            .expect("[workspace] members is an array")
            .iter()
            .map(|m| m.as_str().expect("member is a string").to_string())
            .collect();
        for member in &members {
            let accounted = member.starts_with("xtask/")
                || PARTLY_RUN_CRATES
                    .iter()
                    .any(|(dir, _)| *member == if dir.is_empty() { "." } else { dir });
            assert!(
                accounted,
                "workspace member {member} is neither an xtask (run whole by XTASK_TESTS) \
                 nor scanned by the guard — add it to PARTLY_RUN_CRATES and exclude it \
                 from XTASK_TESTS"
            );
        }
        for (_, package) in PARTLY_RUN_CRATES {
            assert!(
                XTASK_TESTS
                    .windows(2)
                    .any(|pair| pair == ["--exclude", *package]),
                "XTASK_TESTS must exclude {package}"
            );
        }
    }
}
