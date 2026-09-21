//! Whether the branch under test changes BUILD-TIME code, decided before
//! anything is built.
//!
//! # Why this exists
//!
//! `cargo build --locked --bin dot-agent-deck` runs in the build clone on the
//! host, outside the namespace, as the operator — like any local build of the
//! branch. Whatever that command executes runs with the operator's files,
//! processes, sockets and network: the package's build scripts, every proc
//! macro, and any compiler wrapper or linker the cargo configuration names. So
//! a branch that supplies build-time code of its own gets that authority the
//! moment the harness builds it, before the namespace exists.
//!
//! This module compares the branch with its merge-base with `main` of `--repo`
//! and classifies every changed path against the build-time surface. A branch
//! that changes any of it is refused before `cargo build` runs, unless the
//! caller opts in with `--allow-build-changes`; either way the evidence file
//! records the answer.
//!
//! # What counts
//!
//! Two layers, both conservative in the same direction — a path wrongly counted
//! costs a refusal, a path wrongly missed costs the gate:
//!
//! * **Name rules**, applied anywhere in the tree: any `build.rs`, any
//!   `Cargo.toml`, any `Cargo.lock` (a new or changed registry crate brings its
//!   own build script and proc macros), `rust-toolchain` and
//!   `rust-toolchain.toml` (they select the compiler rustup runs), and anything
//!   under a `.cargo/` directory (a wrapper, a linker, a source replacement).
//! * **Derived at the merge-base**, from mainline's own tree, which is why the
//!   derivation may run cargo there and nowhere else:
//!   * `cargo metadata --no-deps` names the workspace packages; walking the
//!     local dependency graph from the package that owns the
//!     `dot-agent-deck` bin, over normal and build edges (never dev), finds
//!     each reached package's build script and every reached package that
//!     executes on the host — a proc macro, a build-dependency, or anything
//!     those depend on — whose whole directory then counts;
//!   * each build script's module closure: the files it pulls in with `mod`,
//!     `#[path]`, `include!`, `include_str!` or `include_bytes!` and a literal
//!     path (the root's `build.rs` pulls in `build_version_resolve.rs`);
//!   * every repository path a string in `.cargo/config.toml` (or
//!     `.cargo/config`) names — the root's `linker` is `./scripts/link-gate.sh`
//!     — plus, for a file, each sibling script its text names (the linker
//!     script execs `build-gate.sh` beside it), recursively;
//!   * every `[patch]` or `[replace]` path in the root `Cargo.toml`.
//!
//! The closure rests on one argument: a changed file can make the build
//! execute a NEW file only by naming it, and a file that names one in the ways
//! the scan follows is itself counted, so the change shows up as a change to a
//! counted file. Its edge is exactly those ways — `mod`, `#[path]` and
//! `include*!` with a literal path in a build script's module tree, sibling
//! file names in a config-named script. A file a counted build script runs
//! through `Command`, a counted script runs by a non-sibling path, or anything
//! reaches by a computed name is not followed, so an EXISTING reference of that
//! kind would leave the file it names uncounted. None exists at the time of
//! writing (`build.rs` runs only `git`; the linker script execs no repository
//! file but its sibling `build-gate.sh`).
//!
//! # What this does not do
//!
//! It detects **changes**. The merge-base's own build-time code runs on every
//! build and is trusted by construction; registry crates pinned by an unchanged
//! `Cargo.lock` are unchanged; and a build the caller opts into runs the
//! branch's code with exactly the authority described above. It also trusts
//! `main` of `--repo`: with the default repository that is mainline, with
//! another it is whatever that repository's `main` holds. And compiling ordinary
//! source reads host files too — `include_str!` can name any path the operator
//! can read, past the runtime namespace's home mask — which is not execution
//! and not classified here. Building inside a namespace of its own is the fix
//! for all of that; this is detection.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

/// The base branch every comparison is made against, on `--repo`.
///
/// Deliberately not an option: a caller-chosen base could be the branch itself,
/// which makes every diff empty and the gate a formality.
pub const BASE_BRANCH: &str = "main";

/// File names that are build-time code wherever in the tree they sit.
pub const BUILD_TIME_NAMES: &[(&str, &str)] = &[
    ("build.rs", "a build script name"),
    ("Cargo.toml", "a cargo manifest"),
    (
        "Cargo.lock",
        "the lockfile: a new or changed registry crate brings its own build script and proc macros",
    ),
    (
        "rust-toolchain",
        "a toolchain override: it selects the compiler rustup runs",
    ),
    (
        "rust-toolchain.toml",
        "a toolchain override: it selects the compiler rustup runs",
    ),
];

const CARGO_DIR_REASON: &str = "cargo configuration (`.cargo/`): it can name a compiler wrapper, a linker or a source replacement";

/// The paths derived at the merge-base, each with why it counts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Surface {
    pub files: BTreeMap<String, String>,
    /// Directories whose whole subtree counts. `""` is the repository root.
    pub dirs: BTreeMap<String, String>,
}

impl Surface {
    pub fn merge(&mut self, other: Surface) {
        for (k, v) in other.files {
            self.files.entry(k).or_insert(v);
        }
        for (k, v) in other.dirs {
            self.dirs.entry(k).or_insert(v);
        }
    }

    /// One line naming what was compared, for the evidence file.
    pub fn describe(&self) -> String {
        let names: Vec<String> = BUILD_TIME_NAMES
            .iter()
            .map(|(n, _)| format!("`{n}`"))
            .collect();
        let mut derived: Vec<String> = self.files.keys().map(|f| format!("`{f}`")).collect();
        derived.extend(self.dirs.keys().map(|d| {
            if d.is_empty() {
                "the whole repository".to_string()
            } else {
                format!("`{d}/`")
            }
        }));
        format!(
            "anywhere in the tree: {} and anything under a `.cargo/` directory; derived at the \
             merge-base: {}",
            names.join(", "),
            if derived.is_empty() {
                "nothing".to_string()
            } else {
                derived.join(", ")
            }
        )
    }
}

/// Why `path` (repository-relative, `/`-separated) is build-time code, or
/// `None` when it is not.
pub fn classify(path: &str, surface: &Surface) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if let Some((_, why)) = BUILD_TIME_NAMES.iter().find(|(n, _)| *n == name) {
        return Some((*why).to_string());
    }
    if path.split('/').any(|c| c == ".cargo") {
        return Some(CARGO_DIR_REASON.to_string());
    }
    if let Some(why) = surface.files.get(path) {
        return Some(why.clone());
    }
    surface
        .dirs
        .iter()
        .find(|(dir, _)| dir.is_empty() || path.starts_with(&format!("{dir}/")))
        .map(|(_, why)| why.clone())
}

/// The changed paths that are build-time code, each with why.
pub fn build_time_changes(changed: &BTreeSet<String>, surface: &Surface) -> Vec<(String, String)> {
    changed
        .iter()
        .filter_map(|p| classify(p, surface).map(|why| (p.clone(), why)))
        .collect()
}

/// What a comparison was made against, for the evidence file and refusals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub repo: String,
    /// `main` of `repo` when it was fetched.
    pub base_sha: String,
    /// Usually one; a criss-cross history has several, and each is compared.
    pub merge_bases: Vec<String>,
    /// [`Surface::describe`] of the union across the merge-bases.
    pub surface: String,
}

impl Comparison {
    fn base(&self) -> String {
        format!(
            "the merge-base with `origin/{BASE_BRANCH}` (`{}` `{BASE_BRANCH}` at `{}`; merge-base \
             {})",
            self.repo,
            self.base_sha,
            self.merge_bases
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn list_changes(changes: &[(String, String)]) -> String {
    changes
        .iter()
        .map(|(p, why)| format!("`{p}` ({why})"))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The gate's decision. `Ok` is the evidence line for a run that proceeds;
/// `Err` is the refusal, returned before `cargo build` runs.
pub fn decide(
    branch: &str,
    changes: &[(String, String)],
    cmp: &Comparison,
    allow: bool,
    skip_build: bool,
) -> Result<String, String> {
    if changes.is_empty() {
        return Ok(format!(
            "build-time code identical to {}: no changed path is build-time code (compared: {})",
            cmp.base(),
            cmp.surface
        ));
    }
    let list = list_changes(changes);
    if skip_build {
        return Ok(format!(
            "**the branch CHANGES build-time code** relative to {}: {list}. `--skip-build`: \
             `cargo build` was not run, so none of it executed in this run{}",
            cmp.base(),
            if allow {
                " (`--allow-build-changes` was also given, and had nothing to allow)"
            } else {
                ""
            }
        ));
    }
    if allow {
        return Ok(format!(
            "**the branch CHANGES build-time code** relative to {}: {list}. The caller opted in \
             with `--allow-build-changes`, so `cargo build` executed those changes on the host, \
             outside the namespace, with the operator's files, processes, sockets and network",
            cmp.base()
        ));
    }
    Err(format!(
        "refusing before `cargo build` — nothing was compiled. Branch `{branch}` changes \
         build-time code relative to {}: {list}. `cargo build` would execute that code on this \
         host, outside the namespace, as you: with your files, processes, sockets and network. \
         Review those changes, then re-run with `--allow-build-changes` to build them anyway.",
        cmp.base()
    ))
}

// ---------------------------------------------------------------------------
// Deriving the surface at the merge-base
// ---------------------------------------------------------------------------

/// Lexically join `rel` onto `base` (both repository-relative) and resolve `.`
/// and `..`. `None` when the result climbs out of the repository or `rel` is
/// absolute — neither names a file the branch can change.
pub fn normalize(base: &str, rel: &str) -> Option<String> {
    if rel.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = base.split('/').filter(|c| !c.is_empty()).collect();
    for c in rel.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Every `mod <ident>` in `text`, with whether it is followed by `{` (an
/// inline module) rather than `;`. Comment- and string-blind on purpose: a
/// false hit can only add a file that exists, never drop one.
pub fn mod_decls(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(off) = text[i..].find("mod") {
        let at = i + off;
        i = at + 3;
        let before_ok = at == 0 || !is_ident_char(text[..at].chars().next_back().unwrap_or(' '));
        let rest = &text[at + 3..];
        let trimmed = rest.trim_start();
        if !before_ok || trimmed.len() == rest.len() {
            continue;
        }
        let ident: String = trimmed.chars().take_while(|c| is_ident_char(*c)).collect();
        if ident.is_empty() || ident.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            continue;
        }
        let after = trimmed[ident.len()..].trim_start();
        match after.as_bytes().first() {
            Some(b';') => out.push((ident, false)),
            Some(b'{') => out.push((ident, true)),
            _ => {}
        }
    }
    out
}

/// The literal-path kinds a Rust file can pull another file in by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LitKind {
    /// `#[path = "…"]` — a module file.
    PathAttr,
    /// `include!("…")` — Rust source, spliced in.
    Include,
    /// `include_str!` / `include_bytes!` — data.
    Data,
}

/// The string literal starting at `s` (which must begin with `"`), without
/// its quotes. Escapes are not interpreted beyond skipping `\"`.
fn string_literal(s: &str) -> Option<String> {
    let body = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => {
                out.push(chars.next()?);
            }
            c => out.push(c),
        }
    }
    None
}

/// Every literal path `text` names through `#[path]`, `include!`,
/// `include_str!` or `include_bytes!`.
pub fn literal_paths(text: &str) -> Vec<(String, LitKind)> {
    let mut out = Vec::new();
    for (needle, kind, opener) in [
        ("include_bytes!", LitKind::Data, '('),
        ("include_str!", LitKind::Data, '('),
        ("include!", LitKind::Include, '('),
    ] {
        let mut i = 0;
        while let Some(off) = text[i..].find(needle) {
            let at = i + off;
            i = at + needle.len();
            let before_ok =
                at == 0 || !is_ident_char(text[..at].chars().next_back().unwrap_or(' '));
            let rest = text[i..].trim_start();
            if !before_ok || !rest.starts_with(opener) {
                continue;
            }
            if let Some(lit) = string_literal(rest[1..].trim_start()) {
                out.push((lit, kind));
            }
        }
    }
    // `#[path = "…"]`, allowing whitespace between every token.
    let mut i = 0;
    while let Some(off) = text[i..].find('#') {
        let at = i + off;
        i = at + 1;
        let rest = text[i..].trim_start();
        let Some(rest) = rest.strip_prefix('[') else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(rest) = rest.strip_prefix("path") else {
            continue;
        };
        if rest.chars().next().is_some_and(is_ident_char) {
            continue;
        }
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        if let Some(lit) = string_literal(rest.trim_start()) {
            out.push((lit, LitKind::PathAttr));
        }
    }
    out
}

/// A read-only view of one tree, repository-relative.
pub trait Tree {
    /// The file's text, or `None` when it does not exist (or is not a file).
    fn read(&self, path: &str) -> Option<String>;
    fn is_file(&self, path: &str) -> bool;
    fn is_dir(&self, path: &str) -> bool;
    /// The names of the files directly in `dir`.
    fn files_in(&self, dir: &str) -> Vec<String>;
}

/// The files and directories a crate rooted at `root` can pull in by a literal
/// path, starting with `root` itself. Rust's own resolution rules, applied
/// over-generously where the rule depends on context the scan does not track
/// (inline module blocks): an inline `mod x { … }` counts `x/` whole.
pub fn module_closure(root: &str, tree: &dyn Tree) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut files = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    // (file, whether it resolves submodules like a crate root or `mod.rs`)
    let mut queue = vec![(root.to_string(), true)];
    while let Some((file, mod_rs_like)) = queue.pop() {
        if !files.insert(file.clone()) {
            continue;
        }
        let Some(text) = tree.read(&file) else {
            continue;
        };
        let dir = parent(&file).to_string();
        let mod_dir = if mod_rs_like {
            dir.clone()
        } else {
            let stem = file
                .rsplit('/')
                .next()
                .unwrap_or(&file)
                .trim_end_matches(".rs");
            join(&dir, stem)
        };
        for (name, inline) in mod_decls(&text) {
            let sub = join(&mod_dir, &name);
            if inline {
                if tree.is_dir(&sub) {
                    dirs.insert(sub);
                }
                continue;
            }
            let flat = format!("{sub}.rs");
            if tree.is_file(&flat) {
                queue.push((flat, false));
            }
            let nested = join(&sub, "mod.rs");
            if tree.is_file(&nested) {
                queue.push((nested, true));
            }
        }
        for (lit, kind) in literal_paths(&text) {
            let mut bases = vec![dir.clone()];
            if kind == LitKind::PathAttr && mod_dir != dir {
                bases.push(mod_dir.clone());
            }
            for base in bases {
                let Some(p) = normalize(&base, &lit) else {
                    continue;
                };
                if tree.is_dir(&p) {
                    dirs.insert(p);
                } else if tree.is_file(&p) {
                    match kind {
                        LitKind::Data => {
                            files.insert(p);
                        }
                        LitKind::PathAttr | LitKind::Include => queue.push((p, true)),
                    }
                }
            }
        }
    }
    (files, dirs)
}

/// Whether `name` appears in `text` as a whole token — bounded by something
/// that cannot be part of a file name, or by the text's ends.
fn names_token(text: &str, name: &str) -> bool {
    let in_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let mut i = 0;
    while let Some(off) = text[i..].find(name) {
        let at = i + off;
        i = at + name.len();
        let before = text[..at].chars().next_back();
        let after = text[at + name.len()..].chars().next();
        if !before.is_some_and(in_name) && !after.is_some_and(in_name) {
            return true;
        }
    }
    false
}

/// `start` and every sibling file its text names, recursively. What a script
/// the cargo configuration names can exec by a literal sibling name.
pub fn script_closure(start: &str, tree: &dyn Tree) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut queue = vec![start.to_string()];
    while let Some(file) = queue.pop() {
        if !out.insert(file.clone()) {
            continue;
        }
        let Some(text) = tree.read(&file) else {
            continue;
        };
        let dir = parent(&file);
        let own = file.rsplit('/').next().unwrap_or(&file);
        for name in tree.files_in(dir) {
            if name != own && names_token(&text, &name) {
                queue.push(join(dir, &name));
            }
        }
    }
    out
}

/// Every repository path a string anywhere in `value` names, resolved against
/// `base` the way cargo resolves a config-relative path (relative to the parent
/// of the `.cargo` directory, and for a manifest to its own directory). Each
/// string is split on whitespace, `=` and `,` so a `-Clinker=./x` in a flags
/// list is found too. A bare name (no `/`) is a `PATH` lookup, not a repository
/// path, and is skipped. Returns `(path, is_dir)`.
pub fn named_paths(value: &toml::Value, base: &str, tree: &dyn Tree) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut stack = vec![value];
    while let Some(v) = stack.pop() {
        match v {
            toml::Value::String(s) => {
                for tok in s.split(|c: char| c.is_whitespace() || c == '=' || c == ',') {
                    if !tok.contains('/') {
                        continue;
                    }
                    let Some(p) = normalize(base, tok) else {
                        continue;
                    };
                    if p.is_empty() {
                        continue;
                    }
                    if tree.is_dir(&p) {
                        out.push((p, true));
                    } else if tree.is_file(&p) {
                        out.push((p, false));
                    }
                }
            }
            toml::Value::Array(a) => stack.extend(a.iter()),
            toml::Value::Table(t) => stack.extend(t.values()),
            _ => {}
        }
    }
    out
}

/// What `cargo metadata --no-deps` at the merge-base says executes at build
/// time, before the module scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MetaFacts {
    /// `(build script path, owning package)`.
    pub build_scripts: Vec<(String, String)>,
    /// Package directories that execute on the host, with why.
    pub host_dirs: BTreeMap<String, String>,
}

/// Walk `cargo metadata --no-deps` output from the package owning the `bin`
/// target: over normal and build edges, never dev ones (`cargo build` builds
/// no dev-dependency), through local path dependencies. Optional and
/// platform-specific dependencies are followed too, because a feature or a
/// target could enable them.
pub fn walk_metadata(meta: &serde_json::Value, bin: &str) -> Result<MetaFacts, String> {
    let root = meta
        .get("workspace_root")
        .and_then(|v| v.as_str())
        .ok_or("`cargo metadata` output has no `workspace_root`")?;
    let rel = |p: &str| -> Option<String> {
        if p == root {
            return Some(String::new());
        }
        p.strip_prefix(root)
            .and_then(|r| r.strip_prefix('/'))
            .map(str::to_string)
    };
    let packages = meta
        .get("packages")
        .and_then(|v| v.as_array())
        .ok_or("`cargo metadata` output has no `packages`")?;
    let mut by_dir: BTreeMap<String, &serde_json::Value> = BTreeMap::new();
    for p in packages {
        let manifest = p
            .get("manifest_path")
            .and_then(|v| v.as_str())
            .ok_or("a package has no `manifest_path`")?;
        if let Some(dir) = rel(parent(manifest)) {
            by_dir.insert(dir, p);
        }
    }
    let targets = |p: &serde_json::Value| -> Vec<(Vec<String>, String, String)> {
        p.get("targets")
            .and_then(|v| v.as_array())
            .map(|ts| {
                ts.iter()
                    .map(|t| {
                        let kinds = t
                            .get("kind")
                            .and_then(|k| k.as_array())
                            .map(|a| {
                                a.iter()
                                    .filter_map(|k| k.as_str().map(str::to_string))
                                    .collect()
                            })
                            .unwrap_or_default();
                        let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let src = t.get("src_path").and_then(|v| v.as_str()).unwrap_or("");
                        (kinds, name.to_string(), src.to_string())
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let name_of = |p: &serde_json::Value| {
        p.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    };
    let start = by_dir
        .iter()
        .find(|(_, p)| {
            targets(p)
                .iter()
                .any(|(k, n, _)| n == bin && k.iter().any(|k| k == "bin"))
        })
        .map(|(d, _)| d.clone())
        .ok_or_else(|| {
            format!("no package in the merge-base's workspace has a `bin` target named `{bin}`")
        })?;

    let mut facts = MetaFacts::default();
    let mut seen: BTreeSet<(String, bool)> = BTreeSet::new();
    let mut queue = vec![(start, false, String::new())];
    while let Some((dir, host, why)) = queue.pop() {
        if !seen.insert((dir.clone(), host)) {
            continue;
        }
        let Some(pkg) = by_dir.get(&dir) else {
            // A local path dependency `--no-deps` does not list (excluded from
            // the workspace): its targets are unknown, so all of it counts.
            facts.host_dirs.entry(dir.clone()).or_insert(format!(
                "a local path dependency the merge-base's workspace does not list{why}"
            ));
            continue;
        };
        let name = name_of(pkg);
        let ts = targets(pkg);
        let proc_macro = ts
            .iter()
            .any(|(k, _, _)| k.iter().any(|k| k == "proc-macro"));
        let runs_on_host = host || proc_macro;
        if runs_on_host {
            let reason = if proc_macro {
                format!("the sources of `{name}`, a proc-macro crate the build reaches")
            } else {
                format!("the sources of `{name}`, a build-time dependency{why}")
            };
            facts.host_dirs.entry(dir.clone()).or_insert(reason);
        }
        for (kinds, _, src) in &ts {
            if kinds.iter().any(|k| k == "custom-build")
                && let Some(src) = rel(src)
                && !facts.build_scripts.iter().any(|(s, _)| *s == src)
            {
                facts.build_scripts.push((src, name.clone()));
            }
        }
        for dep in pkg
            .get("dependencies")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let kind = dep.get("kind").and_then(|v| v.as_str());
            if kind == Some("dev") {
                continue;
            }
            let Some(path) = dep.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(child) = rel(path) else {
                continue;
            };
            let child_host = runs_on_host || kind == Some("build");
            let child_why = if child_host && !runs_on_host {
                format!(" of `{name}`'s build script")
            } else {
                why.clone()
            };
            queue.push((child, child_host, child_why));
        }
    }
    Ok(facts)
}

/// Everything derived at one merge-base, from its extracted tree and its
/// `cargo metadata --no-deps` output.
pub fn derive_surface(meta: &serde_json::Value, tree: &dyn Tree) -> Result<Surface, String> {
    let facts = walk_metadata(meta, "dot-agent-deck")?;
    let mut s = Surface::default();
    for (dir, why) in facts.host_dirs {
        s.dirs.insert(dir, why);
    }
    for (script, pkg) in &facts.build_scripts {
        let (files, dirs) = module_closure(script, tree);
        for f in files {
            let why = if f == *script {
                format!("the build script of `{pkg}`")
            } else {
                format!("a file `{script}` (the build script of `{pkg}`) pulls in")
            };
            s.files.entry(f).or_insert(why);
        }
        for d in dirs {
            s.dirs
                .entry(d)
                .or_insert(format!("a directory `{script}` pulls modules from"));
        }
    }
    for config in [".cargo/config.toml", ".cargo/config"] {
        let Some(text) = tree.read(config) else {
            continue;
        };
        let value = toml::Value::Table(
            toml::from_str::<toml::Table>(&text)
                .map_err(|e| format!("parse `{config}` at the merge-base: {e}"))?,
        );
        for (p, is_dir) in named_paths(&value, "", tree) {
            if is_dir {
                s.dirs
                    .entry(p)
                    .or_insert(format!("a directory `{config}` names"));
                continue;
            }
            for f in script_closure(&p, tree) {
                let why = if f == p {
                    format!("named by `{config}` (a linker or wrapper cargo executes)")
                } else {
                    format!("a sibling script `{p}` (named by `{config}`) names")
                };
                s.files.entry(f).or_insert(why);
            }
        }
    }
    if let Some(text) = tree.read("Cargo.toml") {
        let manifest: toml::Table = toml::from_str(&text)
            .map_err(|e| format!("parse `Cargo.toml` at the merge-base: {e}"))?;
        for key in ["patch", "replace"] {
            let Some(v) = manifest.get(key) else {
                continue;
            };
            for (p, is_dir) in named_paths(v, "", tree) {
                let why = format!("a `[{key}]` path in the merge-base's `Cargo.toml`");
                if is_dir {
                    s.dirs.entry(p).or_insert(why);
                } else {
                    s.files.entry(p).or_insert(why);
                }
            }
        }
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// The impure half: git and cargo
// ---------------------------------------------------------------------------

/// An extracted tree on disk.
struct DiskTree<'a>(&'a Path);

impl Tree for DiskTree<'_> {
    fn read(&self, path: &str) -> Option<String> {
        let p = self.0.join(path);
        let md = std::fs::symlink_metadata(&p).ok()?;
        if !md.is_file() {
            return None;
        }
        std::fs::read(p)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
    fn is_file(&self, path: &str) -> bool {
        std::fs::symlink_metadata(self.0.join(path)).is_ok_and(|m| m.is_file() || m.is_symlink())
    }
    fn is_dir(&self, path: &str) -> bool {
        std::fs::symlink_metadata(self.0.join(path)).is_ok_and(|m| m.is_dir())
    }
    fn files_in(&self, dir: &str) -> Vec<String> {
        std::fs::read_dir(self.0.join(dir))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// A scratch directory removed when dropped.
///
/// The path is the one [`crate::sandbox::create_private_dir`] created and
/// canonicalized, so `remove_dir_all` is handed the leaf this module made, and
/// it does not follow a symlink at that leaf or under it: std documents that it
/// removes a symlink rather than descending through it, and that on Linux it is
/// protected against one swapped in mid-walk (its Unix implementation opens
/// each directory with `openat` and `O_NOFOLLOW`). A symlink at the leaf and
/// one under it are pinned by
/// `dropping_the_scratch_never_follows_a_symlink_out_of_it`. The leaf being
/// `0700` keeps anyone but its owner from creating entries in it; replacing the
/// leaf itself needs write access to the runs root.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_ok(cmd: &mut Command, what: &str) -> Result<Vec<u8>, String> {
    let out = cmd.output().map_err(|e| format!("{what}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn nul_list(bytes: &[u8]) -> BTreeSet<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// The git and cargo steps the gate is made of. [`evaluate_with`] is the
/// decision over their results — which failure refuses, and what the surface
/// is when none does — so it is tested with a fake in place of git and cargo.
pub trait Steps {
    /// `git merge-base --all <base> <head>`'s output.
    fn merge_bases(&self, base: &str, head: &str) -> Result<Vec<u8>, String>;
    /// `git diff --name-only --no-renames -z <merge_base> <head>`'s output.
    fn diff(&self, merge_base: &str, head: &str) -> Result<Vec<u8>, String>;
    /// Extract `merge_base`'s tree into `dir`, a fresh empty directory.
    fn extract(&self, merge_base: &str, dir: &Path) -> Result<(), String>;
    /// `git ls-tree -r -z <merge_base>`'s output.
    fn ls_tree(&self, merge_base: &str) -> Result<Vec<u8>, String>;
    /// `cargo metadata --no-deps` run in `dir`: its output.
    fn metadata(&self, dir: &Path) -> Result<Vec<u8>, String>;
}

/// [`Steps`] for real: `git` in the build clone, `cargo` in the extraction.
struct Host<'a> {
    clone: &'a Path,
    git: &'a dyn Fn(&Path) -> Command,
    cargo: &'a dyn Fn(&Path) -> Command,
}

impl Steps for Host<'_> {
    fn merge_bases(&self, base: &str, head: &str) -> Result<Vec<u8>, String> {
        run_ok(
            (self.git)(self.clone).args(["merge-base", "--all", base, head]),
            "git merge-base",
        )
    }

    fn diff(&self, merge_base: &str, head: &str) -> Result<Vec<u8>, String> {
        run_ok(
            (self.git)(self.clone).args([
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                merge_base,
                head,
            ]),
            "git diff --name-only",
        )
    }

    fn extract(&self, merge_base: &str, dir: &Path) -> Result<(), String> {
        let mut archive = (self.git)(self.clone)
            .args(["archive", "--format=tar", merge_base])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("git archive {merge_base}: {e}"))?;
        let tar_in = archive.stdout.take().ok_or("git archive: no stdout")?;
        let tar = Command::new("tar")
            .arg("-x")
            .arg("-C")
            .arg(dir)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(tar_in)
            .output()
            .map_err(|e| format!("tar -x: {e}"))?;
        let archived = archive
            .wait()
            .map_err(|e| format!("git archive {merge_base}: {e}"))?;
        if !archived.success() || !tar.status.success() {
            return Err(format!(
                "extracting the merge-base {merge_base} failed (git archive {archived}, tar {}): {}",
                tar.status,
                String::from_utf8_lossy(&tar.stderr).trim()
            ));
        }
        Ok(())
    }

    fn ls_tree(&self, merge_base: &str) -> Result<Vec<u8>, String> {
        run_ok(
            (self.git)(self.clone).args(["ls-tree", "-r", "-z", merge_base]),
            "git ls-tree",
        )
    }

    fn metadata(&self, dir: &Path) -> Result<Vec<u8>, String> {
        run_ok(
            (self.cargo)(dir).args([
                "metadata",
                "--no-deps",
                "--offline",
                "--locked",
                "--format-version",
                "1",
            ]),
            "cargo metadata --no-deps at the merge-base",
        )
    }
}

/// The blob paths a `git ls-tree -r -z` listing names: every file and symlink
/// in the tree. A submodule's `commit` entry holds no file and is skipped. A
/// listing that cannot be read exactly — an entry that is not UTF-8, or has no
/// path — is refused rather than converted lossily or dropped, since either
/// would take a tracked file out of the comparison.
pub fn tracked_blobs(listing: &[u8]) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    for entry in listing.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let entry = std::str::from_utf8(entry).map_err(|_| {
            format!(
                "a `git ls-tree` entry is not UTF-8: {:?}",
                String::from_utf8_lossy(entry)
            )
        })?;
        let Some((meta, path)) = entry.split_once('\t') else {
            return Err(format!("a `git ls-tree` entry names no path: {entry:?}"));
        };
        if meta.split_whitespace().nth(1) == Some("blob") {
            out.insert(path.to_string());
        }
    }
    Ok(out)
}

/// Every non-directory entry under `root` — files and symlinks, which are
/// listed and never followed — as a `/`-separated path relative to it. A name
/// that is not UTF-8 is refused, as in [`tracked_blobs`].
fn extracted_files(root: &Path) -> Result<BTreeSet<String>, String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|n| format!("a name in the extraction is not UTF-8: {n:?}"))?;
            let path = join(&rel, &name);
            let ty = entry
                .file_type()
                .map_err(|e| format!("lstat {path}: {e}"))?;
            if ty.is_dir() {
                stack.push(path);
            } else {
                out.insert(path);
            }
        }
    }
    Ok(out)
}

/// Whether an extraction of `merge_base` is exactly its tree, by path: every
/// blob `git ls-tree -r` lists is present, and nothing else is.
///
/// A **missing** file is what `git archive` does with an `export-ignore`
/// attribute, and a surface derived from a tree that lost, say, its
/// `.cargo/config.toml` has lost the linker that file names. An **extra** file
/// is refused too: `git archive` of the merge-base writes the listed blobs and
/// no other file, so an extra one came from something else, and the tree the
/// derivation and `cargo metadata` would read is not the merge-base's — a
/// `rust-toolchain.toml` or a `.cargo/config.toml` there changes what that
/// `cargo` runs. A `tar` that wrote git's pax header out as a file would be
/// refused here as well, which fails closed.
///
/// Directories are not compared: git records none of its own, and `git
/// archive` writes one for each that holds a file (and for a submodule), so an
/// extra one is empty. The derivation reads files, and counts an existing
/// directory as a whole subtree, which adds to the surface.
pub fn check_extraction(
    merge_base: &str,
    tracked: &BTreeSet<String>,
    extracted: &BTreeSet<String>,
) -> Result<(), String> {
    let missing: Vec<&String> = tracked.difference(extracted).collect();
    let extra: Vec<&String> = extracted.difference(tracked).collect();
    let mut problems = Vec::new();
    if !missing.is_empty() {
        problems.push(format!(
            "is missing {} tracked file(s) (an `export-ignore` attribute?), e.g. {:?}",
            missing.len(),
            &missing[..missing.len().min(5)]
        ));
    }
    if !extra.is_empty() {
        problems.push(format!(
            "holds {} file(s) its tree does not, e.g. {:?}",
            extra.len(),
            &extra[..extra.len().min(5)]
        ));
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(format!(
        "the extracted merge-base {merge_base} {}",
        problems.join(", and ")
    ))
}

/// The surface at one merge-base: extract its tree into a fresh owner-only
/// directory under `scratch_parent`, prove the extraction is exactly the tree
/// `git ls-tree` lists, and only then run `cargo metadata --no-deps` there and
/// derive.
fn surface_at(
    steps: &dyn Steps,
    merge_base: &str,
    scratch_parent: &Path,
) -> Result<Surface, String> {
    let short: String = merge_base.chars().take(12).collect();
    let scratch = Scratch(crate::sandbox::create_private_dir(
        "the merge-base scratch parent",
        scratch_parent,
        &format!(
            ".xver-merge-base-{short}-{}-{}",
            crate::epoch_secs(),
            std::process::id()
        ),
    )?);
    steps.extract(merge_base, &scratch.0)?;
    // `git archive` honours `export-ignore`; a tree that dropped a file is not
    // the tree cargo would see, so the extraction must be exactly the tree —
    // checked before `cargo` runs in it.
    let tracked = tracked_blobs(&steps.ls_tree(merge_base)?)?;
    check_extraction(merge_base, &tracked, &extracted_files(&scratch.0)?)?;
    let meta = steps.metadata(&scratch.0)?;
    let meta: serde_json::Value = serde_json::from_slice(&meta)
        .map_err(|e| format!("parse `cargo metadata` at the merge-base: {e}"))?;
    derive_surface(&meta, &DiskTree(&scratch.0))
}

/// Compare `head` with its merge-base(s) with `base`, in the build clone.
/// Every failure is an `Err`, which the caller treats as a refusal: a gate
/// that could not be evaluated has not been passed.
pub fn evaluate(
    clone: &Path,
    repo: &str,
    base_sha: &str,
    head_sha: &str,
    scratch_parent: &Path,
    git: &dyn Fn(&Path) -> Command,
    cargo: &dyn Fn(&Path) -> Command,
) -> Result<(Comparison, Vec<(String, String)>), String> {
    evaluate_with(
        &Host { clone, git, cargo },
        repo,
        base_sha,
        head_sha,
        scratch_parent,
    )
}

/// [`evaluate`] over any [`Steps`]. Every step's failure, and a history with
/// no merge-base, is a refusal; several merge-bases are each compared, and the
/// changed paths and the surfaces are unioned across them.
pub fn evaluate_with(
    steps: &dyn Steps,
    repo: &str,
    base_sha: &str,
    head_sha: &str,
    scratch_parent: &Path,
) -> Result<(Comparison, Vec<(String, String)>), String> {
    let refuse = |e: String| {
        format!(
            "refusing before `cargo build` — nothing was compiled: the build-time comparison \
             with `origin/{BASE_BRANCH}` could not be made ({e})"
        )
    };
    let mbs = steps
        .merge_bases(base_sha, head_sha)
        .map_err(|e| refuse(format!("no merge-base: {e}")))?;
    let merge_bases: Vec<String> = String::from_utf8_lossy(&mbs)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if merge_bases.is_empty() {
        return Err(refuse(format!(
            "`{head_sha}` and `{base_sha}` share no history"
        )));
    }
    let mut changed = BTreeSet::new();
    let mut surface = Surface::default();
    for mb in &merge_bases {
        changed.extend(nul_list(&steps.diff(mb, head_sha).map_err(refuse)?));
        surface.merge(surface_at(steps, mb, scratch_parent).map_err(refuse)?);
    }
    let changes = build_time_changes(&changed, &surface);
    Ok((
        Comparison {
            repo: repo.to_string(),
            base_sha: base_sha.to_string(),
            merge_bases,
            surface: surface.describe(),
        },
        changes,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    /// An in-memory tree: path → contents; directories are implied.
    struct Mem(BTreeMap<String, String>);

    fn mem(files: &[(&str, &str)]) -> Mem {
        Mem(files
            .iter()
            .map(|(p, t)| (p.to_string(), t.to_string()))
            .collect())
    }

    impl Tree for Mem {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).cloned()
        }
        fn is_file(&self, path: &str) -> bool {
            self.0.contains_key(path)
        }
        fn is_dir(&self, path: &str) -> bool {
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{path}/")
            };
            self.0.keys().any(|k| k.starts_with(&prefix)) && !self.0.contains_key(path)
        }
        fn files_in(&self, dir: &str) -> Vec<String> {
            self.0
                .keys()
                .filter(|k| parent(k) == dir)
                .map(|k| k.rsplit('/').next().unwrap_or(k).to_string())
                .collect()
        }
    }

    fn surface(files: &[&str], dirs: &[&str]) -> Surface {
        Surface {
            files: files
                .iter()
                .map(|f| (f.to_string(), "derived".to_string()))
                .collect(),
            dirs: dirs
                .iter()
                .map(|d| (d.to_string(), "derived dir".to_string()))
                .collect(),
        }
    }

    #[test]
    fn the_name_rules_count_anywhere_in_the_tree() {
        let s = Surface::default();
        for p in [
            "build.rs",
            "desktop/src-tauri/build.rs",
            "Cargo.toml",
            "xtask/spec/Cargo.toml",
            "Cargo.lock",
            "rust-toolchain",
            "rust-toolchain.toml",
            "sub/rust-toolchain.toml",
            ".cargo/config.toml",
            ".cargo/config",
            "xtask/foo/.cargo/anything",
        ] {
            assert!(classify(p, &s).is_some(), "{p} is build-time code");
        }
    }

    #[test]
    fn ordinary_runtime_source_docs_and_lookalikes_do_not_count() {
        let s = surface(&["build_version_resolve.rs"], &["xtask/spec"]);
        for p in [
            "src/main.rs",
            "src/daemon.rs",
            "docs/develop/cross-version-harness.md",
            "tests/e2e_handshake.rs",
            "rebuild.rs",
            "build.rs.orig",
            "Cargo.toml.md",
            "docs/Cargo.lock.md",
            "cargo/config.toml",
            "x.cargo/config",
            "xtask/spec-other/src/lib.rs",
            "xtask/specs.md",
        ] {
            assert_eq!(classify(p, &s), None, "{p} is not build-time code");
        }
    }

    #[test]
    fn derived_files_and_directory_subtrees_count() {
        let s = surface(
            &["build_version_resolve.rs", "scripts/link-gate.sh"],
            &["xtask/spec"],
        );
        assert!(classify("build_version_resolve.rs", &s).is_some());
        assert!(classify("scripts/link-gate.sh", &s).is_some());
        assert!(classify("xtask/spec/src/lib.rs", &s).is_some());
        assert!(classify("xtask/spec/new.rs", &s).is_some());
        assert_eq!(classify("scripts/notify.sh", &s), None);
        let whole = surface(&[], &[""]);
        assert!(
            classify("anything/at/all.md", &whole).is_some(),
            "the root dir counts everything"
        );
    }

    fn cmp() -> Comparison {
        Comparison {
            repo: "vfarcic/dot-agent-deck".into(),
            base_sha: "b".repeat(40),
            merge_bases: vec!["m".repeat(40)],
            surface: "the rules".into(),
        }
    }

    #[test]
    fn no_change_proceeds_and_says_identical_to_the_merge_base() {
        let note = decide("agent/x", &[], &cmp(), false, false).expect("proceeds");
        assert!(
            note.starts_with("build-time code identical to the merge-base with `origin/main`"),
            "{note}"
        );
        assert!(note.contains(&"m".repeat(40)), "{note}");
    }

    #[test]
    fn a_change_without_the_opt_in_is_refused_naming_the_files() {
        let changes = vec![("Cargo.lock".to_string(), "the lockfile".to_string())];
        let err = decide("renovate/foo", &changes, &cmp(), false, false).expect_err("refused");
        assert!(
            err.starts_with("refusing before `cargo build` — nothing was compiled"),
            "{err}"
        );
        assert!(err.contains("`Cargo.lock`"), "{err}");
        assert!(err.contains("--allow-build-changes"), "{err}");
        assert!(err.contains("Review those changes"), "{err}");
    }

    #[test]
    fn the_opt_in_proceeds_and_records_what_ran() {
        let changes = vec![("build.rs".to_string(), "a build script name".to_string())];
        let note = decide("b", &changes, &cmp(), true, false).expect("opted in");
        assert!(note.contains("CHANGES build-time code"), "{note}");
        assert!(note.contains("`build.rs`"), "{note}");
        assert!(
            note.contains("opted in with `--allow-build-changes`"),
            "{note}"
        );
    }

    #[test]
    fn skip_build_proceeds_because_nothing_is_executed_and_says_so() {
        let changes = vec![("build.rs".to_string(), "a build script name".to_string())];
        let note = decide("b", &changes, &cmp(), false, true).expect("nothing is built");
        assert!(note.contains("none of it executed"), "{note}");
    }

    #[test]
    fn normalize_resolves_dots_and_refuses_to_leave_the_repository() {
        assert_eq!(
            normalize("", "./scripts/link-gate.sh").as_deref(),
            Some("scripts/link-gate.sh")
        );
        assert_eq!(normalize("a/b", "../c.rs").as_deref(), Some("a/c.rs"));
        assert_eq!(normalize("", "../outside.rs"), None);
        assert_eq!(normalize("", "/usr/bin/cc"), None);
    }

    #[test]
    fn mod_decls_finds_file_and_inline_modules_and_nothing_else() {
        let got = mod_decls(
            "mod a;\npub mod b ;\npub(crate) mod c { mod d; }\nmodule x;\nlet model = 1;\nmod 9x;\n// mod e;\n",
        );
        assert_eq!(
            got,
            vec![
                ("a".to_string(), false),
                ("b".to_string(), false),
                ("c".to_string(), true),
                ("d".to_string(), false),
                ("e".to_string(), false),
            ]
        );
    }

    #[test]
    fn literal_paths_finds_every_kind_with_whitespace() {
        let got = literal_paths(
            "#[path = \"x/y.rs\"] mod y;\n# [ path=\"z.rs\" ]\ninclude!(\"gen.rs\");\n\
             include_str! ( \"data.txt\" );\ninclude_bytes!(\"b.bin\");\n\
             include!(concat!(env!(\"OUT_DIR\"), \"/o.rs\"));\n`include!` in prose\n#[pathology = \"no\"]\n",
        );
        let mut got: Vec<(String, LitKind)> = got;
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("b.bin".to_string(), LitKind::Data),
                ("data.txt".to_string(), LitKind::Data),
                ("gen.rs".to_string(), LitKind::Include),
                ("x/y.rs".to_string(), LitKind::PathAttr),
                ("z.rs".to_string(), LitKind::PathAttr),
            ]
        );
    }

    #[test]
    fn the_roots_build_script_closure_is_itself_and_the_file_it_mods() {
        let tree = mem(&[
            ("build.rs", "mod build_version_resolve;\nfn main() {}\n"),
            (
                "build_version_resolve.rs",
                "// `#[path = \"../build_version_resolve.rs\"] mod build_version_resolve;`\n",
            ),
            ("src/main.rs", "fn main() {}"),
        ]);
        let (files, dirs) = module_closure("build.rs", &tree);
        assert_eq!(
            files.into_iter().collect::<Vec<_>>(),
            vec![
                "build.rs".to_string(),
                "build_version_resolve.rs".to_string()
            ]
        );
        assert!(dirs.is_empty(), "{dirs:?}");
    }

    #[test]
    fn a_module_closure_follows_nested_path_include_and_inline_modules() {
        let tree = mem(&[
            (
                "tools/build.rs",
                "mod a;\nmod inl { mod x; }\ninclude!(\"gen/g.rs\");\n",
            ),
            (
                "tools/a.rs",
                "mod b;\n#[path = \"../shared/s.rs\"] mod s;\nconst D: &str = include_str!(\"d.txt\");\n",
            ),
            ("tools/a/b.rs", ""),
            ("tools/a/d.txt", "not scanned"),
            ("tools/d.txt", "data"),
            ("tools/inl/x.rs", ""),
            ("tools/gen/g.rs", "mod h;\n"),
            ("tools/gen/h.rs", ""),
            ("shared/s.rs", ""),
            ("src/lib.rs", ""),
        ]);
        let (files, dirs) = module_closure("tools/build.rs", &tree);
        for f in [
            "tools/build.rs",
            "tools/a.rs",
            "tools/a/b.rs",
            "tools/d.txt",
            "tools/gen/g.rs",
            "tools/gen/h.rs",
            "shared/s.rs",
        ] {
            assert!(files.contains(f), "{f} is in the closure: {files:?}");
        }
        assert!(!files.contains("src/lib.rs"));
        assert!(dirs.contains("tools/inl"), "{dirs:?}");
    }

    #[test]
    fn the_config_named_linker_brings_in_the_sibling_script_it_execs() {
        let tree = mem(&[
            (
                ".cargo/config.toml",
                "[target.x86_64-unknown-linux-gnu]\nlinker = \"./scripts/link-gate.sh\"\n[alias]\nxtask = \"run --quiet --package xtask-linkage-check --\"\n",
            ),
            (
                "scripts/link-gate.sh",
                "gate=\"$here/build-gate.sh\"\nexec \"$gate\"\n",
            ),
            ("scripts/build-gate.sh", "# the link-gate.sh caller\n"),
            ("scripts/notify.sh", "unrelated"),
            ("scripts/build-gate.sh.bak", "not named as a token"),
        ]);
        let meta = serde_json::json!({
            "workspace_root": "/w",
            "packages": [{
                "name": "dot-agent-deck",
                "manifest_path": "/w/Cargo.toml",
                "targets": [{"kind": ["bin"], "name": "dot-agent-deck", "src_path": "/w/src/main.rs"}],
                "dependencies": []
            }]
        });
        let s = derive_surface(&meta, &tree).expect("derives");
        assert!(s.files.contains_key("scripts/link-gate.sh"), "{s:?}");
        assert!(s.files.contains_key("scripts/build-gate.sh"), "{s:?}");
        assert!(!s.files.contains_key("scripts/notify.sh"), "{s:?}");
        assert!(!s.files.contains_key("scripts/build-gate.sh.bak"), "{s:?}");
    }

    fn pkg(name: &str, dir: &str, kinds: &[&str], deps: serde_json::Value) -> serde_json::Value {
        let root = if dir.is_empty() {
            "/w".to_string()
        } else {
            format!("/w/{dir}")
        };
        let mut targets: Vec<serde_json::Value> = kinds
            .iter()
            .map(|k| {
                serde_json::json!({"kind": [k], "name": if *k == "bin" { "dot-agent-deck" } else { name }, "src_path": format!("{root}/src/lib.rs")})
            })
            .collect();
        if kinds.contains(&"custom-build") {
            targets.retain(|t| t["kind"][0] != "custom-build");
            targets.push(serde_json::json!({"kind": ["custom-build"], "name": "build-script-build", "src_path": format!("{root}/build.rs")}));
        }
        serde_json::json!({
            "name": name,
            "manifest_path": format!("{root}/Cargo.toml"),
            "targets": targets,
            "dependencies": deps,
        })
    }

    #[test]
    fn the_metadata_walk_skips_dev_edges_and_counts_host_executed_packages_whole() {
        let meta = serde_json::json!({
            "workspace_root": "/w",
            "packages": [
                pkg("dot-agent-deck", "", &["lib", "bin", "custom-build"], serde_json::json!([
                    {"name": "spec", "kind": "dev", "path": "/w/xtask/spec"},
                    {"name": "semver", "kind": "build"},
                    {"name": "helper", "kind": null, "path": "/w/crates/helper"},
                    {"name": "gen", "kind": "build", "path": "/w/crates/gen"},
                    {"name": "mac", "kind": null, "path": "/w/crates/mac"},
                ])),
                pkg("spec", "xtask/spec", &["proc-macro"], serde_json::json!([])),
                pkg("helper", "crates/helper", &["lib", "custom-build"], serde_json::json!([])),
                pkg("gen", "crates/gen", &["lib"], serde_json::json!([
                    {"name": "genutil", "kind": null, "path": "/w/crates/genutil"},
                ])),
                pkg("genutil", "crates/genutil", &["lib"], serde_json::json!([])),
                pkg("mac", "crates/mac", &["proc-macro"], serde_json::json!([
                    {"name": "macutil", "kind": null, "path": "/w/crates/macutil"},
                ])),
                pkg("macutil", "crates/macutil", &["lib"], serde_json::json!([])),
                pkg("desktop", "desktop/src-tauri", &["lib", "custom-build"], serde_json::json!([
                    {"name": "dot-agent-deck", "kind": null, "path": "/w"},
                ])),
            ]
        });
        let f = walk_metadata(&meta, "dot-agent-deck").expect("walks");
        let scripts: Vec<&str> = f.build_scripts.iter().map(|(s, _)| s.as_str()).collect();
        assert!(scripts.contains(&"build.rs"), "{scripts:?}");
        assert!(scripts.contains(&"crates/helper/build.rs"), "{scripts:?}");
        assert!(
            !scripts.contains(&"desktop/src-tauri/build.rs"),
            "desktop is not in this build's graph: {scripts:?}"
        );
        let dirs: Vec<&str> = f.host_dirs.keys().map(String::as_str).collect();
        assert_eq!(
            dirs,
            vec![
                "crates/gen",
                "crates/genutil",
                "crates/mac",
                "crates/macutil"
            ],
            "build-deps, proc macros and what they depend on; not the dev-only proc macro, not a \
             plain lib dependency"
        );
    }

    #[test]
    fn the_metadata_walk_refuses_a_workspace_without_the_bin() {
        let meta = serde_json::json!({"workspace_root": "/w", "packages": [
            pkg("other", "", &["lib"], serde_json::json!([])),
        ]});
        assert!(walk_metadata(&meta, "dot-agent-deck").is_err());
    }

    #[test]
    fn an_unlisted_local_path_dependency_counts_whole() {
        let meta = serde_json::json!({"workspace_root": "/w", "packages": [
            pkg("dot-agent-deck", "", &["bin"], serde_json::json!([
                {"name": "vendored", "kind": null, "path": "/w/vendor/v"},
            ])),
        ]});
        let f = walk_metadata(&meta, "dot-agent-deck").expect("walks");
        assert!(f.host_dirs.contains_key("vendor/v"), "{f:?}");
    }

    #[test]
    fn a_patch_path_in_the_root_manifest_counts() {
        let tree = mem(&[
            (
                "Cargo.toml",
                "[package]\nname = \"x\"\n[patch.crates-io]\nfoo = { path = \"vendor/foo\" }\n",
            ),
            ("vendor/foo/src/lib.rs", ""),
        ]);
        let meta = serde_json::json!({"workspace_root": "/w", "packages": [
            pkg("dot-agent-deck", "", &["bin"], serde_json::json!([])),
        ]});
        let s = derive_surface(&meta, &tree).expect("derives");
        assert!(s.dirs.contains_key("vendor/foo"), "{s:?}");
    }

    #[test]
    fn build_time_changes_keeps_only_the_counted_paths() {
        let changed: BTreeSet<String> = [
            "src/main.rs",
            "Cargo.lock",
            "docs/x.md",
            "build_version_resolve.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let got = build_time_changes(&changed, &surface(&["build_version_resolve.rs"], &[]));
        let paths: Vec<&str> = got.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["Cargo.lock", "build_version_resolve.rs"]);
    }

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn the_tree_listing_is_its_blobs_and_refuses_what_it_cannot_read_exactly() {
        let listing = b"100644 blob aaaa\tsrc/main.rs\x00120000 blob bbbb\t.agents/skills\x00\
                        160000 commit cccc\tvendor/sub\x00100755 blob dddd\tscripts/a b.sh\x00";
        assert_eq!(
            tracked_blobs(listing).expect("reads"),
            set(&[".agents/skills", "scripts/a b.sh", "src/main.rs"]),
            "files and symlinks are blobs; a submodule is not"
        );
        let err = tracked_blobs(b"100644 blob aaaa\tsrc/\xffx.rs\0").expect_err("not UTF-8");
        assert!(err.contains("not UTF-8"), "{err}");
        let err = tracked_blobs(b"100644 blob aaaa src/main.rs\0").expect_err("no path");
        assert!(err.contains("names no path"), "{err}");
    }

    #[test]
    fn an_extraction_that_is_exactly_the_tree_passes() {
        let tree = set(&["Cargo.toml", ".cargo/config.toml", "scripts/link-gate.sh"]);
        check_extraction("m", &tree, &tree.clone()).expect("complete");
    }

    #[test]
    fn an_extraction_missing_a_tracked_file_is_refused_as_an_export_ignore() {
        let tree = set(&["Cargo.toml", ".cargo/config.toml", "scripts/link-gate.sh"]);
        let err = check_extraction("m", &tree, &set(&["Cargo.toml", "scripts/link-gate.sh"]))
            .expect_err("incomplete");
        assert!(err.contains("missing 1 tracked file"), "{err}");
        assert!(err.contains("export-ignore"), "{err}");
        assert!(err.contains(".cargo/config.toml"), "{err}");
    }

    /// Refused, not tolerated: `git archive` writes no file the tree lacks, so
    /// the extra one came from elsewhere and the tree is not the merge-base's.
    #[test]
    fn an_extraction_holding_a_file_the_tree_lacks_is_refused() {
        let tree = set(&["Cargo.toml"]);
        let err = check_extraction("m", &tree, &set(&["Cargo.toml", "rust-toolchain.toml"]))
            .expect_err("not the merge-base's tree");
        assert!(err.contains("holds 1 file(s) its tree does not"), "{err}");
        assert!(err.contains("rust-toolchain.toml"), "{err}");
        let err = check_extraction("m", &tree, &set(&["x"])).expect_err("both at once");
        assert!(err.contains("missing") && err.contains("holds"), "{err}");
    }

    const M1: &str = "1111111111111111111111111111111111111111";
    const M2: &str = "2222222222222222222222222222222222222222";

    const METADATA: &str = r#"{"workspace_root": "/w", "packages": [{
        "name": "dot-agent-deck", "manifest_path": "/w/Cargo.toml", "dependencies": [],
        "targets": [
            {"kind": ["bin"], "name": "dot-agent-deck", "src_path": "/w/src/main.rs"},
            {"kind": ["custom-build"], "name": "build-script-build", "src_path": "/w/build.rs"}
        ]}]}"#;

    /// [`Steps`] with no git and no cargo behind it. One tree serves every
    /// merge-base: `ls-tree` lists it, `extract` writes it — with a symlink to
    /// a directory in it, and an empty directory for a submodule, as `git
    /// archive` would.
    struct Fake {
        /// The step that fails: `merge-base`, `diff`, `extract`, `ls-tree` or
        /// `metadata`.
        fail: Option<&'static str>,
        /// What `git merge-base --all` prints.
        bases: &'static str,
        /// The paths each merge-base's diff with the head names.
        changed: Vec<(&'static str, Vec<&'static str>)>,
        tree: Vec<(&'static str, &'static str)>,
        /// Tracked files `extract` leaves out, as an `export-ignore` would.
        ignored: Vec<&'static str>,
        /// Files `extract` writes that the tree does not hold.
        planted: Vec<&'static str>,
        /// `cargo metadata`'s output, in place of [`METADATA`].
        metadata: Option<&'static str>,
        /// Each directory `extract` was handed, as it found it.
        extracted_into: RefCell<Vec<(PathBuf, std::fs::Metadata)>>,
        cargo_ran: Cell<bool>,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                fail: None,
                bases: "1111111111111111111111111111111111111111\n",
                changed: vec![],
                tree: vec![
                    ("Cargo.toml", "[package]\nname = \"dot-agent-deck\"\n"),
                    ("build.rs", "mod helper;\nfn main() {}\n"),
                    ("helper.rs", "pub fn version() {}\n"),
                    ("src/main.rs", "fn main() {}\n"),
                ],
                ignored: vec![],
                planted: vec![],
                metadata: None,
                extracted_into: RefCell::new(vec![]),
                cargo_ran: Cell::new(false),
            }
        }

        fn step(&self, name: &str) -> Result<(), String> {
            match self.fail {
                Some(f) if f == name => Err(format!("{name} broke")),
                _ => Ok(()),
            }
        }
    }

    fn write(root: &Path, path: &str, text: &str) {
        let p = root.join(path);
        std::fs::create_dir_all(p.parent().expect("a parent")).expect("mkdir");
        std::fs::write(p, text).expect("write");
    }

    impl Steps for Fake {
        fn merge_bases(&self, _: &str, _: &str) -> Result<Vec<u8>, String> {
            self.step("merge-base")?;
            Ok(self.bases.as_bytes().to_vec())
        }

        fn diff(&self, merge_base: &str, _: &str) -> Result<Vec<u8>, String> {
            self.step("diff")?;
            let names = self
                .changed
                .iter()
                .find(|(m, _)| *m == merge_base)
                .map(|(_, p)| p.join("\0"))
                .unwrap_or_default();
            Ok(names.into_bytes())
        }

        fn extract(&self, _: &str, dir: &Path) -> Result<(), String> {
            let md = std::fs::symlink_metadata(dir).expect("the extraction dir exists");
            self.extracted_into
                .borrow_mut()
                .push((dir.to_path_buf(), md));
            self.step("extract")?;
            for (path, text) in &self.tree {
                if !self.ignored.contains(path) {
                    write(dir, path, text);
                }
            }
            for path in &self.planted {
                write(dir, path, "");
            }
            std::os::unix::fs::symlink("src", dir.join("src-link")).expect("symlink");
            std::fs::create_dir_all(dir.join("vendor/sub")).expect("submodule dir");
            Ok(())
        }

        fn ls_tree(&self, _: &str) -> Result<Vec<u8>, String> {
            self.step("ls-tree")?;
            let mut out = String::new();
            for (path, _) in &self.tree {
                out.push_str(&format!("100644 blob {}\t{path}\0", "a".repeat(40)));
            }
            out.push_str(&format!("120000 blob {}\tsrc-link\0", "b".repeat(40)));
            out.push_str(&format!("160000 commit {}\tvendor/sub\0", "c".repeat(40)));
            Ok(out.into_bytes())
        }

        fn metadata(&self, _: &Path) -> Result<Vec<u8>, String> {
            self.cargo_ran.set(true);
            self.step("metadata")?;
            Ok(self.metadata.unwrap_or(METADATA).as_bytes().to_vec())
        }
    }

    /// A fresh, empty parent for the gate's scratch leaves.
    fn scratch_parent(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xver-gate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch parent");
        std::fs::canonicalize(dir).expect("canonical")
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read the scratch parent")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn gate(fake: &Fake, parent: &Path) -> Result<(Comparison, Vec<(String, String)>), String> {
        evaluate_with(fake, "vfarcic/dot-agent-deck", M2, M1, parent)
    }

    /// Every step's failure is a refusal naming it, and whatever scratch leaf
    /// the run had made goes with it.
    #[test]
    fn every_failing_step_refuses_before_cargo_build_and_leaves_no_scratch() {
        let parent = scratch_parent("fail");
        let refusal = "refusing before `cargo build` — nothing was compiled";
        for step in ["merge-base", "diff", "extract", "ls-tree", "metadata"] {
            let fake = Fake {
                fail: Some(step),
                ..Fake::new()
            };
            let err = gate(&fake, &parent).expect_err(step);
            assert!(err.starts_with(refusal), "{step}: {err}");
            assert!(err.contains(&format!("{step} broke")), "{step}: {err}");
            assert!(leftovers(&parent).is_empty(), "{step}");
        }
        for (why, fake, says) in [
            (
                "no merge-base",
                Fake {
                    bases: "\n",
                    ..Fake::new()
                },
                "share no history",
            ),
            (
                "unreadable metadata",
                Fake {
                    metadata: Some("not json"),
                    ..Fake::new()
                },
                "parse `cargo metadata` at the merge-base",
            ),
            (
                "metadata without the bin",
                Fake {
                    metadata: Some(r#"{"workspace_root": "/w", "packages": []}"#),
                    ..Fake::new()
                },
                "has a `bin` target named `dot-agent-deck`",
            ),
        ] {
            let err = gate(&fake, &parent).expect_err(why);
            assert!(err.starts_with(refusal), "{why}: {err}");
            assert!(err.contains(says), "{why}: {err}");
            assert!(leftovers(&parent).is_empty(), "{why}");
        }
        let _ = std::fs::remove_dir_all(parent);
    }

    /// The `export-ignore` case end to end. Without the guard the surface
    /// would lack `helper.rs`, which `build.rs` pulls in, and a branch changing
    /// it would pass as changing no build-time code.
    #[test]
    fn an_export_ignored_file_refuses_before_cargo_runs_in_the_extraction() {
        let parent = scratch_parent("ignored");
        let fake = Fake {
            ignored: vec!["helper.rs"],
            changed: vec![(M1, vec!["helper.rs"])],
            ..Fake::new()
        };
        let err = gate(&fake, &parent).expect_err("an incomplete extraction");
        assert!(err.starts_with("refusing before `cargo build`"), "{err}");
        assert!(err.contains("missing 1 tracked file"), "{err}");
        assert!(err.contains("helper.rs"), "{err}");
        assert!(
            !fake.cargo_ran.get(),
            "cargo ran in a tree that is not the merge-base's"
        );
        assert!(leftovers(&parent).is_empty());
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn a_file_the_tree_lacks_refuses_before_cargo_runs_in_the_extraction() {
        let parent = scratch_parent("planted");
        let fake = Fake {
            planted: vec!["rust-toolchain.toml"],
            ..Fake::new()
        };
        let err = gate(&fake, &parent).expect_err("an extraction with an extra file");
        assert!(err.contains("holds 1 file(s) its tree does not"), "{err}");
        assert!(err.contains("rust-toolchain.toml"), "{err}");
        assert!(
            !fake.cargo_ran.get(),
            "cargo ran in a tree that is not the merge-base's"
        );
        let _ = std::fs::remove_dir_all(parent);
    }

    /// Two merge-bases: each is extracted into its own owner-only leaf, which
    /// is gone afterwards, and the changed paths and surfaces are unioned. The
    /// tree's symlink to a directory passes as one blob — the walk lists it
    /// and does not follow it into `src/`.
    #[test]
    fn each_merge_base_is_derived_in_an_owner_only_leaf_removed_after() {
        use std::os::unix::fs::MetadataExt;
        let parent = scratch_parent("ok");
        let fake = Fake {
            bases: "1111111111111111111111111111111111111111\n\
                    2222222222222222222222222222222222222222\n",
            changed: vec![
                (M1, vec!["src/main.rs"]),
                (M2, vec!["helper.rs", "docs/x.md"]),
            ],
            ..Fake::new()
        };
        let (cmp, changes) = gate(&fake, &parent).expect("evaluates");
        assert_eq!(cmp.merge_bases, vec![M1.to_string(), M2.to_string()]);
        assert!(cmp.surface.contains("`helper.rs`"), "{}", cmp.surface);
        let paths: Vec<&str> = changes.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec!["helper.rs"], "{changes:?}");
        let dirs = fake.extracted_into.borrow();
        assert_eq!(dirs.len(), 2, "one extraction per merge-base");
        for (dir, md) in dirs.iter() {
            assert_eq!(dir.parent(), Some(parent.as_path()), "{}", dir.display());
            assert!(md.file_type().is_dir(), "{}", dir.display());
            assert_eq!(md.uid(), crate::sandbox::current_uid(), "{}", dir.display());
            assert_eq!(
                md.mode() & 0o077,
                0,
                "{} must be owner-only, is {:o}",
                dir.display(),
                md.mode() & 0o777
            );
        }
        assert!(leftovers(&parent).is_empty(), "{:?}", leftovers(&parent));
        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn dropping_the_scratch_never_follows_a_symlink_out_of_it() {
        let parent = scratch_parent("drop");
        let outside = parent.join("outside");
        write(&outside, "keep.txt", "kept");
        let under = crate::sandbox::create_private_dir("test", &parent, "under").expect("leaf");
        std::os::unix::fs::symlink(&outside, under.join("link")).expect("symlink under");
        drop(Scratch(under.clone()));
        assert!(
            std::fs::symlink_metadata(&under).is_err(),
            "the leaf is removed"
        );
        assert!(
            outside.join("keep.txt").exists(),
            "a symlink under it is not followed"
        );
        let at = crate::sandbox::create_private_dir("test", &parent, "at").expect("leaf");
        std::fs::remove_dir(&at).expect("rmdir");
        std::os::unix::fs::symlink(&outside, &at).expect("symlink at the leaf");
        drop(Scratch(at.clone()));
        assert!(
            std::fs::symlink_metadata(&at).is_err(),
            "the symlink itself is removed"
        );
        assert!(
            outside.join("keep.txt").exists(),
            "a symlink at the leaf is not followed"
        );
        let _ = std::fs::remove_dir_all(parent);
    }
}
