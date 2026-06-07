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
//!
//! - `docs` — invokes the `xtask-docs` binary's logic (paired-`.md`
//!   generator). Forwards remaining args.
//! - `list-tests` — PRD #77 Decision 31: emits a Markdown report of
//!   every `#[spec]` test created or modified in this branch versus
//!   `origin/main`, plus per-catalog-entry prose diffs and any
//!   `m2.allowlist` changes. The orchestrator surfaces this to the
//!   user before delegating release.
//!
//! Exits 0 on success, 1 on any failure with a per-finding summary.

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

    // Scan tests/ AND src/ for `#[spec(...)]` annotations + function
    // defs. PRD #83 added per-tab-selection `#[spec]` unit tests in
    // `src/tab.rs`; the e2e-only checks below key off the `e2e_`
    // filename prefix, so library sources never trip the sleep/polling
    // rules.
    let mut test_files = collect_test_rs_files(&tests_dir);
    test_files.extend(collect_test_rs_files(&root.join("src")));
    let mut annotations: Vec<SpecAnnotation> = Vec::new();
    let mut e2e_violations: Vec<String> = Vec::new();
    let mut ignore_violations: Vec<String> = Vec::new();

    let spec_re = Regex::new(r#"#\[spec\("([^"]+)"\)\]"#).expect("spec attr regex compiles");
    let fn_re = Regex::new(r"^\s*fn\s+([A-Za-z_][A-Za-z0-9_]*)").expect("fn regex compiles");
    let ignore_re = Regex::new(r"#\[ignore\b").expect("ignore regex compiles");
    // Decision 21: forbidden in test bodies.
    let sleep_re =
        Regex::new(r"(std::thread::sleep|tokio::time::sleep)\b").expect("sleep regex compiles");
    let polling_re =
        Regex::new(r"for\s+_\s+in\s+0\.\.\s*\d+\s*\{").expect("polling regex compiles");

    for file in &test_files {
        let text = match std::fs::read_to_string(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("failed to read {}: {e}", file.display());
                continue;
            }
        };

        // M2.1 auditor Nit 5: strip line + block comments before running
        // the no-sleep / no-ignore regex checks so a comment that
        // mentions `std::thread::sleep` (e.g. explaining why the
        // harness does NOT call it) does not register as a violation.
        // The spec-attribute and fn regexes do NOT use the stripped
        // copy — they intentionally allow the `#[spec(...)]` line to
        // sit next to `// doc comment` content.
        let stripped = strip_rust_comments(&text);
        let raw_lines: Vec<&str> = text.lines().collect();
        let stripped_lines: Vec<&str> = stripped.lines().collect();
        let file_name = file
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let is_e2e = file_name.starts_with("e2e_") && file_name.ends_with(".rs");

        // Walk lines, link each `#[spec("...")]` to the next function
        // definition. The annotation may be followed by other
        // attributes (`#[test]`, `#[ignore]`) before the `fn`; we
        // accumulate those and stop at the first `fn`. Use the stripped
        // view so `#[ignore]` inside a comment does not count.
        for (i, line) in stripped_lines.iter().enumerate() {
            if let Some(caps) = spec_re.captures(line) {
                let id = caps.get(1).unwrap().as_str().to_string();
                let mut fn_name: Option<String> = None;
                let mut between_ignored = false;
                for next in &stripped_lines[i + 1..] {
                    if ignore_re.is_match(next) {
                        between_ignored = true;
                    }
                    if let Some(c) = fn_re.captures(next) {
                        fn_name = Some(c.get(1).unwrap().as_str().to_string());
                        break;
                    }
                }
                if between_ignored {
                    ignore_violations.push(format!(
                        "{}: #[spec({id:?})] annotates an #[ignore]-d test (Decision 26)",
                        file.display()
                    ));
                }
                annotations.push(SpecAnnotation {
                    id,
                    file: file.clone(),
                    fn_name,
                });
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

    let mut annotated_ids: BTreeSet<&str> = BTreeSet::new();
    for ann in &annotations {
        annotated_ids.insert(&ann.id);

        // Check 2: annotation references a real catalog ID.
        if !catalog_ids.contains(&ann.id) {
            failures.push(format!(
                "[2] {} carries #[spec({:?})] which is not in the catalog",
                ann.file.display(),
                ann.id
            ));
        }

        // Check 4: function name carries `<sub>_<NNN>` prefix per
        // Decision 17. M2.1 reviewer S1: hyphenated sub-areas
        // (e.g. `prompt/pane-input/001`, `lifecycle/daemon-idle/001`)
        // become snake_case in Rust identifiers — `sub_area_prefix`
        // normalizes `-` → `_` on the sub-area before comparing.
        if let Some(fname) = &ann.fn_name {
            let sub_nnn = sub_area_prefix(&ann.id).unwrap_or_default();
            if !sub_nnn.is_empty() && !fname.starts_with(&sub_nnn) {
                failures.push(format!(
                    "[4] {} fn `{}` does not start with `{}` (Decision 17, derived from #[spec({:?})])",
                    ann.file.display(),
                    fname,
                    sub_nnn,
                    ann.id
                ));
            }
        } else {
            failures.push(format!(
                "[4] {} #[spec({:?})] is not followed by a `fn` definition",
                ann.file.display(),
                ann.id
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

    // Check 7 (PRD #77 Decision 30 / M4.3): every #[spec] test has
    // a `/// Scenario:` doc comment with a body AND
    // `cargo xtask docs --tests` succeeds against the current source
    // + catalog. The xtask-docs library raises `Err` on a missing
    // Scenario or a malformed test source, which is exactly the two
    // failure modes we want to surface here. The byte-identity check
    // against on-disk `.md` is gone in M4.3: `.dot-agent-deck/` is
    // gitignored, so on a fresh clone there is no `.md` to compare.
    let docs_config = xtask_docs::DocsConfig::from_workspace(root.clone());
    if let Err(e) = xtask_docs::check_rule_7(&docs_config) {
        failures.push(format!("[7] {e}"));
    }

    if failures.is_empty() {
        println!(
            "linkage-check: ok ({} catalog ids, {} annotations, {} allowlisted, 7 rules)",
            catalog_ids.len(),
            annotations.len(),
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

struct SpecAnnotation {
    id: String,
    file: PathBuf,
    fn_name: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
