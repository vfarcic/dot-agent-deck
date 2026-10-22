//! The deck never removes a dispatch claim (PRD #421 M3.3(b)).
//!
//! Issue dispatch labels a dispatched issue `agent-deck/claimed` and writes a
//! claimant comment, and by decision nothing ever takes either away: not on
//! tab close, not on a re-claim, not anywhere. A removal path is the
//! destructive operation that decision was made to exclude, so its absence is
//! held for the whole of `src/` rather than for the call sites that exist
//! today — a removal added anywhere later fails here.

use std::path::{Path, PathBuf};

/// Spellings of a label removal through `gh`: the `issue edit` / `pr edit`
/// flag, and the GraphQL mutation `removeLabelsFromLabelable`.
const LABEL_REMOVAL: &[&str] = &["remove-label", "removeLabels"];

/// The claim's own modules, where a `"DELETE"` string literal could only be a
/// comment or label deletion through `gh api --method`.
const CLAIM_MODULES: &[&str] = &["src/issue_dispatch.rs", "src/issue_dispatch_run.rs"];

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

#[test]
fn no_source_file_removes_a_label() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    files_under(&root.join("src"), &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("issue_dispatch_run.rs")),
        "the scan must reach the claim's own module"
    );
    let offenders: Vec<String> = files
        .iter()
        .filter_map(|file| {
            let text = std::fs::read_to_string(file).ok()?;
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
        "no code path may remove a label from an issue (PRD #421 M3.3(b)); found: {offenders:?}"
    );
}

#[test]
fn the_claim_modules_delete_nothing_through_gh_api() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for module in CLAIM_MODULES {
        let text = std::fs::read_to_string(root.join(module))
            .unwrap_or_else(|e| panic!("read {module}: {e}"));
        assert!(
            !text.contains("\"DELETE\""),
            "{module} must make no DELETE call: the claim comment and label are never removed \
             (PRD #421 M3.3(b))"
        );
    }
}
