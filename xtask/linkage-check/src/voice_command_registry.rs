//! PRD #802 M3 — rule 14: the voice command table and the frontend action
//! registry must resolve against each other.
//!
//! # What it asserts
//!
//! Five things, each a cross-file consistency claim that no compiler makes,
//! because one side is TOML, one is TypeScript and the rest are read out of
//! three further files:
//!
//! 1. every `invoke` in `commands.toml` names a key in `VOICE_ACTIONS`;
//! 2. every `screens` entry is a known screen — the `kind` literals of
//!    `DeckView` in `desktop/src/types.ts`, so the table and the app's own type
//!    stay in step without a second list;
//! 3. every param `kind` is in the closed resolver set, read off `ParamKind` in
//!    `voice/table.rs` for the same reason;
//! 4. every `VOICE_ACTIONS` entry is **classified** — `voice: true` when a row
//!    invokes it, a non-empty `no_voice` reason when none does, never both and
//!    never neither;
//! 5. the registry literal is **statically readable** — no computed key and no
//!    spread among its entries, the refusal
//!    [`crate::desktop_settings_secrets`] already applies to a spread in
//!    `normalizeDesktopSettings`, and for the same reason: a scan of an object
//!    literal is only sound while the literal's keys can be read from the text.
//!
//! # What it does NOT see, and this is the half that gets assumed wrong
//!
//! **The rule proves every registry ENTRY is classified. It does not prove the
//! registry is COMPLETE.** A control wired with a bare `onClick` that never
//! reaches `VOICE_ACTIONS` is invisible to it — 80 `onClick=` sites in non-test
//! `.tsx` under `desktop/src` when PRD #802 was written, against 7 static
//! palette entries and 7 rail buttons. Most of the sampled ones are chrome, and
//! the rule could not tell a capability from a close button anyway, so the
//! residue is not claimed to be harmless.
//!
//! What makes the narrower claim worth having is that the registry is
//! **load-bearing at runtime**: the rail, the palette, the agent tile's
//! open/close pair and the overview's row all dispatch through it, so an entry
//! cannot be deleted without breaking a control. That is the property that earns
//! the definition — not the rule.
//!
//! The one mechanical approximation to "did you forget a capability" is pinning
//! a count of interactive controls, which PRD #802's Open Question 3 recommends
//! against for this PR with its churn cost named. [`VOICE_REGISTRY_RULE`] says
//! all of this at the failure itself, because the PRD's own Risks section is
//! explicit that reading the guard as stronger is the risk.
//!
//! # Budget
//!
//! The same as its neighbours: it reads five files and does nothing else — no
//! network, no git, no subprocess, no sleep. Every input it cannot read, parse
//! or find is a **finding**, never a skip: a required check that inspects
//! nothing and passes reports safety it never established.
//!
//! # Why a text scan of TypeScript
//!
//! It is the established idiom here rather than a compromise.
//! `desktop_settings_secrets.rs` already scans `desktop/src/lib/bridge.ts` for a
//! declared interface's fields and for `localStorage` key expressions, from
//! Rust, in a required gate. The alternative — a typed Rust enum the compiler
//! checks — would mean editing the table, a variant and a match arm for one new
//! command, and the last two are implementation. "A new command changes no
//! implementation" is PRD #802's whole design; M8 measured what one does cost
//! (seven files, five test-only) and this rule's own planted-bad-input tests are
//! one of the seven, because a plant naming a specific unspoken entry has to move
//! when that entry starts speaking.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::Path;

/// The voice command table.
pub const COMMANDS_TOML: &str = "desktop/src-tauri/src/voice/commands.toml";
/// The frontend action registry `invoke` dispatches through.
pub const REGISTRY_TS: &str = "desktop/src/lib/voiceActions.ts";
/// Where `DeckView`'s `kind` literals — the closed screen set — are declared.
pub const DECK_VIEW_TS: &str = "desktop/src/types.ts";
/// Where `ParamKind` — the closed resolver set — is declared.
pub const PARAM_KIND_RS: &str = "desktop/src-tauri/src/voice/table.rs";
/// Where the only spoken phrases the model is never asked about live.
///
/// **It used to be the voice surface** (`VoiceControlPanel.tsx`), because the
/// dictation MODE matched its exit phrases in the webview. The mode is gone and
/// the two lists that replaced it sit ahead of the resolver in Rust instead, so
/// the assertion moved with them rather than being deleted — which is the whole
/// point of it existing.
pub const DICTATION_RS: &str = "desktop/src-tauri/src/voice/dictation.rs";

/// The row whose `description` has to name every phrase [`OPENER_LIST`] matches
/// locally, and the row for [`SUBMIT_LIST`].
const DICTATE_ROW: &str = "dictate_to_agent";
const SUBMIT_ROW: &str = "submit_prompt";
/// The fast path's dictation openers.
const OPENER_LIST: &str = "DICTATION_OPENERS";
/// The whole-utterance phrases that press Enter.
const SUBMIT_LIST: &str = "SUBMIT_PHRASES";

/// The rule sentence, quoted in every failure.
///
/// It carries the residual as well as the rule, because the residual is what a
/// reader infers wrongly: a green check here says the registry is internally
/// consistent, not that the app's capabilities are all in it.
pub const VOICE_REGISTRY_RULE: &str = "PRD #802 rule 14: `desktop/src-tauri/src/voice/commands.toml` and `VOICE_ACTIONS` \
     (`desktop/src/lib/voiceActions.ts`) must resolve against each other, and every registry entry must carry either \
     `voice: true` or a written `no_voice` reason, and the two locally-matched phrase lists in \
     `desktop/src-tauri/src/voice/dictation.rs` must stay disjoint and covered by their own rows' \
     descriptions. WHAT THIS RULE DOES NOT SEE: a control wired with a bare `onClick` \
     that never reaches the registry — 80 such sites in non-test `.tsx` when this was written — so a green rule 14 is \
     NOT evidence that no capability was forgotten";

/// The texts the rule reads, so the assertions can be driven from planted input
/// as well as from the tree.
///
/// (It was *four*, and the count is deliberately no longer in this sentence —
/// see the note in `voiceActions.ts` about numbers in comments.)
pub struct Sources {
    pub commands_toml: String,
    pub registry_ts: String,
    pub deck_view_ts: String,
    pub param_kind_rs: String,
    pub dictation_rs: String,
}

/// Read every file and check them. Every read failure is a finding.
pub fn run(root: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    let read = |rel: &'static str, missing: &mut Vec<String>| match std::fs::read_to_string(
        root.join(rel),
    ) {
        Ok(text) => text,
        Err(error) => {
            missing.push(format!(
                "could not read {rel}: {error} — rule 14 cannot see a file it cannot read, so this is a failure \
                 rather than a skip. If the file moved, move this rule with it"
            ));
            String::new()
        }
    };
    let sources = Sources {
        commands_toml: read(COMMANDS_TOML, &mut missing),
        registry_ts: read(REGISTRY_TS, &mut missing),
        deck_view_ts: read(DECK_VIEW_TS, &mut missing),
        param_kind_rs: read(PARAM_KIND_RS, &mut missing),
        dictation_rs: read(DICTATION_RS, &mut missing),
    };
    if !missing.is_empty() {
        return missing.into_iter().map(annotate).collect();
    }
    check(&sources)
}

/// Every finding carries the rule sentence, so a reader of the summary is never
/// left to infer what was violated — or to over-read what was proved.
fn annotate(finding: String) -> String {
    format!("{finding} — {VOICE_REGISTRY_RULE}")
}

/// The whole rule, over texts rather than paths.
pub fn check(sources: &Sources) -> Vec<String> {
    let mut findings = Vec::new();
    let registry = registry_entries(&sources.registry_ts, &mut findings);
    let screens = deck_view_kinds(&sources.deck_view_ts, &mut findings);
    let kinds = param_kinds(&sources.param_kind_rs, &mut findings);
    let rows = command_rows(&sources.commands_toml, &mut findings);

    // Assertion 1: every `invoke` names a registry entry.
    let mut invoked: BTreeSet<&str> = BTreeSet::new();
    for row in &rows {
        if !registry.contains_key(&row.invoke) {
            findings.push(format!(
                "{COMMANDS_TOML}: row `{}` invokes `{}`, which is not a key of VOICE_ACTIONS. The known keys are: {}",
                row.id,
                row.invoke,
                joined(registry.keys().map(String::as_str)),
            ));
        }
        invoked.insert(&row.invoke);

        // Assertion 2: every `screens` entry is a known screen.
        for screen in &row.screens {
            if !screens.contains(screen) {
                findings.push(format!(
                    "{COMMANDS_TOML}: row `{}` names the screen `{screen}`, which is not a `kind` of `DeckView` in \
                     {DECK_VIEW_TS}. The known screens are: {}",
                    row.id,
                    joined(screens.iter().map(String::as_str)),
                ));
            }
        }

        // Assertion 3: every param `kind` is in the closed resolver set.
        for (param, kind) in &row.params {
            if !kinds.contains(kind) {
                findings.push(format!(
                    "{COMMANDS_TOML}: row `{}` declares param `{param}` of kind `{kind}`, which is not a `ParamKind` \
                     in {PARAM_KIND_RS}. The known kinds are: {}",
                    row.id,
                    joined(kinds.iter().map(String::as_str)),
                ));
            }
        }
    }

    // Assertion 4: every registry entry is classified, and the classification
    // agrees with the table. A `voice: true` entry no row invokes is a claim
    // the table does not back; a `no_voice` entry a row DOES invoke is a
    // reason that has been overtaken and is now lying to the next reader.
    for (action, entry) in &registry {
        let invoked_here = invoked.contains(action.as_str());
        match (entry.voice, entry.no_voice.as_deref()) {
            (true, Some(_)) => findings.push(format!(
                "{REGISTRY_TS}:{}: `{action}` carries both `voice: true` and a `no_voice` reason; an entry is one or \
                 the other",
                entry.line
            )),
            (false, None) => findings.push(format!(
                "{REGISTRY_TS}:{}: `{action}` is unclassified — it needs `voice: true` (and a `commands.toml` row \
                 invoking it) or a written `no_voice` reason saying what makes it a poor fit for a spoken command",
                entry.line
            )),
            (false, Some(reason)) if reason.trim().is_empty() => findings.push(format!(
                "{REGISTRY_TS}:{}: `{action}` has an empty `no_voice` reason. The reason is read by a person, so it \
                 has to say something",
                entry.line
            )),
            (true, None) if !invoked_here => findings.push(format!(
                "{REGISTRY_TS}:{}: `{action}` claims `voice: true` and no {COMMANDS_TOML} row invokes it. Add the row \
                 or give the entry a `no_voice` reason",
                entry.line
            )),
            (false, Some(reason)) if invoked_here => findings.push(format!(
                "{REGISTRY_TS}:{}: `{action}` carries a `no_voice` reason ({reason:?}) and a {COMMANDS_TOML} row \
                 invokes it anyway. The row wins — flip the entry to `voice: true`",
                entry.line
            )),
            _ => {}
        }
    }

    // Assertions 5 and 6 (PRD #802 D6, rebuilt): the two phrase lists matched
    // ahead of the resolver.
    //
    // **These are the only spoken words in the product the model is never asked
    // about**, and they exist for a stated reason: an utterance that opens with
    // `type` is a dictation whatever a model would have said about it, and a
    // whole-utterance `send it` presses Enter without spending a round trip on
    // the question. The trade is a second place where wording lives, and the
    // whole point of this rule is that a second place does not get to drift.
    phrase_lists(sources, &rows, &mut findings);

    findings.into_iter().map(annotate).collect()
}

/// Assertions 5 and 6, over the phrase lists matched ahead of the resolver.
///
/// **5 — every phrase matched locally is one the model was also told about.** A
/// phrase in a list and not in its row's `description` is the worst kind of
/// drift here: the fast path answers it and the model would not, so the same
/// words work one way through one path and are a no-match through the other,
/// and nothing at run time would say so. It matters in both directions now that
/// the fast path is only an OPTIMISATION rather than the vocabulary — the
/// fallback has to be able to reach the same row for the same words.
///
/// **6 — the two lists are disjoint.** The submit phrases are checked FIRST, so
/// a phrase in both would silently make that order load-bearing: an utterance
/// that was both an opener and a submit phrase would submit, and the list it
/// was added to second would look like it had no effect.
///
/// The check runs one way only. A phrasing in a row's `description` that no
/// list matches is not an inconsistency at all — that is precisely the
/// fallback's job, and the openers are deliberately a fraction of what the
/// model will accept. Stated here because the old version of this rule ran the
/// same direction for a different reason and a reader would otherwise infer the
/// stronger property from a green check.
fn phrase_lists(sources: &Sources, rows: &[Row], findings: &mut Vec<String>) {
    let text: Vec<char> = sources.dictation_rs.chars().collect();
    let masked = mask(&text);
    let list = |name: &str, findings: &mut Vec<String>| match array_body(&masked, name) {
        Some(body) => {
            let found = string_literals(&text, &masked, body);
            if found.is_empty() {
                findings.push(format!(
                    "{DICTATION_RS}: `{name}` yielded no phrases — rule 14 cannot compare a list it cannot read, so \
                     this is a failure rather than a skip"
                ));
            }
            found
        }
        None => {
            findings.push(format!(
                "{DICTATION_RS}: holds no `{name} = [ … ]` array literal. If the list moved, move this assertion with \
                 it rather than deleting it — it is the only thing keeping the locally-matched phrases and the rows' \
                 descriptions in step"
            ));
            BTreeSet::new()
        }
    };
    let openers = list(OPENER_LIST, findings);
    let submits = list(SUBMIT_LIST, findings);

    // Assertion 5, once per list against its own row.
    for (list_name, phrases, row_id) in [
        (OPENER_LIST, &openers, DICTATE_ROW),
        (SUBMIT_LIST, &submits, SUBMIT_ROW),
    ] {
        match rows.iter().find(|row| row.id == row_id) {
            Some(row) => {
                // Whitespace-collapsed before the substring test, because a
                // `"""…"""` description keeps its newlines: a phrase that
                // happened to straddle a line break would fail this check while
                // reading perfectly to the model, so a reflow of the prose would
                // go red for no reason anyone could act on.
                let description = row
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                for phrase in phrases {
                    if !description.contains(&phrase.to_lowercase()) {
                        findings.push(format!(
                            "{DICTATION_RS}: `{list_name}` matches {phrase:?}, which the `{row_id}` row's \
                             description in {COMMANDS_TOML} does not name. The model is never told about it, so \
                             those words work through the fast path and are a no-match through the model"
                        ));
                    }
                }
            }
            None => findings.push(format!(
                "{COMMANDS_TOML}: holds no `{row_id}` row, so the phrases {DICTATION_RS} matches locally back nothing"
            )),
        }
    }

    // Assertion 6.
    for phrase in openers.intersection(&submits) {
        findings.push(format!(
            "{DICTATION_RS}: {phrase:?} is in both `{OPENER_LIST}` and `{SUBMIT_LIST}`. The submit list is checked \
             first, so a shared phrase makes that precedence silently load-bearing"
        ));
    }
}

/// The body of the array literal that `name` is assigned, `[` excluded.
///
/// [`literal_body`]'s sibling for `[ … ]` rather than `{ … }`. Separate rather
/// than parameterised on the bracket, because the two are read by different
/// assertions and a shared helper taking a delimiter reads worse than two that
/// say which shape they are for.
fn array_body(masked: &[char], name: &str) -> Option<Range<usize>> {
    let at = find(masked, 0, name)?;
    let equals = masked[at..].iter().position(|c| *c == '=')? + at;
    let open = skip_space(masked, equals + 1);
    if open >= masked.len() || masked[open] != '[' {
        return None;
    }
    let mut depth = 0usize;
    for (offset, character) in masked[open..].iter().enumerate() {
        match character {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + 1..open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn joined<'a>(values: impl Iterator<Item = &'a str>) -> String {
    let all: Vec<&str> = values.collect();
    if all.is_empty() {
        "(none)".to_string()
    } else {
        all.join(", ")
    }
}

// -- the command table ------------------------------------------------------

/// One row, reduced to what this rule checks.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    id: String,
    invoke: String,
    screens: Vec<String>,
    /// `(name, kind)`, so a bad kind can be reported against the param that
    /// declared it rather than against the row alone.
    params: Vec<(String, String)>,
    /// The model-facing prompt, read by assertion 5 and nothing else. Kept even
    /// though it is prose, because the panel's local phrase list has to be a
    /// subset of what the model was told about.
    description: String,
}

/// Parse `commands.toml` with the same library `table.rs` parses it with.
///
/// Deliberately `toml_edit` and not a hand-scan: two readers of one file is two
/// answers to what the file says, and the lenient one always wins the argument
/// by accepting what the strict one rejected. `table.rs` is what ships, so this
/// rule reads the file the way that does.
fn command_rows(source: &str, findings: &mut Vec<String>) -> Vec<Row> {
    let document = match source.parse::<toml_edit::DocumentMut>() {
        Ok(document) => document,
        Err(error) => {
            findings.push(format!(
                "{COMMANDS_TOML}: is not valid TOML: {error} — rule 14 cannot see a table it cannot parse"
            ));
            return Vec::new();
        }
    };
    let Some(commands) = document
        .get("commands")
        .and_then(toml_edit::Item::as_array_of_tables)
    else {
        findings.push(format!(
            "{COMMANDS_TOML}: holds no `[[commands]]` array of tables — rule 14 would then pass by inspecting \
             nothing, which is worse than failing"
        ));
        return Vec::new();
    };

    let mut rows = Vec::new();
    for (index, table) in commands.iter().enumerate() {
        let position = format!("row {}", index + 1);
        let Some(id) = string_field(table, "id") else {
            findings.push(format!("{COMMANDS_TOML}: {position} has no string `id`"));
            continue;
        };
        let Some(invoke) = string_field(table, "invoke") else {
            findings.push(format!(
                "{COMMANDS_TOML}: row `{id}` has no string `invoke`, so nothing connects it to a registry entry"
            ));
            continue;
        };
        // Absent or empty `screens` means "everywhere" — `table.rs` says so at
        // the column — so an absent key is not a finding, while a key holding
        // something that is not an array of strings is.
        let mut screens = Vec::new();
        match table.get("screens") {
            None => {}
            Some(item) => match item.as_array() {
                Some(array) => {
                    for value in array.iter() {
                        match value.as_str() {
                            Some(screen) => screens.push(screen.to_string()),
                            None => findings.push(format!(
                                "{COMMANDS_TOML}: row `{id}` has a `screens` entry that is not a string: {value}"
                            )),
                        }
                    }
                }
                None => findings.push(format!(
                    "{COMMANDS_TOML}: row `{id}` has a `screens` that is not an array"
                )),
            },
        }
        let mut params = Vec::new();
        if let Some(item) = table.get("params") {
            match item.as_array_of_tables() {
                Some(declared) => {
                    for (param_index, param) in declared.iter().enumerate() {
                        let name = string_field(param, "name")
                            .unwrap_or_else(|| format!("#{}", param_index + 1));
                        match string_field(param, "kind") {
                            Some(kind) => params.push((name, kind)),
                            None => findings.push(format!(
                                "{COMMANDS_TOML}: row `{id}` param `{name}` has no string `kind`"
                            )),
                        }
                    }
                }
                None => findings.push(format!(
                    "{COMMANDS_TOML}: row `{id}` has a `params` that is not a `[[commands.params]]` array of tables"
                )),
            }
        }
        rows.push(Row {
            id,
            invoke,
            screens,
            params,
            // Absent is an empty string rather than a finding: `description` is
            // required by `table.rs`'s own parser, which fails the build first,
            // and duplicating that refusal here would report one malformation
            // twice.
            description: string_field(table, "description").unwrap_or_default(),
        });
    }
    rows
}

fn string_field(table: &toml_edit::Table, key: &str) -> Option<String> {
    table
        .get(key)
        .and_then(toml_edit::Item::as_str)
        .map(str::to_string)
}

// -- the registry -----------------------------------------------------------

/// One `VOICE_ACTIONS` entry, reduced to its classification.
#[derive(Debug, Default, PartialEq, Eq)]
struct Entry {
    voice: bool,
    no_voice: Option<String>,
    /// 1-indexed, for a clickable failure.
    line: usize,
}

/// The `VOICE_ACTIONS` object literal, read as text.
///
/// Sound only while the literal is statically readable, which is assertion 5 —
/// a spread or a computed key among the entries is reported here rather than
/// silently producing a shorter key set than the file has.
fn registry_entries(source: &str, findings: &mut Vec<String>) -> BTreeMap<String, Entry> {
    let text: Vec<char> = source.chars().collect();
    let masked = mask(&text);
    let Some(body) = literal_body(&masked, "VOICE_ACTIONS") else {
        findings.push(format!(
            "{REGISTRY_TS}: no `VOICE_ACTIONS = {{ … }}` object literal found — rule 14 would then have an empty key \
             set and pass by inspecting nothing"
        ));
        return BTreeMap::new();
    };

    let mut entries = BTreeMap::new();
    for property in properties(&text, &masked, body, REGISTRY_TS, findings) {
        let line = property.line;
        if entries.contains_key(&property.key) {
            findings.push(format!(
                "{REGISTRY_TS}:{line}: `{}` is declared twice; the second silently shadows the first",
                property.key
            ));
            continue;
        }
        let Some(inner) = literal_body_at(&masked, property.value.clone()) else {
            findings.push(format!(
                "{REGISTRY_TS}:{line}: `{}` is not an object literal, so its classification cannot be read",
                property.key
            ));
            continue;
        };
        let mut entry = Entry {
            line,
            ..Entry::default()
        };
        for field in properties(&text, &masked, inner, REGISTRY_TS, findings) {
            match field.key.as_str() {
                "voice" => {
                    let literal: String = text[field.value.clone()].iter().collect();
                    if literal.trim() == "true" {
                        entry.voice = true;
                    } else {
                        findings.push(format!(
                            "{REGISTRY_TS}:{}: `{}`'s `voice` is {:?}; the only classification this rule reads is the \
                             literal `true`",
                            field.line,
                            property.key,
                            literal.trim()
                        ));
                    }
                }
                "no_voice" => match string_value(&text, field.value.clone()) {
                    Some(reason) => entry.no_voice = Some(reason),
                    None => findings.push(format!(
                        "{REGISTRY_TS}:{}: `{}`'s `no_voice` is not a plain string literal, so the reason cannot be \
                         read. Write it as one string, or as string literals joined by `+`",
                        field.line, property.key
                    )),
                },
                _ => {}
            }
        }
        entries.insert(property.key, entry);
    }
    entries
}

// -- the two closed sets ----------------------------------------------------

/// The `kind` literals of the `DeckView` union in `types.ts`.
fn deck_view_kinds(source: &str, findings: &mut Vec<String>) -> BTreeSet<String> {
    let text: Vec<char> = source.chars().collect();
    let masked = mask(&text);
    let Some(start) = find(&masked, 0, "export type DeckView") else {
        findings.push(format!(
            "{DECK_VIEW_TS}: no `export type DeckView` declaration found — rule 14 cannot check a screen against a \
             set it could not read"
        ));
        return BTreeSet::new();
    };
    // The union runs to the first `;` outside any `{ … }` member.
    let mut depth = 0usize;
    let mut end = masked.len();
    for (offset, character) in masked[start..].iter().enumerate() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ';' if depth == 0 => {
                end = start + offset;
                break;
            }
            _ => {}
        }
    }

    let mut kinds = BTreeSet::new();
    let mut at = start;
    while let Some(found) = find(&masked, at, "kind") {
        if found >= end {
            break;
        }
        at = found + 4;
        // A whole word followed by a colon and a string literal, so `deckId`
        // and prose in a masked doc comment cannot contribute.
        if found > 0 && is_word(masked[found - 1]) {
            continue;
        }
        let after = skip_space(&masked, at);
        if masked.get(after) != Some(&':') {
            continue;
        }
        let value = skip_space(&masked, after + 1);
        if let Some(literal) = string_at(&text, &masked, value) {
            kinds.insert(literal);
        }
    }
    if kinds.is_empty() {
        findings.push(format!(
            "{DECK_VIEW_TS}: `DeckView` yielded no `kind` literals — rule 14 would then reject every screen, or \
             accept none, depending on nothing"
        ));
    }
    kinds
}

/// The closed resolver set, read off `ParamKind::as_str` in `table.rs`.
fn param_kinds(source: &str, findings: &mut Vec<String>) -> BTreeSet<String> {
    let text: Vec<char> = source.chars().collect();
    let masked = mask(&text);
    let kinds = block_after(&masked, 0, "impl ParamKind")
        .and_then(|block| block_after(&masked, block.start, "fn as_str").map(|body| (block, body)))
        .map(|(block, body)| {
            let bounded = body.start..body.end.min(block.end);
            string_literals(&text, &masked, bounded)
        })
        .unwrap_or_default();
    if kinds.is_empty() {
        findings.push(format!(
            "{PARAM_KIND_RS}: `impl ParamKind`'s `as_str` yielded no kind literals — rule 14 would then reject every \
             declared param kind, or accept none. If `ParamKind` moved, move this rule with it"
        ));
    }
    kinds
}

// -- the text scanner -------------------------------------------------------

/// One property of an object literal.
#[derive(Debug)]
struct Property {
    key: String,
    /// Char range of the value, within the source's char vector.
    value: Range<usize>,
    /// 1-indexed line of the key.
    line: usize,
}

/// Every top-level property of the object literal whose body is `body`.
///
/// **Assertion 5 lives here.** A spread and a computed key are each reported as
/// a finding rather than skipped, because both mean the key set this function
/// returns is smaller than the one the file declares — and a guard that quietly
/// returns less than it was asked about reports safety it never established.
/// Anything else it cannot read (a method shorthand, a shorthand property) is
/// reported for the same reason.
///
/// Values are split at the first `,` outside any `{}`, `[]` or `()`. A TypeScript
/// generic's comma is not tracked, which is sound here because a generic can
/// only appear in a type annotation and an object literal's property value is
/// never one.
fn properties(
    text: &[char],
    masked: &[char],
    body: Range<usize>,
    file: &str,
    findings: &mut Vec<String>,
) -> Vec<Property> {
    let mut found = Vec::new();
    let mut at = body.start;
    while at < body.end {
        at = skip_space(masked, at);
        if at >= body.end {
            break;
        }
        let line = line_of(masked, at);
        if masked[at..body.end.min(at + 3)] == ['.', '.', '.'] {
            findings.push(format!(
                "{file}:{line}: a spread inside the object literal. The keys of this literal are read as text, so a \
                 spread makes the set unreadable — write the entries out"
            ));
            at = next_entry(masked, at, body.end);
            continue;
        }
        if masked[at] == '[' {
            findings.push(format!(
                "{file}:{line}: a computed key inside the object literal. The keys are read as text, so a computed one \
                 cannot be read — write a plain key"
            ));
            at = next_entry(masked, at, body.end);
            continue;
        }
        let key_end = if masked[at] == '"' || masked[at] == '\'' {
            match closing_quote(masked, at) {
                Some(end) => end + 1,
                None => {
                    findings.push(format!("{file}:{line}: unterminated quoted key"));
                    break;
                }
            }
        } else {
            let mut end = at;
            while end < body.end && is_word(masked[end]) {
                end += 1;
            }
            end
        };
        if key_end == at {
            findings.push(format!(
                "{file}:{line}: this rule cannot read a key here, so it cannot say what the literal declares"
            ));
            at = next_entry(masked, at, body.end);
            continue;
        }
        let key: String = text[at..key_end]
            .iter()
            .collect::<String>()
            .trim_matches(['"', '\''])
            .to_string();
        let colon = skip_space(masked, key_end);
        if masked.get(colon) != Some(&':') {
            findings.push(format!(
                "{file}:{line}: `{key}` is not written as `key: value`. A method shorthand or a shorthand property is \
                 not read by this rule, so it is refused rather than skipped"
            ));
            at = next_entry(masked, at, body.end);
            continue;
        }
        let value_start = skip_space(masked, colon + 1);
        let value_end = after_value(masked, value_start, body.end);
        let value = value_start..trim_end(masked, value_start, value_end.min(body.end));
        found.push(Property { key, value, line });
        at = next_entry(masked, value_start, body.end);
    }
    found
}

/// Where the NEXT property starts: past the value beginning at `from` and past
/// its terminating comma.
///
/// Every refusal above ends here rather than at [`after_value`], and that is not
/// tidiness: `after_value` returns `from` itself when `from` already sits on a
/// comma, so a refused entry that left the cursor there would be re-read
/// forever. Measured — the first version of the spread test hung.
fn next_entry(masked: &[char], from: usize, limit: usize) -> usize {
    let end = after_value(masked, from, limit);
    if end < limit && masked[end] == ',' {
        end + 1
    } else {
        end.max(from + 1).min(limit)
    }
}

/// Where the value starting at `from` ends: the first `,` at depth zero, or
/// `limit`.
fn after_value(masked: &[char], from: usize, limit: usize) -> usize {
    let mut depth = 0usize;
    let mut at = from;
    while at < limit {
        match masked[at] {
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return at,
            _ => {}
        }
        at += 1;
    }
    limit
}

fn trim_end(masked: &[char], from: usize, mut to: usize) -> usize {
    while to > from && masked[to - 1].is_whitespace() {
        to -= 1;
    }
    to
}

/// The body of the object literal assigned to `name`, if that is what follows.
fn literal_body(masked: &[char], name: &str) -> Option<Range<usize>> {
    let at = find(masked, 0, name)?;
    let equals = masked[at..].iter().position(|c| *c == '=')? + at;
    literal_body_at(masked, equals + 1..masked.len())
}

/// The body of the object literal that `range` starts with.
fn literal_body_at(masked: &[char], range: Range<usize>) -> Option<Range<usize>> {
    let open = skip_space(masked, range.start);
    if open >= range.end || masked[open] != '{' {
        return None;
    }
    let mut depth = 0usize;
    for (offset, character) in masked[open..range.end].iter().enumerate() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + 1..open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// The balanced `{ … }` body that follows `header`, searching from `from`.
fn block_after(masked: &[char], from: usize, header: &str) -> Option<Range<usize>> {
    let at = find(masked, from, header)?;
    literal_body_at(
        masked,
        masked[at..].iter().position(|c| *c == '{')? + at..masked.len(),
    )
}

/// Every double-quoted literal inside `range`.
fn string_literals(text: &[char], masked: &[char], range: Range<usize>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut at = range.start;
    while at < range.end {
        if masked[at] == '"'
            && let Some(literal) = string_at(text, masked, at)
        {
            found.insert(literal);
            at = closing_quote(masked, at).map_or(range.end, |end| end + 1);
            continue;
        }
        at += 1;
    }
    found
}

/// The value of a string literal starting at `at`, unescaped.
fn string_at(text: &[char], masked: &[char], at: usize) -> Option<String> {
    if !matches!(masked.get(at), Some('"' | '\'' | '`')) {
        return None;
    }
    let end = closing_quote(masked, at)?;
    Some(unescape(&text[at + 1..end]))
}

/// A value that is one string literal, or several joined by `+`.
///
/// Anything else — an identifier, a template literal with an interpolation, a
/// call — comes back as `None`, which the caller reports. A reason this rule
/// cannot read is a reason it cannot prove is non-empty.
fn string_value(text: &[char], range: Range<usize>) -> Option<String> {
    let masked = mask(text);
    let mut out = String::new();
    let mut at = skip_space(&masked, range.start);
    loop {
        if at >= range.end {
            return None;
        }
        let end = closing_quote(&masked, at)?;
        if end >= range.end {
            return None;
        }
        let piece: String = text[at..=end].iter().collect();
        if piece.contains("${") {
            return None;
        }
        out.push_str(&unescape(&text[at + 1..end]));
        at = skip_space(&masked, end + 1);
        if at >= range.end {
            return Some(out);
        }
        if masked[at] != '+' {
            return None;
        }
        at = skip_space(&masked, at + 1);
    }
}

fn closing_quote(masked: &[char], at: usize) -> Option<usize> {
    let quote = *masked.get(at)?;
    if !matches!(quote, '"' | '\'' | '`') {
        return None;
    }
    masked[at + 1..]
        .iter()
        .position(|c| *c == quote)
        .map(|offset| at + 1 + offset)
}

fn unescape(raw: &[char]) -> String {
    let mut out = String::new();
    let mut escaped = false;
    for character in raw {
        if escaped {
            out.push(match character {
                'n' => '\n',
                't' => '\t',
                other => *other,
            });
            escaped = false;
        } else if *character == '\\' {
            escaped = true;
        } else {
            out.push(*character);
        }
    }
    out
}

fn find(masked: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    if needle.is_empty() || from >= masked.len() {
        return None;
    }
    masked[from..]
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
        .map(|offset| from + offset)
}

fn skip_space(masked: &[char], mut at: usize) -> usize {
    while at < masked.len() && masked[at].is_whitespace() {
        at += 1;
    }
    at
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

fn line_of(masked: &[char], at: usize) -> usize {
    masked[..at.min(masked.len())]
        .iter()
        .filter(|c| **c == '\n')
        .count()
        + 1
}

/// Blank the interiors of comments and string literals, preserving length.
///
/// Character-for-character rather than byte-for-byte, so a multi-byte character
/// inside a comment or a reason cannot move a span. Quote delimiters survive —
/// a scan still has to be able to SEE where a literal starts — while everything
/// between them becomes a space, so a `,` or a `{` inside prose can never be
/// read as structure. Newlines survive so line numbers stay true.
///
/// The same reasoning as `desktop_settings_secrets::mask_comments`, extended to
/// string interiors because this rule's inputs are sentences.
///
/// # What it does NOT handle: regex literals
///
/// `/`, `//`, `/* */` and the three quote kinds are the whole state machine. A
/// **regex literal** is none of them, so `/[},]/` in a scanned range would have
/// its `}` and `,` read as structure, and a `/` immediately before a `*` —
/// `/\*/` — would be read as the start of a block comment and swallow the rest
/// of the file up to the next `*/`. There is no current impact: no regex
/// literal and no bare division appears in a code position of either scanned
/// file. That makes this a **documented gap rather than a checked property** —
/// a regex literal must not appear in a scanned range, and the remedy if one
/// ever needs to is to teach this function the state rather than to work around
/// it at the call site. It joins the `< >` generic gap `properties` documents.
fn mask(text: &[char]) -> Vec<char> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Str(char),
    }

    let mut out = Vec::with_capacity(text.len());
    let mut state = State::Code;
    let mut escaped = false;
    let mut at = 0usize;
    while at < text.len() {
        let character = text[at];
        let next = text.get(at + 1).copied();
        match state {
            State::Code => {
                if character == '/' && next == Some('*') {
                    state = State::Block;
                    out.push(' ');
                    out.push(' ');
                    at += 2;
                    continue;
                }
                if character == '/' && next == Some('/') {
                    state = State::Line;
                    out.push(' ');
                    out.push(' ');
                    at += 2;
                    continue;
                }
                if matches!(character, '"' | '\'' | '`') {
                    state = State::Str(character);
                    escaped = false;
                }
                out.push(character);
            }
            State::Line => {
                if character == '\n' {
                    state = State::Code;
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            State::Block => {
                if character == '*' && next == Some('/') {
                    state = State::Code;
                    out.push(' ');
                    out.push(' ');
                    at += 2;
                    continue;
                }
                out.push(if character == '\n' { '\n' } else { ' ' });
            }
            State::Str(quote) => {
                if escaped {
                    escaped = false;
                    out.push(if character == '\n' { '\n' } else { ' ' });
                } else if character == '\\' {
                    escaped = true;
                    out.push(' ');
                } else if character == quote {
                    state = State::Code;
                    out.push(character);
                } else if character == '\n' && quote != '`' {
                    // An unterminated single-line string is a syntax error, not
                    // a licence to swallow the rest of the file.
                    state = State::Code;
                    out.push('\n');
                } else {
                    out.push(if character == '\n' { '\n' } else { ' ' });
                }
            }
        }
        at += 1;
    }
    out
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
        let read = |rel: &str| {
            std::fs::read_to_string(repo_root().join(rel))
                .unwrap_or_else(|error| panic!("could not read {rel}: {error}"))
        };
        Sources {
            commands_toml: read(COMMANDS_TOML),
            registry_ts: read(REGISTRY_TS),
            deck_view_ts: read(DECK_VIEW_TS),
            param_kind_rs: read(PARAM_KIND_RS),
            dictation_rs: read(DICTATION_RS),
        }
    }

    /// The tree itself. This is what makes the four planted-input tests below
    /// mean something: they prove the scanner CAN fail, and this proves the
    /// checked-in sources do not.
    #[test]
    fn the_checked_in_table_and_registry_resolve_against_each_other() {
        let findings = check(&checked_in());
        assert!(findings.is_empty(), "rule 14: {}", findings.join("\n"));
    }

    /// And that the scan is not vacuous: a rule that found no rows, no entries
    /// and no closed sets would also report nothing.
    #[test]
    fn the_scan_reads_a_table_and_a_registry_that_are_really_there() {
        let sources = checked_in();
        let mut findings = Vec::new();
        let rows = command_rows(&sources.commands_toml, &mut findings);
        let registry = registry_entries(&sources.registry_ts, &mut findings);
        let screens = deck_view_kinds(&sources.deck_view_ts, &mut findings);
        let kinds = param_kinds(&sources.param_kind_rs, &mut findings);
        assert!(findings.is_empty(), "{}", findings.join("\n"));
        assert!(rows.len() >= 4, "only {} rows parsed", rows.len());
        assert!(
            registry.len() >= rows.len(),
            "only {} registry entries parsed",
            registry.len()
        );
        assert_eq!(
            screens,
            ["agent", "deck", "overview"]
                .into_iter()
                .map(str::to_string)
                .collect()
        );
        assert_eq!(
            kinds,
            [
                "agent_ref",
                "agent_type_ref",
                "deck_ref",
                "dir_ref",
                "mode_ref",
                "orchestration_ref",
                "spoken_prefix",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
        // Every row's `invoke` is classified `voice: true`, which is the state
        // assertion 4 is about.
        for row in &rows {
            let entry = registry
                .get(&row.invoke)
                .unwrap_or_else(|| panic!("`{}` is not in the registry", row.invoke));
            assert!(entry.voice, "`{}` is not voice-classified", row.invoke);
        }
        // And at least one entry is deliberately excluded, so the `no_voice`
        // half is exercised by the tree rather than only by a fixture.
        assert!(
            registry.values().any(|entry| entry
                .no_voice
                .as_deref()
                .is_some_and(|reason| !reason.trim().is_empty())),
            "no entry carries a `no_voice` reason"
        );
    }

    fn planted(edit: impl Fn(&mut Sources)) -> Vec<String> {
        let mut sources = checked_in();
        edit(&mut sources);
        check(&sources)
    }

    /// Replace the first line whose trimmed start is `needle`, so a plant does
    /// not have to reproduce the rest of a written `no_voice` sentence — and
    /// cannot accidentally leave half of one behind, which is how the first
    /// version of the empty-reason plant planted a non-empty reason.
    fn replace_first_line(source: &str, needle: &str, replacement: &str) -> String {
        let mut out = Vec::new();
        let mut done = false;
        for line in source.lines() {
            if !done && line.trim_start().starts_with(needle) {
                out.push(replacement.to_string());
                done = true;
            } else {
                out.push(line.to_string());
            }
        }
        assert!(done, "no line starts with {needle:?}");
        out.join("\n")
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

    /// Assertion 1.
    #[test]
    fn a_planted_invoke_naming_no_registry_entry_is_caught() {
        let findings = planted(|sources| {
            sources.commands_toml = sources
                .commands_toml
                .replace("invoke      = \"openAgent\"", "invoke      = \"opneAgent\"");
        });
        assert_reports(&findings, "invokes `opneAgent`");
    }

    /// Assertion 2.
    #[test]
    fn a_planted_unknown_screen_is_caught() {
        let findings = planted(|sources| {
            sources.commands_toml = sources
                .commands_toml
                .replace("screens     = [\"agent\"]", "screens     = [\"dashboard\"]");
        });
        assert_reports(&findings, "names the screen `dashboard`");
    }

    /// Assertion 3.
    #[test]
    fn a_planted_unknown_param_kind_is_caught() {
        let findings = planted(|sources| {
            sources.commands_toml = sources
                .commands_toml
                .replace("kind = \"agent_ref\"", "kind = \"agent_name\"");
        });
        assert_reports(&findings, "of kind `agent_name`");
    }

    /// Assertion 4, in all four of the ways an entry can be misclassified.
    #[test]
    fn a_planted_unclassified_registry_entry_is_caught() {
        let unclassified = planted(|sources| {
            sources.registry_ts = replace_first_line(
                &sources.registry_ts,
                "no_voice:",
                "    unrelated: \"why not\",",
            );
        });
        // Re-pointed from `openProjects` to `closeAgentView` when PRD #802's
        // dictation rebuild folded `close_agent_view` into `close`: this plant
        // edits the FIRST `no_voice:` in the file, and that entry is now the
        // one the pane's X still dispatches through.
        assert_reports(&unclassified, "`closeAgentView` is unclassified");

        let empty = planted(|sources| {
            sources.registry_ts =
                replace_first_line(&sources.registry_ts, "no_voice:", "    no_voice: \"   \",");
        });
        assert_reports(&empty, "empty `no_voice` reason");

        let unreadable = planted(|sources| {
            sources.registry_ts = replace_first_line(
                &sources.registry_ts,
                "no_voice:",
                "    no_voice: PROJECT_PICKER_REASON,",
            );
        });
        assert_reports(&unreadable, "is not a plain string literal");

        let both = planted(|sources| {
            sources.registry_ts = replace_first_line(
                &sources.registry_ts,
                "no_voice:",
                "    voice: true,\n    no_voice: \"still excluded\",",
            );
        });
        assert_reports(&both, "carries both");

        let unbacked = planted(|sources| {
            sources.commands_toml = sources
                .commands_toml
                .replace("id          = \"open_deck\"", "id          = \"open_dek\"");
            sources.commands_toml = sources
                .commands_toml
                .replace("invoke      = \"openDeck\"", "invoke      = \"openDek\"");
        });
        assert_reports(&unbacked, "`openDeck` claims `voice: true`");

        // Re-pointed from `openSettings` to `openProjects` when PRD #802 M8
        // gave the former a row: the plant has to name an entry that still
        // carries a `no_voice` reason, or it exercises the `unbacked` case
        // above instead of this one.
        let overtaken = planted(|sources| {
            sources.commands_toml = sources.commands_toml.replace(
                "invoke      = \"openDeck\"",
                "invoke      = \"openProjects\"",
            );
        });
        assert_reports(&overtaken, "`openProjects` carries a `no_voice` reason");
    }

    /// Assertion 5, both halves.
    #[test]
    fn a_planted_spread_or_computed_key_in_the_registry_is_caught() {
        let spread = planted(|sources| {
            sources.registry_ts = sources.registry_ts.replacen(
                "  openAgent: {",
                "  ...EXTRA_ACTIONS,\n  openAgent: {",
                1,
            );
        });
        assert_reports(&spread, "a spread inside the object literal");

        let computed = planted(|sources| {
            sources.registry_ts = sources.registry_ts.replacen(
                "  openAgent: {",
                "  [DYNAMIC_ID]: {},\n  openAgent: {",
                1,
            );
        });
        assert_reports(&computed, "a computed key inside the object literal");
    }

    /// Every input is a finding when it cannot be read, and that has to hold
    /// for each of the five separately — a rule that goes quiet because one of
    /// its inputs vanished is the vacuous pass this module exists to avoid.
    #[test]
    fn a_source_this_rule_cannot_read_is_a_finding_rather_than_a_skip() {
        let no_registry =
            planted(|sources| sources.registry_ts = "export const OTHER = {};".to_string());
        assert_reports(&no_registry, "no `VOICE_ACTIONS =");

        let no_table = planted(|sources| sources.commands_toml = "# nothing here\n".to_string());
        assert_reports(&no_table, "holds no `[[commands]]`");

        let bad_toml =
            planted(|sources| sources.commands_toml = "[[commands]\nid = \"x\"".to_string());
        assert_reports(&bad_toml, "is not valid TOML");

        let no_screens =
            planted(|sources| sources.deck_view_ts = "export type Other = string;".to_string());
        assert_reports(&no_screens, "no `export type DeckView`");

        let no_kinds = planted(|sources| sources.param_kind_rs = "pub enum Other {}".to_string());
        assert_reports(&no_kinds, "yielded no kind literals");

        let no_phrases = planted(|sources| {
            sources.dictation_rs = "pub const OTHER: [&str; 0] = [];".to_string()
        });
        assert_reports(&no_phrases, "holds no `DICTATION_OPENERS = [");
    }

    /// Assertions 5 and 6, over the phrase lists matched ahead of the resolver.
    #[test]
    fn a_planted_phrase_the_model_was_never_told_about_is_caught() {
        // A phrase the fast path matches and the row's description does not
        // name: it dictates through one path and is a no-match through the
        // other, and nothing at run time would say so.
        let unnamed = planted(|sources| {
            sources.dictation_rs = sources
                .dictation_rs
                .replace("\"dictate\"]", "\"dictate\", \"scribble\"]");
        });
        assert_reports(&unnamed, "\"scribble\"");

        // The same for the submit list, against its own row.
        let unnamed_submit = planted(|sources| {
            sources.dictation_rs = sources
                .dictation_rs
                .replace("\"press enter\"]", "\"press enter\", \"fire away\"]");
        });
        assert_reports(&unnamed_submit, "\"fire away\"");

        // A phrase straddling a line break in the `"""…"""` description still
        // counts: the description is prose the model reads as one paragraph,
        // and a reflow must not go red.
        let reflowed = planted(|sources| {
            sources.commands_toml = sources
                .commands_toml
                .replace("send it, submit it", "send it,\nsubmit it")
        });
        assert!(
            reflowed.is_empty(),
            "a reflowed description went red: {}",
            reflowed.join("\n")
        );

        // The same drift from the other end: the row is gone, so the phrases
        // back nothing at all.
        let no_row = planted(|sources| {
            sources.commands_toml = sources.commands_toml.replace(
                "id          = \"submit_prompt\"",
                "id          = \"submit_prompts\"",
            );
        });
        assert_reports(&no_row, "holds no `submit_prompt` row");

        // Assertion 6: the submit list is checked first, so a shared phrase
        // makes that precedence silently load-bearing.
        let shared = planted(|sources| {
            sources.dictation_rs = sources
                .dictation_rs
                .replace("\"dictate\"]", "\"dictate\", \"submit\"]");
        });
        assert_reports(&shared, "is in both");

        // And the lists are genuinely read rather than assumed empty, which is
        // what would make every assertion above vacuous.
        let empty = planted(|sources| {
            sources.dictation_rs = sources.dictation_rs.replace(
                "pub const DICTATION_OPENERS: [&str; 4] = [",
                "pub const DICTATION_OPENERS: [&str; 0] = [];\nconst UNUSED: [&str; 1] = [",
            );
        });
        assert_reports(&empty, "yielded no phrases");
    }

    /// The masker is what makes every scan above safe, so it is pinned
    /// separately: prose in a comment or inside a reason must never be read as
    /// structure, and a multi-byte character must never move a span.
    #[test]
    fn prose_and_multibyte_text_cannot_be_read_as_structure() {
        let source = "const A = {\n  a: \"one, two { three\", // a comment with a } in it\n  b: \"héllo — wörld, ok\",\n};";
        let text: Vec<char> = source.chars().collect();
        let masked = mask(&text);
        assert_eq!(masked.len(), text.len());
        let body = literal_body(&masked, "A").expect("literal body");
        let mut findings = Vec::new();
        let found = properties(&text, &masked, body, "fixture.ts", &mut findings);
        assert!(findings.is_empty(), "{}", findings.join("\n"));
        assert_eq!(
            found.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(
            string_value(&text, found[1].value.clone()).as_deref(),
            Some("héllo — wörld, ok")
        );
    }
}
