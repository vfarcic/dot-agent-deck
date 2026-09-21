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

/// The surface at one merge-base: extract its tree with `git archive` into a
/// fresh directory under `scratch_parent`, prove the extraction complete
/// against `git ls-tree`, run `cargo metadata --no-deps` there, and derive.
fn surface_at(
    clone: &Path,
    merge_base: &str,
    scratch_parent: &Path,
    git: &dyn Fn(&Path) -> Command,
    cargo: &dyn Fn(&Path) -> Command,
) -> Result<Surface, String> {
    let short: String = merge_base.chars().take(12).collect();
    let dir = scratch_parent.join(format!(
        ".xver-merge-base-{short}-{}-{}",
        crate::epoch_secs(),
        std::process::id()
    ));
    std::fs::create_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let scratch = Scratch(dir);
    let mut archive = git(clone)
        .args(["archive", "--format=tar", merge_base])
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git archive {merge_base}: {e}"))?;
    let tar_in = archive.stdout.take().ok_or("git archive: no stdout")?;
    let tar = Command::new("tar")
        .arg("-x")
        .arg("-C")
        .arg(&scratch.0)
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
    // `git archive` honours `export-ignore`; a tree that dropped a file is not
    // the tree cargo would see, so the extraction must be complete.
    let listing = run_ok(
        git(clone).args(["ls-tree", "-r", "-z", merge_base]),
        "git ls-tree",
    )?;
    let mut missing = Vec::new();
    for entry in listing.split(|b| *b == 0).filter(|s| !s.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        if meta.split_whitespace().nth(1) == Some("blob")
            && std::fs::symlink_metadata(scratch.0.join(path)).is_err()
        {
            missing.push(path.to_string());
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "the extracted merge-base {merge_base} is missing {} tracked file(s) (an \
             `export-ignore` attribute?), e.g. {:?}",
            missing.len(),
            &missing[..missing.len().min(5)]
        ));
    }
    let meta = run_ok(
        cargo(&scratch.0).args([
            "metadata",
            "--no-deps",
            "--offline",
            "--locked",
            "--format-version",
            "1",
        ]),
        "cargo metadata --no-deps at the merge-base",
    )?;
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
    let refuse = |e: String| {
        format!(
            "refusing before `cargo build` — nothing was compiled: the build-time comparison \
             with `origin/{BASE_BRANCH}` could not be made ({e})"
        )
    };
    let mbs = run_ok(
        git(clone).args(["merge-base", "--all", base_sha, head_sha]),
        "git merge-base",
    )
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
        changed.extend(nul_list(
            &run_ok(
                git(clone).args(["diff", "--name-only", "--no-renames", "-z", mb, head_sha]),
                "git diff --name-only",
            )
            .map_err(refuse)?,
        ));
        surface.merge(surface_at(clone, mb, scratch_parent, git, cargo).map_err(refuse)?);
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
}
