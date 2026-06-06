//! PRD #77 Decision 31 — `cargo xtask list-tests`.
//!
//! Emits a Markdown report of every synthetic-test delta between the
//! current branch and `origin/main`. Used by the orchestrator before
//! delegating release (per Decision 31) so the user agrees with the
//! synthetic-test inventory before the merge, and by PR reviewers as
//! a one-command answer to "which tests changed in this branch?".
//!
//! The four sections always print, even when empty (`_(none)_`), so
//! the report's structure is stable for downstream consumers (the
//! orchestrator pastes it verbatim).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;

use xtask_docs::{CatalogEntry, DocsConfig, parse_catalog};

/// One synthetic test as observed at a single git ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestEntry {
    pub spec_id: String,
    pub fn_name: String,
    /// Path relative to the repo root, e.g. `tests/e2e_hook_delivery.rs`.
    pub file: String,
    /// `/// Scenario:` doc comment with paragraph breaks preserved.
    /// Empty string when the test has no Scenario comment (linkage-check
    /// rule 7 catches this separately).
    pub scenario: String,
    /// Stable fingerprint of the test function body — token stream
    /// serialized to text. Two functions with identical bodies produce
    /// the same fingerprint, so a Same-id-different-fingerprint pair
    /// flags a body modification.
    pub body_fingerprint: String,
}

/// What changed about an existing `#[spec]` test between merge-base
/// and HEAD. At least one of `scenario_changed` or `body_changed` is
/// always true for modified entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifiedRow {
    pub spec_id: String,
    pub fn_name: String,
    pub file: String,
    pub scenario_changed: bool,
    pub body_changed: bool,
}

/// One catalog entry whose prose body changed between refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProseRow {
    pub spec_id: String,
    /// Human-readable summary of which catalog fields changed
    /// (`headline`, `Asserts`, etc.).
    pub what_changed: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowlistChange {
    Added,
    Removed,
}

/// One allowlist line that was added or removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowlistRow {
    pub spec_id: String,
    pub change: AllowlistChange,
    /// Inline comment on the allowlist line, if any (the `# foo` tail).
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Public entrypoint
// ---------------------------------------------------------------------------

/// Build the synthetic-test inventory report against `origin/main`.
/// `workspace_root` is the repo root (where the workspace `Cargo.toml`
/// lives). Returns the rendered Markdown.
pub fn run(workspace_root: &Path) -> Result<String, String> {
    let merge_base = git_merge_base()?;

    let base_tests = collect_tests_at_ref(&merge_base)?;
    let head_tests = collect_tests_on_disk(workspace_root)?;

    let base_catalog = parse_catalog_at_ref(workspace_root, &merge_base)?;
    let head_catalog = parse_catalog_on_disk(workspace_root)?;

    let base_allowlist = read_allowlist_at_ref(&merge_base)?;
    let head_allowlist = read_allowlist_on_disk(workspace_root)?;

    let created = compute_created(&base_tests, &head_tests);
    let modified = compute_modified(&base_tests, &head_tests);
    let catalog_delta = compute_catalog_prose_delta(&base_catalog, &head_catalog);
    let allowlist_delta = compute_allowlist_delta(&base_allowlist, &head_allowlist);

    Ok(render_markdown(
        &created,
        &modified,
        &catalog_delta,
        &allowlist_delta,
        &head_catalog,
    ))
}

// ---------------------------------------------------------------------------
// Pure helpers — covered by unit tests in this file
// ---------------------------------------------------------------------------

/// IDs in `head` not in `base`. Sorted by spec_id.
pub fn compute_created(
    base: &BTreeMap<String, TestEntry>,
    head: &BTreeMap<String, TestEntry>,
) -> Vec<TestEntry> {
    let mut out: Vec<TestEntry> = head
        .iter()
        .filter(|(id, _)| !base.contains_key(*id))
        .map(|(_, t)| t.clone())
        .collect();
    out.sort_by(|a, b| a.spec_id.cmp(&b.spec_id));
    out
}

/// IDs in BOTH `base` and `head` where either the function body or the
/// Scenario doc comment differs. Sorted by spec_id.
pub fn compute_modified(
    base: &BTreeMap<String, TestEntry>,
    head: &BTreeMap<String, TestEntry>,
) -> Vec<ModifiedRow> {
    let mut out: Vec<ModifiedRow> = Vec::new();
    for (id, head_entry) in head {
        let Some(base_entry) = base.get(id) else {
            continue;
        };
        let scenario_changed = base_entry.scenario != head_entry.scenario;
        let body_changed = base_entry.body_fingerprint != head_entry.body_fingerprint;
        if scenario_changed || body_changed {
            out.push(ModifiedRow {
                spec_id: id.clone(),
                fn_name: head_entry.fn_name.clone(),
                file: head_entry.file.clone(),
                scenario_changed,
                body_changed,
            });
        }
    }
    out.sort_by(|a, b| a.spec_id.cmp(&b.spec_id));
    out
}

/// Catalog entries present in both refs whose prose body (any non-id
/// field) changed. Entries added or removed are NOT surfaced here —
/// those are captured by the test-side Created section (catalog adds
/// without a test trip the "catalog ID without test" rule 1 anyway).
pub fn compute_catalog_prose_delta(
    base: &BTreeMap<String, CatalogEntry>,
    head: &BTreeMap<String, CatalogEntry>,
) -> Vec<CatalogProseRow> {
    let mut out: Vec<CatalogProseRow> = Vec::new();
    for (id, head_entry) in head {
        let Some(base_entry) = base.get(id) else {
            continue;
        };
        let mut diffs: Vec<&str> = Vec::new();
        if base_entry.headline != head_entry.headline {
            diffs.push("headline");
        }
        if base_entry.layer != head_entry.layer {
            diffs.push("Layer");
        }
        if base_entry.agent != head_entry.agent {
            diffs.push("Agent");
        }
        if base_entry.asserts != head_entry.asserts {
            diffs.push("Asserts");
        }
        if base_entry.does_not_assert != head_entry.does_not_assert {
            diffs.push("Does not assert");
        }
        if base_entry.platform_coverage != head_entry.platform_coverage {
            diffs.push("Platform coverage");
        }
        if base_entry.cost_note != head_entry.cost_note {
            diffs.push("Cost note");
        }
        if diffs.is_empty() {
            continue;
        }
        out.push(CatalogProseRow {
            spec_id: id.clone(),
            what_changed: diffs.join(", "),
        });
    }
    out.sort_by(|a, b| a.spec_id.cmp(&b.spec_id));
    out
}

/// Lines added or removed in `xtask/linkage-check/m2.allowlist`. A
/// catalog ID promoted off the allowlist (because a test landed in
/// the branch) shows up as Removed; a new allowlist entry shows up as
/// Added.
pub fn compute_allowlist_delta(base: &str, head: &str) -> Vec<AllowlistRow> {
    let base_set: BTreeMap<String, Option<String>> = parse_allowlist(base);
    let head_set: BTreeMap<String, Option<String>> = parse_allowlist(head);

    let mut out: Vec<AllowlistRow> = Vec::new();
    for (id, reason) in &head_set {
        if !base_set.contains_key(id) {
            out.push(AllowlistRow {
                spec_id: id.clone(),
                change: AllowlistChange::Added,
                reason: reason.clone(),
            });
        }
    }
    for (id, reason) in &base_set {
        if !head_set.contains_key(id) {
            out.push(AllowlistRow {
                spec_id: id.clone(),
                change: AllowlistChange::Removed,
                reason: reason.clone(),
            });
        }
    }
    out.sort_by(|a, b| match a.spec_id.cmp(&b.spec_id) {
        std::cmp::Ordering::Equal => a.change.cmp(&b.change),
        other => other,
    });
    out
}

/// Parse an allowlist text body into `id -> Option<reason-comment>`.
/// Lines may have an inline `# foo` comment after the id; both halves
/// are preserved. Blank lines and full-line comments are ignored.
fn parse_allowlist(text: &str) -> BTreeMap<String, Option<String>> {
    let mut out: BTreeMap<String, Option<String>> = BTreeMap::new();
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((id_part, comment_part)) = trimmed.split_once('#') {
            let id = id_part.trim().to_string();
            let reason = comment_part.trim().to_string();
            if !id.is_empty() {
                out.insert(
                    id,
                    if reason.is_empty() {
                        None
                    } else {
                        Some(reason)
                    },
                );
            }
        } else {
            out.insert(trimmed.to_string(), None);
        }
    }
    out
}

impl PartialOrd for AllowlistChange {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AllowlistChange {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn ordinal(c: &AllowlistChange) -> u8 {
            match c {
                AllowlistChange::Added => 0,
                AllowlistChange::Removed => 1,
            }
        }
        ordinal(self).cmp(&ordinal(other))
    }
}

// ---------------------------------------------------------------------------
// syn-based source parsing (testable via collect_tests_from_sources)
// ---------------------------------------------------------------------------

/// Parse a set of `(file_path, source_text)` pairs into the per-id
/// inventory. The path is preserved verbatim so it can be displayed
/// in the rendered report.
pub fn collect_tests_from_sources(
    sources: &[(String, String)],
) -> Result<BTreeMap<String, TestEntry>, String> {
    let mut out: BTreeMap<String, TestEntry> = BTreeMap::new();
    for (path, source) in sources {
        let parsed = match syn::parse_file(source) {
            Ok(p) => p,
            Err(e) => return Err(format!("parse {path}: {e}")),
        };
        for item in &parsed.items {
            if let syn::Item::Fn(item_fn) = item
                && let Some(spec_id) = read_spec_attr(&item_fn.attrs)
            {
                let fn_name = item_fn.sig.ident.to_string();
                let scenario = read_scenario_doc(&item_fn.attrs).unwrap_or_default();
                let body_fingerprint = fingerprint_block(&item_fn.block);
                out.insert(
                    spec_id.clone(),
                    TestEntry {
                        spec_id,
                        fn_name,
                        file: path.clone(),
                        scenario,
                        body_fingerprint,
                    },
                );
            }
        }
    }
    Ok(out)
}

fn read_spec_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("spec") {
            continue;
        }
        let parsed: Result<syn::LitStr, _> = attr.parse_args();
        if let Ok(lit) = parsed {
            return Some(lit.value());
        }
    }
    None
}

fn read_scenario_doc(attrs: &[syn::Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .filter_map(|a| {
            let mv: syn::MetaNameValue = a.meta.require_name_value().ok().cloned()?;
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = mv.value
            {
                Some(s.value())
            } else {
                None
            }
        })
        .collect();
    let scenario_marker = Regex::new(r"(?i)^\s*scenario(?:\s*:|\s+|\s*$)").expect("scenario regex");
    let start = lines.iter().position(|l| scenario_marker.is_match(l))?;
    let first_line = scenario_marker
        .replace(&lines[start], "")
        .trim()
        .to_string();
    let mut current: Vec<String> = Vec::new();
    if !first_line.is_empty() {
        current.push(first_line);
    }
    for line in lines.iter().skip(start + 1) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if scenario_marker.is_match(line) {
            break;
        }
        current.push(trimmed.to_string());
    }
    if current.is_empty() {
        None
    } else {
        Some(current.join(" "))
    }
}

fn fingerprint_block(block: &syn::Block) -> String {
    use quote_compat::quote_to_string;
    quote_to_string(block)
}

/// Minimal `quote!`-free token-to-string helper for fingerprinting.
/// `syn`'s `Block` doesn't expose its source bytes directly; we walk
/// the underlying token stream and serialize via `Display`. Two
/// functionally-identical bodies stringify identically; whitespace
/// only differences vanish (token tree is whitespace-insensitive).
mod quote_compat {
    use proc_macro2::TokenStream;
    use syn::__private::ToTokens;

    pub fn quote_to_string<T: ToTokens>(value: &T) -> String {
        let mut ts = TokenStream::new();
        value.to_tokens(&mut ts);
        ts.to_string()
    }
}

/// First sentence of a scenario doc comment, for the Created-row
/// `Scenario` column. Falls back to the whole scenario if there's no
/// `.` boundary.
pub fn first_sentence(scenario: &str) -> String {
    if scenario.is_empty() {
        return "(missing /// Scenario:)".to_string();
    }
    match scenario.find(". ") {
        Some(idx) => scenario[..idx + 1].to_string(),
        None => scenario.to_string(),
    }
}

/// Derive a one-word layer label from the spec id + catalog entry.
pub fn layer_label(spec_id: &str, catalog_entry: Option<&CatalogEntry>) -> String {
    if let Some(entry) = catalog_entry
        && let Some(layer) = entry.layer.as_deref()
        && layer.to_lowercase().contains("l1")
    {
        return "L1".to_string();
    }
    if spec_id.starts_with("chain-smoke/") {
        return "chain-smoke".to_string();
    }
    "L2 synthetic".to_string()
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

pub fn render_markdown(
    created: &[TestEntry],
    modified: &[ModifiedRow],
    catalog_delta: &[CatalogProseRow],
    allowlist_delta: &[AllowlistRow],
    head_catalog: &BTreeMap<String, CatalogEntry>,
) -> String {
    let mut s = String::new();
    s.push_str("# Synthetic-test inventory\n\n");

    s.push_str("## Created in this branch\n\n");
    if created.is_empty() {
        s.push_str("_(none)_\n\n");
    } else {
        s.push_str("| Catalog ID | Layer | Function | File | Scenario |\n");
        s.push_str("|---|---|---|---|---|\n");
        for t in created {
            let layer = layer_label(&t.spec_id, head_catalog.get(&t.spec_id));
            s.push_str(&format!(
                "| {} | {} | `{}` | `{}` | {} |\n",
                t.spec_id,
                layer,
                t.fn_name,
                t.file,
                escape_table_cell(&first_sentence(&t.scenario)),
            ));
        }
        s.push('\n');
    }

    s.push_str("## Modified in this branch\n\n");
    if modified.is_empty() {
        s.push_str("_(none)_\n\n");
    } else {
        s.push_str("| Catalog ID | Function | File | What changed |\n");
        s.push_str("|---|---|---|---|\n");
        for m in modified {
            let mut what: Vec<&str> = Vec::new();
            if m.scenario_changed {
                what.push("Scenario");
            }
            if m.body_changed {
                what.push("body");
            }
            s.push_str(&format!(
                "| {} | `{}` | `{}` | {} |\n",
                m.spec_id,
                m.fn_name,
                m.file,
                what.join(", "),
            ));
        }
        s.push('\n');
    }

    s.push_str("## Catalog entries with prose changes\n\n");
    if catalog_delta.is_empty() {
        s.push_str("_(none)_\n\n");
    } else {
        s.push_str("| Catalog ID | What changed |\n");
        s.push_str("|---|---|\n");
        for c in catalog_delta {
            s.push_str(&format!(
                "| {} | {} |\n",
                c.spec_id,
                escape_table_cell(&c.what_changed),
            ));
        }
        s.push('\n');
    }

    s.push_str("## Linkage-allowlist deltas\n\n");
    if allowlist_delta.is_empty() {
        s.push_str("_(none)_\n");
    } else {
        s.push_str("| Catalog ID | Change | Reason |\n");
        s.push_str("|---|---|---|\n");
        for a in allowlist_delta {
            let change = match a.change {
                AllowlistChange::Added => "added",
                AllowlistChange::Removed => "removed",
            };
            let reason = a.reason.as_deref().unwrap_or("");
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                a.spec_id,
                change,
                escape_table_cell(reason),
            ));
        }
    }

    s
}

fn escape_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace('\n', " ")
}

// ---------------------------------------------------------------------------
// I/O — git + filesystem
// ---------------------------------------------------------------------------

fn git_merge_base() -> Result<String, String> {
    let out = Command::new("git")
        .args(["merge-base", "HEAD", "origin/main"])
        .output()
        .map_err(|e| format!("invoke git merge-base: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git merge-base HEAD origin/main failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if sha.is_empty() {
        return Err("git merge-base returned empty output".to_string());
    }
    Ok(sha)
}

fn git_show(reference: &str, path: &str) -> Result<String, String> {
    let out = Command::new("git")
        .args(["show", &format!("{reference}:{path}")])
        .output()
        .map_err(|e| format!("invoke git show {reference}:{path}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git show {reference}:{path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_ls_tree(reference: &str, path: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args(["ls-tree", "-r", "--name-only", reference, path])
        .output()
        .map_err(|e| format!("invoke git ls-tree {reference} {path}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-tree {reference} {path} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect())
}

fn collect_tests_at_ref(reference: &str) -> Result<BTreeMap<String, TestEntry>, String> {
    let files = git_ls_tree(reference, "tests")?;
    let mut sources: Vec<(String, String)> = Vec::new();
    for f in files {
        if !f.ends_with(".rs") {
            continue;
        }
        // Skip the harness module itself — it carries no #[spec] but
        // is huge and slows the parse for no reason.
        if f == "tests/common/mod.rs" {
            continue;
        }
        let body = git_show(reference, &f)?;
        sources.push((f, body));
    }
    collect_tests_from_sources(&sources)
}

fn collect_tests_on_disk(root: &Path) -> Result<BTreeMap<String, TestEntry>, String> {
    let tests_dir = root.join("tests");
    let mut sources: Vec<(String, String)> = Vec::new();
    walk_rs_files(&tests_dir, &mut |abs_path| {
        if abs_path.ends_with("common/mod.rs") {
            return Ok(());
        }
        let rel = abs_path
            .strip_prefix(root)
            .map_err(|e| format!("strip prefix {}: {e}", abs_path.display()))?
            .to_string_lossy()
            .into_owned();
        let body = std::fs::read_to_string(abs_path)
            .map_err(|e| format!("read {}: {e}", abs_path.display()))?;
        sources.push((rel, body));
        Ok(())
    })?;
    collect_tests_from_sources(&sources)
}

fn walk_rs_files(
    dir: &Path,
    visit: &mut dyn FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read_dir {}: {e}", dir.display())),
    };
    for entry in rd {
        let entry = entry.map_err(|e| format!("read_dir entry: {e}"))?;
        let p = entry.path();
        let meta = std::fs::symlink_metadata(&p)
            .map_err(|e| format!("symlink_metadata {}: {e}", p.display()))?;
        let ft = meta.file_type();
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            walk_rs_files(&p, visit)?;
        } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rs") {
            visit(&p)?;
        }
    }
    Ok(())
}

fn parse_catalog_at_ref(
    workspace_root: &Path,
    reference: &str,
) -> Result<BTreeMap<String, CatalogEntry>, String> {
    let prd_rel = "prds/77-tui-testing-harness.md";
    let body = git_show(reference, prd_rel)?;
    // parse_catalog wants a file path; stage the body in a tempfile.
    let tmp = tempfile_for_catalog(workspace_root, &body)?;
    let result = parse_catalog(&tmp);
    let _ = std::fs::remove_file(&tmp);
    result
}

fn parse_catalog_on_disk(workspace_root: &Path) -> Result<BTreeMap<String, CatalogEntry>, String> {
    let config = DocsConfig::from_workspace(workspace_root);
    parse_catalog(&config.prd_path)
}

fn tempfile_for_catalog(workspace_root: &Path, body: &str) -> Result<PathBuf, String> {
    let dir = workspace_root.join("target");
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "xtask-list-tests-catalog-{}-{nanos}.md",
        std::process::id()
    ));
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

fn read_allowlist_at_ref(reference: &str) -> Result<String, String> {
    let path = "xtask/linkage-check/m2.allowlist";
    // A branch where the allowlist was removed entirely would error
    // here, but the path is load-bearing so we treat the failure as
    // an empty allowlist rather than a fatal.
    match git_show(reference, path) {
        Ok(s) => Ok(s),
        Err(_) => Ok(String::new()),
    }
}

fn read_allowlist_on_disk(workspace_root: &Path) -> Result<String, String> {
    let path = workspace_root
        .join("xtask")
        .join("linkage-check")
        .join("m2.allowlist");
    match std::fs::read_to_string(&path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(spec_id: &str, fn_name: &str, scenario: &str, body: &str) -> TestEntry {
        TestEntry {
            spec_id: spec_id.to_string(),
            fn_name: fn_name.to_string(),
            file: format!("tests/e2e_{}.rs", spec_id.replace('/', "_")),
            scenario: scenario.to_string(),
            body_fingerprint: body.to_string(),
        }
    }

    fn cat_entry(id: &str, headline: &str, asserts: &str) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            headline: headline.to_string(),
            layer: Some("L2.".to_string()),
            agent: None,
            asserts: Some(asserts.to_string()),
            does_not_assert: None,
            platform_coverage: None,
            cost_note: None,
        }
    }

    #[test]
    fn created_section_lists_only_ids_new_in_head() {
        let mut base: BTreeMap<String, TestEntry> = BTreeMap::new();
        base.insert(
            "hooks/delivery/001".to_string(),
            entry("hooks/delivery/001", "delivery_001_x", "x", "body-a"),
        );
        let mut head: BTreeMap<String, TestEntry> = base.clone();
        head.insert(
            "dashboard/pane/005".to_string(),
            entry(
                "dashboard/pane/005",
                "pane_005_y",
                "Render a card",
                "body-b",
            ),
        );
        head.insert(
            "chain-smoke/claude/002".to_string(),
            entry(
                "chain-smoke/claude/002",
                "claude_002_z",
                "Drive Claude end to end",
                "body-c",
            ),
        );

        let created = compute_created(&base, &head);
        // hooks/delivery/001 is in both → not Created.
        // Sorted by spec_id: chain-smoke/claude/002 comes before
        // dashboard/pane/005.
        assert_eq!(created.len(), 2);
        assert_eq!(created[0].spec_id, "chain-smoke/claude/002");
        assert_eq!(created[1].spec_id, "dashboard/pane/005");
    }

    #[test]
    fn modified_section_lists_ids_with_changed_body_or_scenario() {
        let mut base: BTreeMap<String, TestEntry> = BTreeMap::new();
        base.insert(
            "hooks/delivery/001".to_string(),
            entry(
                "hooks/delivery/001",
                "delivery_001_x",
                "Original.",
                "body-a",
            ),
        );
        base.insert(
            "hooks/delivery/002".to_string(),
            entry("hooks/delivery/002", "delivery_002_x", "Same.", "body-x"),
        );
        base.insert(
            "hooks/delivery/003".to_string(),
            entry("hooks/delivery/003", "delivery_003_x", "Same.", "body-y"),
        );

        let mut head: BTreeMap<String, TestEntry> = base.clone();
        // Scenario change only.
        head.get_mut("hooks/delivery/001").unwrap().scenario = "Updated narrative.".to_string();
        // Body change only.
        head.get_mut("hooks/delivery/002").unwrap().body_fingerprint = "body-x-changed".to_string();
        // delivery/003 unchanged.

        let modified = compute_modified(&base, &head);
        assert_eq!(modified.len(), 2);
        assert_eq!(modified[0].spec_id, "hooks/delivery/001");
        assert!(modified[0].scenario_changed);
        assert!(!modified[0].body_changed);
        assert_eq!(modified[1].spec_id, "hooks/delivery/002");
        assert!(!modified[1].scenario_changed);
        assert!(modified[1].body_changed);
    }

    #[test]
    fn catalog_prose_delta_flags_changed_fields() {
        let mut base: BTreeMap<String, CatalogEntry> = BTreeMap::new();
        base.insert(
            "hooks/delivery/001".to_string(),
            cat_entry(
                "hooks/delivery/001",
                "Old headline",
                "Asserts the old behavior",
            ),
        );
        base.insert(
            "dashboard/pane/004".to_string(),
            cat_entry("dashboard/pane/004", "Card title row", "Renders cleanly"),
        );

        let mut head: BTreeMap<String, CatalogEntry> = base.clone();
        // Change headline AND asserts on the first entry.
        head.get_mut("hooks/delivery/001").unwrap().headline = "New headline".to_string();
        head.get_mut("hooks/delivery/001").unwrap().asserts =
            Some("Asserts the new behavior".to_string());
        // dashboard/pane/004 unchanged.

        let delta = compute_catalog_prose_delta(&base, &head);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].spec_id, "hooks/delivery/001");
        assert!(delta[0].what_changed.contains("headline"));
        assert!(delta[0].what_changed.contains("Asserts"));
    }

    #[test]
    fn allowlist_delta_lists_additions_and_removals() {
        let base = "\
            # comment line
            hooks/delivery/001
            hooks/delivery/002  # parked for M3
            dashboard/pane/005
        ";
        let head = "\
            # comment line
            hooks/delivery/002  # parked for M3
            dashboard/pane/006  # new entry M4
            chain-smoke/opencode/001  # blocked on deck plugin
        ";

        let delta = compute_allowlist_delta(base, head);
        let added: Vec<&AllowlistRow> = delta
            .iter()
            .filter(|r| matches!(r.change, AllowlistChange::Added))
            .collect();
        let removed: Vec<&AllowlistRow> = delta
            .iter()
            .filter(|r| matches!(r.change, AllowlistChange::Removed))
            .collect();
        // Added: chain-smoke/opencode/001, dashboard/pane/006 (sorted).
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].spec_id, "chain-smoke/opencode/001");
        assert_eq!(added[0].reason.as_deref(), Some("blocked on deck plugin"));
        assert_eq!(added[1].spec_id, "dashboard/pane/006");
        // Removed: dashboard/pane/005, hooks/delivery/001 (sorted).
        assert_eq!(removed.len(), 2);
        assert_eq!(removed[0].spec_id, "dashboard/pane/005");
        assert_eq!(removed[1].spec_id, "hooks/delivery/001");
    }

    #[test]
    fn render_markdown_emits_none_for_empty_sections() {
        let empty_catalog: BTreeMap<String, CatalogEntry> = BTreeMap::new();
        let report = render_markdown(&[], &[], &[], &[], &empty_catalog);
        assert!(report.starts_with("# Synthetic-test inventory"));
        assert!(report.contains("## Created in this branch\n\n_(none)_"));
        assert!(report.contains("## Modified in this branch\n\n_(none)_"));
        assert!(report.contains("## Catalog entries with prose changes\n\n_(none)_"));
        assert!(report.contains("## Linkage-allowlist deltas\n\n_(none)_"));
    }

    #[test]
    fn render_markdown_populates_created_table_with_layer_and_scenario() {
        let mut head_catalog: BTreeMap<String, CatalogEntry> = BTreeMap::new();
        head_catalog.insert(
            "dashboard/pane/004".to_string(),
            CatalogEntry {
                id: "dashboard/pane/004".into(),
                headline: "Card title row".into(),
                layer: Some("L1 (ratatui TestBackend + insta).".into()),
                agent: None,
                asserts: None,
                does_not_assert: None,
                platform_coverage: None,
                cost_note: None,
            },
        );
        let created = vec![TestEntry {
            spec_id: "dashboard/pane/004".into(),
            fn_name: "pane_004_card_title_row".into(),
            file: "tests/render_dashboard.rs".into(),
            scenario: "Render a single dashboard card. Pin it.".into(),
            body_fingerprint: "fp".into(),
        }];
        let report = render_markdown(&created, &[], &[], &[], &head_catalog);
        assert!(report.contains("| dashboard/pane/004 | L1 |"));
        assert!(report.contains("`pane_004_card_title_row`"));
        assert!(report.contains("Render a single dashboard card."));
    }

    #[test]
    fn collect_tests_from_sources_extracts_spec_id_and_scenario() {
        let src = r#"
            #[spec("hooks/delivery/001")]
            #[test]
            /// Scenario: A short, in-process check.
            fn delivery_001_x() {
                let x = 1 + 1;
                assert_eq!(x, 2);
            }
        "#;
        let sources = vec![("tests/e2e_x.rs".to_string(), src.to_string())];
        let map = collect_tests_from_sources(&sources).expect("parses");
        assert_eq!(map.len(), 1);
        let t = &map["hooks/delivery/001"];
        assert_eq!(t.fn_name, "delivery_001_x");
        assert_eq!(t.scenario, "A short, in-process check.");
        assert!(!t.body_fingerprint.is_empty());
    }

    #[test]
    fn layer_label_picks_l1_chain_smoke_or_l2_synthetic() {
        let mut catalog: BTreeMap<String, CatalogEntry> = BTreeMap::new();
        catalog.insert(
            "dashboard/pane/004".into(),
            CatalogEntry {
                id: "dashboard/pane/004".into(),
                headline: "x".into(),
                layer: Some("L1 (ratatui).".into()),
                agent: None,
                asserts: None,
                does_not_assert: None,
                platform_coverage: None,
                cost_note: None,
            },
        );
        assert_eq!(
            layer_label("dashboard/pane/004", catalog.get("dashboard/pane/004")),
            "L1"
        );
        assert_eq!(layer_label("chain-smoke/claude/001", None), "chain-smoke");
        assert_eq!(layer_label("hooks/delivery/001", None), "L2 synthetic");
    }
}
