//! PRD #77 catalog ↔ test linkage check + `xtask` subcommand
//! multiplexer.
//!
//! Invoked as `cargo xtask <subcommand>` (alias in `.cargo/config.toml`).
//! Subcommands:
//!
//! - `linkage-check` (default) — performs the seven checks listed
//!   in Decision 7 + Decision 30:
//!
//!   1. Every catalog ID has at least one `#[spec("...")]` referencing
//!      it OR is on the allowlist (`m2.allowlist`).
//!   2. Every `#[spec("...")]` references a real catalog ID.
//!   3. Catalog IDs match the format regex.
//!   4. Function name carries the `<sub>_<NNN>` prefix (Decision 17).
//!   5. No raw `std::thread::sleep` / `tokio::time::sleep` /
//!      `for _ in 0..N` polling in `tests/e2e_*.rs` bodies (Decision 21).
//!   6. No `#[ignore]` on `#[spec(...)]`-annotated tests (Decision 26).
//!   7. Every `#[spec(...)]` test carries a `/// Scenario:` doc
//!      comment with a body AND `cargo xtask docs --tests` exits 0
//!      against the current source + catalog (Decision 30 / M4.3).
//!      The byte-identity diff against the on-disk `.md` is gone:
//!      `.dot-agent-deck/` is gitignored dev-time state and would
//!      not exist on a fresh clone.
//!   8. No bare `tempfile` constructor — directory (`tempdir()`,
//!      `TempDir::new()`) *or* file (`NamedTempFile::new()`,
//!      `tempfile()`) — anywhere under `tests/`, or in the files on
//!      [`EXTRA_TEMP_COVERED`]. Issue #322. See
//!      [`BARE_TEMPDIR_RULE`].
//!
//!   Checks 1/2/4/6 bind each `#[spec("…")]` to its test function
//!   through the SAME syn walker rule 7 uses
//!   ([`xtask_docs::discover_tests`]) rather than a line regex. Issue
//!   #406: the old regex matched `^\s*fn\s+` only, so an `async fn`
//!   test was invisible and its annotation silently re-bound to the
//!   next plain `fn` in the file — which either blamed an unrelated,
//!   correctly-named function for a prefix mismatch or, when that
//!   function happened to share the prefix, let a wrongly-named test
//!   pass unchecked. A text scan still locates every annotation so
//!   that one syn could NOT bind to a function is reported explicitly
//!   instead of drifting onto its neighbour.
//!
//! - `docs` — invokes the `xtask-docs` binary's logic (paired-`.md`
//!   generator). Forwards remaining args.
//! - `clean-e2e-tmp` — issue #322: reaps stale e2e harness temp dirs left
//!   behind by SIGKILLed test processes. Dry-run unless `--apply`.
//! - `list-tests` — PRD #77 Decision 31: emits a Markdown report of
//!   every `#[spec]` test created or modified in this branch versus
//!   `origin/main`, plus per-catalog-entry prose diffs and any
//!   `m2.allowlist` changes. The orchestrator surfaces this to the
//!   user before delegating release.
//!
//! Exits 0 on success, 1 on any failure with a per-finding summary.

mod clean_tmp;
mod list_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use regex::Regex;

// The Test-Case Catalog's permanent home. Relocated out of
// `prds/77-tui-testing-harness.md` (PRD #77 was archived to `prds/done/`,
// which broke the old hardcoded path) into a PRD-lifecycle-independent file.
const CATALOG_PATH: &str = "tests/CATALOG.md";
const ALLOWLIST_PATH: &str = "xtask/linkage-check/m2.allowlist";
const TESTS_DIR: &str = "tests";

/// Check 8 (issue #322): why a bare `tempfile` constructor is forbidden under
/// `tests/`, spelled out here because the violation is invisible at the call
/// site.
///
/// The harness redirects `tempfile`'s process-global default temp dir at its own
/// per-process root — but it can only do that from inside
/// `harness_temp_root()`'s lazy initialisation, i.e. the first time something
/// asks the harness for a directory. nextest runs one process per test, so a
/// bare `tempfile::tempdir()` that runs *before* any harness call in that test
/// is the first allocation of the process and lands in the OS temp dir instead:
/// commonly the RAM-backed `/tmp` this whole issue is about, at `tempfile`'s
/// default mode rather than 0o700, outside the free-space pre-flight, and — the
/// part that bites — left behind on SIGKILL under `.tmp*`, a name the reaper
/// deliberately will not touch by default because it belongs to every Rust
/// program on the machine.
///
/// This was not theoretical: `e2e_issue_dispatch` cloned whole repositories
/// through exactly that ordering. Rather than depend on every call site
/// happening to be preceded by a harness call, the suite calls
/// `common::harness_tempdir()`, which initialises the root first and then
/// allocates inside it. This rule is what keeps that true — the ordering
/// argument is invisible in a diff, so it cannot be left to review.
///
/// **Files, not just directories.** The rule originally matched only the
/// directory constructors, which left a hole inside the territory it claimed to
/// cover: `tempfile::NamedTempFile::new()` allocates in the OS temp dir on
/// exactly the same terms, and the Codex-auth pre-flight in `tests/common/`
/// used one. Measured on `5e8e0ed` as four zero-byte `/tmp/.tmp*` files. The
/// byte count is irrelevant — the rule's job is to keep the containment claim
/// true, and a constructor it cannot see makes the claim false. The `…_in`
/// forms (`tempdir_in`, `tempfile_in`, `new_in`) name their parent explicitly
/// and are therefore fine; the no-argument forms are not.
///
/// **Scope: all of `tests/`**, plus [`EXTRA_TEMP_COVERED`] for the lib target.
///
/// It used to be an enumerated list — `tests/e2e_*.rs`, `tests/common/`, and
/// two named files — because covering the rest of the fast tier was priced at
/// pulling `tests/common/mod.rs` into six more binaries and duplicating its
/// ~530 executions to contain small L1 `TempDir`s. That price was real when it
/// was measured, and it is no longer what the choice costs: `src/test_temp.rs`
/// is deliberately self-contained, so a fast-tier crate `#[path]`-includes it
/// for **two** extra executions. Six crates, twelve executions, measured — so
/// the enumeration outlived the measurement that justified it. A whole-
/// directory rule is also the only version a *new* file under `tests/` inherits
/// automatically; an enumerated one silently does not cover it.
///
/// The escape hatch is [`BARE_TEMPDIR_ALLOW`] on the same line, which the
/// harness's own defence-in-depth regression test uses — the one test whose
/// subject *is* the bare constructor.
const BARE_TEMPDIR_RULE: &str = "bare tempfile constructor — use `common::harness_tempdir()` / \
     `harness_tempfile()` (or `test_temp::tempdir()` outside the harness) so it \
     lands under the harness temp root even when it is the process's FIRST \
     allocation (issue #322)";

/// Files outside `tests/` that check 8 also covers.
///
/// `src/dispatch.rs` — lib-target unit tests that build real git repos and
/// worktrees. They do not link `tests/common/` at all and use
/// `crate::test_temp::tempdir()`; one of them was measured holding a live
/// 184 KiB `/tmp/.tmpYN3lNF` with a cloned repo in it during a recorded
/// `cargo test-e2e`, so the rule is what stops a bare constructor coming back.
///
/// The **rest** of `src/`'s unit tests are deliberately not here — ~82 call
/// sites across 22 files, a large mechanical diff that would move fast-tier
/// churn onto `/var/tmp` for no measured benefit. That is the one remaining
/// documented gap in `docs/develop/e2e-temp-dirs.md`; everything under
/// `tests/` is covered by the directory rule above.
///
/// Paths are repo-relative and compared with the platform separator
/// normalised, so this works on Windows too.
const EXTRA_TEMP_COVERED: &[&str] = &["src/dispatch.rs"];

/// Opt-out marker for check 8, on the offending line.
const BARE_TEMPDIR_ALLOW: &str = "linkage-check:allow-bare-tempdir";

/// The `tempfile` constructors that allocate in the **default** temp dir.
///
/// Directories: `tempfile::tempdir()`, `TempDir::new()`, `TempDir::with_prefix()`,
/// `TempDir::with_suffix()`, and the builder's `.tempdir()`. Files:
/// `NamedTempFile::new()`, `NamedTempFile::with_prefix()`,
/// `NamedTempFile::with_suffix()`, `tempfile::tempfile()`,
/// `spooled_tempfile()`, and the builder's `.tempfile()`. Every `…_in(parent)` /
/// `…_new_in(parent)` form names its destination and is deliberately NOT matched
/// — that is what the wrappers themselves call, and `…_in` sits between the name
/// and the `(` so none of the patterns here can reach it.
///
/// Factored out of `main` so it can be unit-tested; the file half of it was
/// missing for a while and nothing caught that.
///
/// **The `with_prefix` / `with_suffix` / `spooled` family was missing too**, and
/// the same argument applies: they are ordinary safe-looking constructors that
/// allocate in `std::env::temp_dir()`, verified present in the pinned
/// `tempfile 3.27.0` (`src/dir/mod.rs:269`/`:294`, `src/file/mod.rs:630`/`:657`),
/// and each has an `…_in` counterpart, so the rule is satisfiable. There was no
/// live call site when this was added — the value is that the guard now matches
/// the claim it makes in the module header ("no bare `tempfile` constructor …
/// anywhere under `tests/`") instead of enumerating a subset of it. A rule that
/// covers most of its stated territory is the shape that let
/// `NamedTempFile::new()` sit inside its own scope undetected.
fn bare_temp_ctor_re() -> Regex {
    Regex::new(
        r"tempfile::tempdir\s*\(|TempDir::new\s*\(|TempDir::with_prefix\s*\(|TempDir::with_suffix\s*\(|\.tempdir\s*\(\s*\)|NamedTempFile::new\s*\(|NamedTempFile::with_prefix\s*\(|NamedTempFile::with_suffix\s*\(|tempfile::tempfile\s*\(|spooled_tempfile\s*\(|\.tempfile\s*\(\s*\)",
    )
    .expect("bare temp constructor regex compiles")
}

/// Whether check 8 applies to `file`.
///
/// Everything under `tests/`, plus the explicit [`EXTRA_TEMP_COVERED`] list for
/// the lib target. `is_e2e` is no longer consulted — an `e2e_` file is under
/// `tests/` by construction — but stays in the signature because the caller has
/// it and because dropping it would make the two scoping rules look unrelated.
fn temp_ctor_rule_covers(file: &Path, root: &Path, tests_dir: &Path, _is_e2e: bool) -> bool {
    if file.starts_with(tests_dir) {
        return true;
    }
    EXTRA_TEMP_COVERED
        .iter()
        .any(|rel| file == root.join(rel).as_path())
}

fn main() -> ExitCode {
    // PRD #77 M4: route subcommands through this binary so the
    // single `cargo xtask` alias can drive both linkage-check and
    // docs. `cargo xtask docs --tests` → docs generator;
    // anything else (including no first arg or `linkage-check`) →
    // the seven Decision-7 / Decision-30 checks below.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(args.first().map(String::as_str), Some("docs")) {
        return run_docs(&args[1..]);
    }
    if matches!(args.first().map(String::as_str), Some("list-tests")) {
        return run_list_tests(&args[1..]);
    }
    if matches!(args.first().map(String::as_str), Some("clean-e2e-tmp")) {
        return clean_tmp::run(&args[1..]);
    }

    let root = repo_root();
    let catalog_path = root.join(CATALOG_PATH);
    let allowlist_path = root.join(ALLOWLIST_PATH);
    let tests_dir = root.join(TESTS_DIR);

    let mut failures: Vec<String> = Vec::new();

    let catalog_ids = match parse_catalog_ids(&catalog_path) {
        Ok(ids) => ids,
        Err(e) => {
            eprintln!("failed to parse catalog at {}: {e}", catalog_path.display());
            return ExitCode::from(2);
        }
    };
    let allowlist = match read_allowlist(&allowlist_path) {
        Ok(set) => set,
        Err(e) => {
            eprintln!(
                "failed to read allowlist at {}: {e}",
                allowlist_path.display()
            );
            return ExitCode::from(2);
        }
    };

    // Check 3: format regex on catalog IDs.
    let id_re = Regex::new(r"^[a-z][a-z0-9-]*/[a-z][a-z0-9-]*/\d{3}$")
        .expect("catalog ID format regex compiles");
    for id in &catalog_ids {
        if !id_re.is_match(id) {
            failures.push(format!(
                "[3] catalog ID {id:?} does not match `<area>/<sub>/<NNN>`"
            ));
        }
    }

    // Scan tests/ AND src/ for `#[spec(...)]` annotations. PRD #83
    // added per-tab-selection `#[spec]` unit tests in `src/tab.rs`; the
    // e2e-only checks below key off the `e2e_` filename prefix, so
    // library sources never trip the sleep/polling rules.
    //
    // This text scan no longer decides which FUNCTION an annotation
    // belongs to — syn does that below (issue #406). It only records
    // where each annotation is, so an annotation syn could not bind is
    // reported at its own line.
    let mut test_files = collect_test_rs_files(&tests_dir);
    test_files.extend(collect_test_rs_files(&root.join("src")));
    let mut occurrences: Vec<SpecOccurrence> = Vec::new();
    let mut e2e_violations: Vec<String> = Vec::new();
    let mut ignore_violations: Vec<String> = Vec::new();

    let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("spec attr regex compiles");
    // Decision 21: forbidden in test bodies.
    let sleep_re =
        Regex::new(r"(std::thread::sleep|tokio::time::sleep)\b").expect("sleep regex compiles");
    let polling_re =
        Regex::new(r"for\s+_\s+in\s+0\.\.\s*\d+\s*\{").expect("polling regex compiles");
    let bare_tempdir_re = bare_temp_ctor_re();
    let mut bare_tempdir_violations: Vec<String> = Vec::new();

    for file in &test_files {
        let text = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read {}: {e}", file.display());
                continue;
            }
        };

        // M2.1 auditor Nit 5: strip line + block comments before running
        // the no-sleep regex check so a comment that mentions
        // `std::thread::sleep` (e.g. explaining why the harness does
        // NOT call it) does not register as a violation. The spec-
        // attribute scan uses the stripped copy too, so a commented-out
        // `#[spec(...)]` is not counted as a live annotation.
        let stripped = strip_rust_comments(&text);
        let raw_lines: Vec<&str> = text.lines().collect();
        let stripped_lines: Vec<&str> = stripped.lines().collect();
        let file_name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let is_e2e = file_name.starts_with("e2e_") && file_name.ends_with(".rs");

        for (id, line_no) in scan_spec_occurrences(&stripped_lines, &spec_re) {
            occurrences.push(SpecOccurrence {
                id,
                file: file.clone(),
                line: line_no,
            });
        }

        // Check 8 (issue #322): all of `tests/`, plus `EXTRA_TEMP_COVERED` for
        // the lib target. The e2e tier is where the allocations are whole cloned
        // repositories, where nextest's `slow-timeout terminate-after` SIGKILLs
        // a process before it can clean up, and where real agent credentials
        // get seeded — but the fast tier is no longer excluded, because the
        // exclusion was priced at pulling the PTY harness into six more
        // binaries and the `#[path]`-included `src/test_temp.rs` costs two test
        // executions per crate instead. Its files bind Unix domain sockets and,
        // on SIGKILL, survive as untagged `.tmp*` the reaper will not remove by
        // default. Run against the stripped view so a comment naming the
        // constructor is not a violation, but report the raw line number.
        if temp_ctor_rule_covers(file, &root, &tests_dir, is_e2e) {
            for (idx, raw) in raw_lines.iter().enumerate() {
                let stripped_line = stripped_lines.get(idx).copied().unwrap_or("");
                if bare_tempdir_re.is_match(stripped_line) && !raw.contains(BARE_TEMPDIR_ALLOW) {
                    bare_tempdir_violations.push(format!(
                        "{}:{}: {BARE_TEMPDIR_RULE}",
                        file.display(),
                        idx + 1
                    ));
                }
            }
        }

        if is_e2e {
            // Check 5: forbidden waits / polling in e2e test bodies.
            // Run against the stripped (comment-free) view so a
            // commented-out `// std::thread::sleep` doesn't trip the
            // check, but keep the raw line numbers in the error message
            // so violators are easy to locate.
            for (idx, _raw) in raw_lines.iter().enumerate() {
                let stripped_line = stripped_lines.get(idx).copied().unwrap_or("");
                if sleep_re.is_match(stripped_line) {
                    e2e_violations.push(format!(
                        "{}:{}: forbidden sleep call (Decision 21)",
                        file.display(),
                        idx + 1
                    ));
                }
                if polling_re.is_match(stripped_line) {
                    e2e_violations.push(format!(
                        "{}:{}: forbidden fixed-count polling loop (Decision 21)",
                        file.display(),
                        idx + 1
                    ));
                }
            }
        }
    }

    // Bind every annotation to its test function with syn — the same
    // walker rule 7 runs (issue #406). A parse failure here is fatal:
    // with no reliable binding, checks 1/2/4/6 would report garbage.
    let docs_config = xtask_docs::DocsConfig::from_workspace(root.clone());
    let discovered = match discover_spec_tests(&docs_config) {
        Ok(tests) => tests,
        Err(e) => {
            eprintln!("failed to parse #[spec] test sources: {e}");
            return ExitCode::from(2);
        }
    };

    // Issue #406, the honest-failure half: every `#[spec(...)]` the text
    // scan found must correspond to a function syn bound. One that does
    // not (annotating a non-`fn` item, or emitted from inside a macro
    // body syn does not expand) is named at its own file:line rather
    // than silently attaching itself to a neighbouring function.
    failures.extend(unattached_annotation_failures(&occurrences, &discovered));

    let mut annotated_ids: BTreeSet<&str> = BTreeSet::new();
    for ann in &discovered {
        annotated_ids.insert(&ann.spec_id);

        // Check 2: annotation references a real catalog ID.
        if !catalog_ids.contains(&ann.spec_id) {
            failures.push(format!(
                "[2] {} carries #[spec({:?})] which is not in the catalog",
                ann.source_path.display(),
                ann.spec_id
            ));
        }

        // Check 4: function name carries a Decision-17 prefix derived
        // from the catalog ID. We accept EITHER the short `<sub>_<NNN>`
        // form OR the category-qualified `<area>_<sub>_<NNN>` full-ID
        // form (both hyphen → underscore normalized for Rust idents,
        // M2.1 reviewer S1). The qualified form is what lets tests whose
        // short prefix collides across categories carry unambiguous
        // names WITHOUT renaming — e.g. `chain-smoke/pi/001` and
        // `scheduler/pi/001` both shorten to `pi_001`, so they use
        // `chain_smoke_pi_001` / `scheduler_pi_001` (PRD #201). The short
        // form stays valid so the many pre-existing short-named tests —
        // including other colliding sub-areas that predate this rule
        // (`help_001`, `form_001`, `live_001`, `spawn_001`,
        // `selection_001`, `layout_001`, …) — keep passing. See
        // `fn_name_matches_spec`.
        if !fn_name_matches_spec(&ann.spec_id, &ann.fn_name) {
            failures.push(format!(
                "[4] {} fn `{}` does not start with `{}` (short) or `{}` (category-qualified) (Decision 17, derived from #[spec({:?})])",
                ann.source_path.display(),
                ann.fn_name,
                sub_area_prefix(&ann.spec_id).unwrap_or_default(),
                qualified_id_prefix(&ann.spec_id).unwrap_or_default(),
                ann.spec_id
            ));
        }

        // Check 6 (Decision 26): read straight off the function's own
        // attributes. The old line scan credited this test with any
        // `#[ignore]` sitting between the annotation and the next plain
        // `fn`, which could belong to a different function entirely.
        if ann.ignored {
            ignore_violations.push(format!(
                "{}: #[spec({:?})] annotates an #[ignore]-d test `{}` (Decision 26)",
                ann.source_path.display(),
                ann.spec_id,
                ann.fn_name
            ));
        }
    }

    // Check 1: every catalog ID has at least one annotation OR is on
    // the allowlist (M2 ships only `dashboard/pane/004` and
    // `hooks/delivery/001`; M4+ ticks IDs off the allowlist as it
    // lands tests).
    for id in &catalog_ids {
        if annotated_ids.contains(id.as_str()) {
            continue;
        }
        if allowlist.contains(id) {
            continue;
        }
        failures.push(format!(
            "[1] catalog ID `{id}` has no #[spec({id:?})]-annotated test and is not on the M2 allowlist"
        ));
    }

    failures.extend(e2e_violations);
    failures.extend(ignore_violations);
    failures.extend(
        bare_tempdir_violations
            .into_iter()
            .map(|v| format!("[8] {v}")),
    );

    // Check 7 (PRD #77 Decision 30 / M4.3): every #[spec] test has
    // a `/// Scenario:` doc comment with a body AND
    // `cargo xtask docs --tests` succeeds against the current source
    // + catalog. The xtask-docs library raises `Err` on a missing
    // Scenario or a malformed test source, which is exactly the two
    // failure modes we want to surface here. The byte-identity check
    // against on-disk `.md` is gone in M4.3: `.dot-agent-deck/` is
    // gitignored, so on a fresh clone there is no `.md` to compare.
    if let Err(e) = xtask_docs::check_rule_7(&docs_config) {
        failures.push(format!("[7] {e}"));
    }

    if failures.is_empty() {
        println!(
            "linkage-check: ok ({} catalog ids, {} annotations, {} allowlisted, 8 rules)",
            catalog_ids.len(),
            discovered.len(),
            allowlist.len()
        );
        ExitCode::SUCCESS
    } else {
        eprintln!("linkage-check: {} failure(s):", failures.len());
        for f in &failures {
            eprintln!("  {f}");
        }
        ExitCode::FAILURE
    }
}

/// `cargo xtask docs --tests` dispatch. Performs the same work as
/// the `xtask-docs` binary's main, in-process — we share the
/// library entry points so the two binaries stay in lockstep.
fn run_docs(args: &[String]) -> ExitCode {
    for arg in args {
        match arg.as_str() {
            "--tests" => {}
            "-h" | "--help" => {
                println!("usage: cargo xtask docs --tests");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("xtask docs: unknown argument {other:?}");
                eprintln!("usage: cargo xtask docs --tests");
                return ExitCode::from(2);
            }
        }
    }
    let root = repo_root();
    let config = xtask_docs::DocsConfig::from_workspace(root.clone());
    let generated = match xtask_docs::generate_all(&config) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::FAILURE;
        }
    };
    let written = match xtask_docs::write_all(&generated) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("xtask docs: {e}");
            return ExitCode::FAILURE;
        }
    };
    for path in &written {
        let rel = path.strip_prefix(&root).unwrap_or(path.as_path());
        println!("wrote {}", rel.display());
    }
    ExitCode::SUCCESS
}

/// `cargo xtask list-tests` dispatch (PRD #77 Decision 31). Emits a
/// Markdown synthetic-test inventory between the current branch and
/// `origin/main` on stdout. The orchestrator runs this before
/// delegating release.
fn run_list_tests(args: &[String]) -> ExitCode {
    if let Some(first) = args.first() {
        match first.as_str() {
            "-h" | "--help" => {
                println!("usage: cargo xtask list-tests");
                println!();
                println!("Emits a Markdown report of every #[spec] test created or");
                println!("modified in this branch versus origin/main, plus per-catalog");
                println!("prose diffs and any xtask/linkage-check/m2.allowlist changes.");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("xtask list-tests: unknown argument {other:?}");
                eprintln!("usage: cargo xtask list-tests");
                return ExitCode::from(2);
            }
        }
    }
    let root = repo_root();
    match list_tests::run(&root) {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("xtask list-tests: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One `#[spec("…")]` attribute as *located by text scan* — where it is
/// written, not what it annotates. Deciding which function it belongs to
/// is syn's job (issue #406); this exists so an annotation syn does not
/// bind can be reported at its own line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecOccurrence {
    id: String,
    file: PathBuf,
    line: usize,
}

/// Find every `#[spec("…")]` in `lines` (already comment-stripped),
/// returning `(catalog id, 1-based line number)` in source order.
///
/// This deliberately does NOT look for a following `fn`. The old walker
/// did, with `^\s*fn\s+`, and scanned to end-of-file for a match — so an
/// `async fn` test was skipped and its annotation re-bound to whatever
/// plain `fn` came next, hundreds of lines away (issue #406).
fn scan_spec_occurrences(lines: &[&str], spec_re: &Regex) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = spec_re.captures(line) {
            out.push((caps.get(1).unwrap().as_str().to_string(), i + 1));
        }
    }
    out
}

/// Collect every `#[spec]` test under `tests/` and `src/` using the
/// generator's syn walker, so linkage-check and rule 7 can never
/// disagree about which functions exist (issue #406).
fn discover_spec_tests(
    config: &xtask_docs::DocsConfig,
) -> Result<Vec<xtask_docs::DiscoveredTest>, String> {
    let mut tests = xtask_docs::discover_tests(&config.tests_dir)?;
    // PRD #83: `#[spec]` tests also live in the library crate.
    tests.extend(xtask_docs::discover_tests(&config.src_dir)?);
    Ok(tests)
}

/// Report any `#[spec(...)]` occurrence that syn did not bind to a
/// function. Matching is per `(file, catalog id)` by COUNT: syn knows
/// the function name but not its line, and the same id may legitimately
/// be annotated on more than one test in a file, so an excess of text
/// occurrences over bound functions is the reliable signal. The message
/// carries every line the id appears on in that file, which is enough to
/// find the stray one.
fn unattached_annotation_failures(
    occurrences: &[SpecOccurrence],
    discovered: &[xtask_docs::DiscoveredTest],
) -> Vec<String> {
    let mut bound: BTreeMap<(&Path, &str), usize> = BTreeMap::new();
    for t in discovered {
        *bound
            .entry((t.source_path.as_path(), t.spec_id.as_str()))
            .or_insert(0) += 1;
    }
    let mut scanned: BTreeMap<(&Path, &str), Vec<usize>> = BTreeMap::new();
    for o in occurrences {
        scanned
            .entry((o.file.as_path(), o.id.as_str()))
            .or_default()
            .push(o.line);
    }

    let mut out = Vec::new();
    for ((file, id), lines) in scanned {
        let bound_count = bound.get(&(file, id)).copied().unwrap_or(0);
        if lines.len() <= bound_count {
            continue;
        }
        let unbound = lines.len() - bound_count;
        let where_ = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        out.push(format!(
            "[4] {} {unbound} of {} #[spec({id:?})] annotation(s) (line(s) {where_}) is not attached to a `fn` definition \
             — an attribute on a non-function item, or inside a macro body the parser does not expand",
            file.display(),
            lines.len(),
        ));
    }
    out
}

/// Locate the workspace root by walking up from the binary's
/// `current_dir()` until we see the workspace `Cargo.toml` (which has
/// a `[workspace]` block).
fn repo_root() -> PathBuf {
    let mut dir = std::env::current_dir().expect("current_dir is readable");
    loop {
        let candidate = dir.join("Cargo.toml");
        if let Ok(s) = std::fs::read_to_string(&candidate)
            && s.contains("[workspace]")
        {
            return dir;
        }
        if !dir.pop() {
            panic!("could not locate workspace root from {dir:?}");
        }
    }
}

/// Parse `## Test Case Catalog` out of the PRD: extract every
/// occurrence of `##### <area>/<sub>/<NNN>` (the catalog entry header
/// form). The deliberate-skips table at the bottom uses table rows,
/// not headers, so it is excluded by construction.
fn parse_catalog_ids(catalog_path: &Path) -> std::io::Result<BTreeSet<String>> {
    let text = std::fs::read_to_string(catalog_path)?;
    let mut in_catalog = false;
    let header_re = Regex::new(r"^#####\s+([a-z][a-z0-9-]*/[a-z][a-z0-9-]*/\d{3})\b")
        .expect("catalog header regex compiles");
    let mut ids: BTreeSet<String> = BTreeSet::new();
    for line in text.lines() {
        if line.starts_with("## ") {
            in_catalog = line.starts_with("## Test Case Catalog");
            continue;
        }
        if !in_catalog {
            continue;
        }
        if let Some(caps) = header_re.captures(line) {
            ids.insert(caps.get(1).unwrap().as_str().to_string());
        }
    }
    Ok(ids)
}

fn read_allowlist(path: &Path) -> std::io::Result<BTreeSet<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e),
    };
    let mut set = BTreeSet::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        set.insert(line.to_string());
    }
    Ok(set)
}

fn collect_test_rs_files(tests_dir: &Path) -> Vec<PathBuf> {
    let mut out: BTreeMap<PathBuf, ()> = BTreeMap::new();
    visit(tests_dir, &mut out);
    out.into_keys().collect()
}

fn visit(dir: &Path, acc: &mut BTreeMap<PathBuf, ()>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            visit(&p, acc);
        } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rs") {
            acc.insert(p, ());
        }
    }
}

/// Strip Rust `//` line comments and `/* … */` block comments from
/// `src`, replacing each stripped byte with a space. Line endings are
/// preserved 1-for-1 so per-line indexing into the stripped text
/// matches the raw source. String literals are honoured so a `//`
/// inside `"…"` is not mistakenly treated as a comment.
///
/// M4.6 P2: also recognises raw string literals (`r"…"`,
/// `r#"…"#`, `r##"…"##`, etc.). The closing delimiter is `"`
/// followed by exactly the same number of `#` characters that
/// opened the literal — an embedded `"` inside the body does NOT
/// close the string unless it has the matching hash suffix.
fn strip_rust_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut block_depth: usize = 0;
    // M4.6 P2: when inside a raw string literal, this holds the
    // number of `#` characters required between the closing `"` and
    // the end of the literal. `None` outside any raw string.
    let mut raw_string_hashes: Option<usize> = None;
    while i < bytes.len() {
        let c = bytes[i] as char;
        let next = bytes.get(i + 1).map(|b| *b as char);

        if let Some(needed_hashes) = raw_string_hashes {
            // Inside a raw string — content passes through verbatim;
            // only the matched `"` + `#…` sequence closes it. No
            // escape processing.
            out.push(c);
            if c == '"' {
                let mut hashes_seen = 0usize;
                while hashes_seen < needed_hashes
                    && bytes.get(i + 1 + hashes_seen).copied() == Some(b'#')
                {
                    hashes_seen += 1;
                }
                if hashes_seen == needed_hashes {
                    // Emit the trailing hashes verbatim and exit raw
                    // mode.
                    for _ in 0..hashes_seen {
                        out.push('#');
                    }
                    i += 1 + hashes_seen;
                    raw_string_hashes = None;
                    continue;
                }
            }
            i += 1;
            continue;
        }

        if block_depth > 0 {
            // Inside a block comment — only `*/` or nested `/*` matter;
            // newlines are preserved so line numbers align.
            if c == '/' && next == Some('*') {
                block_depth += 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '*' && next == Some('/') {
                block_depth -= 1;
                out.push(' ');
                out.push(' ');
                i += 2;
                continue;
            }
            if c == '\n' {
                out.push('\n');
            } else {
                out.push(' ');
            }
            i += 1;
            continue;
        }

        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }

        if in_char {
            out.push(c);
            if c == '\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }

        // Raw string literal start: `r`, `r"`, or `r#…"`. The `r`
        // must be at a token boundary (previous byte is not an
        // identifier-continuation char) so the matcher doesn't fire
        // on `for`, `let_r`, etc.
        if c == 'r' {
            let prev = i.checked_sub(1).and_then(|p| bytes.get(p)).copied();
            let is_token_boundary = match prev {
                None => true,
                Some(b) => {
                    let pc = b as char;
                    !(pc.is_ascii_alphanumeric() || pc == '_')
                }
            };
            if is_token_boundary {
                let mut j = i + 1;
                while bytes.get(j).copied() == Some(b'#') {
                    j += 1;
                }
                if bytes.get(j).copied() == Some(b'"') {
                    let hashes = j - (i + 1);
                    // Emit the prefix verbatim: r + hashes + opening "
                    out.push('r');
                    for _ in 0..hashes {
                        out.push('#');
                    }
                    out.push('"');
                    i = j + 1;
                    raw_string_hashes = Some(hashes);
                    continue;
                }
            }
            // Fall through — `r` is just an identifier char.
        }

        if c == '/' && next == Some('/') {
            // Line comment — eat until newline (preserve the newline).
            while i < bytes.len() && bytes[i] as char != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            block_depth = 1;
            out.push(' ');
            out.push(' ');
            i += 2;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '\'' {
            // Heuristic: only treat `'` as a char literal start when the
            // following byte is not an identifier continuation (lifetimes
            // look like `'a`). Comments inside lifetime annotations can't
            // exist anyway, so being conservative is fine.
            let after_after = bytes.get(i + 2).map(|b| *b as char);
            let looks_like_lifetime = next.is_some_and(|n| n.is_ascii_alphabetic() || n == '_')
                && after_after.is_some_and(|a| a != '\'');
            if !looks_like_lifetime {
                in_char = true;
            }
            out.push(c);
            i += 1;
            continue;
        }

        out.push(c);
        i += 1;
    }
    out
}

/// Derive the Decision-17 `<sub>_<NNN>` prefix from a catalog ID,
/// applying the hyphen → underscore normalization used by Rust
/// identifiers (M2.1 reviewer S1). Returns `None` if the ID is not
/// of the expected three-segment shape.
fn sub_area_prefix(id: &str) -> Option<String> {
    let (rest, nnn) = id.rsplit_once('/')?;
    let (_area, sub) = rest.rsplit_once('/')?;
    Some(format!("{}_{nnn}", sub.replace('-', "_")))
}

/// Derive the category-qualified Decision-17 prefix from a catalog ID:
/// the FULL `<area>/<sub>/<NNN>` with `/` and `-` replaced by `_`
/// (e.g. `chain-smoke/pi/001` → `chain_smoke_pi_001`). This is the
/// unambiguous form used when a short `<sub>_<NNN>` prefix would collide
/// across categories. Returns `None` for malformed IDs (same
/// three-segment shape guard as [`sub_area_prefix`]).
fn qualified_id_prefix(id: &str) -> Option<String> {
    let (rest, nnn) = id.rsplit_once('/')?;
    let (area, sub) = rest.rsplit_once('/')?;
    Some(format!(
        "{}_{}_{nnn}",
        area.replace('-', "_"),
        sub.replace('-', "_")
    ))
}

/// Decision-17 acceptance: does `fname` carry a prefix traceable to
/// catalog `id`? Accepts EITHER the short `<sub>_<NNN>` form
/// ([`sub_area_prefix`]) OR the category-qualified `<area>_<sub>_<NNN>`
/// form ([`qualified_id_prefix`]).
///
/// Accepting both is deliberate: the qualified form disambiguates
/// cross-category same-sub-area IDs whose short prefixes collide
/// (`chain-smoke/pi/001` and `scheduler/pi/001` both shorten to
/// `pi_001`), while the short form keeps the many pre-existing
/// short-named tests valid — including other colliding sub-areas
/// (`help_001`, `form_001`, `live_001`, `spawn_001`, …) that predate
/// this rule. We do NOT reject the short form on collision: that would
/// force ~20 already-shipped tests to rename, which is out of scope and
/// contrary to the "keep existing short-form names valid" contract.
///
/// A malformed ID with no derivable prefix is treated as vacuously OK —
/// the ID-format check (check 3) already flags it.
fn fn_name_matches_spec(id: &str, fname: &str) -> bool {
    let short = sub_area_prefix(id).unwrap_or_default();
    let qualified = qualified_id_prefix(id).unwrap_or_default();
    if short.is_empty() && qualified.is_empty() {
        return true;
    }
    (!short.is_empty() && fname.starts_with(&short))
        || (!qualified.is_empty() && fname.starts_with(&qualified))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check 8 must see the *file* constructors, not only the directory ones.
    /// This is the hole that let the Codex-auth pre-flight's
    /// `NamedTempFile::new()` sit inside the rule's own scope, measured live in
    /// `/tmp` on `5e8e0ed`.
    #[test]
    fn bare_temp_ctor_re_matches_file_constructors() {
        let re = bare_temp_ctor_re();
        for line in [
            "    let f = tempfile::NamedTempFile::new()",
            "    let f = NamedTempFile::new().unwrap();",
            "    let f = tempfile::tempfile().unwrap();",
            "    let f = tempfile::Builder::new().tempfile()?;",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    #[test]
    fn bare_temp_ctor_re_still_matches_dir_constructors() {
        let re = bare_temp_ctor_re();
        for line in [
            "    let d = tempfile::tempdir().unwrap();",
            "    let d = TempDir::new().unwrap();",
            "    let d = tempfile::Builder::new().tempdir()?;",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    /// The `…_in` forms name their parent explicitly — they are what the
    /// wrappers themselves call, so matching them would make the rule
    /// unsatisfiable.
    #[test]
    fn bare_temp_ctor_re_allows_explicit_parent_forms() {
        let re = bare_temp_ctor_re();
        for line in [
            "    tempfile::Builder::new().tempdir_in(harness_temp_root())",
            "    tempfile::Builder::new().tempfile_in(harness_temp_root())",
            "    NamedTempFile::new_in(parent)?",
            "    TempDir::new_in(parent)?",
            "    tempfile::tempfile_in(parent)?",
            // The widened family's own `…_in` counterparts. `_in` sits between
            // the name and the `(`, which is what keeps these out.
            "    TempDir::with_prefix_in(\"codex-home-\", parent)?",
            "    TempDir::with_suffix_in(\".git\", parent)?",
            "    NamedTempFile::with_prefix_in(\"auth-\", parent)?",
            "    NamedTempFile::with_suffix_in(\".json\", parent)?",
            "    tempfile::spooled_tempfile_in(4096, parent)?",
        ] {
            assert!(!re.is_match(line), "should NOT be a violation: {line}");
        }
    }

    /// The `with_prefix` / `with_suffix` / `spooled` family allocates in
    /// `std::env::temp_dir()` exactly like `new()` does, and the rule claims to
    /// cover every bare constructor under `tests/`. These had no live call site
    /// when this test was written; it exists so the claim and the guard cannot
    /// drift apart again, which is how `NamedTempFile::new()` went unmatched.
    #[test]
    fn bare_temp_ctor_re_matches_the_prefix_suffix_and_spooled_family() {
        let re = bare_temp_ctor_re();
        for line in [
            "    let d = TempDir::with_prefix(\"codex-home-\").unwrap();",
            "    let d = tempfile::TempDir::with_suffix(\"-repo\").unwrap();",
            "    let f = NamedTempFile::with_prefix(\"auth-\").unwrap();",
            "    let f = tempfile::NamedTempFile::with_suffix(\".json\").unwrap();",
            "    let f = tempfile::spooled_tempfile(4096);",
        ] {
            assert!(re.is_match(line), "should be a violation: {line}");
        }
    }

    /// Scope: **all** of `tests/` — the e2e tier, the harness, and the fast-tier
    /// crates that used to be excluded — plus `src/dispatch.rs`. The rest of
    /// `src/` stays out; that is the one remaining documented gap in
    /// `docs/develop/e2e-temp-dirs.md`.
    #[test]
    fn temp_ctor_rule_covers_the_documented_scope() {
        let root = Path::new("/repo");
        let tests_dir = root.join("tests");
        let covers = |rel: &str, is_e2e: bool| {
            temp_ctor_rule_covers(&root.join(rel), root, &tests_dir, is_e2e)
        };

        assert!(covers("tests/e2e_handshake.rs", true));
        assert!(covers("tests/common/mod.rs", false));
        assert!(covers("tests/daemon_protocol.rs", false));
        assert!(covers("src/dispatch.rs", false));

        // The six converted in the same commit that widened this rule, and a
        // name that does not exist yet — the whole point of a directory rule is
        // that a new file under `tests/` inherits it without being listed.
        assert!(covers("tests/rehydration.rs", false));
        assert!(covers("tests/pane_close.rs", false));
        assert!(covers("tests/codex_hooks_safety.rs", false));
        assert!(covers("tests/features.rs", false));
        assert!(covers("tests/devin_hook_ingestion.rs", false));
        assert!(covers("tests/codex_hook_ingestion.rs", false));
        assert!(covers("tests/some_future_suite.rs", false));

        // Still outside: everything in `src/` except the one listed file.
        assert!(!covers("src/config.rs", false));
        assert!(!covers("src/test_temp.rs", false));
    }

    #[test]
    fn sub_area_prefix_handles_plain_sub_area() {
        assert_eq!(
            sub_area_prefix("dashboard/pane/004").as_deref(),
            Some("pane_004")
        );
    }

    #[test]
    fn sub_area_prefix_normalizes_hyphens_in_sub_area() {
        // PRD #77 catalog has these in the M2 allowlist; without the
        // hyphen → underscore normalization the function-name prefix
        // would be `pane-input_001_…` which is not a valid Rust ident.
        assert_eq!(
            sub_area_prefix("prompt/pane-input/001").as_deref(),
            Some("pane_input_001")
        );
        assert_eq!(
            sub_area_prefix("lifecycle/daemon-idle/002").as_deref(),
            Some("daemon_idle_002")
        );
        assert_eq!(
            sub_area_prefix("error/agent-spawn/001").as_deref(),
            Some("agent_spawn_001")
        );
    }

    #[test]
    fn sub_area_prefix_rejects_malformed_id() {
        assert_eq!(sub_area_prefix("not-an-id"), None);
        assert_eq!(sub_area_prefix("only/two"), None);
    }

    #[test]
    fn qualified_id_prefix_builds_full_id_form() {
        // Full-ID form: `/` and `-` → `_` across all three segments.
        assert_eq!(
            qualified_id_prefix("chain-smoke/pi/001").as_deref(),
            Some("chain_smoke_pi_001")
        );
        assert_eq!(
            qualified_id_prefix("scheduler/pi/001").as_deref(),
            Some("scheduler_pi_001")
        );
        assert_eq!(
            qualified_id_prefix("pi/live/002").as_deref(),
            Some("pi_live_002")
        );
        assert_eq!(
            qualified_id_prefix("dashboard/pane/004").as_deref(),
            Some("dashboard_pane_004")
        );
    }

    #[test]
    fn qualified_id_prefix_rejects_malformed_id() {
        assert_eq!(qualified_id_prefix("not-an-id"), None);
        assert_eq!(qualified_id_prefix("only/two"), None);
    }

    #[test]
    fn fn_name_matches_spec_accepts_short_form() {
        // Non-colliding sub-areas keep their short `<sub>_<NNN>` names.
        assert!(fn_name_matches_spec(
            "dashboard/pane/004",
            "pane_004_card_renders"
        ));
        // Colliding short forms that predate the qualified rule stay
        // valid via the short prefix (`help_001` is shared by three
        // catalog IDs, `live_002` by three, etc.).
        assert!(fn_name_matches_spec(
            "keybindings/help/001",
            "help_001_overlay"
        ));
        assert!(fn_name_matches_spec(
            "scheduler/live/002",
            "live_002_focusing_scheduled_card"
        ));
    }

    #[test]
    fn fn_name_matches_spec_accepts_category_qualified_form() {
        // PRD #201: the pi tests' short forms collide across categories
        // (`pi_001` from chain-smoke/pi + scheduler/pi), so they carry
        // category-qualified names — which must now be accepted WITHOUT
        // renaming them.
        assert!(fn_name_matches_spec(
            "chain-smoke/pi/001",
            "chain_smoke_pi_001_orchestrator_delegates_to_real_worker"
        ));
        assert!(fn_name_matches_spec(
            "scheduler/pi/001",
            "scheduler_pi_001_scheduled_unattended_status_via_extension"
        ));
        assert!(fn_name_matches_spec(
            "chain-smoke/pi/002",
            "chain_smoke_pi_002_worker_receives_delegate_and_signals_work_done"
        ));
        assert!(fn_name_matches_spec(
            "pi/live/001",
            "pi_live_001_live_pane_shows_identity_and_status"
        ));
        assert!(fn_name_matches_spec(
            "pi/live/002",
            "pi_live_002_native_seeded_orchestration_delegates_live"
        ));
    }

    #[test]
    fn fn_name_matches_spec_rejects_unrelated_prefix() {
        // A name matching neither the short nor the qualified prefix is
        // still flagged.
        assert!(!fn_name_matches_spec(
            "chain-smoke/pi/001",
            "totally_unrelated_name"
        ));
        assert!(!fn_name_matches_spec(
            "dashboard/pane/004",
            "widget_004_something"
        ));
    }

    #[test]
    fn fn_name_matches_spec_vacuously_ok_for_malformed_id() {
        // Malformed IDs have no derivable prefix; check 3 flags the
        // format, so check 4 must not double-report.
        assert!(fn_name_matches_spec("not-an-id", "whatever_name"));
    }

    /// Build a `DiscoveredTest` standing in for one syn-bound test.
    fn bound(file: &str, spec_id: &str, fn_name: &str) -> xtask_docs::DiscoveredTest {
        xtask_docs::DiscoveredTest {
            spec_id: spec_id.to_string(),
            fn_name: fn_name.to_string(),
            source_path: PathBuf::from(file),
            scenario: Some("Scenario: synthetic.".to_string()),
            steps: Vec::new(),
            ignored: false,
        }
    }

    fn occurrence(file: &str, id: &str, line: usize) -> SpecOccurrence {
        SpecOccurrence {
            id: id.to_string(),
            file: PathBuf::from(file),
            line,
        }
    }

    #[test]
    fn scan_spec_occurrences_records_ids_and_line_numbers() {
        let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("regex");
        let lines = vec![
            "mod common;",
            r#"#[spec("hooks/delivery/001")]"#,
            "#[tokio::test]",
            "async fn delivery_001_async() {}",
            "",
            r#"#[spec("dashboard/pane/005")]"#,
            "#[test]",
            "fn pane_005_plain() {}",
        ];
        assert_eq!(
            scan_spec_occurrences(&lines, &spec_re),
            vec![
                ("hooks/delivery/001".to_string(), 2),
                ("dashboard/pane/005".to_string(), 6),
            ]
        );
    }

    #[test]
    fn scan_spec_occurrences_is_indifferent_to_what_follows() {
        // Issue #406: the scan no longer looks for a following `fn` at
        // all, so an `async fn` (or any other item shape) is recorded
        // identically. Binding is syn's job.
        let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("regex");
        let lines = vec![r#"#[spec("hooks/delivery/001")]"#, "async fn whatever() {}"];
        assert_eq!(
            scan_spec_occurrences(&lines, &spec_re),
            vec![("hooks/delivery/001".to_string(), 1)]
        );
    }

    #[test]
    fn unattached_annotations_are_silent_when_every_one_is_bound() {
        let occ = vec![
            occurrence("tests/a.rs", "hooks/delivery/001", 10),
            occurrence("tests/a.rs", "dashboard/pane/005", 20),
        ];
        let found = vec![
            bound("tests/a.rs", "hooks/delivery/001", "delivery_001_x"),
            bound("tests/a.rs", "dashboard/pane/005", "pane_005_y"),
        ];
        assert!(unattached_annotation_failures(&occ, &found).is_empty());
    }

    #[test]
    fn unattached_annotation_is_reported_at_its_own_location() {
        // The honest-failure half of issue #406: an annotation syn could
        // not bind is named in ITS file, with its line — never silently
        // charged to a neighbouring function.
        let occ = vec![occurrence("tests/a.rs", "hooks/delivery/001", 42)];
        let failures = unattached_annotation_failures(&occ, &[]);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("tests/a.rs"), "{}", failures[0]);
        assert!(failures[0].contains("line(s) 42"), "{}", failures[0]);
        assert!(
            failures[0].contains("hooks/delivery/001"),
            "{}",
            failures[0]
        );
    }

    #[test]
    fn unattached_annotation_counts_duplicates_per_file_and_id() {
        // The same catalog ID may legitimately be annotated on more than
        // one test in a file, so the check compares COUNTS: two
        // occurrences with one bound fn means exactly one is stray.
        let occ = vec![
            occurrence("tests/a.rs", "hooks/delivery/001", 10),
            occurrence("tests/a.rs", "hooks/delivery/001", 30),
        ];
        let found = vec![bound("tests/a.rs", "hooks/delivery/001", "delivery_001_x")];
        let failures = unattached_annotation_failures(&occ, &found);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("1 of 2"), "{}", failures[0]);
        assert!(failures[0].contains("line(s) 10, 30"), "{}", failures[0]);

        // Two occurrences, two bound fns → nothing to report.
        let found_both = vec![
            bound("tests/a.rs", "hooks/delivery/001", "delivery_001_x"),
            bound("tests/a.rs", "hooks/delivery/001", "delivery_001_y"),
        ];
        assert!(unattached_annotation_failures(&occ, &found_both).is_empty());
    }

    #[test]
    fn unattached_annotation_does_not_match_across_files() {
        // A bound test in another file must not satisfy this file's
        // annotation.
        let occ = vec![occurrence("tests/a.rs", "hooks/delivery/001", 10)];
        let found = vec![bound("tests/b.rs", "hooks/delivery/001", "delivery_001_x")];
        let failures = unattached_annotation_failures(&occ, &found);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("tests/a.rs"), "{}", failures[0]);
    }

    #[test]
    fn strip_rust_comments_removes_line_comments() {
        let src = "fn foo() { /* keep this */ let x = 1; // and this\nlet y = 2;}";
        let out = strip_rust_comments(src);
        // The `// and this` content disappears; the `let y = 2;` survives.
        assert!(!out.contains("and this"));
        assert!(out.contains("let y = 2;"));
    }

    #[test]
    fn strip_rust_comments_preserves_string_literal_double_slashes() {
        let src = r#"let url = "https://example.com/path";"#;
        let out = strip_rust_comments(src);
        assert!(out.contains("https://example.com/path"));
    }

    #[test]
    fn strip_rust_comments_preserves_line_count() {
        let src = "// line1\nlet x = 0;\n// line3";
        let out = strip_rust_comments(src);
        // Three lines in → three lines out — the per-line indexing in
        // check 5/6 depends on this invariant.
        assert_eq!(out.lines().count(), src.lines().count());
    }

    #[test]
    fn strip_rust_comments_handles_raw_string_with_embedded_quote() {
        // M4.6 P2: a raw string can legally contain a bare `"`
        // because the closing delimiter is `"#`. The stripper must
        // not exit string mode on the embedded `"` and start
        // treating the rest of the file as bare code, which would
        // re-enable the line/block comment scanner and could strip
        // `// foo` text the author intended to keep.
        let src = r##"let s = r#"contains " and // not a comment"#; // real comment
let x = 1;"##;
        let out = strip_rust_comments(src);
        // The literal `// not a comment` inside the raw string must
        // survive (raw-string content passes through verbatim).
        assert!(
            out.contains("// not a comment"),
            "raw-string body should pass through verbatim: {out}"
        );
        // The trailing `// real comment` outside the raw string
        // must be stripped.
        assert!(
            !out.contains("real comment"),
            "real line comment after the raw string must be stripped: {out}"
        );
        // Code after the comment line is still present.
        assert!(out.contains("let x = 1;"));
    }

    #[test]
    fn strip_rust_comments_handles_nested_hash_raw_string() {
        // `r##"…"##` requires TWO `#` after the closing `"`. An
        // embedded `"#` (one hash) must NOT terminate the literal.
        let src = r###"let s = r##"contains "# (single-hash) here // not a comment"##; // real
let y = 2;"###;
        let out = strip_rust_comments(src);
        assert!(
            out.contains("// not a comment"),
            "embedded `\"#` inside r##\"...\"## must not exit raw mode: {out}"
        );
        assert!(
            !out.contains("real"),
            "real comment outside the raw string must be stripped: {out}"
        );
        assert!(out.contains("let y = 2;"));
    }

    #[test]
    fn strip_rust_comments_does_not_misidentify_identifier_starting_with_r() {
        // `for` starts with `f`, not `r`, but `let r_value = "…"`
        // is the corner case: the bare `r` is an identifier prefix,
        // followed by `_value`. The stripper must not treat that
        // `r` as a raw-string opener (no `#` or `"` follows
        // immediately). Same for `for` (the `r` is not at a token
        // boundary).
        let src = r#"for r_value in 0..3 { let _ = r_value; }
// line comment after"#;
        let out = strip_rust_comments(src);
        // Identifiers preserved.
        assert!(out.contains("for r_value in 0..3"));
        assert!(out.contains("let _ = r_value;"));
        // The trailing line comment is still stripped.
        assert!(!out.contains("line comment after"));
    }
}
