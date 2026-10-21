//! Issue #1701 — rule 14's control-site ratchet: a new desktop control reaches
//! the voice registry, or a written reason says why it does not.
//!
//! # Why rule 14 needed it
//!
//! Rule 14's own assertions ([`crate::voice_command_registry`]) prove every
//! `VOICE_ACTIONS` **entry** is classified. A control wired with a bare
//! `onClick` never becomes an entry, so it was invisible to them — and PR
//! #1672's Host button was exactly that: a new desktop control with no voice
//! command, missed by its coder and two review rounds, found by the user
//! saying "open host info". PRD #802's Open Question 3 had rejected *pinning a
//! count* of `onClick` sites because a count cannot tell a capability from a
//! close button. This is not a count: every existing site is listed by
//! identity, each with a sentence a person wrote saying what it is, so a new
//! one cannot hide inside a number that happened to stay the same.
//!
//! # What it asserts
//!
//! A **control site** is a JSX attribute in a production `.tsx` file under
//! `desktop/src` naming one of [`HANDLERS`] — `onClick={ … }` and its siblings
//! — whose expression is not *inert* (a handler that does nothing but
//! `event.stopPropagation()` or `event.preventDefault()` keeps a click inside
//! its panel; it is never a control). Then:
//!
//! 1. a site whose expression is **one** registry dispatch —
//!    `VOICE_ACTIONS.<id>.run(…)` or `dispatchVoiceAction(…)`, optionally
//!    behind an arrow, braces, `return` or `void` — goes through an entry
//!    rule 14's other assertions already classify, and needs nothing more. A
//!    handler that dispatches AND does something else is a site like any
//!    other: the something else is not reached by voice (PR #1771 review);
//! 2. every other site is listed in [`BASELINE`] — by file, handler and the
//!    handler's whole code, whitespace-collapsed — with a non-empty `reason`. A site
//!    that is not listed is a finding, and the finding prints the stanza to
//!    add, so the cheapest way past it is still to write the sentence;
//! 3. a listed entry that matches no site is a finding too, so **the baseline
//!    only shrinks**: a site that moves onto the registry, or is deleted, takes
//!    its entry with it rather than leaving a reason that describes nothing and
//!    would quietly cover the next site written the same way.
//!
//! Two sites in one file with the same handler and the same code are the same
//! key, listed once per site: the tree may hold as many as the baseline lists
//! and no more.
//!
//! **The scan reads the raw text, not the lexer's mask alone** (PR #1771
//! review). Rule 18's lexer pairs two apostrophes on one line of JSX text
//! (`Don't … you're`) as a string and blanks what lies between, so a handler
//! written there would vanish from a masked-only scan. A handler the lexer
//! blanked is skipped only when it lies inside a **comment**; one it took for
//! a string is re-lexed from its own brace and counted, which also counts the
//! rare handler-shaped text inside a real string literal — failing safe.
//!
//! # What it does NOT see — the rule sentence says so at every failure
//!
//! - **What a listed reason says.** A reason is read by a person; the rule
//!   proves only that one was written. Adding a stanza is as easy as routing
//!   through the registry, which is the point of the CLAUDE.md rule that sits
//!   beside it: the question is answered while the control is designed.
//! - **Handlers outside [`HANDLERS`]**: keyboard handlers (`onKeyDown`), a
//!   component's own callback props (`onOpen`, `onClose` — the DOM handler
//!   that eventually calls one IS scanned, wherever it is written), listeners
//!   added imperatively (`addEventListener`), and a `{...props}` spread that
//!   carries a handler in.
//! - **Whether a registry-routed site is voice-reachable.** Naming
//!   `VOICE_ACTIONS` routes the click through an entry; whether that entry is
//!   `voice: true` or carries a `no_voice` reason is rule 14's assertion 4.
//! - **A changed meaning behind unchanged code.** The key is the handler's
//!   code, so `onClick={onOpen}` keeps its entry whatever the host passes as
//!   `onOpen`.
//! - **A swap between identical sites.** Sites with the same file, handler
//!   and code are counted, not told apart, so deleting one and adding another
//!   with identical code in the same file nets to no change.
//! - **A handler after `//` in JSX text** (a URL in a sentence): the lexer
//!   reads the rest of that line as a comment, and a commented handler is
//!   skipped. Telling the two apart needs JSX context the lexer does not track.
//! - **Non-`.tsx` sources and anything outside `desktop/src`.**
//!
//! # Budget
//!
//! The same as its neighbours: it reads the production sources under
//! `desktop/src` and one TOML file, and does nothing else — no network, no git,
//! no subprocess, no sleep. Every input it cannot read, lex or parse is a
//! **finding**, never a skip.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;

use crate::voice_capability_state::{line_of, mask_tsx, matching, walk};

/// Where the scanned sources live.
pub const DESKTOP_SRC: &str = "desktop/src";
/// The checked-in list of today's control sites that do not dispatch through
/// the registry, each with its reason.
pub const BASELINE: &str = "xtask/linkage-check/voice-control-sites.toml";
/// The JSX attributes that make an element a control. `onKeyDown` is absent on
/// purpose — the module comment says why and what that leaves unseen.
pub const HANDLERS: &[&str] = &[
    "onClick",
    "onDoubleClick",
    "onContextMenu",
    "onChange",
    "onInput",
    "onSubmit",
    "onMouseDown",
    "onPointerDown",
];

/// The rule sentence, quoted in every failure — with the residual, because the
/// residual is what a reader infers wrongly from a green check.
pub const VOICE_CONTROL_RULE: &str = "Issue #1701, rule 14's control-site ratchet: every `onClick`/`onChange`/`onSubmit`/\
     `onMouseDown` (and sibling) handler in production `.tsx` under `desktop/src` either is one \
     `VOICE_ACTIONS.<id>.run(…)` / `dispatchVoiceAction(…)` call, only stops the event \
     (`stopPropagation`/`preventDefault`), or is listed in `xtask/linkage-check/voice-control-sites.toml` with \
     a reason; listed entries that match no site must be deleted, so the list only shrinks. A NEW desktop control \
     is reachable by voice through the registry (a `voice: true` entry and a `commands.toml` row), or its \
     registry entry carries a `no_voice` reason — see CLAUDE.md rule 23 and docs/develop/voice-first-design.md. \
     WHAT THIS RULE DOES NOT SEE: whether a written reason is true, keyboard handlers, a component's own \
     callback props, imperatively added listeners, a handler spread in with `{...props}`, a handler after `//` \
     in JSX text, a deleted site swapped for one with identical code in the same file, and anything outside \
     `.tsx` under `desktop/src` — a green check is NOT evidence that every capability is voice-reachable";

/// The texts the rule reads, so it can be driven from planted input as well as
/// from the tree.
pub struct Sources {
    /// Every production `.ts`/`.tsx` under [`DESKTOP_SRC`], keyed by
    /// repo-relative path with `/` separators.
    pub production: BTreeMap<String, String>,
    /// [`BASELINE`]'s text.
    pub baseline: String,
}

/// What identifies a site across edits that do not touch it: line numbers
/// would churn on every unrelated change above it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Key {
    file: String,
    handler: String,
    code: String,
}

/// Read the tree and the baseline and check them. Every read failure is a
/// finding.
pub fn run(root: &Path) -> Vec<String> {
    let mut findings = Vec::new();
    let mut production = BTreeMap::new();
    walk(
        root,
        &root.join(DESKTOP_SRC),
        "rule 14's control-site ratchet",
        &mut production,
        &mut findings,
    );
    let baseline = match std::fs::read_to_string(root.join(BASELINE)) {
        Ok(text) => text,
        Err(error) => {
            findings.push(format!(
                "could not read {BASELINE}: {error} — without it every control site would be reported, or none \
                 would be checked; if it moved, move this rule with it"
            ));
            String::new()
        }
    };
    if !findings.is_empty() {
        return findings.into_iter().map(annotate).collect();
    }
    check(&Sources {
        production,
        baseline,
    })
}

fn annotate(finding: String) -> String {
    format!("{finding} — {VOICE_CONTROL_RULE}")
}

/// The whole ratchet, over texts rather than paths.
pub fn check(sources: &Sources) -> Vec<String> {
    let mut findings = Vec::new();
    let listed = baseline_entries(&sources.baseline, &mut findings);
    let mut found: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
    // Every handler attribute read, routed and inert ones included: a tree whose
    // controls all dispatch through the registry lists nothing and passes, while
    // a scan that read no handler at all inspected nothing.
    let mut scanned = 0usize;
    for (file, text) in &sources.production {
        if !file.ends_with(".tsx") {
            continue;
        }
        for (key, line) in control_sites(file, text, &mut scanned, &mut findings) {
            found.entry(key).or_default().push(line);
        }
    }
    if scanned == 0 {
        findings.push(format!(
            "no handler attribute was found in any `.tsx` file under {DESKTOP_SRC} — the ratchet would then pass \
             by inspecting nothing"
        ));
    }

    for (key, lines) in &found {
        let allowed = listed.get(key).map_or(0, Vec::len);
        if lines.len() > allowed {
            let at = lines
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            findings.push(format!(
                "{}:{at}: a `{}` control that does not dispatch through `VOICE_ACTIONS` ({} such site(s) with this \
                 code, {allowed} listed in {BASELINE}). Route it through a registry entry — `voice: true` with a \
                 `commands.toml` row, or a `no_voice` reason — or, if it is not a capability (a dismissal, a \
                 field edit inside a form), add this stanza to {BASELINE} with a reason a person can disagree \
                 with:\n\n{}",
                key.file,
                key.handler,
                lines.len(),
                stanza(key),
            ));
        }
    }
    for (key, entries) in &listed {
        let present = found.get(key).map_or(0, Vec::len);
        if entries.len() > present {
            findings.push(format!(
                "{BASELINE}:{}: lists {} `{}` site(s) in {} with the code {:?} and the tree holds {present}. Delete \
                 the extra entr(y/ies) — the list only shrinks, and a reason that describes nothing would quietly \
                 cover the next control written the same way",
                entries[0],
                entries.len(),
                key.handler,
                key.file,
                key.code,
            ));
        }
    }
    findings.into_iter().map(annotate).collect()
}

/// The stanza that lists `key`, exactly as the baseline spells one.
fn stanza(key: &Key) -> String {
    let mut table = toml_edit::Table::new();
    table.insert("file", toml_edit::value(key.file.as_str()));
    table.insert("handler", toml_edit::value(key.handler.as_str()));
    table.insert("code", toml_edit::value(key.code.as_str()));
    table.insert(
        "reason",
        toml_edit::value("<what this control does, and why it is not a voice capability>"),
    );
    let mut sites = toml_edit::ArrayOfTables::new();
    sites.push(table);
    let mut document = toml_edit::DocumentMut::new();
    document.insert("site", toml_edit::Item::ArrayOfTables(sites));
    document.to_string()
}

/// Every control site in one file, with its 1-indexed line. `scanned` counts
/// every handler attribute read, exempt or not.
fn control_sites(
    file: &str,
    text: &str,
    scanned: &mut usize,
    findings: &mut Vec<String>,
) -> Vec<(Key, usize)> {
    let lexed = mask_tsx(text);
    if let Some(problem) = lexed.problem {
        findings.push(format!(
            "{file}: cannot be read as TSX ({problem}), so its control sites cannot be listed"
        ));
        return Vec::new();
    }
    let masked = lexed.masked;
    let attribute = Regex::new(&format!(r"(?:^|[\s{{(])({})\s*=\s*\{{", HANDLERS.join("|")))
        .expect("static regex");
    let inert = Regex::new(
        r"^\(?\s*([A-Za-z_$][\w$]*)\s*\)?\s*=>\s*(?:\{\s*)?([A-Za-z_$][\w$]*)\.(?:stopPropagation|preventDefault)\(\)\s*;?\s*\}?$",
    )
    .expect("static regex");

    let mut sites = Vec::new();
    for found in attribute.captures_iter(text) {
        let handler = found.get(1).expect("group 1");
        if lexed
            .comments
            .iter()
            .any(|comment| comment.contains(&handler.start()))
        {
            continue;
        }
        *scanned += 1;
        let open = found.get(0).expect("group 0").end() - 1;
        let line = line_of(&masked, handler.start());
        // Read as code by the lexer: its own mask holds the expression. Blanked
        // outside a comment: the lexer took it for a string (two apostrophes in
        // JSX text pair up), so lex again from the brace, where the state is
        // known to be code.
        let (relexed, base) = if masked[handler.range()] == text[handler.range()] {
            (None, 0)
        } else {
            (Some(mask_tsx(&text[open..]).masked), open)
        };
        let view = relexed.as_deref().unwrap_or(&masked);
        let Some(close) = matching(view, open - base).map(|close| close + base) else {
            findings.push(format!(
                "{file}:{line}: `{}={{` has no closing brace this rule can find",
                handler.as_str()
            ));
            continue;
        };
        let code_masked = collapse(&view[open + 1 - base..close - base]);
        if is_one_dispatch(&code_masked) {
            continue;
        }
        if let Some(inert) = inert.captures(&code_masked)
            && inert[1] == inert[2]
        {
            continue;
        }
        sites.push((
            Key {
                file: file.to_string(),
                handler: handler.as_str().to_string(),
                code: collapse(&text[open + 1..close]),
            },
            line,
        ));
    }
    sites
}

/// Whether a handler's masked, collapsed code is exactly one registry
/// dispatch: `VOICE_ACTIONS.<id>.run(…)` (or `VOICE_ACTIONS[…].run(…)`) or
/// `dispatchVoiceAction(…)`, optionally behind an arrow, one pair of braces,
/// `return` and `void`. Anything after the call's closing parenthesis makes it
/// a site, because that part is not what the registry entry does.
fn is_one_dispatch(code: &str) -> bool {
    let arrow =
        Regex::new(r"^(?:async\s+)?(?:\([^()]*\)|[A-Za-z_$][\w$]*)\s*=>\s*").expect("static regex");
    let callee = Regex::new(
        r"^(?:VOICE_ACTIONS(?:\.[A-Za-z_$][\w$]*|\[[^\]]*\])+\.run|dispatchVoiceAction)\s*\(",
    )
    .expect("static regex");
    let mut rest = code.trim();
    if let Some(found) = arrow.find(rest) {
        rest = rest[found.end()..].trim();
    }
    if let Some(inner) = rest
        .strip_prefix('{')
        .and_then(|inner| inner.strip_suffix('}'))
    {
        rest = inner.trim();
    }
    rest = rest.strip_suffix(';').unwrap_or(rest).trim();
    for prefix in ["return ", "void "] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest).trim_start();
    }
    let Some(call) = callee.find(rest) else {
        return false;
    };
    matching(rest, call.end() - 1).is_some_and(|close| rest[close + 1..].trim().is_empty())
}

fn collapse(code: &str) -> String {
    code.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The baseline, keyed by site, each value the 1-indexed lines its entries
/// start on — one per listed site. Every malformation is a finding.
fn baseline_entries(source: &str, findings: &mut Vec<String>) -> BTreeMap<Key, Vec<usize>> {
    let mut entries: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
    let document = match source.parse::<toml_edit::DocumentMut>() {
        Ok(document) => document,
        Err(error) => {
            findings.push(format!("{BASELINE}: is not valid TOML: {error}"));
            return entries;
        }
    };
    // An explicit `site = []` is the list's end state: every control
    // dispatches through the registry. An absent key is not, because a file
    // that lost its entries by accident would otherwise read the same way.
    if document
        .get("site")
        .and_then(toml_edit::Item::as_array)
        .is_some_and(toml_edit::Array::is_empty)
    {
        return entries;
    }
    let Some(sites) = document
        .get("site")
        .and_then(toml_edit::Item::as_array_of_tables)
    else {
        findings.push(format!(
            "{BASELINE}: holds no `[[site]]` array of tables. If every control now dispatches through the \
             registry, write `site = []` rather than deleting the entries' key"
        ));
        return entries;
    };
    // `toml_edit` keeps no line numbers on a parsed table, so the n-th `[[site]]`
    // header in the text is the n-th table.
    let headers: Vec<usize> = source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "[[site]]")
        .map(|(index, _)| index + 1)
        .collect();
    for (index, table) in sites.iter().enumerate() {
        let line = headers.get(index).copied().unwrap_or(0);
        let field = |name: &str| table.get(name).and_then(toml_edit::Item::as_str);
        for (name, _) in table.iter() {
            if !matches!(name, "file" | "handler" | "code" | "reason") {
                findings.push(format!(
                    "{BASELINE}:{line}: unknown key `{name}` — an entry is `file`, `handler`, `code` and `reason`"
                ));
            }
        }
        let (Some(file), Some(handler), Some(code)) =
            (field("file"), field("handler"), field("code"))
        else {
            findings.push(format!(
                "{BASELINE}:{line}: an entry needs string `file`, `handler` and `code` fields"
            ));
            continue;
        };
        if !HANDLERS.contains(&handler) {
            findings.push(format!(
                "{BASELINE}:{line}: `{handler}` is not a handler this rule scans ({})",
                HANDLERS.join(", ")
            ));
        }
        match field("reason").map(str::trim) {
            Some(reason) if !reason.is_empty() && !reason.starts_with('<') => {}
            _ => findings.push(format!(
                "{BASELINE}:{line}: the `{handler}` entry for {file} has no reason. The reason is read by a person, \
                 so it has to say what the control does and why it is not a voice capability"
            )),
        }
        entries
            .entry(Key {
                file: file.to_string(),
                handler: handler.to_string(),
                code: code.to_string(),
            })
            .or_default()
            .push(line);
    }
    entries
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

    fn checked_in() -> Sources {
        let root = repo_root();
        let mut production = BTreeMap::new();
        let mut findings = Vec::new();
        walk(
            &root,
            &root.join(DESKTOP_SRC),
            "test",
            &mut production,
            &mut findings,
        );
        assert!(findings.is_empty(), "{findings:#?}");
        let baseline = std::fs::read_to_string(root.join(BASELINE))
            .unwrap_or_else(|error| panic!("could not read {BASELINE}: {error}"));
        Sources {
            production,
            baseline,
        }
    }

    /// The file a plant edits: one the tree is sure to keep.
    const PLANTED_FILE: &str = "desktop/src/components/ConfirmDialog.tsx";

    fn planted(edit: impl Fn(&mut Sources)) -> Vec<String> {
        let mut sources = checked_in();
        edit(&mut sources);
        check(&sources)
    }

    /// Append a component holding `jsx` to [`PLANTED_FILE`].
    fn plant_jsx(sources: &mut Sources, jsx: &str) {
        let text = sources
            .production
            .get_mut(PLANTED_FILE)
            .unwrap_or_else(|| panic!("{PLANTED_FILE} is not in the tree"));
        text.push_str(&format!(
            "\nexport function Planted() {{\n  return {jsx};\n}}\n"
        ));
    }

    fn assert_reports(findings: &[String], needle: &str) {
        assert!(
            findings.iter().any(|finding| finding.contains(needle)),
            "no finding mentioned {needle:?}:\n{}",
            findings.join("\n")
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.contains("DOES NOT SEE")),
            "a finding did not carry the rule's own residual:\n{}",
            findings.join("\n")
        );
    }

    /// Scenario: scan every production `.tsx` under `desktop/src` against the
    /// checked-in baseline. Every control site names the registry, only stops
    /// its event, or is listed with a reason, and no listed entry is stale.
    #[test]
    fn the_checked_in_controls_match_the_baseline() {
        let findings = check(&checked_in());
        assert!(findings.is_empty(), "{}", findings.join("\n\n"));
    }

    /// And the scan is not vacuous: it found real sites, both listed and
    /// registry-routed, in more than one file.
    #[test]
    fn the_scan_reads_real_control_sites() {
        let sources = checked_in();
        let mut findings = Vec::new();
        let listed = baseline_entries(&sources.baseline, &mut findings);
        assert!(findings.is_empty(), "{findings:#?}");
        assert!(listed.len() >= 50, "only {} baseline keys", listed.len());
        let files: std::collections::BTreeSet<&str> =
            listed.keys().map(|key| key.file.as_str()).collect();
        assert!(files.len() >= 10, "only {} files listed", files.len());
        let app = sources
            .production
            .get("desktop/src/App.tsx")
            .expect("App.tsx");
        assert!(
            app.contains("onClick={() => VOICE_ACTIONS."),
            "App.tsx no longer holds a registry-routed onClick; this test needs another witness"
        );
    }

    /// Scenario: plant a new button wired with a bare `onClick` into a desktop
    /// component. The ratchet reports it, names its file and handler, and
    /// prints the stanza that would list it.
    #[test]
    fn a_planted_bare_on_click_control_is_caught() {
        let findings = planted(|sources| {
            plant_jsx(
                sources,
                "<button onClick={() => restartTheWorld()}>Restart</button>",
            )
        });
        assert_reports(&findings, PLANTED_FILE);
        assert_reports(&findings, "a `onClick` control that does not dispatch");
        assert_reports(&findings, "code = \"() => restartTheWorld()\"");
    }

    /// The siblings count too, so a capability cannot dodge the ratchet by
    /// being wired to `onMouseDown` or `onChange` instead.
    #[test]
    fn a_planted_control_on_a_sibling_handler_is_caught() {
        for handler in ["onMouseDown", "onChange", "onSubmit", "onPointerDown"] {
            let findings = planted(|sources| {
                plant_jsx(
                    sources,
                    &format!("<div {handler}={{() => restartTheWorld()}} />"),
                )
            });
            assert_reports(&findings, &format!("a `{handler}` control"));
        }
    }

    /// A second copy of a listed site is a new site: the baseline allows as
    /// many as it lists and no more.
    #[test]
    fn a_duplicate_of_a_listed_site_is_caught() {
        let sources = checked_in();
        let mut findings = Vec::new();
        let listed = baseline_entries(&sources.baseline, &mut findings);
        let (key, _) = listed
            .iter()
            .find(|(key, lines)| lines.len() == 1 && key.handler == "onClick")
            .expect("a singly listed onClick entry");
        let key = key.clone();
        let findings = planted(|sources| {
            let text = sources.production.get_mut(&key.file).expect("listed file");
            text.push_str(&format!(
                "\nexport function Planted() {{\n  return <button onClick={{{}}}>Again</button>;\n}}\n",
                key.code
            ));
        });
        assert_reports(&findings, "2 such site(s) with this code, 1 listed");
    }

    /// A control routed through the registry needs no entry, and neither does
    /// an inert handler that only stops an event's propagation.
    #[test]
    fn registry_routed_and_inert_handlers_are_not_sites() {
        let findings = planted(|sources| {
            plant_jsx(
                sources,
                "<><button onClick={() => VOICE_ACTIONS.openSettings.run(context)}>Settings</button>\
                 <div onMouseDown={(event) => event.stopPropagation()} />\
                 <form onSubmit={(e) => { e.preventDefault(); }} /></>",
            )
        });
        assert!(findings.is_empty(), "{}", findings.join("\n\n"));
    }

    /// An inert-looking handler that stops a DIFFERENT object's event is not
    /// inert, and a comment naming the registry does not route anything.
    #[test]
    fn near_misses_of_the_exemptions_are_still_sites() {
        let other_object = planted(|sources| {
            plant_jsx(
                sources,
                "<div onMouseDown={(event) => other.stopPropagation()} />",
            )
        });
        assert_reports(&other_object, "a `onMouseDown` control");

        let comment = planted(|sources| {
            plant_jsx(
                sources,
                "<button onClick={() => /* VOICE_ACTIONS */ restartTheWorld()}>Go</button>",
            )
        });
        assert_reports(&comment, "a `onClick` control");
    }

    /// Scenario: plant a button whose handler dispatches a registry entry and
    /// then does something the entry does not. It is a site: the second half
    /// is not reached by voice, so naming `VOICE_ACTIONS` does not exempt it.
    #[test]
    fn a_handler_that_dispatches_and_does_more_is_caught() {
        let findings = planted(|sources| {
            plant_jsx(
                sources,
                "<button onClick={() => { VOICE_ACTIONS.openSettings.run(context); restartTheWorld(); }}>Go</button>",
            )
        });
        assert_reports(&findings, "restartTheWorld()");

        let chained = planted(|sources| {
            plant_jsx(
                sources,
                "<button onClick={() => VOICE_ACTIONS.openSettings.run(context) && restartTheWorld()}>Go</button>",
            )
        });
        assert_reports(&chained, "restartTheWorld()");
    }

    /// The key is the WHOLE handler: a listed long handler replaced by one that
    /// shares its first stretch and differs at the end is a new site, not the
    /// old one under its old reason.
    #[test]
    fn a_long_handler_changed_after_its_prefix_is_a_new_site() {
        let prefix = "() => { first(); second(); third(); fourth(); fifth(); sixth(); seventh(); eighth(); ninth(); tenth(); eleventh(); twelfth();";
        let listed = format!("{prefix} original(); }}");
        let changed = format!("{prefix} replacement(); }}");
        let entry = format!(
            "\n[[site]]\nfile = \"{PLANTED_FILE}\"\nhandler = \"onClick\"\ncode = \"{listed}\"\nreason = \"r\"\n"
        );
        let same = planted(|sources| {
            plant_jsx(
                sources,
                &format!("<button onClick={{{listed}}}>Go</button>"),
            );
            sources.baseline.push_str(&entry);
        });
        assert!(same.is_empty(), "{}", same.join("\n\n"));

        let replaced = planted(|sources| {
            plant_jsx(
                sources,
                &format!("<button onClick={{{changed}}}>Go</button>"),
            );
            sources.baseline.push_str(&entry);
        });
        assert_reports(&replaced, "replacement()");
        assert_reports(&replaced, "the tree holds 0");
    }

    /// Scenario: a button written inside JSX text holding two apostrophes, which
    /// the TSX lexer pairs up as a string. The scan still finds its handler.
    /// A handler inside a comment is not a control.
    #[test]
    fn a_handler_hidden_between_two_apostrophes_is_caught_and_a_commented_one_is_not() {
        let hidden = planted(|sources| {
            plant_jsx(
                sources,
                "<p>Don't <button onClick={restartTheWorld}>Restart</button> if you're busy.</p>",
            )
        });
        assert_reports(&hidden, "code = \"restartTheWorld\"");

        let commented = planted(|sources| {
            plant_jsx(
                sources,
                "<div>{/* <button onClick={restartTheWorld}>Restart</button> */}</div>",
            )
        });
        assert!(commented.is_empty(), "{}", commented.join("\n\n"));
    }

    /// The list's end state passes: every control routed, and `site = []`.
    #[test]
    fn a_fully_routed_tree_with_an_empty_baseline_passes() {
        let mut production = BTreeMap::new();
        production.insert(
            PLANTED_FILE.to_string(),
            "export function Planted() {\n  return <button onClick={() => VOICE_ACTIONS.openSettings.run(context)}>Go</button>;\n}\n"
                .to_string(),
        );
        let findings = check(&Sources {
            production,
            baseline: "site = []\n".to_string(),
        });
        assert!(findings.is_empty(), "{}", findings.join("\n\n"));
    }

    /// The list only shrinks: an entry that matches nothing is a finding.
    #[test]
    fn a_stale_baseline_entry_is_caught() {
        let findings = planted(|sources| {
            sources.baseline.push_str(
                "\n[[site]]\nfile = \"desktop/src/components/ConfirmDialog.tsx\"\nhandler = \"onClick\"\n\
                 code = \"() => removedLongAgo()\"\nreason = \"was a button\"\n",
            );
        });
        assert_reports(&findings, "the tree holds 0");
    }

    /// Every malformed entry is a finding: no reason, a placeholder reason, an
    /// unknown handler, a misspelt key.
    #[test]
    fn a_malformed_baseline_entry_is_caught() {
        let entry = |reason: &str, handler: &str, extra: &str| {
            planted(move |sources| {
                plant_jsx(
                    sources,
                    "<button onClick={() => restartTheWorld()}>Go</button>",
                );
                sources.baseline.push_str(&format!(
                    "\n[[site]]\nfile = \"{PLANTED_FILE}\"\nhandler = \"{handler}\"\n\
                     code = \"() => restartTheWorld()\"\n{reason}{extra}\n"
                ));
            })
        };
        assert_reports(&entry("", "onClick", ""), "has no reason");
        assert_reports(&entry("reason = \"   \"\n", "onClick", ""), "has no reason");
        assert_reports(
            &entry(
                "reason = \"<what this control does, and why it is not a voice capability>\"\n",
                "onClick",
                "",
            ),
            "has no reason",
        );
        assert_reports(
            &entry("reason = \"r\"\n", "onKeyDown", ""),
            "`onKeyDown` is not a handler this rule scans",
        );
        assert_reports(
            &entry("reason = \"r\"\n", "onClick", "reson = \"typo\"\n"),
            "unknown key `reson`",
        );
    }

    /// An input the rule cannot read is a finding, never a skip.
    #[test]
    fn an_unreadable_input_is_a_finding_rather_than_a_skip() {
        let bad_toml = planted(|sources| sources.baseline = "[[site]\n".to_string());
        assert_reports(&bad_toml, "is not valid TOML");

        let no_sites = planted(|sources| sources.baseline = "# nothing\n".to_string());
        assert_reports(&no_sites, "holds no `[[site]]`");

        let no_tsx =
            planted(|sources| sources.production.retain(|file, _| !file.ends_with(".tsx")));
        assert_reports(&no_tsx, "no handler attribute was found");

        let unlexable = planted(|sources| {
            sources
                .production
                .insert(PLANTED_FILE.to_string(), "export const x = (\n".to_string());
        });
        assert_reports(&unlexable, "cannot be read as TSX");

        let tree = tempfile::tempdir().expect("a temp dir");
        std::fs::create_dir_all(tree.path().join(DESKTOP_SRC)).expect("src");
        let missing = run(tree.path());
        assert_reports(&missing, &format!("could not read {BASELINE}"));
    }

    /// The printed stanza is what the baseline parses back to the same key, so
    /// pasting it (with a reason) is enough to list the site.
    #[test]
    fn the_printed_stanza_round_trips() {
        let key = Key {
            file: PLANTED_FILE.to_string(),
            handler: "onClick".to_string(),
            code: r#"() => setSort("name\\") /* "quoted" */"#.to_string(),
        };
        let text = stanza(&key).replace(
            "<what this control does, and why it is not a voice capability>",
            "a reason",
        );
        let mut findings = Vec::new();
        let parsed = baseline_entries(&text, &mut findings);
        assert!(findings.is_empty(), "{findings:#?}");
        assert_eq!(parsed.keys().collect::<Vec<_>>(), vec![&key]);
    }
}
