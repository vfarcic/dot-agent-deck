//! Rule 17 (issue #688): a `src/` unit test that spawns an emitter pins the
//! child's deck endpoints.
//!
//! ## What it is for
//!
//! A child process that posts hook events — the deck binary itself, or a real
//! agent whose hooks call it — finds its daemon through
//! `DOT_AGENT_DECK_SOCKET`, and with that variable ABSENT it resolves one on
//! its own: `platform::paths::socket_path` falls back to
//! `$XDG_RUNTIME_DIR/dot-agent-deck.sock` when `XDG_RUNTIME_DIR` is set, which
//! on a developer's machine is typically their live daemon. So clearing the
//! test process's environment (`test_isolation::detach_from_any_live_deck`) and
//! `agent_pty::spawn`'s `env_remove` of the inherited endpoints stop a child
//! INHERITING a route to a real deck, and do nothing about it RESOLVING one.
//! Issue #688 measured that: a fixture run with the full scrub applied still
//! produced 3 foreign `SessionStart`s in 8 runs. What closes it is a pin in the
//! CHILD's environment at a path nothing listens on, so the emit fails closed —
//! `src/test_isolation.rs`'s `pin_unreachable_endpoints` /
//! `unreachable_endpoints`.
//!
//! A pin is per spawn, so no process-level helper can apply it for a test that
//! forgets. That is why this is a build-time tripwire: the failure it guards is
//! silent, intermittent, and visible only as a stray card on a developer's
//! dashboard, so the suite that introduces it does not catch it.
//!
//! ## What counts as an emitter
//!
//! The one real instance (#666's `scheduler/dispatch/016`) did not come from a
//! command at all: `SpawnOptions::agent_type = Some(AgentType::Codex)` makes
//! `spawn` rewrite the command as `dot-agent-deck wrap --agent codex -- …`, so a
//! `/bin/cat` byte sink became a second deck. The rule recognises three shapes,
//! all in `src/` **test code** and all by literal:
//!
//! 1. a `SpawnOptions { … }` literal whose `agent_type` field names a
//!    Wrapper-strategy `AgentType` variant;
//! 2. a `SpawnOptions { … }` literal whose `command` field holds a string
//!    literal whose program — its first word, or any word after a
//!    [`LAUNCHERS`] first word such as `sh -c` — is a registered agent basename
//!    or `dot-agent-deck`, or which calls one of [`DECK_BINARY_RESOLVERS`];
//! 3. `Command::new(…)` / `CommandBuilder::new(…)` with a program of shape 2,
//!    or any `.arg(…)` / `.args(…)` chained directly onto one whose program
//!    literal is a launcher, with a word naming such a program. Arguments to
//!    any other program are data (`printf pi` names no agent).
//!
//! Wrapper-strategy variants and agent basenames are read from
//! `src/agent_registry.rs`'s `AgentSpec` statics rather than listed here, so a
//! new agent registered there the same way is covered without editing this.
//!
//! "Test code" is structural: an item, impl item, `let`, block or statement
//! gated by a test-only `cfg` (the same predicate rule 12 uses — `cfg(test)`,
//! `cfg(all(test, …))`, not `cfg(any(test, …))`), a `#[test]`-style `fn`, a file
//! opening `#![cfg(test)]`, and every file declared as a module from test code
//! (`#[cfg(test)] mod test_isolation;` in `lib.rs`), transitively.
//!
//! ## What clears it
//!
//! A CALL to either pin helper, matched by the last segment of the called
//! path, anywhere in the innermost `fn` that holds the emitter (closures inside
//! it included). Naming the helper without calling it — importing it, or
//! passing it as a value — does not count. Or
//! the marker [`ALLOW`] in a comment on the `fn` line or directly above it —
//! for a child that is deliberately pinned at the test's own sandbox daemon.
//!
//! ## What it does NOT see, so it is not mistaken for coverage
//!
//! - a command or agent type held in a variable, a `const`, or a helper's
//!   parameter — `spawn_typed_byte_target`'s `agent_type` is one — because the
//!   rule reads literals;
//! - an agent reached through a program outside [`LAUNCHERS`] that runs its
//!   arguments (`timeout 5 codex`, say) or a launcher's program held in a
//!   variable;
//! - spawns that go some other way: `AttachRequest::StartAgent` to an
//!   in-process daemon, `DaemonClient::start_agent`, a TUI-driven spawn, or
//!   `.arg` on a `Command` held in a variable;
//! - an in-process connect to a default endpoint the test resolved itself;
//! - macro bodies that do not parse as comma-separated expressions;
//! - per-spawn clearance: a `fn` that pins one child and not a second passes;
//! - `tests/` (where `common::init_test_env` scrubs the inherited endpoints),
//!   `desktop/` and `xtask/`, none of which this rule scans.
//!
//! No `src/` test matches today (`the_checkout_has_no_unpinned_emitter` asserts
//! it), so a finding from this rule is the next case, which is the case it is
//! for.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use syn::visit::Visit;

use crate::desktop_project_boundary::{cfg_selects_test_only, impl_item_attrs, item_attrs};

/// Repo-relative root of the sources this rule covers.
const SRC: &str = "src";

/// Where the agent registry lives, as components so the joined path is native
/// on every platform (issue #1137's concern).
const REGISTRY: [&str; 2] = ["src", "agent_registry.rs"];

/// Where the pin helpers live, as components.
const ISOLATION: [&str; 2] = ["src", "test_isolation.rs"];

/// The helpers that clear the rule, matched by the last path segment.
pub const PIN_HELPERS: [&str; 2] = ["pin_unreachable_endpoints", "unreachable_endpoints"];

/// Functions whose result is the deck binary (or a command running it).
pub const DECK_BINARY_RESOLVERS: [&str; 3] =
    ["binary_name", "deck_binary_for_wrap", "wrap_launch_command"];

/// The deck binary's own basename.
const DECK_BINARY: &str = "dot-agent-deck";

/// Opt-out marker, in a comment on the `fn` line or directly above it.
pub const ALLOW: &str = "linkage-check:allow-unpinned-emitter";

/// The rule sentence, quoted in every finding.
pub const UNPINNED_EMITTER_RULE: &str = "a `src/` unit test spawns a child that can post hook \
     events without pinning that child's deck endpoints. Clearing this process's environment \
     stops the child INHERITING an endpoint, not RESOLVING one: with `DOT_AGENT_DECK_SOCKET` \
     absent it falls back to the default hook endpoint (`$XDG_RUNTIME_DIR/dot-agent-deck.sock` \
     when that variable is set), which on a developer's machine is typically their live \
     daemon — measured at 3 foreign `SessionStart`s in 8 runs of one fixture (issue #688). \
     Fix: in this `fn`, build the child's env with \
     `crate::test_isolation::pin_unreachable_endpoints(…)` (a `SpawnOptions::env`) or pass \
     `crate::test_isolation::unreachable_endpoints()` to a `Command`'s `.envs(…)`. A deliberate \
     exception — a child pinned at the test's own sandbox daemon — carries \
     `linkage-check:allow-unpinned-emitter` in a comment on or directly above the `fn` line";

/// What the registry says about the agents, as far as this rule needs.
#[derive(Debug, Default)]
pub struct Agents {
    /// Every `detect_basenames` entry across the registered agents.
    pub basenames: BTreeSet<String>,
    /// `AgentType` variants whose spec declares the Wrapper strategy.
    pub wrapper_variants: BTreeSet<String>,
}

/// Read [`Agents`] out of `src/agent_registry.rs`'s source.
///
/// `Err` when the shape this depends on has moved: no `AgentSpec` static at
/// all, a spec missing one of the three fields read here, or no basename in
/// any spec. Each would otherwise empty a trigger silently.
pub fn read_agents(text: &str) -> Result<Agents, String> {
    let file = syn::parse_file(text).map_err(|e| format!("does not parse: {e}"))?;
    let mut agents = Agents::default();
    let mut specs = 0usize;
    for item in &file.items {
        let syn::Item::Static(item) = item else {
            continue;
        };
        let syn::Expr::Struct(lit) = item.expr.as_ref() else {
            continue;
        };
        if last_segment(&lit.path).as_deref() != Some("AgentSpec") {
            continue;
        }
        specs += 1;
        let field = |name: &str| {
            lit.fields.iter().find(|f| match &f.member {
                syn::Member::Named(ident) => ident == name,
                syn::Member::Unnamed(_) => false,
            })
        };
        let (Some(agent_type), Some(basenames), Some(strategy)) = (
            field("agent_type"),
            field("detect_basenames"),
            field("strategy"),
        ) else {
            return Err(format!(
                "`static {}: AgentSpec` lacks one of `agent_type`, `detect_basenames`, \
                 `strategy` — the fields this rule reads its triggers from",
                item.ident
            ));
        };
        let mut lits = Collect::default();
        lits.visit_expr(&basenames.expr);
        agents.basenames.extend(lits.strings);
        let mut strat = Collect::default();
        strat.visit_expr(&strategy.expr);
        if strat.path_ends.contains("Wrapper") {
            let syn::Expr::Path(p) = &agent_type.expr else {
                return Err(format!(
                    "`static {}: AgentSpec` declares the Wrapper strategy but its `agent_type` \
                     is not a plain `AgentType::…` path",
                    item.ident
                ));
            };
            if let Some(variant) = last_segment(&p.path) {
                agents.wrapper_variants.insert(variant);
            }
        }
    }
    if specs == 0 {
        return Err(
            "holds no `static …: AgentSpec = AgentSpec { … }` — the registry moved, \
                    and this rule would match no agent"
                .into(),
        );
    }
    if agents.basenames.is_empty() {
        return Err("no `AgentSpec` declares a `detect_basenames` entry".into());
    }
    Ok(agents)
}

/// Run the check against `root` (the repository root).
pub fn run(root: &Path) -> Vec<String> {
    let src = root.join(SRC);
    if !src.is_dir() {
        return vec![format!(
            "{SRC}/ not found under {} — this rule scanned nothing",
            root.display()
        )];
    }
    let registry_path = join(root, &REGISTRY);
    let agents = match std::fs::read_to_string(&registry_path) {
        Ok(text) => match read_agents(&text) {
            Ok(agents) => agents,
            Err(e) => return vec![format!("{}: {e}", REGISTRY.join("/"))],
        },
        Err(e) => {
            return vec![format!(
                "{}: cannot be read ({e}) — the agent registry this rule reads its triggers \
                 from moved; point the rule at it rather than deleting the rule",
                REGISTRY.join("/")
            )];
        }
    };
    let mut out = Vec::new();
    match std::fs::read_to_string(join(root, &ISOLATION)) {
        Ok(text) => out.extend(missing_helpers(&text).into_iter().map(|helper| {
            format!(
                "{}: defines no `fn {helper}` — the pin this rule tells every finding to use \
                 is gone or renamed",
                ISOLATION.join("/")
            )
        })),
        Err(e) => out.push(format!(
            "{}: cannot be read ({e}) — the pin helpers this rule points at are gone",
            ISOLATION.join("/")
        )),
    }

    let mut files = Vec::new();
    let mut unreadable = Vec::new();
    collect_rs(&src, &mut files, &mut unreadable);
    out.extend(unreadable.into_iter().map(|(dir, e)| {
        format!(
            "{}: cannot be listed ({e}) — the unit tests under it were not scanned, which is \
             a failure rather than a skip",
            display(root, &dir)
        )
    }));
    files.sort();
    if files.is_empty() {
        out.push(format!(
            "{SRC}/ holds no .rs files — this rule scanned nothing"
        ));
        return out;
    }

    let mut parsed: BTreeMap<PathBuf, (String, syn::File)> = BTreeMap::new();
    for file in &files {
        let display = display(root, file);
        let Ok(text) = std::fs::read_to_string(file) else {
            out.push(format!("{display}: could not be read as UTF-8"));
            continue;
        };
        match syn::parse_file(&text) {
            Ok(ast) => {
                parsed.insert(file.clone(), (text, ast));
            }
            Err(e) => out.push(format!(
                "{display}: does not parse ({e}) — a file this rule cannot read is a failure, \
                 never a skip"
            )),
        }
    }

    let test_files = test_only_files(&parsed);
    let mut spawn_literals = 0usize;
    for (file, (text, ast)) in &parsed {
        let report = scan(
            &display(root, file),
            text,
            ast,
            test_files.contains(file),
            &agents,
        );
        spawn_literals += report.spawn_literals;
        out.extend(report.findings);
    }
    if spawn_literals == 0 {
        out.push(format!(
            "{SRC}/: saw no `SpawnOptions {{ … }}` literal in test code — the type was renamed \
             or moved, and this rule is now matching nothing"
        ));
    }
    out
}

/// Pin helpers `test_isolation.rs` does not define.
fn missing_helpers(text: &str) -> Vec<&'static str> {
    let Ok(file) = syn::parse_file(text) else {
        return PIN_HELPERS.to_vec();
    };
    let defined: BTreeSet<String> = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(f) => Some(f.sig.ident.to_string()),
            _ => None,
        })
        .collect();
    PIN_HELPERS
        .into_iter()
        .filter(|h| !defined.contains(*h))
        .collect()
}

/// Every file reached by a `mod x;` declaration from test code, transitively.
fn test_only_files(parsed: &BTreeMap<PathBuf, (String, syn::File)>) -> BTreeSet<PathBuf> {
    let mut decls: BTreeMap<&PathBuf, Vec<(PathBuf, bool)>> = BTreeMap::new();
    for (file, (_, ast)) in parsed {
        let mut found = Vec::new();
        let file_test = cfg_selects_test_only(&ast.attrs);
        collect_mod_decls(
            &ast.items,
            file,
            &module_dir(file),
            file_test,
            false,
            &mut found,
        );
        decls.insert(file, found);
    }
    let mut set = BTreeSet::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    for found in decls.values() {
        for (child, test) in found {
            if *test && set.insert(child.clone()) {
                queue.push_back(child.clone());
            }
        }
    }
    while let Some(file) = queue.pop_front() {
        if let Some(found) = decls.get(&file) {
            for (child, _) in found {
                if set.insert(child.clone()) {
                    queue.push_back(child.clone());
                }
            }
        }
    }
    set
}

/// `mod x;` declarations in `items`, resolved to the file each loads, with
/// whether the declaration sits in test code.
fn collect_mod_decls(
    items: &[syn::Item],
    file: &Path,
    dir: &Path,
    in_test: bool,
    inline: bool,
    out: &mut Vec<(PathBuf, bool)>,
) {
    for item in items {
        let syn::Item::Mod(m) = item else {
            continue;
        };
        let test = in_test || cfg_selects_test_only(&m.attrs);
        let explicit = m.attrs.iter().find_map(|attr| {
            if !attr.path().is_ident("path") {
                return None;
            }
            let syn::Meta::NameValue(nv) = &attr.meta else {
                return None;
            };
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &nv.value
            else {
                return None;
            };
            Some(s.value())
        });
        match &m.content {
            Some((_, inner)) => {
                collect_mod_decls(inner, file, &dir.join(m.ident.to_string()), test, true, out);
            }
            None => {
                // A `#[path]` at a file's top level is relative to the file's
                // own directory; inside an inline module it is relative to
                // that module's directory, which is what `dir` has accumulated.
                let base = if inline {
                    dir
                } else {
                    file.parent().unwrap_or(dir)
                };
                let candidates = match &explicit {
                    Some(p) => vec![base.join(p)],
                    None => {
                        let name = m.ident.to_string();
                        vec![
                            dir.join(format!("{name}.rs")),
                            dir.join(&name).join("mod.rs"),
                        ]
                    }
                };
                if let Some(found) = candidates.into_iter().find(|c| c.is_file()) {
                    out.push((found, test));
                }
            }
        }
    }
}

/// The directory a file's `mod x;` declarations resolve against.
fn module_dir(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("")).to_path_buf();
    match file.file_name().and_then(|n| n.to_str()) {
        Some("mod.rs" | "lib.rs" | "main.rs") => parent,
        _ => match file.file_stem() {
            Some(stem) => parent.join(stem),
            None => parent,
        },
    }
}

/// What [`scan`] found in one file.
#[derive(Debug, Default)]
pub struct Report {
    pub findings: Vec<String>,
    /// `SpawnOptions` literals seen in test code — the vacuity check's input.
    pub spawn_literals: usize,
}

/// Scan one parsed file. `whole_file_is_test` is set for a file some test
/// code declared as a module.
pub fn scan(
    display: &str,
    text: &str,
    ast: &syn::File,
    whole_file_is_test: bool,
    agents: &Agents,
) -> Report {
    let mut scan = Scan {
        agents,
        test_depth: usize::from(whole_file_is_test || cfg_selects_test_only(&ast.attrs)),
        fns: Vec::new(),
        unpinned: Vec::new(),
        spawn_literals: 0,
    };
    scan.visit_file(ast);
    let raw: Vec<&str> = text.lines().collect();
    let findings = scan
        .unpinned
        .into_iter()
        .filter_map(|u| {
            let line = u.line;
            if let Some(line) = line
                && allowed(&raw, line)
            {
                return None;
            }
            let at = line.map(|l| format!(":{l}")).unwrap_or_default();
            let who = match &u.name {
                Some(name) => format!("fn {name}"),
                None => "outside any fn".into(),
            };
            Some(format!(
                "{display}{at}: {who}: {} — {UNPINNED_EMITTER_RULE}",
                u.triggers.join("; ")
            ))
        })
        .collect();
    Report {
        findings,
        spawn_literals: scan.spawn_literals,
    }
}

/// Whether the opt-out marker sits on `line` (1-indexed) or in the unbroken
/// run of comment / attribute lines directly above it.
fn allowed(raw: &[&str], line: usize) -> bool {
    let Some(idx) = line.checked_sub(1) else {
        return false;
    };
    if raw.get(idx).is_some_and(|l| l.contains(ALLOW)) {
        return true;
    }
    for l in raw[..idx.min(raw.len())].iter().rev() {
        let t = l.trim_start();
        if !(t.starts_with("//") || t.starts_with("#[") || t.starts_with('*')) {
            return false;
        }
        if t.contains(ALLOW) {
            return true;
        }
    }
    false
}

struct FnScope {
    name: Option<String>,
    /// The `fn` keyword's line, from syn's span (`proc-macro2`'s
    /// `span-locations`), so a `fn`-shaped token inside a macro body cannot
    /// shift it the way a text search would.
    line: usize,
    triggers: Vec<String>,
    pinned: bool,
}

struct Unpinned {
    name: Option<String>,
    line: Option<usize>,
    triggers: Vec<String>,
}

struct Scan<'a> {
    agents: &'a Agents,
    test_depth: usize,
    fns: Vec<FnScope>,
    unpinned: Vec<Unpinned>,
    spawn_literals: usize,
}

impl Scan<'_> {
    fn in_test(&self) -> bool {
        self.test_depth > 0
    }

    fn enter_fn(&mut self, sig: &syn::Signature, is_test_fn: bool) {
        let line = sig.fn_token.span.start().line;
        let name = sig.ident.to_string();
        if is_test_fn {
            self.test_depth += 1;
        }
        self.fns.push(FnScope {
            name: Some(name),
            line,
            triggers: Vec::new(),
            pinned: false,
        });
    }

    fn leave_fn(&mut self, is_test_fn: bool) {
        if is_test_fn {
            self.test_depth -= 1;
        }
        let scope = self.fns.pop().expect("balanced fn scopes");
        // Triggers are only ever recorded from test code, so a production `fn`
        // with a `#[cfg(test)]` block inside it is judged on that block alone.
        if !scope.pinned && !scope.triggers.is_empty() {
            self.unpinned.push(Unpinned {
                name: scope.name,
                line: Some(scope.line),
                triggers: scope.triggers,
            });
        }
    }

    fn trigger(&mut self, what: String) {
        if !self.in_test() {
            return;
        }
        match self.fns.last_mut() {
            Some(scope) => scope.triggers.push(what),
            None => self.unpinned.push(Unpinned {
                name: None,
                line: None,
                triggers: vec![what],
            }),
        }
    }

    /// Whether `word` is the deck binary or a registered agent, and which.
    fn emitter_word(&self, word: &str) -> Option<String> {
        if word == DECK_BINARY {
            Some("the deck binary".into())
        } else if self.agents.basenames.contains(word) {
            Some(format!("the `{word}` agent"))
        } else {
            None
        }
    }

    /// What in `expr` makes it a command that runs an emitter, if anything.
    ///
    /// `expr` sits in PROGRAM position — a `SpawnOptions::command`, or
    /// `Command::new`'s argument — unless `as_args` is set, in which case it is
    /// what a [`LAUNCHERS`] program was handed. In program position only a
    /// literal's first word is the program, so `echo pi` names no agent; the
    /// rest of the literal is read too only when that first word is itself a
    /// launcher (`sh -c 'codex exec'`), the way `AgentType::from_command` sees
    /// through one. Every word of a launcher's arguments is read.
    fn command_emitter(&self, expr: &syn::Expr, as_args: bool) -> Option<String> {
        let mut c = Collect::default();
        c.visit_expr(expr);
        for s in &c.strings {
            let words: Vec<&str> = if as_args {
                command_tokens(s).collect()
            } else {
                let mut split = s.split_whitespace();
                let Some(first) = split.next().map(program_basename) else {
                    continue;
                };
                if LAUNCHERS.contains(&first) {
                    std::iter::once(first)
                        .chain(split.flat_map(command_tokens))
                        .collect()
                } else {
                    vec![first]
                }
            };
            if let Some(what) = words.into_iter().find_map(|w| self.emitter_word(w)) {
                return Some(format!("command names {what} (`{s}`)"));
            }
        }
        DECK_BINARY_RESOLVERS
            .into_iter()
            .find(|r| c.call_ends.contains(*r))
            .map(|r| format!("command is the deck binary (`{r}(…)`)"))
    }

    fn scoped<F: FnOnce(&mut Self)>(&mut self, test: bool, f: F) {
        if test {
            self.test_depth += 1;
        }
        f(self);
        if test {
            self.test_depth -= 1;
        }
    }
}

/// `std::process::Command::new`, `tokio::process::Command::new`,
/// `portable_pty::CommandBuilder::new`, however much of the path is written.
fn is_command_ctor(expr: &syn::Expr) -> bool {
    let syn::Expr::Path(p) = expr else {
        return false;
    };
    let segs: Vec<String> = p
        .path
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect();
    matches!(
        segs.as_slice(),
        [.., ty, new] if new == "new" && (ty == "Command" || ty == "CommandBuilder")
    )
}

/// The program basename of the command constructor a method-call chain is
/// rooted at, when that program is a string literal.
fn command_ctor_program(mut expr: &syn::Expr) -> Option<String> {
    loop {
        match expr {
            syn::Expr::MethodCall(m) => expr = &m.receiver,
            syn::Expr::Paren(p) => expr = &p.expr,
            syn::Expr::Reference(r) => expr = &r.expr,
            syn::Expr::Call(c) if is_command_ctor(&c.func) => {
                let mut lits = Collect::default();
                lits.visit_expr(c.args.first()?);
                return lits
                    .strings
                    .first()
                    .map(|s| program_basename(s).to_string());
            }
            _ => return None,
        }
    }
}

/// Programs that run another program named in their arguments, so their
/// arguments are read as commands too. Anything else's arguments are data.
pub const LAUNCHERS: [&str; 14] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "env",
    "sudo",
    "exec",
    "nohup",
    "time",
    "xargs",
    "cmd",
    "powershell",
    "pwsh",
];

/// A program word's basename, quotes and a trailing `.exe` removed:
/// `'/usr/bin/codex'` and `C:\bin\dot-agent-deck.exe` yield `codex` and
/// `dot-agent-deck`.
fn program_basename(word: &str) -> &str {
    let word = word.trim_matches(|c| c == '\'' || c == '"');
    let base = word.rsplit(['/', '\\']).next().unwrap_or(word);
    base.strip_suffix(".exe").unwrap_or(base)
}

/// Split a command literal into the words a basename comparison needs:
/// `"/usr/bin/codex --x"` yields `usr`, `bin`, `codex`, `--x`. A trailing
/// `.exe` is dropped so a Windows spelling matches too.
fn command_tokens(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .filter(|t| !t.is_empty())
        .map(|t| t.strip_suffix(".exe").unwrap_or(t))
}

fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs
        .iter()
        .any(|a| a.path().segments.last().is_some_and(|s| s.ident == "test"))
}

fn last_segment(path: &syn::Path) -> Option<String> {
    path.segments.last().map(|s| s.ident.to_string())
}

/// Try a macro's tokens as comma-separated expressions — `vec![…]`,
/// `assert!(…)`, `format!(…)` — so what sits inside one is visible.
fn macro_exprs(mac: &syn::Macro) -> Vec<syn::Expr> {
    use syn::parse::Parser;
    syn::punctuated::Punctuated::<syn::Expr, syn::Token![,]>::parse_terminated
        .parse2(mac.tokens.clone())
        .map(|p| p.into_iter().collect())
        .unwrap_or_default()
}

impl<'ast> Visit<'ast> for Scan<'_> {
    fn visit_item(&mut self, item: &'ast syn::Item) {
        let test = item_attrs(item).is_some_and(cfg_selects_test_only);
        self.scoped(test, |s| syn::visit::visit_item(s, item));
    }

    fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
        let test = impl_item_attrs(item).is_some_and(cfg_selects_test_only);
        self.scoped(test, |s| syn::visit::visit_impl_item(s, item));
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        let test = cfg_selects_test_only(&node.attrs);
        self.scoped(test, |s| syn::visit::visit_local(s, node));
    }

    fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
        let test = cfg_selects_test_only(&node.attrs);
        self.scoped(test, |s| syn::visit::visit_expr_block(s, node));
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        let test = cfg_selects_test_only(&node.attrs);
        self.scoped(test, |s| syn::visit::visit_stmt_macro(s, node));
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let is_test = has_test_attr(&node.attrs);
        self.enter_fn(&node.sig, is_test);
        syn::visit::visit_item_fn(self, node);
        self.leave_fn(is_test);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        let is_test = has_test_attr(&node.attrs);
        self.enter_fn(&node.sig, is_test);
        syn::visit::visit_impl_item_fn(self, node);
        self.leave_fn(is_test);
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        let is_test = has_test_attr(&node.attrs);
        self.enter_fn(&node.sig, is_test);
        syn::visit::visit_trait_item_fn(self, node);
        self.leave_fn(is_test);
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if self.in_test() && last_segment(&node.path).as_deref() == Some("SpawnOptions") {
            self.spawn_literals += 1;
            for field in &node.fields {
                let syn::Member::Named(name) = &field.member else {
                    continue;
                };
                if name == "agent_type" {
                    let mut c = Collect::default();
                    c.visit_expr(&field.expr);
                    if let Some(variant) = self
                        .agents
                        .wrapper_variants
                        .iter()
                        .find(|v| c.path_ends.contains(*v))
                    {
                        self.trigger(format!(
                            "`SpawnOptions` declares `agent_type` `AgentType::{variant}`, a \
                             Wrapper-strategy agent, so `spawn` runs the command under \
                             `dot-agent-deck wrap`"
                        ));
                    }
                } else if name == "command"
                    && let Some(what) = self.command_emitter(&field.expr, false)
                {
                    self.trigger(format!("`SpawnOptions` {what}"));
                }
            }
        }
        syn::visit::visit_expr_struct(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        // Only a CALL pins: naming the helper as a value, or importing it,
        // applies nothing to any child.
        if let syn::Expr::Path(p) = node.func.as_ref()
            && last_segment(&p.path).is_some_and(|l| PIN_HELPERS.contains(&l.as_str()))
            && let Some(scope) = self.fns.last_mut()
        {
            scope.pinned = true;
        }
        if self.in_test() && is_command_ctor(&node.func) {
            let found = node
                .args
                .iter()
                .find_map(|a| self.command_emitter(a, false));
            if let Some(what) = found {
                self.trigger(format!("`Command::new` {what}"));
            }
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        if self.in_test()
            && (node.method == "arg" || node.method == "args")
            && command_ctor_program(&node.receiver)
                .is_some_and(|program| LAUNCHERS.contains(&program.as_str()))
        {
            let found = node.args.iter().find_map(|a| self.command_emitter(a, true));
            if let Some(what) = found {
                self.trigger(format!("`.{}(…)` on a `Command` — {what}", node.method));
            }
        }
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        for expr in macro_exprs(node) {
            self.visit_expr(&expr);
        }
    }

    /// Attributes are not scanned: `///` doc comments reach syn as
    /// `#[doc = "…"]`, and prose about an emitter is not one.
    fn visit_attribute(&mut self, _node: &'ast syn::Attribute) {}
}

/// String literals, the last segment of every path, and the last segment of
/// every called function's path, inside one expression.
#[derive(Default)]
struct Collect {
    strings: Vec<String>,
    path_ends: BTreeSet<String>,
    call_ends: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Collect {
    fn visit_lit_str(&mut self, node: &'ast syn::LitStr) {
        self.strings.push(node.value());
    }

    fn visit_path(&mut self, node: &'ast syn::Path) {
        if let Some(last) = last_segment(node) {
            self.path_ends.insert(last);
        }
        syn::visit::visit_path(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = node.func.as_ref()
            && let Some(last) = last_segment(&p.path)
        {
            self.call_ends.insert(last);
        }
        syn::visit::visit_expr_call(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.call_ends.insert(node.method.to_string());
        syn::visit::visit_expr_method_call(self, node);
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        for expr in macro_exprs(node) {
            self.visit_expr(&expr);
        }
    }
}

fn join(root: &Path, parts: &[&str]) -> PathBuf {
    parts.iter().fold(root.to_path_buf(), |p, c| p.join(c))
}

fn display(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Every `.rs` under `dir`. A directory or entry that cannot be read is
/// recorded in `errors` rather than dropped, so a subtree this rule could not
/// see is a finding instead of a quiet pass.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<(PathBuf, std::io::Error)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push((dir.to_path_buf(), e));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push((dir.to_path_buf(), e));
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out, errors);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY_FIXTURE: &str = r#"
pub static CLAUDE_CODE: AgentSpec = AgentSpec {
    agent_type: AgentType::ClaudeCode,
    detect_basenames: &["claude"],
    strategy: Some(IntegrationStrategy::NativeHooks),
};
pub static CODEX: AgentSpec = AgentSpec {
    agent_type: AgentType::Codex,
    detect_basenames: &["codex"],
    strategy: Some(IntegrationStrategy::Wrapper),
};
pub static NONE: AgentSpec = AgentSpec {
    agent_type: AgentType::None,
    detect_basenames: &[],
    strategy: None,
};
"#;

    fn agents() -> Agents {
        read_agents(REGISTRY_FIXTURE).expect("fixture registry parses")
    }

    fn findings(src: &str) -> Vec<String> {
        let ast = syn::parse_file(src).expect("fixture parses");
        scan("src/x.rs", src, &ast, false, &agents()).findings
    }

    #[test]
    fn registry_yields_every_basename_and_only_wrapper_variants() {
        let a = agents();
        assert_eq!(
            a.basenames.iter().map(String::as_str).collect::<Vec<_>>(),
            ["claude", "codex"]
        );
        assert_eq!(
            a.wrapper_variants
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["Codex"]
        );
    }

    #[test]
    fn a_registry_whose_shape_moved_is_an_error_not_an_empty_trigger() {
        assert!(read_agents("pub static X: u8 = 1;").is_err());
        let renamed = REGISTRY_FIXTURE.replace("strategy:", "integration:");
        let err = read_agents(&renamed).expect_err("a missing field must be reported");
        assert!(err.contains("`strategy`"), "{err}");
    }

    /// The #666 shape: a declared Wrapper-strategy type at the spawn.
    #[test]
    fn a_wrapper_agent_type_in_a_test_spawn_is_reported_at_its_fn() {
        let src = "\
#[cfg(test)]
mod tests {
    #[test]
    fn spawns_codex() {
        registry.spawn_agent(SpawnOptions {
            command: Some(\"/bin/cat\"),
            agent_type: Some(AgentType::Codex),
            ..SpawnOptions::default()
        });
    }
}
";
        let found = findings(src);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("src/x.rs:4: fn spawns_codex: "),
            "{}",
            found[0]
        );
        assert!(found[0].contains("AgentType::Codex"), "{}", found[0]);
        assert!(found[0].contains(UNPINNED_EMITTER_RULE));
    }

    #[test]
    fn a_native_agent_type_is_not_a_wrapper_trigger() {
        let src = "\
#[cfg(test)]
mod tests {
    fn f() {
        let _ = SpawnOptions { command: Some(\"/bin/cat\"), agent_type: Some(AgentType::ClaudeCode), ..Default::default() };
    }
}
";
        assert!(findings(src).is_empty());
    }

    #[test]
    fn command_literals_naming_an_agent_or_the_deck_are_reported() {
        for command in [
            "codex",
            "/usr/local/bin/claude --model haiku",
            "sh -c 'codex exec hi'",
            "dot-agent-deck hook",
            r"C:\\bin\\dot-agent-deck.exe",
        ] {
            let src = format!(
                "#[cfg(test)]\nmod t {{\n    fn f() {{\n        let _ = SpawnOptions {{ command: Some({command:?}), ..Default::default() }};\n    }}\n}}\n"
            );
            assert_eq!(findings(&src).len(), 1, "{command:?} was not reported");
        }
        // Data handed to a program that is not a launcher is not a command,
        // and a word that merely CONTAINS an agent name is not one.
        for command in ["echo pi", "printf codex", "/bin/codexish claude-like"] {
            let src = format!(
                "#[cfg(test)]\nmod t {{\n    fn f() {{\n        let _ = SpawnOptions {{ command: Some({command:?}), ..Default::default() }};\n    }}\n}}\n"
            );
            assert!(findings(&src).is_empty(), "{command:?} was reported");
        }
        let src = "#[cfg(test)]\nmod t {\n    fn f() {\n        let _ = SpawnOptions { command: Some(\"/bin/codexish claude-like\"), ..Default::default() };\n    }\n}\n";
        assert!(findings(src).is_empty());
    }

    #[test]
    fn a_deck_binary_resolver_as_the_command_is_reported() {
        let src = "\
#[cfg(test)]
mod t {
    fn f() {
        let _ = std::process::Command::new(crate::platform::paths::binary_name());
        let _ = SpawnOptions { command: Some(&wrap_launch_command(\"/bin/cat\", &t)), ..Default::default() };
    }
}
";
        let found = findings(src);
        assert_eq!(found.len(), 1, "one finding per fn: {found:#?}");
        assert!(found[0].contains("binary_name"), "{}", found[0]);
        assert!(found[0].contains("wrap_launch_command"), "{}", found[0]);
    }

    #[test]
    fn command_args_chained_onto_a_ctor_are_checked() {
        let src = "\
#[cfg(test)]
mod t {
    fn f() {
        let _ = tokio::process::Command::new(\"sh\").arg(\"-c\").args([\"codex\"]).spawn();
        let _ = std::process::Command::new(\"git\").args([\"log\", \"--oneline\"]).spawn();
        let _ = std::process::Command::new(\"printf\").arg(\"pi\").spawn();
    }
}
";
        let found = findings(src);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("`.args(…)`"), "{}", found[0]);
    }

    #[test]
    fn either_pin_helper_in_the_same_fn_clears_it() {
        for pin in [
            "env: crate::test_isolation::pin_unreachable_endpoints(vec![]),",
            "env: { let e = test_isolation::unreachable_endpoints(); e },",
        ] {
            let src = format!(
                "#[cfg(test)]\nmod t {{\n    fn f() {{\n        let _ = SpawnOptions {{ command: Some(\"codex\"), {pin} ..Default::default() }};\n    }}\n}}\n"
            );
            assert!(findings(&src).is_empty(), "{pin} did not clear the rule");
        }
        // Pinned in a closure inside the fn still counts: the closure is the fn.
        let src = "#[cfg(test)]\nmod t {\n    fn f() {\n        let pin = || pin_unreachable_endpoints(vec![]);\n        let _ = SpawnOptions { command: Some(\"codex\"), env: pin(), ..Default::default() };\n    }\n}\n";
        assert!(findings(src).is_empty());
    }

    /// Clearance is per `fn`, innermost: a pin in a sibling does not reach.
    #[test]
    fn a_pin_in_another_fn_does_not_clear_it() {
        let src = "\
#[cfg(test)]
mod t {
    fn pins() { let _ = pin_unreachable_endpoints(vec![]); }
    fn spawns() {
        let _ = SpawnOptions { command: Some(\"codex\"), ..Default::default() };
    }
}
";
        let found = findings(src);
        assert_eq!(found.len(), 1);
        assert!(
            found[0].starts_with("src/x.rs:4: fn spawns: "),
            "{}",
            found[0]
        );
    }

    #[test]
    fn production_code_is_out_of_scope() {
        let src = "\
fn launch() {
    let _ = SpawnOptions { command: Some(\"codex\"), agent_type: Some(AgentType::Codex), ..Default::default() };
}
#[cfg(any(test, debug_assertions))]
fn debug_too() {
    let _ = std::process::Command::new(\"dot-agent-deck\");
}
#[cfg(not(test))]
fn not_test() {
    let _ = std::process::Command::new(\"dot-agent-deck\");
}
";
        assert!(findings(src).is_empty());
    }

    /// A `#[test]` fn is test code with or without a `cfg(test)` module round
    /// it, and so is a whole file declared from test code.
    #[test]
    fn test_attrs_and_test_only_files_are_in_scope() {
        let src = "#[tokio::test]\nasync fn f() {\n    let _ = std::process::Command::new(\"codex\");\n}\n";
        assert_eq!(findings(src).len(), 1);

        let src = "fn helper() {\n    let _ = std::process::Command::new(\"codex\");\n}\n";
        assert!(findings(src).is_empty(), "a plain fn is production");
        let ast = syn::parse_file(src).unwrap();
        assert_eq!(
            scan("src/support.rs", src, &ast, true, &agents())
                .findings
                .len(),
            1,
            "the same fn in a test-only file is test code"
        );
    }

    #[test]
    fn a_cfg_test_block_inside_a_production_fn_is_in_scope() {
        let src = "fn prod() {\n    #[cfg(test)]\n    {\n        let _ = std::process::Command::new(\"codex\");\n    }\n}\n";
        assert_eq!(findings(src).len(), 1);
    }

    #[test]
    fn emitters_inside_macros_are_seen() {
        let src = "#[cfg(test)]\nmod t {\n    fn f() {\n        let _ = vec![SpawnOptions { agent_type: Some(AgentType::Codex), ..Default::default() }];\n    }\n}\n";
        assert_eq!(findings(src).len(), 1);
    }

    #[test]
    fn prose_about_an_emitter_is_not_one() {
        let src = "\
#[cfg(test)]
mod t {
    /// `SpawnOptions { command: Some(\"codex\") }` would be an emitter.
    fn f() {
        // std::process::Command::new(\"dot-agent-deck\")
        let _ = \"codex\";
    }
}
";
        assert!(findings(src).is_empty());
    }

    #[test]
    fn the_marker_above_or_on_the_fn_line_opts_out() {
        let above = "\
#[cfg(test)]
mod t {
    // linkage-check:allow-unpinned-emitter: pinned at this test's own daemon.
    #[test]
    fn f() {
        let _ = std::process::Command::new(\"codex\");
    }
}
";
        assert!(findings(above).is_empty());
        let on = "#[cfg(test)]\nmod t {\n    fn f() { // linkage-check:allow-unpinned-emitter\n        let _ = std::process::Command::new(\"codex\");\n    }\n}\n";
        assert!(findings(on).is_empty());
        // A marker separated from the fn by code does not reach it.
        let far = "#[cfg(test)]\nmod t {\n    // linkage-check:allow-unpinned-emitter\n    const X: u8 = 1;\n    fn f() {\n        let _ = std::process::Command::new(\"codex\");\n    }\n}\n";
        assert_eq!(findings(far).len(), 1);
    }

    /// Two fns with one name in different modules: the line is the right one.
    #[test]
    fn the_reported_line_is_the_right_fn_of_a_shared_name() {
        let src = "\
fn f() {}
#[cfg(test)]
mod t {
    fn f() {
        let _ = std::process::Command::new(\"codex\");
    }
}
";
        let found = findings(src);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("src/x.rs:4: fn f: "), "{}", found[0]);
    }

    #[test]
    fn test_only_modules_are_followed_transitively() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("support")).unwrap();
        let files = [
            ("lib.rs", "#[cfg(test)]\nmod support;\nmod prod;\n"),
            ("support.rs", "mod inner;\n"),
            ("support/inner.rs", "fn f() {}\n"),
            ("prod.rs", "fn g() {}\n"),
        ];
        let mut parsed = BTreeMap::new();
        for (name, text) in files {
            let path = src.join(name);
            std::fs::write(&path, text).unwrap();
            parsed.insert(path, (text.to_string(), syn::parse_file(text).unwrap()));
        }
        let set = test_only_files(&parsed);
        assert!(set.contains(&src.join("support.rs")));
        assert!(set.contains(&src.join("support/inner.rs")));
        assert!(!set.contains(&src.join("prod.rs")));
    }

    #[test]
    fn missing_inputs_are_findings_not_a_vacuous_pass() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(run(dir.path())[0].contains("scanned nothing"));

        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let found = run(dir.path());
        assert!(found[0].contains("agent_registry.rs"), "{found:#?}");

        std::fs::write(src.join("agent_registry.rs"), REGISTRY_FIXTURE).unwrap();
        std::fs::write(src.join("lib.rs"), "fn f() {}\n").unwrap();
        let found = run(dir.path());
        assert!(
            found.iter().any(|f| f.contains("test_isolation.rs")),
            "{found:#?}"
        );
        assert!(
            found.iter().any(|f| f.contains("saw no `SpawnOptions")),
            "{found:#?}"
        );
    }

    /// Naming a helper is not calling it (PR #1315 review): an import, or the
    /// helper passed around as a value, pins nothing.
    #[test]
    fn naming_a_pin_helper_without_calling_it_does_not_clear_it() {
        let src = "\
#[cfg(test)]
mod t {
    fn f() {
        use crate::test_isolation::unreachable_endpoints;
        let _unused = unreachable_endpoints;
        let _ = std::process::Command::new(\"codex\");
    }
}
";
        assert_eq!(findings(src).len(), 1);
    }

    /// A `fn`-shaped token inside a macro body sits before the real `fn` of
    /// the same name. The line — and so the marker — must be the real one's.
    #[test]
    fn a_fn_token_in_a_macro_does_not_move_the_line_or_the_marker() {
        let src = "\
macro_rules! m { () => { fn f() {} }; }
#[cfg(test)]
mod t {
    fn f() {
        let _ = std::process::Command::new(\"codex\");
    }
}
";
        let found = findings(src);
        assert_eq!(found.len(), 1);
        assert!(found[0].starts_with("src/x.rs:4: fn f: "), "{}", found[0]);

        let marked = src.replace(
            "    fn f() {\n",
            "    // linkage-check:allow-unpinned-emitter\n    fn f() {\n",
        );
        assert!(findings(&marked).is_empty(), "the marker on the real fn");
        let misplaced = format!("// linkage-check:allow-unpinned-emitter\n{src}");
        assert_eq!(
            findings(&misplaced).len(),
            1,
            "a marker above the macro must not reach the real fn"
        );
    }

    /// `#[path]` inside an inline test module resolves against that module's
    /// directory, not the file's (PR #1315 review).
    #[test]
    fn a_path_attr_inside_an_inline_test_module_is_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("host").join("tests")).unwrap();
        let files = [
            (
                "host.rs",
                "#[cfg(test)]\nmod tests {\n    #[path = \"fixtures.rs\"]\n    mod fixtures;\n}\n",
            ),
            ("host/tests/fixtures.rs", "fn f() {}\n"),
            // A decoy at the file-relative location the old code resolved.
            ("fixtures.rs", "fn g() {}\n"),
        ];
        let mut parsed = BTreeMap::new();
        for (name, text) in files {
            let path = src.join(name);
            std::fs::write(&path, text).unwrap();
            parsed.insert(path, (text.to_string(), syn::parse_file(text).unwrap()));
        }
        let set = test_only_files(&parsed);
        assert!(
            set.contains(&src.join("host/tests/fixtures.rs")),
            "{set:#?}"
        );
        assert!(!set.contains(&src.join("fixtures.rs")), "{set:#?}");
    }

    #[cfg(unix)]
    #[test]
    fn an_unlistable_directory_is_a_finding() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let locked = dir.path().join("src").join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        // The walk is reached only once the registry reads, so give it one.
        std::fs::write(dir.path().join("src/agent_registry.rs"), REGISTRY_FIXTURE).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Root ignores the mode bits, so there is nothing to observe.
            eprintln!("SKIP: running with permission to read a mode-000 directory");
            return;
        }
        let found = run(dir.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            found
                .iter()
                .any(|f| f.starts_with("src/locked: cannot be listed")),
            "{found:#?}"
        );
    }

    /// The live tree is clean — which only means something because every
    /// shape above is proven to fire on synthetic input.
    #[test]
    fn the_checkout_has_no_unpinned_emitter() {
        let found = run(&crate::repo_root());
        assert!(found.is_empty(), "{found:#?}");
    }
}
