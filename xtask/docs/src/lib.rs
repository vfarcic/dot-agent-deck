//! PRD #77 Decision 30 — auto-generate a Markdown doc next to every
//! `#[spec]`-annotated test under `.dot-agent-deck/<milestone>-recordings/`.
//!
//! The generator is deterministic: running it twice on unchanged
//! inputs produces byte-identical output. The CI linkage-check rule
//! 7 leans on that — it re-runs the generator into a tempdir and
//! diffs against the on-disk file.
//!
//! Inputs:
//!   - The catalog under `prds/77-tui-testing-harness.md` (parsed
//!     for entry headlines + body fields).
//!   - Every `#[spec("…")]`-annotated test fn under `tests/` (parsed
//!     via `syn` for fn name, doc comments, and statement-level
//!     method calls on the harness builder / handle).
//!
//! Output:
//!   - One `.md` per `#[spec]` test, written under the recordings
//!     dir mapped by milestone (m2 for the seed tests,
//!     m3 for chain-smoke, future milestones add their own).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use regex::Regex;
use syn::spanned::Spanned;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Configuration the binary + harness pass in: where to read from
/// and where to write to. Absolute paths so cwd doesn't matter.
#[derive(Debug, Clone)]
pub struct DocsConfig {
    pub workspace_root: PathBuf,
    pub prd_path: PathBuf,
    pub tests_dir: PathBuf,
    /// `.dot-agent-deck` — the parent of every `<milestone>-recordings/`.
    pub recordings_root: PathBuf,
}

impl DocsConfig {
    /// Build the canonical config rooted at `workspace_root`.
    pub fn from_workspace(workspace_root: impl Into<PathBuf>) -> Self {
        let workspace_root = workspace_root.into();
        Self {
            prd_path: workspace_root.join("prds/77-tui-testing-harness.md"),
            tests_dir: workspace_root.join("tests"),
            recordings_root: workspace_root.join(".dot-agent-deck"),
            workspace_root,
        }
    }
}

/// One generated doc — its destination path and the byte contents
/// the linkage-check rule 7 diff compares against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedDoc {
    pub spec_id: String,
    pub fn_name: String,
    pub source_path: PathBuf,
    pub output_path: PathBuf,
    pub content: String,
}

/// A `#[spec("…")]` test found in `tests/`. Public so the harness
/// can pass a single spec id straight into [`generate_for_spec`].
#[derive(Debug, Clone)]
pub struct DiscoveredTest {
    pub spec_id: String,
    pub fn_name: String,
    pub source_path: PathBuf,
    pub scenario: Option<String>,
    pub steps: Vec<String>,
}

/// Generate the `.md` content for every `#[spec]` test under
/// `tests/`. Sorted deterministically by `spec_id` so the output
/// order is stable.
pub fn generate_all(config: &DocsConfig) -> Result<Vec<GeneratedDoc>, String> {
    let catalog = parse_catalog(&config.prd_path)?;
    let mut tests = discover_tests(&config.tests_dir)?;
    tests.sort_by(|a, b| a.spec_id.cmp(&b.spec_id));
    let mut out: Vec<GeneratedDoc> = Vec::with_capacity(tests.len());
    for t in &tests {
        let entry = catalog.get(&t.spec_id).ok_or_else(|| {
            format!(
                "test `{}` in {} carries #[spec({:?})] which is not in the catalog",
                t.fn_name,
                t.source_path.display(),
                t.spec_id,
            )
        })?;
        let scenario = t.scenario.as_deref().unwrap_or("").trim();
        if scenario.is_empty() {
            return Err(format!(
                "test `{}` in {} is missing a `/// Scenario:` doc comment with a body \
                 (PRD #77 Decision 30 / linkage-check rule 7)",
                t.fn_name,
                t.source_path.display(),
            ));
        }
        let output_path = resolve_output_path(&config.recordings_root, &t.spec_id, &t.fn_name);
        let cast_name = format!("{}.cast", t.fn_name);
        let content = render_markdown(t, entry, scenario, &cast_name);
        out.push(GeneratedDoc {
            spec_id: t.spec_id.clone(),
            fn_name: t.fn_name.clone(),
            source_path: t.source_path.clone(),
            output_path,
            content,
        });
    }
    Ok(out)
}

/// Generate exactly one doc, by spec id. Used by the harness's
/// regen-on-record hook so we don't re-parse all tests for one
/// captured run. Returns `Ok(None)` when the spec id has no
/// matching test (forgiving: harness doesn't want to noisily fail
/// during a test run).
pub fn generate_for_spec(
    config: &DocsConfig,
    spec_id: &str,
) -> Result<Option<GeneratedDoc>, String> {
    let all = generate_all(config)?;
    Ok(all.into_iter().find(|d| d.spec_id == spec_id))
}

/// Write every generated doc to disk under its `output_path`. Creates
/// parent dirs as needed. Returns the list of paths written.
pub fn write_all(generated: &[GeneratedDoc]) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::with_capacity(generated.len());
    for g in generated {
        if let Some(parent) = g.output_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create parent dir {}: {e}", parent.display()))?;
        }
        std::fs::write(&g.output_path, &g.content)
            .map_err(|e| format!("write {}: {e}", g.output_path.display()))?;
        paths.push(g.output_path.clone());
    }
    Ok(paths)
}

/// Compare the on-disk `.md` for every `#[spec]` test to a fresh
/// generation. Returns the list of out-of-sync paths (empty == all
/// in sync). Linkage-check rule 7 fails when this returns a
/// non-empty Vec.
pub fn check_in_sync(config: &DocsConfig) -> Result<Vec<PathBuf>, String> {
    let generated = generate_all(config)?;
    let mut drift: Vec<PathBuf> = Vec::new();
    for g in &generated {
        match std::fs::read_to_string(&g.output_path) {
            Ok(on_disk) if on_disk == g.content => {}
            Ok(_) => drift.push(g.output_path.clone()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                drift.push(g.output_path.clone());
            }
            Err(e) => {
                return Err(format!("read {}: {e}", g.output_path.display()));
            }
        }
    }
    Ok(drift)
}

// ---------------------------------------------------------------------------
// Catalog parsing
// ---------------------------------------------------------------------------

/// Single catalog entry parsed from the PRD's `## Test Case Catalog`
/// block. The fields mirror the bullets every entry carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub id: String,
    pub headline: String,
    pub layer: Option<String>,
    pub agent: Option<String>,
    pub asserts: Option<String>,
    pub does_not_assert: Option<String>,
    pub platform_coverage: Option<String>,
    pub cost_note: Option<String>,
}

/// Parse every `##### <id> — <headline>` heading + its bullet lines
/// out of the PRD's `## Test Case Catalog` section. Returns a map
/// keyed by catalog id.
pub fn parse_catalog(prd_path: &Path) -> Result<BTreeMap<String, CatalogEntry>, String> {
    let text = std::fs::read_to_string(prd_path)
        .map_err(|e| format!("read {}: {e}", prd_path.display()))?;
    let header_re = Regex::new(r"^#####\s+([a-z][a-z0-9-]*/[a-z][a-z0-9-]*/\d{3})\s*[—-]\s*(.+)$")
        .expect("catalog header regex compiles");
    let bullet_re =
        Regex::new(r"^- \*\*([^*]+):\*\*\s*(.+)$").expect("catalog bullet regex compiles");

    let mut out: BTreeMap<String, CatalogEntry> = BTreeMap::new();
    let mut in_catalog = false;
    let mut current: Option<CatalogEntry> = None;
    for line in text.lines() {
        if line.starts_with("## ") {
            // Section boundary — flush whatever entry is open before
            // we change in/out of catalog.
            if let Some(entry) = current.take() {
                out.insert(entry.id.clone(), entry);
            }
            in_catalog = line.starts_with("## Test Case Catalog");
            continue;
        }
        if !in_catalog {
            continue;
        }
        if let Some(caps) = header_re.captures(line) {
            if let Some(entry) = current.take() {
                out.insert(entry.id.clone(), entry);
            }
            current = Some(CatalogEntry {
                id: caps.get(1).unwrap().as_str().to_string(),
                headline: caps.get(2).unwrap().as_str().trim().to_string(),
                layer: None,
                agent: None,
                asserts: None,
                does_not_assert: None,
                platform_coverage: None,
                cost_note: None,
            });
            continue;
        }
        if let Some(caps) = bullet_re.captures(line)
            && let Some(entry) = current.as_mut()
        {
            let key = caps.get(1).unwrap().as_str().trim().to_lowercase();
            let value = caps.get(2).unwrap().as_str().trim().to_string();
            match key.as_str() {
                "layer" => entry.layer = Some(value),
                "agent" => entry.agent = Some(value),
                "asserts" => entry.asserts = Some(value),
                "does not assert" => entry.does_not_assert = Some(value),
                "platform coverage" => entry.platform_coverage = Some(value),
                "cost note" => entry.cost_note = Some(value),
                _ => {}
            }
        }
    }
    if let Some(entry) = current.take() {
        out.insert(entry.id.clone(), entry);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Test source discovery
// ---------------------------------------------------------------------------

/// Walk `tests/` recursively and return every `#[spec("…")]`-annotated
/// test fn we find, sorted by spec id for determinism.
pub fn discover_tests(tests_dir: &Path) -> Result<Vec<DiscoveredTest>, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_rs_files(tests_dir, &mut files);
    files.sort();
    let mut out: Vec<DiscoveredTest> = Vec::new();
    for path in files {
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                return Err(format!("read test source {}: {e}", path.display()));
            }
        };
        let parsed = match syn::parse_file(&source) {
            Ok(p) => p,
            Err(e) => {
                return Err(format!("parse test source {}: {e}", path.display()));
            }
        };
        for item in &parsed.items {
            if let syn::Item::Fn(item_fn) = item
                && let Some(spec_id) = read_spec_attr(&item_fn.attrs)
            {
                let fn_name = item_fn.sig.ident.to_string();
                let scenario = read_scenario_doc(&item_fn.attrs);
                let steps = extract_steps_from_body(&item_fn.block);
                out.push(DiscoveredTest {
                    spec_id,
                    fn_name,
                    source_path: path.clone(),
                    scenario,
                    steps,
                });
            }
        }
    }
    Ok(out)
}

fn collect_rs_files(dir: &Path, acc: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            collect_rs_files(&p, acc);
        } else if ft.is_file() && p.extension().and_then(|e| e.to_str()) == Some("rs") {
            acc.push(p);
        }
    }
}

fn read_spec_attr(attrs: &[syn::Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("spec") {
            continue;
        }
        // #[spec("…")] — the inner token stream is one string literal.
        let parsed: Result<syn::LitStr, _> = attr.parse_args();
        if let Ok(lit) = parsed {
            return Some(lit.value());
        }
    }
    None
}

/// Scan the function's doc attributes for the first `/// Scenario:`
/// line and capture everything from there until a blank doc line
/// (which terminates the paragraph) or the end of the doc block.
/// Tolerates the variants the task spec calls out:
/// `///Scenario:`, `/// Scenario`, `/// scenario:`, etc.
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
    let mut iter = lines.iter().enumerate();
    let scenario_marker = Regex::new(r"(?i)^\s*scenario\b\s*:?\s*").expect("scenario regex");
    let start_idx = loop {
        let (i, line) = iter.next()?;
        if scenario_marker.is_match(line) {
            break (i, line);
        }
    };
    let (i, line) = start_idx;
    let mut paragraph: Vec<String> = Vec::new();
    // The first line may carry inline content after `Scenario:`.
    let head = scenario_marker.replace(line, "").trim().to_string();
    if !head.is_empty() {
        paragraph.push(head);
    }
    // Continue across subsequent doc lines until a blank doc line
    // ends the paragraph.
    for line in lines.iter().skip(i + 1) {
        if line.trim().is_empty() {
            break;
        }
        // Skip if it's a new `Scenario:` block (defensive — only
        // one paragraph per test).
        if scenario_marker.is_match(line) {
            break;
        }
        paragraph.push(line.trim().to_string());
    }
    if paragraph.is_empty() {
        None
    } else {
        Some(paragraph.join(" "))
    }
}

// ---------------------------------------------------------------------------
// Step extraction
// ---------------------------------------------------------------------------

/// Walk top-level statements in the test fn body and emit one plain
/// English step per recognized harness call. Closures, loops, match
/// arms, and unknown methods all degrade to a `Call: …(...)` raw
/// fallback rather than panicking (Decision 30 / task spec).
fn extract_steps_from_body(block: &syn::Block) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    for stmt in &block.stmts {
        collect_steps_from_stmt(stmt, &mut steps);
    }
    steps
}

fn collect_steps_from_stmt(stmt: &syn::Stmt, steps: &mut Vec<String>) {
    let expr_to_walk: &syn::Expr = match stmt {
        syn::Stmt::Local(local) => match &local.init {
            Some(init) => &init.expr,
            None => return,
        },
        syn::Stmt::Expr(e, _) => e,
        syn::Stmt::Macro(m) => {
            if let Some(step) = step_for_macro(&m.mac) {
                steps.push(step);
            }
            return;
        }
        _ => return,
    };
    walk_expr_for_steps(expr_to_walk, steps);
}

fn walk_expr_for_steps(expr: &syn::Expr, steps: &mut Vec<String>) {
    match expr {
        // Method chain on a builder / deck handle. Walk the
        // receiver first (recursive) so chain order is left-to-right.
        syn::Expr::MethodCall(call) => {
            walk_expr_for_steps(&call.receiver, steps);
            let name = call.method.to_string();
            let args = call.args.iter().map(display_expr).collect::<Vec<_>>();
            if let Some(step) = step_for_method(&name, &args) {
                steps.push(step);
            }
        }
        // Free-standing call like `write_hook_line(socket, payload)`.
        syn::Expr::Call(call) => {
            // Walk into args first (e.g. `format!(...)` inside a call).
            for a in &call.args {
                walk_expr_for_steps(a, steps);
            }
            if let syn::Expr::Path(p) = &*call.func
                && let Some(name) = p.path.segments.last().map(|s| s.ident.to_string())
            {
                let args = call.args.iter().map(display_expr).collect::<Vec<_>>();
                if let Some(step) = step_for_free_call(&name, &args) {
                    steps.push(step);
                }
            }
        }
        syn::Expr::Macro(m) => {
            if let Some(step) = step_for_macro(&m.mac) {
                steps.push(step);
            }
        }
        // `let x = expr?;` — walk through the `Try` / unary wrappers.
        syn::Expr::Try(t) => walk_expr_for_steps(&t.expr, steps),
        syn::Expr::Unary(u) => walk_expr_for_steps(&u.expr, steps),
        syn::Expr::Reference(r) => walk_expr_for_steps(&r.expr, steps),
        syn::Expr::Group(g) => walk_expr_for_steps(&g.expr, steps),
        syn::Expr::Paren(p) => walk_expr_for_steps(&p.expr, steps),
        _ => {}
    }
}

/// Harness method-name → step-template map. Decision 30 / task spec.
/// Keep entries short and add new harness methods one line at a
/// time. Unknown methods fall back to `Call: <name>(...)`.
fn step_for_method(name: &str, args: &[String]) -> Option<String> {
    let s = match name {
        "launch_with_fixture" => format!("Launch the deck with fixture {}", arg_or(args, 0)),
        "try_launch_with_fixture" => format!(
            "Launch the deck with fixture {} (fallible variant)",
            arg_or(args, 0)
        ),
        "wait_for_string" => format!("Wait for {} to appear on screen", arg_or(args, 0)),
        "wait_until_quiescent" => "Wait until the deck stops emitting output".to_string(),
        "with_imported_claude_credentials" => {
            "Import Claude credentials into the test HOME".to_string()
        }
        "with_imported_opencode_credentials" => {
            "Import OpenCode credentials into the test HOME".to_string()
        }
        "with_continue_session" => format!(
            "Stage a saved session {} running {}",
            arg_or(args, 0),
            arg_or(args, 1)
        ),
        "with_env" => format!("Override env {}={}", arg_or(args, 0), arg_or(args, 1)),
        "with_pty_size" => format!("Set the PTY to {}×{}", arg_or(args, 0), arg_or(args, 1)),
        "resize" => format!("Resize the PTY to {}×{}", arg_or(args, 0), arg_or(args, 1)),
        "builder" | "hook_socket_path" | "attach_socket_path" | "snapshot_grid" => {
            // Plumbing — no plain-English step.
            return None;
        }
        _ => format!("Call: {name}({})", args.join(", ")),
    };
    Some(s)
}

fn step_for_free_call(name: &str, args: &[String]) -> Option<String> {
    // Skip a small allowlist of language-level helpers and harness
    // entry-point plumbing that are structural noise (Some/None/Ok/
    // Err constructors, TuiDeck::builder() that just starts the
    // builder chain, etc. — these aren't load-bearing test steps).
    if matches!(
        name,
        "Some"
            | "None"
            | "Ok"
            | "Err"
            | "String"
            | "Vec"
            | "PathBuf"
            | "Default"
            | "format"
            | "builder"
    ) {
        return None;
    }
    let s = match name {
        "write_hook_line" => {
            // Args: socket, payload. The payload is the
            // interesting one; the socket comes from the harness.
            format!("Write {} to the hook socket", arg_or(args, 1))
        }
        "render_card_to_buffer" => {
            "Render the session card into a `ratatui::TestBackend` buffer".to_string()
        }
        // Decision 30 fallback: any other free call surfaces as a
        // raw `Call: name(...)` line so the author can see what was
        // recognized — and so adding a new harness function is
        // visible even before its step template lands in the map.
        other => format!("Call: {other}({})", args.join(", ")),
    };
    Some(s)
}

fn step_for_macro(mac: &syn::Macro) -> Option<String> {
    let name = mac.path.segments.last()?.ident.to_string();
    let body = tokens_to_arg_list(mac.tokens.clone());
    match name.as_str() {
        "skip_unless" => {
            // The arg is a check_*_available() call — match by name
            // so unknown checks fall back to the generic form.
            let arg0 = body.first().map(String::as_str).unwrap_or("");
            if arg0.contains("check_claude_available") {
                Some("Skip unless Claude Code CLI is available".to_string())
            } else if arg0.contains("check_opencode_available") {
                Some("Skip unless OpenCode CLI is available".to_string())
            } else {
                Some(format!("Skip unless {arg0}"))
            }
        }
        "assert_snapshot" => Some("Snapshot the rendered buffer (insta)".to_string()),
        _ => None,
    }
}

fn arg_or(args: &[String], i: usize) -> String {
    args.get(i).cloned().unwrap_or_else(|| "?".to_string())
}

/// Render a syn::Expr argument as a compact display string. Strips
/// outer references / parens and unwraps string literals so the
/// rendered step reads naturally.
fn display_expr(expr: &syn::Expr) -> String {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) => format!("`{}`", s.value()),
        syn::Expr::Lit(syn::ExprLit { lit, .. }) => format!("`{}`", lit_to_string(lit)),
        syn::Expr::Path(p) => {
            // Render the path's last segment to keep it short.
            p.path
                .segments
                .last()
                .map(|s| format!("`{}`", s.ident))
                .unwrap_or_else(|| "`_`".to_string())
        }
        syn::Expr::Reference(r) => display_expr(&r.expr),
        syn::Expr::Paren(p) => display_expr(&p.expr),
        syn::Expr::Group(g) => display_expr(&g.expr),
        syn::Expr::MethodCall(m) => {
            // E.g. `&agent_command` or `cmd.as_str()` — show the
            // method name.
            format!("`{}(…)`", m.method)
        }
        syn::Expr::Macro(m) => {
            // E.g. format!("…") — show the macro path's last segment.
            m.mac
                .path
                .segments
                .last()
                .map(|s| format!("`{}!(…)`", s.ident))
                .unwrap_or_else(|| "`_!(…)`".to_string())
        }
        other => {
            let span = other.span();
            // Fallback to a quote-of-tokens render via the source-text
            // helper. `quote::ToTokens` would be one path; use the
            // simpler `format!` against the span's source representation
            // — but that requires source map access. For safety, fall
            // back to a placeholder.
            let _ = span;
            "`…`".to_string()
        }
    }
}

fn lit_to_string(lit: &syn::Lit) -> String {
    match lit {
        syn::Lit::Str(s) => s.value(),
        syn::Lit::Int(i) => i.base10_digits().to_string(),
        syn::Lit::Bool(b) => b.value.to_string(),
        syn::Lit::Char(c) => c.value().to_string(),
        _ => "_".to_string(),
    }
}

/// Flatten a macro's TokenStream into top-level arguments,
/// splitting on commas at the top level. Used for `skip_unless!`
/// where the body is an expression (typically a function call).
fn tokens_to_arg_list(tokens: proc_macro2::TokenStream) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for tt in tokens {
        match &tt {
            proc_macro2::TokenTree::Punct(p)
                if p.as_char() == ',' && p.spacing() == proc_macro2::Spacing::Alone =>
            {
                out.push(current.trim().to_string());
                current.clear();
            }
            _ => {
                if !current.is_empty() && needs_space_before(&tt) {
                    current.push(' ');
                }
                current.push_str(&tt.to_string());
            }
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn needs_space_before(_tt: &proc_macro2::TokenTree) -> bool {
    // Conservative: never insert spaces. Tokens render with their
    // natural spacing via Display.
    false
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------

/// Figure out which `<milestone>-recordings/` directory the doc
/// belongs in. The convention pins by spec area:
///   dashboard/* + hooks/* → m2-recordings (seed M2 tests).
///   chain-smoke/*         → m3-recordings.
///   anything else         → unmapped-recordings (future PRDs
///                           override this in their own scope).
fn resolve_output_path(recordings_root: &Path, spec_id: &str, fn_name: &str) -> PathBuf {
    let area = spec_id.split('/').next().unwrap_or("");
    let milestone = match area {
        "dashboard" | "hooks" => "m2-recordings",
        "chain-smoke" => "m3-recordings",
        _ => "unmapped-recordings",
    };
    recordings_root
        .join(milestone)
        .join(format!("{fn_name}.md"))
}

fn render_markdown(
    test: &DiscoveredTest,
    entry: &CatalogEntry,
    scenario: &str,
    cast_name: &str,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {} — {}\n\n", entry.id, entry.headline));
    let rel_src = test
        .source_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("?");
    s.push_str(&format!(
        "**Source:** `tests/{}::{}`\n",
        rel_src, test.fn_name
    ));
    s.push_str("**Catalog:** PRD #77 `## Test Case Catalog`\n");
    let layer_is_l2 = entry
        .layer
        .as_deref()
        .map(|l| !l.to_lowercase().contains("l1"))
        .unwrap_or(true);
    if layer_is_l2 {
        s.push_str(&format!("**Cast:** `{cast_name}`\n"));
    }
    s.push('\n');

    s.push_str("## Scenario\n\n");
    s.push_str(scenario);
    s.push_str("\n\n");

    s.push_str("## Steps\n\n");
    if test.steps.is_empty() {
        s.push_str("_(no harness method calls recognised in this test body)_\n");
    } else {
        for (i, step) in test.steps.iter().enumerate() {
            s.push_str(&format!("{}. {step}\n", i + 1));
        }
    }
    s.push('\n');

    s.push_str("## Catalog spec\n\n");
    fn push_bullet(s: &mut String, label: &str, value: Option<&str>) {
        if let Some(v) = value {
            s.push_str(&format!("- **{label}:** {v}\n"));
        }
    }
    push_bullet(&mut s, "Layer", entry.layer.as_deref());
    push_bullet(&mut s, "Agent", entry.agent.as_deref());
    push_bullet(&mut s, "Asserts", entry.asserts.as_deref());
    push_bullet(&mut s, "Does not assert", entry.does_not_assert.as_deref());
    push_bullet(
        &mut s,
        "Platform coverage",
        entry.platform_coverage.as_deref(),
    );
    push_bullet(&mut s, "Cost note", entry.cost_note.as_deref());
    s.push('\n');

    if layer_is_l2 {
        s.push_str("## Replay\n\n```sh\n");
        // Derive the recordings dir from the output path so the
        // command matches the on-disk location.
        let dir_hint = match test.spec_id.split('/').next().unwrap_or("") {
            "dashboard" | "hooks" => "m2-recordings",
            "chain-smoke" => "m3-recordings",
            _ => "unmapped-recordings",
        };
        s.push_str(&format!(
            "asciinema play .dot-agent-deck/{dir_hint}/{cast_name}\n"
        ));
        s.push_str("```\n\n");
    }

    s.push_str("## Rerun\n\n");
    let tier = if layer_is_l2 { "e2e" } else { "fast" };
    // Decision 17's `<sub>_<NNN>_…` prefix is unique by construction,
    // so we filter on `<sub>_<NNN>` which is shorter than the full
    // function name.
    let filter = sub_area_filter(&test.spec_id, &test.fn_name);
    s.push_str(&format!("```sh\ncargo test-{tier} {filter}\n```\n"));

    s
}

fn sub_area_filter(spec_id: &str, fn_name: &str) -> String {
    // Catalog IDs have shape `area/sub/NNN`; the Decision 17 prefix
    // is `<sub>_<NNN>` with `-` → `_` normalization. If we can't
    // derive that, fall back to the full fn name (still unique).
    if let Some((rest, nnn)) = spec_id.rsplit_once('/')
        && let Some((_, sub)) = rest.rsplit_once('/')
    {
        return format!("{}_{nnn}", sub.replace('-', "_"));
    }
    fn_name.to_string()
}
