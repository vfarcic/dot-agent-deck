//! PRD #819 M7 — check 12: a regression tripwire over the desktop crate's
//! production sources.
//!
//! # What this is, and what it is NOT
//!
//! **This is a regression tripwire. It is not enforcement, and it is not a
//! security boundary.** Say that plainly wherever it is described, because the
//! PRD's first draft called it enforcement and the security audit was right to
//! object.
//!
//! `desktop/src-tauri/Cargo.toml` carries `dot-agent-deck = { path = "../.." }`,
//! so **every** `pub` item in the root crate is callable from the desktop crate.
//! That is why `load_project_config` was reachable from the desktop's
//! `lib.rs` at all. Removing today's calls does not remove the reachability, so
//! after PRD #819 nothing but this rule stands between the invariant — *the
//! client resolves no project against a filesystem* — and the next feature
//! reintroducing a client-side project read.
//!
//! What a source rule can catch is what a source rule can see. It **cannot**
//! catch:
//!
//! * a **root-crate wrapper with an innocuous name** — a new `pub fn` in an
//!   already-allowlisted module that calls `load_project_config` internally.
//!   This is the shape most likely to happen by accident, it is pinned by
//!   [`tests::a_root_crate_wrapper_with_an_innocuous_name_is_not_caught`], and
//!   it is the honest headline of this residual;
//! * a **macro** that expands to a forbidden path (syn does not expand macros);
//! * an **already-imported module gaining a new method** that reads project
//!   state — the import is allowlisted, so the call is invisible here;
//! * a **trait method** brought in by an allowlisted `use`;
//! * anything reached through `std::fs` by a path this rule has no literal for.
//!
//! Only removing the desktop's dependency on the full root crate gives
//! compiler-enforced reachability, and that is issue #176 M1.1 — out of scope
//! for PRD #819 and named in its Out of Scope for this reason.
//!
//! # What it does catch
//!
//! Rust is **parsed**, not substring-matched, so a comment or a string literal
//! that merely names a forbidden symbol is not a violation and a sweep that
//! produces noise does not get disabled. Four findings, each a
//! [`FindingKind`]:
//!
//! 1. `root-module` — a `use dot_agent_deck::<m>::…` or a qualified
//!    `dot_agent_deck::<m>::…` path where `<m>` is not on
//!    [`ALLOWED_ROOT_MODULES`]. This is the **positive** half: the rule states
//!    the boundary the production desktop is allowed to reach across, rather
//!    than blacklisting today's four names and going stale the moment a fifth
//!    appears.
//! 2. `forbidden-symbol` — any path segment naming one of
//!    [`FORBIDDEN_SYMBOLS`], wherever it appears: an import, an alias
//!    (`use … as pc` still names `project_config` in the path), a grouped or
//!    multi-line import, or a fully qualified call with no import at all.
//! 3. `project-state-literal` — a string literal carrying one of
//!    [`PROJECT_STATE_LITERALS`]. A client that names the project's own state
//!    files is a client that knows where they are.
//! 4. `cwd-fallback` — `std::env::current_dir`, the fallback
//!    `desktop_project_cwd()` used to guess a project from the process's own
//!    working directory. That guess is the exact defect PRD #819 exists to
//!    remove: on a remote daemon it resolves against the wrong filesystem and
//!    is silently wrong.
//!
//! **Measured, not assumed.** Rule 5's note is that linkage-check's assertions
//! are runtime-only — the compile half is already covered by
//! `cargo clippy --workspace --all-targets`, so a rule gutted to `Vec::new()`
//! stays green. Appending an aliased import, an aliased call, a `format!`-wrapped
//! `.dot-agent-deck.toml` and a `std::env::current_dir()` to
//! `desktop/src-tauri/src/terminal.rs` and running `cargo xtask linkage-check`
//! reported **all five** finding shapes at their own line numbers; reverting the
//! file returned the check to green.
//!
//! Scope is **production code only**. Items under a `#[cfg(test)]` are skipped:
//! the desktop crate's test modules legitimately build project fixtures, and a
//! rule that failed on them would be worked around rather than obeyed.
//!
//! # The allowlist, and why a stale entry is a failure
//!
//! This rule landed *before* PRD #819 M6 (the desktop launch flow), so it went
//! up against a tree that still violated it. Those violations were enumerated
//! in [`PENDING_M6`] rather than the rule being weakened to let them through —
//! the difference matters, because a weakened rule catches nothing new either.
//!
//! An allowlist entry that matches **no** finding is itself a failure, and that
//! forcing function has now fired: **M6 landed, [`PENDING_M6`] is empty**, and
//! the check went red on ten stale entries until they were deleted along with
//! the reads they excused. The allowlist mechanism stays because the property is
//! what makes an allowlist safe to have at all — an excuse cannot quietly
//! outlive what it excused.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use proc_macro2::{TokenStream, TokenTree};
use syn::visit::Visit;

/// Repo-relative root of the sources this rule covers.
const DESKTOP_SRC: &str = "desktop/src-tauri/src";

/// The rule sentence, quoted in every failure so the reader is not left to
/// infer what was violated from a symbol name.
pub const DESKTOP_BOUNDARY_RULE: &str = "client-side project resolution in the desktop crate — the client resolves no project \
     against a filesystem (PRD #819). This is a regression TRIPWIRE, not enforcement and not a \
     security boundary: the desktop path-depends on the whole root crate, so a wrapper with an \
     innocuous name bypasses it. Ask the daemon (list-projects / resolve-project / \
     prepare-workflow) instead of reading the project here";

/// The **positive** boundary: root-crate modules the production desktop may
/// reach across.
///
/// Derived from what `desktop/src-tauri/src/` imports today, minus the two this
/// PRD is removing. Adding a module here is a deliberate widening of the
/// boundary and should be argued in the PR that does it — which is the point of
/// stating the allowed set rather than the forbidden one.
const ALLOWED_ROOT_MODULES: &[&str] = &[
    "agent_pty",
    "agent_registry",
    "build_id",
    "config",
    "daemon_attach",
    "daemon_client",
    "daemon_protocol",
    "daemon_stop",
    "event",
    "platform",
    "prompt_delivery",
    "state",
    "ui",
];

/// Symbols that are a client-side project read wherever they appear, including
/// through an allowlisted module or a re-export.
///
/// The module names are here as well as absent from [`ALLOWED_ROOT_MODULES`]:
/// the module check catches `dot_agent_deck::project_config::…`, and this
/// catches a re-export such as a hypothetical `dot_agent_deck::config::
/// load_project_config`, which the module check would wave through.
const FORBIDDEN_SYMBOLS: &[&str] = &[
    "project_config",
    "orchestrator_context",
    "load_project_config",
    "prepare_orchestrator_prompt",
];

/// Project-state path literals. A client that spells these knows where the
/// project's own files live, which is the knowledge PRD #819 moves daemon-side.
const PROJECT_STATE_LITERALS: &[&str] = &[".dot-agent-deck.toml", "orchestrator-context.md"];

/// The current-directory guess. `desktop_project_cwd()` fell back to it, and on
/// a remote daemon it resolves against the wrong filesystem silently.
const CWD_FALLBACK: &str = "current_dir";

/// What kind of boundary crossing a [`Finding`] is. Stable strings — they are
/// half of an allowlist entry's identity, so renaming one invalidates
/// [`PENDING_M6`] loudly rather than silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    RootModule,
    ForbiddenSymbol,
    ProjectStateLiteral,
    CwdFallback,
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RootModule => "root-module",
            Self::ForbiddenSymbol => "forbidden-symbol",
            Self::ProjectStateLiteral => "project-state-literal",
            Self::CwdFallback => "cwd-fallback",
        }
    }
}

/// One boundary crossing.
///
/// `file` + `kind` + `detail` is the identity an allowlist entry matches, and it
/// deliberately excludes `line`: a line number drifts with every edit above it,
/// and an allowlist that goes stale on unrelated edits gets bulk-refreshed
/// instead of read.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub file: String,
    pub kind: FindingKind,
    pub detail: String,
    pub line: Option<usize>,
}

impl Finding {
    fn render(&self) -> String {
        match self.line {
            Some(line) => format!(
                "{}:{}: {} `{}` — {DESKTOP_BOUNDARY_RULE}",
                self.file,
                line,
                self.kind.as_str(),
                self.detail
            ),
            None => format!(
                "{}: {} `{}` — {DESKTOP_BOUNDARY_RULE}",
                self.file,
                self.kind.as_str(),
                self.detail
            ),
        }
    }
}

/// The violations PRD #819 M6 removed. **Empty, and that is the milestone.**
///
/// Each entry was `(repo-relative file, kind, detail)`. Ten of them excused the
/// desktop's own copy of project resolution — `dot_agent_deck::project_config`
/// and `dot_agent_deck::orchestrator_context` imports, the `load_project_config`
/// and `prepare_orchestrator_prompt` calls behind them, the
/// `.dot-agent-deck.toml` and `orchestrator-context.md` literals they spelled,
/// and `desktop_project_cwd()`'s `std::env::current_dir` guess. M6 replaced the
/// pair with the daemon's `prepare-workflow` verb and deleted the function, so
/// every one of them matched nothing and the forcing function below took the
/// check red until they went with it — which is exactly what it is for.
///
/// **Keep it empty.** A new entry is a new client-side project read being
/// excused rather than fixed, and the excuse outlives whoever wrote it. If a
/// finding here is a false positive, that is a bug in the rule and belongs in
/// [`ALLOWED_ROOT_MODULES`] or in the scan, argued in the PR that widens it.
const PENDING_M6: &[(&str, &str, &str)] = &[];

/// Run the rule over the checkout at `root`. Returns rendered failures, already
/// carrying the rule sentence; an empty vector is a pass.
pub fn run(root: &Path) -> Vec<String> {
    let dir = root.join(DESKTOP_SRC);
    if !dir.is_dir() {
        return vec![format!(
            "{DESKTOP_SRC} is missing — check 12 covers nothing. If the desktop crate moved, move \
             this rule with it; do not leave it pointing at a path that no longer exists"
        )];
    }

    let mut findings = Vec::new();
    let mut failures = Vec::new();
    for file in rust_sources(&dir) {
        let display = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) => {
                failures.push(format!("failed to read {display}: {e}"));
                continue;
            }
        };
        match violations(&display, &text) {
            Ok(found) => findings.extend(found),
            Err(e) => failures.push(format!(
                "failed to parse {display} as Rust: {e} — check 12 cannot see a file it cannot \
                 parse, so this is a failure rather than a skip"
            )),
        }
    }

    failures.extend(apply_allowlist(findings, PENDING_M6));
    failures
}

/// Subtract the allowlist from `findings`, and report entries that matched
/// nothing.
///
/// Split out from [`run`] so the forcing function — a stale entry is a failure —
/// is testable against synthetic findings rather than only against whatever the
/// checkout happens to contain.
pub fn apply_allowlist(findings: Vec<Finding>, allowlist: &[(&str, &str, &str)]) -> Vec<String> {
    let mut matched = vec![false; allowlist.len()];
    let mut failures = Vec::new();

    for finding in findings {
        let hit = allowlist.iter().position(|(file, kind, detail)| {
            *file == finding.file && *kind == finding.kind.as_str() && *detail == finding.detail
        });
        match hit {
            Some(idx) => matched[idx] = true,
            None => failures.push(finding.render()),
        }
    }

    for (idx, entry) in allowlist.iter().enumerate() {
        if !matched[idx] {
            failures.push(format!(
                "stale allowlist entry ({}, {}, {}) matches nothing — the violation it excused is \
                 gone, so delete the entry. An allowlist that outlives what it excused is a rule \
                 nobody can read",
                entry.0, entry.1, entry.2
            ));
        }
    }

    failures
}

/// Every boundary crossing in one production Rust source. `Err` when the file
/// does not parse — the caller treats that as a failure, never a skip.
pub fn violations(display: &str, text: &str) -> Result<Vec<Finding>, syn::Error> {
    let file = syn::parse_file(text)?;
    let mut scan = Scan {
        file: display.to_string(),
        // Comments blanked so a `// … load_project_config …` note does not
        // supply a line number for a violation found elsewhere. String contents
        // are kept: a project-state literal's line IS inside a string.
        located_in: crate::strip_rust_comments(text),
        findings: BTreeSet::new(),
    };
    scan.visit_file(&file);
    Ok(scan.findings.into_iter().collect())
}

struct Scan {
    file: String,
    located_in: String,
    findings: BTreeSet<Finding>,
}

impl Scan {
    fn record(&mut self, kind: FindingKind, detail: impl Into<String>, needle: &str) {
        let detail = detail.into();
        let line = self
            .located_in
            .lines()
            .position(|l| l.contains(needle))
            .map(|idx| idx + 1);
        self.findings.insert(Finding {
            file: self.file.clone(),
            kind,
            detail,
            line,
        });
    }

    /// Check one already-flattened path (a `::`-joined segment list).
    fn check_path_segments(&mut self, segments: &[String]) {
        for (idx, segment) in segments.iter().enumerate() {
            if FORBIDDEN_SYMBOLS.contains(&segment.as_str()) {
                self.record(FindingKind::ForbiddenSymbol, segment.clone(), segment);
            }
            if segment == "dot_agent_deck"
                && let Some(module) = segments.get(idx + 1)
                && !ALLOWED_ROOT_MODULES.contains(&module.as_str())
            {
                self.record(
                    FindingKind::RootModule,
                    format!("dot_agent_deck::{module}"),
                    module,
                );
            }
        }
        if segments.len() >= 2
            && segments[segments.len() - 1] == CWD_FALLBACK
            && segments[segments.len() - 2] == "env"
        {
            self.record(
                FindingKind::CwdFallback,
                "std::env::current_dir",
                CWD_FALLBACK,
            );
        }
    }

    /// Scan a macro's raw token stream.
    ///
    /// syn parses `format!("… .dot-agent-deck.toml")` as an opaque
    /// [`TokenStream`], so neither [`Visit::visit_lit_str`] nor
    /// [`Visit::visit_path`] ever sees inside one — and the desktop's project
    /// prose lives almost entirely inside `format!`. Measured: without this,
    /// check 12 found **zero** of `lib.rs`'s project-state literals.
    ///
    /// This walks the tokens rather than expanding the macro. String literals
    /// are checked as literals; runs of `ident (:: ident)*` are reassembled and
    /// checked as paths. What it therefore still cannot see is a macro that
    /// *generates* a forbidden path out of tokens that are not one — `concat_
    /// idents!`, a `macro_rules!` arm pasting a name together — which joins the
    /// residual in the module docs.
    fn scan_tokens(&mut self, tokens: TokenStream) {
        let mut run: Vec<String> = Vec::new();
        let mut colons = 0usize;
        for tree in tokens {
            match tree {
                TokenTree::Ident(ident) => {
                    if !run.is_empty() && colons != 2 {
                        self.check_path_segments(&run);
                        run.clear();
                    }
                    run.push(ident.to_string());
                    colons = 0;
                }
                TokenTree::Punct(punct) if punct.as_char() == ':' => colons += 1,
                TokenTree::Literal(literal) => {
                    self.flush_run(&mut run, &mut colons);
                    if let syn::Lit::Str(text) = syn::Lit::new(literal) {
                        self.check_literal(&text.value());
                    }
                }
                TokenTree::Group(group) => {
                    self.flush_run(&mut run, &mut colons);
                    self.scan_tokens(group.stream());
                }
                _ => self.flush_run(&mut run, &mut colons),
            }
        }
        self.flush_run(&mut run, &mut colons);
    }

    fn flush_run(&mut self, run: &mut Vec<String>, colons: &mut usize) {
        if !run.is_empty() {
            self.check_path_segments(run);
            run.clear();
        }
        *colons = 0;
    }

    fn check_literal(&mut self, value: &str) {
        for literal in PROJECT_STATE_LITERALS {
            if value.contains(literal) {
                self.record(FindingKind::ProjectStateLiteral, *literal, literal);
            }
        }
    }

    /// Flatten a `use` tree into every full path it names, so a grouped,
    /// nested, multi-line or aliased import is checked exactly like a plain one.
    /// The ALIAS is deliberately not recorded — `use … as pc` still names
    /// `project_config` in its path, and that is what is checked.
    fn walk_use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.walk_use_tree(&p.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                prefix.push(n.ident.to_string());
                self.check_path_segments(prefix);
                prefix.pop();
            }
            syn::UseTree::Rename(r) => {
                prefix.push(r.ident.to_string());
                self.check_path_segments(prefix);
                prefix.pop();
            }
            syn::UseTree::Glob(_) => {
                self.check_path_segments(prefix);
            }
            syn::UseTree::Group(g) => {
                for item in &g.items {
                    self.walk_use_tree(item, prefix);
                }
            }
        }
    }
}

impl<'ast> Visit<'ast> for Scan {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        if item_attrs(item).is_some_and(cfg_selects_test_only) {
            return;
        }
        syn::visit::visit_item(self, item);
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        if impl_item_attrs(item).is_some_and(cfg_selects_test_only) {
            return;
        }
        syn::visit::visit_impl_item(self, item);
    }

    fn visit_item_use(&mut self, node: &'ast syn::ItemUse) {
        let mut prefix = Vec::new();
        self.walk_use_tree(&node.tree, &mut prefix);
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        let segments: Vec<String> = node.segments.iter().map(|s| s.ident.to_string()).collect();
        self.check_path_segments(&segments);
        syn::visit::visit_path(self, node);
    }

    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.check_literal(&node.value());
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let segments: Vec<String> = node
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect();
        self.check_path_segments(&segments);
        self.scan_tokens(node.tokens.clone());
    }

    /// Attributes are not scanned. `///` doc comments reach syn as
    /// `#[doc = "…"]`, so without this a doc comment explaining the rule would
    /// trip it — the noise-that-gets-a-sweep-disabled failure mode, arriving
    /// through the one comment form syn does keep.
    fn visit_attribute(&mut self, _node: &'ast syn::Attribute) {}
}

fn item_attrs(item: &syn::Item) -> Option<&[syn::Attribute]> {
    Some(match item {
        syn::Item::Const(i) => &i.attrs,
        syn::Item::Enum(i) => &i.attrs,
        syn::Item::ExternCrate(i) => &i.attrs,
        syn::Item::Fn(i) => &i.attrs,
        syn::Item::ForeignMod(i) => &i.attrs,
        syn::Item::Impl(i) => &i.attrs,
        syn::Item::Macro(i) => &i.attrs,
        syn::Item::Mod(i) => &i.attrs,
        syn::Item::Static(i) => &i.attrs,
        syn::Item::Struct(i) => &i.attrs,
        syn::Item::Trait(i) => &i.attrs,
        syn::Item::TraitAlias(i) => &i.attrs,
        syn::Item::Type(i) => &i.attrs,
        syn::Item::Union(i) => &i.attrs,
        syn::Item::Use(i) => &i.attrs,
        _ => return None,
    })
}

fn impl_item_attrs(item: &syn::ImplItem) -> Option<&[syn::Attribute]> {
    Some(match item {
        syn::ImplItem::Const(i) => &i.attrs,
        syn::ImplItem::Fn(i) => &i.attrs,
        syn::ImplItem::Type(i) => &i.attrs,
        syn::ImplItem::Macro(i) => &i.attrs,
        _ => return None,
    })
}

/// Whether these attributes put the item behind a test-only `cfg`.
///
/// `#[cfg(test)]` and `#[cfg(all(test, unix))]` are test-only. `#[cfg(not(test))]`
/// is **production** and is deliberately NOT skipped — the presence of `not`
/// anywhere in the predicate makes this answer `false`, which errs toward
/// scanning. A rule that skipped a production block because it mentioned `test`
/// would be a hole shaped exactly like the one it exists to close.
fn cfg_selects_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        let tokens = list.tokens.to_string();
        let idents: Vec<&str> = tokens
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| !t.is_empty())
            .collect();
        idents.contains(&"test") && !idents.contains(&"not")
    })
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rule 5's note: linkage-check's assertions are RUNTIME-only — the compile
    /// half is already covered by `cargo clippy --workspace --all-targets`, so a
    /// rule gutted to `Vec::new()` stays green without these.
    fn kinds(display: &str, src: &str) -> Vec<(String, String)> {
        violations(display, src)
            .expect("fixture parses")
            .into_iter()
            .map(|f| (f.kind.as_str().to_string(), f.detail))
            .collect()
    }

    fn caught(src: &str, kind: &str, detail: &str) -> bool {
        kinds("fixture.rs", src)
            .iter()
            .any(|(k, d)| k == kind && d == detail)
    }

    #[test]
    fn an_alias_import_still_names_the_module_it_aliases() {
        assert!(caught(
            "use dot_agent_deck::project_config as pc;\nfn f() { let _ = pc::load_project_config; }\n",
            "forbidden-symbol",
            "project_config",
        ));
    }

    #[test]
    fn grouped_and_multi_line_imports_are_flattened_before_checking() {
        let src = "\
use dot_agent_deck::{
    event::AgentType,
    project_config::{
        OrchestrationConfig,
        load_project_config,
    },
};
";
        assert!(caught(src, "forbidden-symbol", "project_config"));
        assert!(caught(src, "forbidden-symbol", "load_project_config"));
        // The sibling in the same group is legitimate and must not be dragged in.
        assert!(!caught(src, "root-module", "dot_agent_deck::event"));
    }

    #[test]
    fn a_fully_qualified_call_needs_no_import_to_be_caught() {
        let src = "\
fn seed(config: &C, cwd: &str) -> Option<String> {
    dot_agent_deck::orchestrator_context::prepare_orchestrator_prompt(config, cwd, None)
}
";
        assert!(caught(src, "forbidden-symbol", "orchestrator_context"));
        assert!(caught(
            src,
            "forbidden-symbol",
            "prepare_orchestrator_prompt"
        ));
    }

    #[test]
    fn a_comment_and_a_string_literal_naming_a_symbol_are_not_violations() {
        // The whole reason this rule parses Rust: a substring scan flags both of
        // these, drowns the output, and gets switched off.
        let src = "\
// This deliberately does NOT call load_project_config; ask the daemon instead.
/// See `dot_agent_deck::project_config` for what the daemon reads on our behalf.
fn f() {
    let note = \"load_project_config is the daemon's job\";
    let _ = note;
}
";
        assert_eq!(
            kinds("fixture.rs", src),
            Vec::<(String, String)>::new(),
            "prose about the rule is not a violation of it"
        );
    }

    #[test]
    fn a_doc_comment_quoting_a_project_state_filename_is_not_a_violation() {
        // `///` reaches syn as `#[doc = "…"]`, i.e. as a string literal. Without
        // the attribute skip, documenting the rule would trip it.
        let src = "\
/// The daemon reads `.dot-agent-deck.toml`; this crate never does.
fn f() {}
";
        assert_eq!(kinds("fixture.rs", src), Vec::<(String, String)>::new());
    }

    #[test]
    fn a_project_state_literal_in_real_code_is_a_violation() {
        let src = "\
fn probe(dir: &std::path::Path) -> bool {
    dir.join(\".dot-agent-deck.toml\").is_file()
}
";
        assert!(caught(src, "project-state-literal", ".dot-agent-deck.toml"));
    }

    #[test]
    fn the_current_directory_fallback_is_a_violation() {
        let src = "\
fn guess() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}
";
        assert!(caught(src, "cwd-fallback", "std::env::current_dir"));
    }

    #[test]
    fn the_positive_boundary_rejects_a_root_module_nobody_allowlisted() {
        assert!(caught(
            "use dot_agent_deck::scheduler::Schedule;\n",
            "root-module",
            "dot_agent_deck::scheduler",
        ));
        // …and passes the ones the production desktop legitimately uses.
        let allowed = "\
use dot_agent_deck::daemon_client::DaemonClient;
use dot_agent_deck::event::AgentType;
use dot_agent_deck::platform::ipc::IpcStream;
";
        assert_eq!(kinds("fixture.rs", allowed), Vec::<(String, String)>::new());
    }

    #[test]
    fn cfg_test_modules_are_out_of_scope_and_cfg_not_test_blocks_are_not() {
        let test_only = "\
#[cfg(test)]
mod tests {
    use dot_agent_deck::project_config::load_project_config;
    #[test]
    fn fixture() {
        let _ = load_project_config(std::path::Path::new(\".dot-agent-deck.toml\"));
    }
}
";
        assert_eq!(
            kinds("fixture.rs", test_only),
            Vec::<(String, String)>::new(),
            "test modules legitimately construct project fixtures"
        );

        // `#[cfg(all(test, unix))]` is still test-only.
        let narrowed = "\
#[cfg(all(test, unix))]
mod tests {
    use dot_agent_deck::project_config::load_project_config;
}
";
        assert_eq!(
            kinds("fixture.rs", narrowed),
            Vec::<(String, String)>::new()
        );

        // `#[cfg(not(test))]` is PRODUCTION and must be scanned. Skipping it
        // would be a hole shaped exactly like the one this rule closes.
        let production = "\
#[cfg(not(test))]
fn resolve() {
    let _ = dot_agent_deck::project_config::load_project_config;
}
";
        assert!(caught(
            production,
            "forbidden-symbol",
            "load_project_config"
        ));
    }

    #[test]
    fn a_root_crate_wrapper_with_an_innocuous_name_is_not_caught() {
        // THE RESIDUAL, pinned rather than implied. `refresh_workspace_hints`
        // is imagined as a new `pub fn` in the root crate's already-allowlisted
        // `config` module whose body calls `load_project_config`. Nothing in the
        // desktop's source says so, so nothing here can see it.
        //
        // This is why the module docs call this a tripwire and not enforcement,
        // and why only issue #176 M1.1 — removing the desktop's dependency on
        // the whole root crate — makes the invariant compiler-checked.
        let src = "\
use dot_agent_deck::config::refresh_workspace_hints;

fn project(cwd: &str) -> Option<String> {
    refresh_workspace_hints(cwd)
}
";
        assert_eq!(
            kinds("fixture.rs", src),
            Vec::<(String, String)>::new(),
            "if this ever starts failing, the residual narrowed and the module docs must say so"
        );
    }

    #[test]
    fn a_file_that_does_not_parse_is_an_error_not_a_silent_pass() {
        assert!(violations("broken.rs", "fn f( {").is_err());
    }

    #[test]
    fn findings_carry_the_line_they_were_found_on() {
        let src = "\
fn a() {}

fn b() -> Option<std::path::PathBuf> {
    std::env::current_dir().ok()
}
";
        let found = violations("fixture.rs", src).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, Some(4));
    }

    #[test]
    fn an_allowlisted_violation_passes_and_an_unlisted_one_does_not() {
        let findings = vec![
            Finding {
                file: "a.rs".into(),
                kind: FindingKind::ForbiddenSymbol,
                detail: "load_project_config".into(),
                line: Some(3),
            },
            Finding {
                file: "b.rs".into(),
                kind: FindingKind::ForbiddenSymbol,
                detail: "load_project_config".into(),
                line: Some(9),
            },
        ];
        let failures = apply_allowlist(
            findings,
            &[("a.rs", "forbidden-symbol", "load_project_config")],
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].starts_with("b.rs:9:"));
        // The allowlist is per-file: excusing `a.rs` does not excuse `b.rs`.
        assert!(!failures[0].contains("stale allowlist entry"));
    }

    #[test]
    fn a_stale_allowlist_entry_is_itself_a_failure() {
        // The M6 forcing function: when the last client-side project read goes,
        // this check goes red until the entry that excused it goes too.
        let failures = apply_allowlist(
            Vec::new(),
            &[("gone.rs", "forbidden-symbol", "load_project_config")],
        );
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0].contains("stale allowlist entry"),
            "got {}",
            failures[0]
        );
    }

    #[test]
    fn every_forbidden_module_name_is_absent_from_the_positive_allowlist() {
        // The two halves must not contradict each other: a module both
        // allowlisted and forbidden would pass or fail depending on which check
        // ran first, which is not a boundary anyone can read.
        for symbol in FORBIDDEN_SYMBOLS {
            assert!(
                !ALLOWED_ROOT_MODULES.contains(symbol),
                "`{symbol}` is on both lists"
            );
        }
    }
}
