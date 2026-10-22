//! No source file under `src/` spells a label removal or a REST deletion
//! (PRD #421 M3.3(b)).
//!
//! Issue dispatch labels a dispatched issue `agent-deck/claimed` and writes a
//! claimant comment, and by decision nothing takes either away: not on tab
//! close, not on a re-claim. This guard is the syntactic half of holding that
//! decision: it scans the text of every file under `src/` for the spellings
//! below, so a removal written with one of them anywhere later fails here.
//!
//! It checks spellings, not behaviour. A removal built from parts at run time
//! (a method name formatted into an argv, a GraphQL document assembled from
//! fragments) carries none of these strings and passes it; the e2e tests that
//! read the GitHub stub's state after a re-claim and a tab close are what
//! observe the behaviour.

use std::path::{Path, PathBuf};

/// Spellings of a label removal through `gh`: the `issue edit` / `pr edit`
/// flag, and the GraphQL mutation `removeLabelsFromLabelable`.
const LABEL_REMOVAL: &[&str] = &["remove-label", "removeLabels"];

/// Spellings of a REST deletion through `gh api` (or any HTTP client): the
/// method as a string literal — `"DELETE"`, which is how an argv element or a
/// method constant spells it — and the method flags written inline in a
/// string or a comment (`--method DELETE`, `--method=DELETE`, `-X DELETE`,
/// `-XDELETE`), in any letter case, since `gh` accepts the method in any case.
/// The literal is matched case-sensitively: `"delete"` and `"Delete"` are key
/// names elsewhere in `src/`.
const REST_DELETION: &[&str] = &[r#""DELETE""#, r#"(?i)(?:--method|-X)[\s=]*"?delete\b"#];

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}")) {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Every file under `src/` with its text, after checking the scan reaches the
/// claim's own module (a moved tree would otherwise pass vacuously).
fn source_files() -> Vec<(PathBuf, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    files_under(&root.join("src"), &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("issue_dispatch_run.rs")),
        "the scan must reach the claim's own module"
    );
    files
        .into_iter()
        .filter_map(|file| {
            let text = std::fs::read_to_string(&file).ok()?;
            Some((file, text))
        })
        .collect()
}

#[test]
fn no_source_file_spells_a_label_removal() {
    let offenders: Vec<String> = source_files()
        .iter()
        .filter_map(|(file, text)| {
            let hits: Vec<&str> = LABEL_REMOVAL
                .iter()
                .copied()
                .filter(|needle| text.contains(needle))
                .collect();
            (!hits.is_empty()).then(|| format!("{}: {hits:?}", file.display()))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "no file under src/ may spell a label removal (PRD #421 M3.3(b)); found: {offenders:?}"
    );
}

#[test]
fn no_source_file_spells_a_rest_deletion() {
    let patterns: Vec<regex::Regex> = REST_DELETION
        .iter()
        .map(|pattern| regex::Regex::new(pattern).expect("valid pattern"))
        .collect();
    // The patterns must catch every spelling they claim to.
    for spelling in [
        r#"["api", "--method", "DELETE", "repos/o/r/issues/comments/1"]"#,
        "gh api --method DELETE repos/o/r/issues/comments/1",
        "gh api --method=DELETE repos/o/r/issues/comments/1",
        "gh api --method delete repos/o/r/issues/comments/1",
        "gh api -X DELETE repos/o/r/labels/x",
        "gh api -XDELETE repos/o/r/labels/x",
        r#"vec!["-X", "DELETE"]"#,
    ] {
        assert!(
            patterns.iter().any(|p| p.is_match(spelling)),
            "the guard must catch {spelling:?}"
        );
    }
    let patterns = &patterns;
    let offenders: Vec<String> = source_files()
        .iter()
        .flat_map(|(file, text)| {
            text.lines()
                .enumerate()
                .filter(|(_, line)| patterns.iter().any(|p| p.is_match(line)))
                .map(move |(n, line)| format!("{}:{}: {}", file.display(), n + 1, line.trim()))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "no file under src/ may spell a REST DELETE: the claim comment and label are never \
         removed (PRD #421 M3.3(b)); found: {offenders:?}"
    );
}
