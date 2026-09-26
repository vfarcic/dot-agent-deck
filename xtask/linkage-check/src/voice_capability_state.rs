//! PRD #1195 M2 — rule 18: the state behind a registry capability is reached
//! through `VOICE_ACTIONS`, and new shell state cannot arrive unclassified.
//!
//! # Why this rule exists beside rule 14 rather than inside it
//!
//! Rule 14 ([`crate::voice_command_registry`]) proves every **entry** in
//! `VOICE_ACTIONS` is classified. It cannot prove a **capability** is
//! registered: a control wired straight to a React state setter reaches none of
//! its assertions, and PRD #802 recorded five such second dispatch paths that
//! nothing had caught. PRD #1195 M1 closed them; this rule is what keeps them
//! closed. It is a sibling module rather than more of rule 14 because it reads
//! different files (the app shell, not the table) with a different lexer (TSX,
//! with JSX, regex and template literals, where rule 14's scanner handles plain
//! TypeScript), and because its failure sentence names a different residual.
//!
//! # What it asserts — "guard the state, not the clicks"
//!
//! PRD #802's Open Question 3 rejected pinning a count of `onClick` sites: most
//! are chrome, and a count cannot tell a capability from a close button. So
//! this rule keys on the state that IS a capability instead.
//!
//! 1. **Setter ownership.** A *registry-owned setter* is a `set*` identifier
//!    named inside an **action context construction site** — an object literal
//!    annotated with a context type (`const x: VoiceScreenContext = { … }`,
//!    `useMemo<RailContext>(() => ({ … }))`, optionally through `Partial<…>`).
//!    The context types are themselves derived, never listed: every type alias
//!    in production `desktop/src` whose right-hand side names
//!    `VoiceActionContext`, plus that type itself. So the set of owned setters
//!    is whatever the construction sites say, and adding a member to one moves
//!    the rule with it. In a scanned shell file, every other reference to an
//!    owned setter is a finding naming the file, line and setter — unless it is
//!    - inside another construction site,
//!    - its own declaration (`const [x, setX] = useState(…)`, `const setX = …`,
//!      a destructuring on one line),
//!    - a hook dependency array — the last argument of `useEffect`,
//!      `useMemo`, `useCallback` and their siblings ([`DEPENDENCY_HOOKS`]) —
//!      which reads and never writes; a list handed to any other function is
//!      a setter handed away, and is a finding,
//!    - a **dismissal**: a call whose last argument is the literal `false`. A
//!      close turns a capability off; the registry deliberately holds the
//!      opening half, and `DeckSurface`'s `Escape` handler already argues that
//!      a blanket dismissal is not a registry dispatch,
//!    - or **marked**: a `voice-registry-exempt: <reason>` comment trailing
//!      the same line, or standing alone on the line above. The reason is read
//!      by a person, exactly as a `no_voice` reason is, so it must say
//!      something.
//! 2. **Shell state is classified.** Every `useState` / `useReducer` in the
//!    scanned shell files is either registry-owned — its setter is an owned
//!    setter — or marked the same way. A new overlay boolean therefore fails
//!    the build until its setter reaches a context (and so, through M1's
//!    shape, a registry entry) or somebody writes down why it is not a
//!    capability.
//!
//! A trailing marker covers only its own line — otherwise the reason written
//! for one `useState` would classify whatever was added on the line after it,
//! which is exactly what the first version of this rule did, measured by
//! adding an overlay boolean under the palette's. A marker that exempts nothing
//! is itself a finding, so the written reasons cannot outlive the code they
//! describe.
//!
//! # The scanned set, and why it is these files
//!
//! [`SHELL_FILES`]: `desktop/src/App.tsx`, which holds `DeckShell` (the view),
//! `ControlDeck` and `DeckSurface` (the deck's selection, drawer, tabs and
//! terminal focus, and every construction site the rail, the palette and the
//! in-panel controls dispatch through); and `desktop/src/hooks/useShellOverlays.ts`,
//! where the overlay booleans actually live since issue #1197 moved them out of
//! the deck. Its one `useState` hands its setter out only as `setOverlay`,
//! `closeOverlays` and `deck.set`, which are owned in `App.tsx`, so its
//! classification says exactly that.
//!
//! # What it does NOT see, and the rule sentence says so at every failure
//!
//! - **State outside the scanned files.** A capability whose state lives in a
//!   leaf component is invisible. The known one is `AgentOverview.tsx`: its New
//!   agent dialog (`newAgent`) and stop confirmation (`confirm`) back registry
//!   entries through helpers, and a row's Stop opens that confirmation without
//!   the registry by PRD #802 D5's own decision. It is the named next candidate
//!   for the scanned set, not a file this rule vouches for.
//! - **A capability reached through something that is not a `set*`
//!   identifier.** A callback prop (`onNavigate`) or a helper named in a
//!   construction site is not traced into, and calling one from a new control
//!   is not flagged. That is why `DeckSurface` writes `focusTerminal`'s body
//!   inside its literal rather than beside it.
//! - **A setter under a different name.** Ownership is by identifier within the
//!   file that declares it — sound by scoping, since a binding only leaves its
//!   file by being referenced (and that reference is what this rule checks) —
//!   but an alias (`const open = setOverlay`) is a reference like any other and
//!   is flagged, so the gap is only in what the alias is then used for.
//! - **A construction site written another way** (`{ … } satisfies T`, an
//!   `interface`, a context returned by a helper). It owns nothing, so its
//!   setters' `useState` then fails assertion 2 — the backstop is deliberate,
//!   but it is a backstop. The recognised forms are an annotated `const`/`let`
//!   literal, `useMemo<T>(() => ({ … }))`, and `useMemo<T>(() => { …; return
//!   { … }; })`, whose literal is a `return` at the callback body's top level.
//! - **Two unpaired quotes on one line of JSX text.** A quote that reaches the
//!   end of its line opened no string and is read back as text (a TS string
//!   cannot hold a raw newline), but two on one line — `Don't … won't` — look
//!   like a string between them, and a setter written between the two is
//!   blanked with it. A single apostrophe no longer hides anything.
//! - **A symlinked directory under `desktop/src`** is not followed: it is
//!   reported as an unsupported layout rather than skipped.
//!
//! # Budget
//!
//! The same as its neighbours: it reads the production `.ts`/`.tsx` under
//! `desktop/src` and does nothing else — no network, no git, no subprocess, no
//! sleep. Every file it cannot read, lex or find is a **finding**, never a skip:
//! a required check that inspects nothing and passes reports safety it never
//! established.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

use regex::Regex;

/// Where the context types are declared, and every other scanned file lives.
pub const DESKTOP_SRC: &str = "desktop/src";
/// The app shell, and the one file that must hold a construction site.
pub const APP_TSX: &str = "desktop/src/App.tsx";
/// Where the overlay booleans live (issue #1197).
pub const SHELL_OVERLAYS_TS: &str = "desktop/src/hooks/useShellOverlays.ts";
/// The files whose state is checked. See the module comment for why these.
pub const SHELL_FILES: &[&str] = &[APP_TSX, SHELL_OVERLAYS_TS];
/// The comment that classifies a `useState` or exempts a setter reference.
pub const MARKER: &str = "voice-registry-exempt:";
/// The root of every context type.
const CONTEXT_ROOT: &str = "VoiceActionContext";

/// The rule sentence, quoted in every failure — with the residual, because the
/// residual is what a reader infers wrongly from a green check.
pub const CAPABILITY_STATE_RULE: &str = "PRD #1195 rule 18: a `set*` state setter named inside a voice action \
     context construction site (an object literal annotated with a type built from `VoiceActionContext`) is \
     written nowhere else in `desktop/src/App.tsx` or `desktop/src/hooks/useShellOverlays.ts` unless the write is \
     a dismissal (its last argument is the literal `false`) or carries a `voice-registry-exempt: <reason>` \
     comment, and every `useState` in those files is registry-owned or carries that comment. A new control \
     opens a capability through `VOICE_ACTIONS[id].run(...)`, not through the setter. WHAT THIS RULE DOES NOT \
     SEE: state held outside those two files (`AgentOverview.tsx` is the known one), and a capability reached \
     through a callback prop or helper rather than a `set*` identifier, or written between two unpaired quotes \
     on one line of JSX text — a green rule 18 is NOT evidence that every capability is registered";

/// The texts the rule reads: every production `.ts`/`.tsx` under
/// [`DESKTOP_SRC`], keyed by repo-relative path with `/` separators.
pub struct Sources {
    pub production: BTreeMap<String, String>,
}

/// Read every production source under `desktop/src` and check them. Every
/// read failure is a finding.
pub fn run(root: &Path) -> Vec<String> {
    let mut findings = Vec::new();
    let mut production = BTreeMap::new();
    walk(
        root,
        &root.join(DESKTOP_SRC),
        &mut production,
        &mut findings,
    );
    if !findings.is_empty() {
        return findings.into_iter().map(annotate).collect();
    }
    check(&Sources { production })
}

fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>, findings: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            findings.push(format!(
                "could not list {}: {error} — rule 18 cannot see a directory it cannot read, so this is a \
                 failure rather than a skip",
                dir.display()
            ));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                findings.push(format!(
                    "could not read an entry of {}: {error}",
                    dir.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                findings.push(format!("could not stat {}: {error}", path.display()));
                continue;
            }
        };
        if file_type.is_dir() {
            walk(root, &path, out, findings);
            continue;
        }
        // `file_type` does not follow a symlink, so a linked directory is
        // neither a directory nor a source file above — and skipping it would
        // shrink the scan without a word. The walk does not follow links (a
        // cycle, or a target outside `desktop/src`, would need containment
        // and cycle checks this rule has no use for), so a linked directory is
        // an unsupported layout and says so. A linked FILE is read below like
        // any other, through the link.
        if file_type.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(target) if target.is_dir() => {
                    findings.push(format!(
                        "{} is a symlink to a directory — rule 18 does not follow directory links, so the \
                         sources behind it would go unscanned. Replace it with a real directory",
                        path.display()
                    ));
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    findings.push(format!(
                        "{} is a symlink rule 18 cannot resolve: {error}",
                        path.display()
                    ));
                    continue;
                }
            }
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_production_source(&name) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .map(|rel| rel.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| path.to_string_lossy().into_owned());
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                out.insert(rel, text);
            }
            Err(error) => findings.push(format!(
                "could not read {rel}: {error} — rule 18 cannot see a file it cannot read"
            )),
        }
    }
}

/// A `.ts` or `.tsx` file that is not a test.
fn is_production_source(name: &str) -> bool {
    (name.ends_with(".ts") || name.ends_with(".tsx"))
        && !name.contains(".test.")
        && !name.contains(".spec.")
}

fn annotate(finding: String) -> String {
    format!("{finding} — {CAPABILITY_STATE_RULE}")
}

/// The whole rule, over texts rather than paths.
pub fn check(sources: &Sources) -> Vec<String> {
    let mut findings = Vec::new();
    let context_types = context_types(sources, &mut findings);
    for file in SHELL_FILES {
        match sources.production.get(*file) {
            Some(text) => check_shell_file(file, text, &context_types, &mut findings),
            None => findings.push(format!(
                "{file}: not found among the production sources under {DESKTOP_SRC}. It is in rule 18's scanned \
                 set because it owns navigation or overlay state; if it moved, move the rule with it rather than \
                 dropping it from the set"
            )),
        }
    }
    findings.into_iter().map(annotate).collect()
}

/// Every type alias in production `desktop/src` whose right-hand side names
/// [`CONTEXT_ROOT`], plus the root itself.
fn context_types(sources: &Sources, findings: &mut Vec<String>) -> BTreeSet<String> {
    let alias =
        Regex::new(r"\btype\s+([A-Za-z_$][\w$]*)\s*(?:<[^=;]*>)?\s*=").expect("static regex");
    let root = Regex::new(&format!(r"\b{CONTEXT_ROOT}\b")).expect("static regex");
    let mut types = BTreeSet::new();
    let mut root_declared = false;
    for text in sources.production.values() {
        let masked = mask_tsx(text).masked;
        for found in alias.captures_iter(&masked) {
            let name = &found[1];
            let whole = found.get(0).expect("group 0");
            let rhs = &masked[whole.end()..statement_end(&masked, whole.end())];
            if name == CONTEXT_ROOT {
                root_declared = true;
                types.insert(name.to_string());
            } else if root.is_match(rhs) {
                types.insert(name.to_string());
            }
        }
    }
    if !root_declared {
        findings.push(format!(
            "no `type {CONTEXT_ROOT}` declaration found under {DESKTOP_SRC} — rule 18 derives every context \
             type from it, so without it there is nothing to derive the owned setters from"
        ));
    }
    types
}

/// The first `;` at bracket depth zero from `from`, or the end.
fn statement_end(masked: &str, from: usize) -> usize {
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    for (offset, byte) in bytes[from..].iter().enumerate() {
        match byte {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => return from + offset,
            _ => {}
        }
    }
    bytes.len()
}

/// Both assertions, over one shell file.
fn check_shell_file(
    file: &str,
    text: &str,
    context_types: &BTreeSet<String>,
    findings: &mut Vec<String>,
) {
    let lexed = mask_tsx(text);
    if let Some(problem) = lexed.problem {
        findings.push(format!(
            "{file}: {problem} — rule 18 reads this file as text and has lost its place in it, so it refuses to \
             report on it rather than report on less of it than is there"
        ));
        return;
    }
    let masked = lexed.masked;
    let sites = construction_sites(&masked, context_types);
    let owned = owned_setters(&masked, &sites);
    if file == APP_TSX {
        if sites.is_empty() {
            findings.push(format!(
                "{file}: holds no action context construction site (an object literal annotated with one of: \
                 {}) — rule 18 derives the registry-owned setters from those sites, so with none it would pass \
                 by inspecting nothing",
                joined(context_types)
            ));
        } else if owned.is_empty() {
            findings.push(format!(
                "{file}: its construction sites name no `set*` setter — rule 18 would then own nothing and pass \
                 by inspecting nothing"
            ));
        }
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let masked_lines: Vec<&str> = masked.split('\n').collect();
    let mut markers: BTreeMap<usize, Marker> = BTreeMap::new();
    for (index, line) in lines.iter().enumerate() {
        if let Some(at) = line.find(MARKER) {
            let reason = line[at + MARKER.len()..]
                .trim()
                .trim_end_matches('}')
                .trim_end()
                .trim_end_matches("*/")
                .trim();
            if reason.is_empty() {
                findings.push(format!(
                    "{file}:{}: a `{MARKER}` comment with no reason. The reason is read by a person, so it has to \
                     say why this is not a capability",
                    index + 1
                ));
            }
            // A marker on a line of its own covers the line below; one
            // trailing code covers only its own line. Without that, the reason
            // written for one `useState` silently classified whatever was added
            // on the line after it — measured, before this distinction existed.
            let standalone = masked_lines.get(index).is_some_and(|code| {
                code.chars()
                    .all(|c| c.is_whitespace() || c == '{' || c == '}')
            });
            markers.insert(
                index + 1,
                Marker {
                    standalone,
                    used: false,
                },
            );
        }
    }

    // Assertion 1: setter ownership.
    let setter = Regex::new(r"set[A-Z][\w$]*").expect("static regex");
    for found in setter.find_iter(&masked) {
        let name = found.as_str();
        if !owned.contains(name) || !word_start(&masked, found.start()) {
            continue;
        }
        let at = found.start();
        if sites.iter().any(|site| site.contains(&at))
            || is_declaration(&masked, at)
            || in_dependency_array(&masked, at)
            || is_dismissal(&masked, found.end())
        {
            continue;
        }
        let line = line_of(&masked, at);
        if !exempted(line, &mut markers) {
            findings.push(format!(
                "{file}:{line}: `{name}` is written outside the action context it is named in. A control that \
                 opens or selects this state goes through `VOICE_ACTIONS[id].run(...)`; a write that is not a \
                 control (an invariant, an undo) says so with a `{MARKER} <reason>` comment on this line or the \
                 line above"
            ));
        }
    }

    // Assertion 2: every `useState` / `useReducer` is classified.
    let hook = Regex::new(r"use(?:State|Reducer)\s*[<(]").expect("static regex");
    let binding = Regex::new(
        r"const\s*\[\s*([A-Za-z_$][\w$]*)\s*(?:,\s*([A-Za-z_$][\w$]*)\s*)?\]\s*=\s*(?:React\s*\.\s*)?$",
    )
    .expect("static regex");
    for found in hook.find_iter(&masked) {
        let at = found.start();
        if !word_start(&masked, at) && !masked[..at].ends_with('.') {
            continue;
        }
        let line = line_of(&masked, at);
        let line_start = masked[..at].rfind('\n').map_or(0, |newline| newline + 1);
        let Some(bound) = binding.captures(&masked[line_start..at]) else {
            findings.push(format!(
                "{file}:{line}: a `useState`/`useReducer` rule 18 cannot read. Write it as \
                 `const [value, setValue] = useState(…)` on one line so its setter can be classified"
            ));
            continue;
        };
        let state = bound[1].to_string();
        let setter = bound.get(2).map(|setter| setter.as_str().to_string());
        if setter.as_ref().is_some_and(|setter| owned.contains(setter)) {
            continue;
        }
        if !exempted(line, &mut markers) {
            findings.push(format!(
                "{file}:{line}: the state `{state}` (setter {}) is not registry-owned and not classified. If it is \
                 a capability, name its setter in an action context and dispatch it through a `VOICE_ACTIONS` \
                 entry; if it is transient UI state, say why with a `{MARKER} <reason>` comment on this line or \
                 the line above",
                setter.map_or_else(|| "none".to_string(), |setter| format!("`{setter}`"))
            ));
        }
    }

    for (line, marker) in markers {
        if !marker.used {
            findings.push(format!(
                "{file}:{line}: a `{MARKER}` comment that exempts nothing — neither this line nor the next writes \
                 a registry-owned setter or declares a `useState`. A reason that outlived its code misleads the \
                 next reader; delete it"
            ));
        }
    }
}

/// A `voice-registry-exempt:` comment, by the line it sits on.
struct Marker {
    /// Nothing but the comment on its line, so it covers the line below.
    standalone: bool,
    used: bool,
}

/// Whether a marker exempts `line` — one on the line itself, or one standing
/// alone on the line above — marking it used, so one that exempts nothing can
/// be reported.
fn exempted(line: usize, markers: &mut BTreeMap<usize, Marker>) -> bool {
    if let Some(marker) = markers.get_mut(&line) {
        marker.used = true;
        return true;
    }
    match markers.get_mut(&line.saturating_sub(1)) {
        Some(marker) if marker.standalone => {
            marker.used = true;
            true
        }
        _ => false,
    }
}

/// Every object literal annotated with a context type, as the byte range from
/// its `{` to its `}` inclusive.
fn construction_sites(masked: &str, context_types: &BTreeSet<String>) -> Vec<Range<usize>> {
    if context_types.is_empty() {
        return Vec::new();
    }
    let names = context_types
        .iter()
        .map(|name| regex::escape(name))
        .collect::<Vec<_>>()
        .join("|");
    let annotated = Regex::new(&format!(
        r":\s*(?:Partial\s*<\s*)?(?:{names})\b\s*>?\s*=\s*\{{"
    ))
    .expect("derived regex");
    let memo = Regex::new(&format!(
        r"useMemo\s*<\s*(?:Partial\s*<\s*)?(?:{names})\b\s*>?\s*>\s*\(\s*\(\s*\)\s*=>\s*\(\s*\{{"
    ))
    .expect("derived regex");
    // `useMemo<T>(() => { …; return { … }; })`: the literal is whatever the
    // callback's own body returns — at the body's top level, so a `return`
    // inside a nested function there is not mistaken for it.
    let memo_block = Regex::new(&format!(
        r"useMemo\s*<\s*(?:Partial\s*<\s*)?(?:{names})\b\s*>?\s*>\s*\(\s*\(\s*\)\s*=>\s*\{{"
    ))
    .expect("derived regex");
    let returned = Regex::new(r"\breturn\s*\(?\s*\{").expect("static regex");
    let mut sites = Vec::new();
    for found in annotated.find_iter(masked).chain(memo.find_iter(masked)) {
        let open = found.end() - 1;
        if let Some(close) = matching(masked, open) {
            sites.push(open..close + 1);
        }
    }
    for found in memo_block.find_iter(masked) {
        let body = found.end() - 1;
        let Some(body_end) = matching(masked, body) else {
            continue;
        };
        for literal in returned.find_iter(&masked[body + 1..body_end]) {
            let at = body + 1 + literal.start();
            if depth_within(masked, body + 1, at) != 0 {
                continue;
            }
            let open = body + 1 + literal.end() - 1;
            if let Some(close) = matching(masked, open) {
                sites.push(open..close + 1);
            }
        }
    }
    sites.sort_by_key(|site| site.start);
    sites
}

/// How many brackets are open between `from` and `to`, over masked text.
fn depth_within(masked: &str, from: usize, to: usize) -> usize {
    let mut depth = 0usize;
    for byte in &masked.as_bytes()[from..to] {
        match byte {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

/// The `set*` identifiers named inside the construction sites.
fn owned_setters(masked: &str, sites: &[Range<usize>]) -> BTreeSet<String> {
    let setter = Regex::new(r"set[A-Z][\w$]*").expect("static regex");
    let mut owned = BTreeSet::new();
    for site in sites {
        for found in setter.find_iter(&masked[site.clone()]) {
            if word_start(masked, site.start + found.start()) {
                owned.insert(found.as_str().to_string());
            }
        }
    }
    owned
}

/// Not preceded by an identifier character or a `.` (a property of something
/// else is not this binding).
fn word_start(masked: &str, at: usize) -> bool {
    masked[..at]
        .chars()
        .next_back()
        .is_none_or(|previous| !(previous.is_alphanumeric() || matches!(previous, '_' | '$' | '.')))
}

/// A binding on a one-line `const`/`let`/`var` declaration, before its `=`.
fn is_declaration(masked: &str, at: usize) -> bool {
    let line_start = masked[..at].rfind('\n').map_or(0, |newline| newline + 1);
    Regex::new(r"\b(?:const|let|var)\s[^=]*$")
        .expect("static regex")
        .is_match(&masked[line_start..at])
}

/// The React hooks whose last argument is a dependency array. A list passed to
/// anything else is an ordinary argument, and a setter in it is a setter
/// handed away.
const DEPENDENCY_HOOKS: &[&str] = &[
    "useEffect",
    "useLayoutEffect",
    "useInsertionEffect",
    "useMemo",
    "useCallback",
    "useImperativeHandle",
];

/// Inside a hook's dependency array: a `[` after a `,`, holding only
/// identifiers and member accesses, closed by a `]` that the `)` of a call to
/// one of [`DEPENDENCY_HOOKS`] follows — so it is that call's LAST argument.
/// `register(handler, [setOverlay])` is not one: a list handed to any other
/// function is a second route to the setter, which is what this rule exists to
/// see.
fn in_dependency_array(masked: &str, at: usize) -> bool {
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    let mut open = None;
    for index in (0..at).rev() {
        match bytes[index] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'{' if depth == 0 => return false,
            b'[' if depth == 0 => {
                open = Some(index);
                break;
            }
            b'(' | b'[' | b'{' => depth -= 1,
            _ => {}
        }
    }
    let Some(open) = open else { return false };
    let Some(close) = matching(masked, open) else {
        return false;
    };
    let inside_is_a_list = masked[open + 1..close]
        .chars()
        .all(|character| character.is_alphanumeric() || " \t\r\n_$.,?!".contains(character));
    let before = masked[..open].trim_end();
    let after = masked[close + 1..].trim_start();
    if !(inside_is_a_list && before.ends_with(',') && after.starts_with(')')) {
        return false;
    }
    let call_close = masked.len() - after.len();
    matching_back(masked, call_close).is_some_and(|call_open| {
        callee(masked, call_open).is_some_and(|name| DEPENDENCY_HOOKS.contains(&name))
    })
}

/// The bracket opening the one at `close`, over masked text.
fn matching_back(masked: &str, close: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    for index in (0..=close).rev() {
        match bytes[index] {
            b'}' | b')' | b']' => depth += 1,
            b'{' | b'(' | b'[' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// The identifier a call's `(` at `open` belongs to — through a `React.`
/// prefix and a type-argument list (`useMemo<RailContext>(`) — or `None`.
fn callee(masked: &str, open: usize) -> Option<&str> {
    let mut end = masked[..open].trim_end().len();
    if masked[..end].ends_with('>') {
        // Walk back over `<…>`, not counting the `>` of an arrow `=>`.
        let bytes = masked.as_bytes();
        let mut depth = 0usize;
        let mut index = end;
        loop {
            index = index.checked_sub(1)?;
            match bytes[index] {
                b'>' if index == 0 || bytes[index - 1] != b'=' => depth += 1,
                b'<' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }
        end = masked[..index].trim_end().len();
    }
    let start = masked[..end]
        .rfind(|character: char| !(character.is_alphanumeric() || matches!(character, '_' | '$')))
        .map_or(0, |found| found + 1);
    (start < end).then(|| &masked[start..end])
}

/// A call starting at `after_name` whose last argument is the literal `false`.
fn is_dismissal(masked: &str, after_name: usize) -> bool {
    let rest = &masked[after_name..];
    let open = after_name + (rest.len() - rest.trim_start().len());
    if masked.as_bytes().get(open) != Some(&b'(') {
        return false;
    }
    let Some(close) = matching(masked, open) else {
        return false;
    };
    let mut depth = 0usize;
    let mut last = open + 1;
    let mut arguments = Vec::new();
    for (offset, byte) in masked.as_bytes()[open + 1..close].iter().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                arguments.push(&masked[last..open + 1 + offset]);
                last = open + 1 + offset + 1;
            }
            _ => {}
        }
    }
    arguments.push(&masked[last..close]);
    arguments
        .into_iter()
        .map(str::trim)
        .rfind(|argument| !argument.is_empty())
        == Some("false")
}

/// The bracket closing the one at `open`, over masked text.
fn matching(masked: &str, open: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    for (offset, byte) in bytes[open..].iter().enumerate() {
        match byte {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn line_of(masked: &str, at: usize) -> usize {
    masked.as_bytes()[..at]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn joined(values: &BTreeSet<String>) -> String {
    if values.is_empty() {
        "(none)".to_string()
    } else {
        values.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

/// The lexer's output: the text with every comment, string, template-literal
/// text and regex literal blanked to spaces (newlines kept, byte length kept,
/// so an offset means the same thing in both), and why it gave up, if it did.
struct Lexed {
    masked: String,
    problem: Option<String>,
}

/// Blank everything in `text` that is not code.
///
/// **TSX, not TypeScript**: a `${ … }` inside a template literal is code (a
/// setter can be called there), a regex literal is not, and JSX's `/>` and
/// `</` are not the start of one. A quote that meets the end of its line
/// before its partner opened no string — a TS string cannot hold a raw newline
/// — so it is taken back as JSX text and the rest of the line is read as code.
/// Two unpaired quotes on ONE line of JSX text still pair up, and what lies
/// between them is blanked; the module comment lists that residual.
///
/// It refuses rather than guesses: ending inside a comment, string or
/// template, or with unbalanced brackets, is reported, because every assertion
/// above reads structure off this output.
fn mask_tsx(text: &str) -> Lexed {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Str(char),
        Template,
        Regex { class: bool },
    }
    let blank = |out: &mut String, character: char| {
        if character == '\n' {
            out.push('\n');
        } else {
            out.extend(std::iter::repeat_n(' ', character.len_utf8()));
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut state = State::Code;
    // One entry per open `${`: how many `{` deep the expression is.
    let mut templates: Vec<usize> = Vec::new();
    let mut escaped = false;
    // Where the open quote string began: its char index and `out`'s length
    // then, so a "string" that meets its line's end can be taken back.
    let mut string_start = (0usize, 0usize);
    let mut at = 0usize;
    while at < chars.len() {
        let character = chars[at];
        let next = chars.get(at + 1).copied();
        match state {
            State::Code => {
                if character == '/' && next == Some('*') {
                    state = State::Block;
                    out.push_str("  ");
                    at += 2;
                    continue;
                }
                if character == '/' && next == Some('/') {
                    state = State::Line;
                    out.push_str("  ");
                    at += 2;
                    continue;
                }
                if character == '/' && next != Some('>') && regex_may_start(&out) {
                    state = State::Regex { class: false };
                    escaped = false;
                    out.push(' ');
                    at += 1;
                    continue;
                }
                match character {
                    '"' | '\'' => {
                        state = State::Str(character);
                        escaped = false;
                        string_start = (at, out.len());
                        out.push(' ');
                    }
                    '`' => {
                        state = State::Template;
                        escaped = false;
                        out.push(' ');
                    }
                    '{' => {
                        if let Some(depth) = templates.last_mut() {
                            *depth += 1;
                        }
                        out.push('{');
                    }
                    '}' => match templates.last_mut() {
                        Some(0) => {
                            templates.pop();
                            state = State::Template;
                            out.push(' ');
                        }
                        Some(depth) => {
                            *depth -= 1;
                            out.push('}');
                        }
                        None => out.push('}'),
                    },
                    other => out.push(other),
                }
            }
            State::Line => {
                if character == '\n' {
                    state = State::Code;
                }
                blank(&mut out, character);
            }
            State::Block => {
                if character == '*' && next == Some('/') {
                    state = State::Code;
                    out.push_str("  ");
                    at += 2;
                    continue;
                }
                blank(&mut out, character);
            }
            State::Str(quote) => {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '\n' {
                    // A TS string cannot hold a raw newline, so a quote that
                    // reaches one opened no string: it is JSX text — "Don't",
                    // "5' tall". Take it back and read the rest of its line as
                    // code, where a same-line setter write would otherwise
                    // have been blanked with it.
                    let (quote_at, out_len) = string_start;
                    out.truncate(out_len);
                    out.push(' ');
                    state = State::Code;
                    at = quote_at + 1;
                    continue;
                } else if character == quote {
                    state = State::Code;
                }
                blank(&mut out, character);
            }
            State::Template => {
                if escaped {
                    escaped = false;
                    blank(&mut out, character);
                } else if character == '\\' {
                    escaped = true;
                    blank(&mut out, character);
                } else if character == '`' {
                    state = State::Code;
                    out.push(' ');
                } else if character == '$' && next == Some('{') {
                    templates.push(0);
                    state = State::Code;
                    out.push_str("  ");
                    at += 2;
                    continue;
                } else {
                    blank(&mut out, character);
                }
            }
            State::Regex { class } => {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '[' {
                    state = State::Regex { class: true };
                } else if character == ']' {
                    state = State::Regex { class: false };
                } else if (character == '/' && !class) || character == '\n' {
                    state = State::Code;
                }
                blank(&mut out, character);
            }
        }
        at += 1;
    }
    let problem = if !matches!(state, State::Code | State::Line) || !templates.is_empty() {
        Some("the text ends inside a comment, string, template or regex literal".to_string())
    } else {
        let count = |byte: u8| out.bytes().filter(|found| *found == byte).count();
        [(b'{', b'}'), (b'(', b')'), (b'[', b']')]
            .into_iter()
            .find(|(open, close)| count(*open) != count(*close))
            .map(|(open, close)| {
                format!(
                    "its code has {} `{}` against {} `{}`",
                    count(open),
                    open as char,
                    count(close),
                    close as char
                )
            })
    };
    Lexed {
        masked: out,
        problem,
    }
}

/// Whether a `/` here starts a regex literal rather than dividing: it follows
/// an operator or an opening bracket, not a value. `}` is deliberately absent —
/// JSX's `={x} />` puts one before a `/` that closes a tag.
fn regex_may_start(out: &str) -> bool {
    out.trim_end()
        .chars()
        .next_back()
        .is_none_or(|previous| "(,=:[!&|?;{".contains(previous))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("xtask/linkage-check sits two levels below the workspace root")
            .to_path_buf()
    }

    const REGISTRY: &str = "desktop/src/lib/voiceActions.ts";

    /// A minimal tree: the root context type, a pick of it, an app shell and
    /// the overlay hook.
    fn planted(app: &str) -> Sources {
        let mut production = BTreeMap::new();
        production.insert(
            REGISTRY.to_string(),
            "export type VoiceActionContext = { navigate: () => void; openOverlay: (o: string) => void };\n\
             export type ScreenContext = Pick<VoiceActionContext, \"navigate\">;\n"
                .to_string(),
        );
        production.insert(APP_TSX.to_string(), app.to_string());
        production.insert(
            SHELL_OVERLAYS_TS.to_string(),
            "// voice-registry-exempt: the overlay booleans\nconst [state, setState] = useState(1);\n"
                .to_string(),
        );
        Sources { production }
    }

    /// An app whose one construction site owns `setView` and `setPanelOpen`.
    const APP: &str = "\
export function Shell() {
  const [view, setView] = useState(\"deck\");
  const [panelOpen, setPanelOpen] = useState(false);
  const context: ScreenContext = {
    navigate: () => setView(\"overview\"),
    openOverlay: () => setPanelOpen(true),
  };
  useEffect(() => {}, [context, setView]);
  return <Panel open={panelOpen} onClose={() => setPanelOpen(false)} />;
}
";

    fn findings_for(app: &str) -> Vec<String> {
        check(&planted(app))
    }

    fn assert_one_finding(findings: &[String], needle: &str) {
        assert_eq!(
            findings.len(),
            1,
            "expected one finding, got: {findings:#?}"
        );
        assert!(
            findings[0].contains(needle),
            "finding lacks {needle:?}: {}",
            findings[0]
        );
        assert!(
            findings[0].contains(CAPABILITY_STATE_RULE),
            "finding lacks the rule sentence"
        );
    }

    /// The tree itself. What makes the planted-input tests below mean
    /// something: they prove the scanner CAN fail, and this proves the
    /// checked-in sources do not.
    #[test]
    fn the_checked_in_shell_passes() {
        let findings = run(&repo_root());
        assert!(findings.is_empty(), "rule 18:\n{}", findings.join("\n"));
    }

    /// And that the scan is not vacuous: it derived real context types, found
    /// real construction sites, and owns the setters M1 routed.
    #[test]
    fn the_scan_derives_real_contexts_and_owns_the_setters_m1_routed() {
        let mut production = BTreeMap::new();
        walk(
            &repo_root(),
            &repo_root().join(DESKTOP_SRC),
            &mut production,
            &mut Vec::new(),
        );
        let sources = Sources { production };
        let mut findings = Vec::new();
        let types = context_types(&sources, &mut findings);
        assert!(findings.is_empty(), "{findings:#?}");
        for expected in [
            "VoiceActionContext",
            "VoiceScreenContext",
            "VoiceDispatchContext",
            "RailContext",
        ] {
            assert!(
                types.contains(expected),
                "{expected} not derived: {types:?}"
            );
        }
        let lexed = mask_tsx(&sources.production[APP_TSX]);
        assert!(lexed.problem.is_none(), "{:?}", lexed.problem);
        let sites = construction_sites(&lexed.masked, &types);
        assert!(sites.len() >= 3, "only {} construction sites", sites.len());
        let owned = owned_setters(&lexed.masked, &sites);
        for expected in [
            "setView",
            "setOverlay",
            "setSelectedAgentId",
            "setEvidenceOpen",
        ] {
            assert!(owned.contains(expected), "{expected} not owned: {owned:?}");
        }
        assert!(
            !sources
                .production
                .keys()
                .any(|path| path.contains(".test."))
        );
    }

    #[test]
    fn a_planted_shell_with_only_sanctioned_writes_passes() {
        assert_eq!(findings_for(APP), Vec::<String>::new());
    }

    /// The regression M1 closed: a control opening a capability through its
    /// setter.
    #[test]
    fn a_direct_setter_call_in_a_control_fails_naming_file_and_setter() {
        let app = APP.replace(
            "  return <Panel",
            "  const reopen = <button onClick={() => setPanelOpen(true)} />;\n  return <Panel",
        );
        let findings = findings_for(&app);
        assert_one_finding(
            &findings,
            "desktop/src/App.tsx:9: `setPanelOpen` is written outside",
        );
    }

    /// Ownership is derived: a setter becomes owned by being named in a
    /// construction site, with no list to update.
    #[test]
    fn a_setter_added_to_a_context_becomes_owned_with_no_list_to_edit() {
        let app = APP
            .replace(
                "  const context",
                "  const [drawer, setDrawer] = useState(false);\n  const context",
            )
            .replace(
                "    navigate: () => setView(\"overview\"),",
                "    navigate: () => { setView(\"overview\"); setDrawer(true); },",
            )
            .replace(
                "  return <Panel",
                "  const flip = () => setDrawer(true);\n  return <Panel",
            );
        assert_one_finding(&findings_for(&app), "`setDrawer` is written outside");
    }

    #[test]
    fn a_setter_handed_away_as_a_value_fails() {
        let app = APP.replace(
            "onClose={() => setPanelOpen(false)}",
            "onToggle={setPanelOpen}",
        );
        assert_one_finding(&findings_for(&app), "`setPanelOpen` is written outside");
    }

    #[test]
    fn a_marker_with_a_reason_exempts_its_line_and_the_next() {
        let same_line = APP.replace(
            "  return <Panel",
            "  const undo = () => setView(\"deck\"); // voice-registry-exempt: the undo of a dispatch\n  return <Panel",
        );
        assert_eq!(findings_for(&same_line), Vec::<String>::new());
        let line_above = APP.replace(
            "  return <Panel",
            "  // voice-registry-exempt: the undo of a dispatch\n  const undo = () => setView(\"deck\");\n  return <Panel",
        );
        assert_eq!(findings_for(&line_above), Vec::<String>::new());
    }

    /// A trailing marker classifies its own line and nothing else: a new
    /// `useState` added directly under a classified one is still a finding.
    #[test]
    fn a_trailing_marker_does_not_cover_the_line_below() {
        let app = APP.replace(
            "  const context",
            "  const [draft, setDraft] = useState(\"\"); // voice-registry-exempt: an unsent draft\n  const [modalOpen, setModalOpen] = useState(false);\n  const context",
        );
        assert_one_finding(
            &findings_for(&app),
            "`modalOpen` (setter `setModalOpen`) is not registry-owned",
        );
    }

    #[test]
    fn a_marker_with_no_reason_fails() {
        let app = APP.replace(
            "  return <Panel",
            "  const undo = () => setView(\"deck\"); // voice-registry-exempt:\n  return <Panel",
        );
        assert_one_finding(&findings_for(&app), "with no reason");
    }

    #[test]
    fn a_marker_that_exempts_nothing_fails() {
        let app = APP.replace(
            "  return <Panel",
            "  // voice-registry-exempt: once covered something\n  const x = 1;\n  return <Panel",
        );
        assert_one_finding(&findings_for(&app), "exempts nothing");
    }

    /// Only a literal `false` LAST argument is a dismissal.
    #[test]
    fn only_a_literal_false_last_argument_is_a_dismissal() {
        for (write, ok) in [
            ("setPanelOpen(false)", true),
            ("setPanelOpen( false , )", true),
            ("setPanelOpen(!panelOpen)", false),
            ("setPanelOpen(isFalse)", false),
            ("setPanelOpen(false || other)", false),
        ] {
            let app = APP.replace("setPanelOpen(false)}", &format!("{write}}}"));
            let findings = findings_for(&app);
            assert_eq!(findings.is_empty(), ok, "{write}: {findings:#?}");
        }
    }

    #[test]
    fn an_unclassified_use_state_fails_and_a_classified_one_passes() {
        let unclassified = APP.replace(
            "  const context",
            "  const [modalOpen, setModalOpen] = useState(false);\n  const context",
        );
        assert_one_finding(
            &findings_for(&unclassified),
            "`modalOpen` (setter `setModalOpen`) is not registry-owned",
        );
        let classified = APP.replace(
            "  const context",
            "  const [draft, setDraft] = useState(\"\"); // voice-registry-exempt: an unsent draft\n  const context",
        );
        assert_eq!(findings_for(&classified), Vec::<String>::new());
    }

    #[test]
    fn the_overlay_hook_is_scanned_too() {
        let mut sources = planted(APP);
        sources.production.insert(
            SHELL_OVERLAYS_TS.to_string(),
            "const [state, setState] = useState(1);\n".to_string(),
        );
        assert_one_finding(
            &check(&sources),
            "desktop/src/hooks/useShellOverlays.ts:1: the state `state`",
        );
    }

    #[test]
    fn a_use_state_it_cannot_read_fails() {
        let app = APP.replace(
            "  const context",
            "  const pair = useState(false);\n  const context",
        );
        assert_one_finding(&findings_for(&app), "cannot read");
    }

    #[test]
    fn an_app_with_no_construction_site_fails_rather_than_passing_empty() {
        let app = APP.replace("const context: ScreenContext = {", "const context = {");
        let findings = findings_for(&app);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("holds no action context construction site")),
            "{findings:#?}"
        );
    }

    #[test]
    fn no_root_context_type_and_a_missing_shell_file_are_both_findings() {
        let mut sources = planted(APP);
        sources.production.insert(
            REGISTRY.to_string(),
            "export type Other = {};\n".to_string(),
        );
        sources.production.remove(SHELL_OVERLAYS_TS);
        let findings = check(&sources);
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("no `type VoiceActionContext`")),
            "{findings:#?}"
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.contains("useShellOverlays.ts: not found")),
            "{findings:#?}"
        );
    }

    /// Filesystem errors fail rather than shrink the scan.
    #[test]
    fn an_unreadable_root_is_a_finding() {
        let missing = repo_root().join("xtask/linkage-check/does-not-exist");
        let findings = run(&missing);
        assert!(!findings.is_empty());
        assert!(findings[0].contains("could not list"), "{findings:#?}");
    }

    /// The `useMemo<Context>(() => ({ … }))` form is a construction site.
    #[test]
    fn a_memoised_context_is_a_construction_site() {
        let app = APP
            .replace(
                "  const context: ScreenContext = {",
                "  const context = useMemo<ScreenContext>(() => ({",
            )
            .replace(
                "    openOverlay: () => setPanelOpen(true),\n  };",
                "    openOverlay: () => setPanelOpen(true),\n  }), []);",
            );
        assert_eq!(findings_for(&app), Vec::<String>::new());
    }

    /// Comments, strings, regex literals and JSX are not code; a template's
    /// `${ … }` is.
    #[test]
    fn the_lexer_reads_code_only_and_sees_into_template_expressions() {
        let quiet = APP.replace(
            "  return <Panel",
            "  // setPanelOpen(true) in a comment\n  const s = \"setPanelOpen(true)\";\n  const r = /set[A-Z]\\/}/.test(s);\n  return <Panel",
        );
        assert_eq!(findings_for(&quiet), Vec::<String>::new());
        let loud = APP.replace(
            "  return <Panel",
            "  const s = `${setPanelOpen(true)}`;\n  return <Panel",
        );
        assert_one_finding(&findings_for(&loud), "`setPanelOpen` is written outside");
    }

    /// Review finding (PRD #1195 M2): an apostrophe in JSX text used to open a
    /// "string" that ran to the end of its line and blanked a same-line setter
    /// write. It no longer hides one — and a real string on that line is
    /// still not code.
    #[test]
    fn an_apostrophe_in_jsx_text_hides_no_setter_after_it() {
        let loud = APP.replace(
            "  return <Panel",
            "  const hint = <p>Don't {ready && setPanelOpen(true)}</p>;\n  return <Panel",
        );
        assert_one_finding(&findings_for(&loud), "`setPanelOpen` is written outside");
        let quiet = APP.replace(
            "  return <Panel",
            "  const hint = <p>Don't press {\"setPanelOpen(true)\"}</p>;\n  return <Panel",
        );
        assert_eq!(findings_for(&quiet), Vec::<String>::new());
    }

    /// Review finding: the block-bodied `useMemo<T>(() => { return { … }; })`
    /// is a construction site too — its setters are owned, and a `return`
    /// inside a nested function in that body is not mistaken for the literal.
    #[test]
    fn a_block_bodied_memoised_context_is_a_construction_site() {
        let app = APP
            .replace(
                "  const context: ScreenContext = {",
                "  const [drawer, setDrawer] = useState(false);\n  const context = useMemo<ScreenContext>(() => {\n    const inner = () => { return { navigate: setDrawer }; };\n    return {",
            )
            .replace(
                "    openOverlay: () => setPanelOpen(true),\n  };",
                "    openOverlay: () => setPanelOpen(true),\n    };\n  }, []);",
            );
        // `setDrawer` is named only in the NESTED function's `return`, so it
        // is not owned and its `useState` is unclassified — the one finding —
        // while the real literal's `setView` and `setPanelOpen` are owned.
        assert_one_finding(
            &findings_for(&app),
            "`drawer` (setter `setDrawer`) is not registry-owned",
        );
        let clean = APP
            .replace(
                "  const context: ScreenContext = {",
                "  const context = useMemo<ScreenContext>(() => {\n    return {",
            )
            .replace(
                "    openOverlay: () => setPanelOpen(true),\n  };",
                "    openOverlay: () => setPanelOpen(true),\n    };\n  }, []);",
            );
        assert_eq!(findings_for(&clean), Vec::<String>::new());
    }

    /// Review finding: a list of setters handed to a function that is not a
    /// React hook is a second route to the setter, not a dependency array.
    #[test]
    fn a_setter_list_passed_to_a_non_hook_fails() {
        let handed = APP.replace(
            "  return <Panel",
            "  register(handler, [setPanelOpen]);\n  return <Panel",
        );
        assert_one_finding(&findings_for(&handed), "`setPanelOpen` is written outside");
        for hook in [
            "useMemo<ScreenContext>",
            "useCallback",
            "React.useLayoutEffect",
        ] {
            let deps = APP.replace(
                "  return <Panel",
                &format!("  const x = {hook}(() => 1, [setPanelOpen]);\n  return <Panel"),
            );
            assert_eq!(findings_for(&deps), Vec::<String>::new(), "{hook}");
        }
    }

    /// Review finding: a symlinked directory under `desktop/src` used to be
    /// skipped without a word. It is reported as an unsupported layout.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_reported_rather_than_skipped() {
        let tree = tempfile::tempdir().expect("a temp dir");
        let src = tree.path().join(DESKTOP_SRC);
        let elsewhere = tree.path().join("elsewhere");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::write(elsewhere.join("Hidden.tsx"), "export const x = 1;\n").expect("write");
        std::os::unix::fs::symlink(&elsewhere, src.join("linked")).expect("symlink");
        let mut out = BTreeMap::new();
        let mut findings = Vec::new();
        walk(tree.path(), &src, &mut out, &mut findings);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert!(
            findings[0].contains("symlink to a directory"),
            "{findings:#?}"
        );
        assert!(out.is_empty(), "the linked sources were not read: {out:?}");
    }

    #[test]
    fn a_file_the_lexer_loses_its_place_in_is_refused() {
        let app = format!("{APP}\nconst broken = `never closed");
        assert_one_finding(&findings_for(&app), "lost its place");
    }
}
