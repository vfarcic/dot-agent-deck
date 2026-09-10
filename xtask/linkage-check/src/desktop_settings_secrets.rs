//! Issue #827: the desktop settings store's credential boundary, on the four
//! surfaces `settings.rs`'s own tests cannot reach.
//!
//! PRD #803 set one hard rule — *a secret never goes in `desktop.toml` and
//! never in `localStorage`* — and pinned it with a check that reads key
//! **names**. Issue #827 is the list of things that check cannot see, and this
//! module holds the structural half of that list. The value-following half is
//! in `desktop/src-tauri/src/settings.rs`'s own test module, which can drive
//! the real load/save/IPC path and is the better home for anything that can be
//! proven by running the code.
//!
//! What is here, and what each check actually establishes:
//!
//! 1. **Every field in the Rust settings schema has a type from a pinned
//!    allowlist.** This is the boundary. It constrains the *types* a field may
//!    have rather than its name: none of `u32`, `AppearanceMode` or `ZoomLevel`
//!    can carry arbitrary text, so a credential has no field to sit in whatever
//!    anybody calls it. It is also the check
//!    that closes the "field serde omits" gap — an omitted field still has to
//!    have an allowlisted type — and the one that will go red when PRD #802
//!    adds a `String`, which is exactly the moment somebody has to route it
//!    through the `SecretStore` seam instead. A serde attribute that hides a
//!    field from the serialised document (`skip`, `flatten`) is refused for the
//!    same reason: `settings.rs`'s sentinel sweep walks that document, so a
//!    field missing from it is a field nothing follows a value through.
//! 2. **The TypeScript DTO's declared shape is pinned, field by field, with its
//!    types.** The Rust tripwire cannot see `bridge.ts` at all, so a
//!    frontend-only `apiKey` on `DesktopSettingsDto` was declared where
//!    nothing read it. A new field is a one-line diff here, which is the same
//!    deliberate friction `default_document_shape_is_pinned` applies on the
//!    Rust side.
//! 3. **`normalizeDesktopSettings` constructs its result and never copies its
//!    input.** That function is what the fixture bridge writes to
//!    `localStorage` and what the live bridge coerces an IPC reply through, so
//!    it is the frontend's whole ingress. Building a fresh object literal with
//!    a fixed key set means an extra field — a credential under any name —
//!    is dropped rather than carried; a spread would silently make it a
//!    passthrough, so a spread anywhere in the function is refused.
//! 4. **The `localStorage` key set is pinned.** `desktop.toml` is only half the
//!    rule and the other half had no enforcement at all. Every key the app
//!    stores under is listed here with its literal, every access has to name
//!    one of those constants, and `localStorage` may only be spelled
//!    `localStorage.<op>(...)` — the last part is what stops an alias
//!    (`const store = window.localStorage`) naming a key nothing can read. So a
//!    new key is a deliberate edit to [`PINNED_STORAGE_KEYS`] rather than a
//!    line nobody sees, which the developer doc has asked for in prose since
//!    #824 with nothing behind it. This is deliberately **not** a judgment
//!    about which keys *belong* there: that is issue #824's question, and this
//!    check would pass unchanged whichever way #824 goes.
//!
//!    Its scope is `localStorage` in `.ts`/`.tsx` under `desktop/src`, and the
//!    boundary is worth stating rather than implying: **`sessionStorage`,
//!    IndexedDB, `document.cookie` and a `.js` file are outside it.** None of
//!    the four appears under `desktop/src` today — checked, not assumed — so
//!    this is a scope statement rather than a known hole; the moment one does,
//!    it needs its own row here.
//!
//! It lives here rather than in vitest for the reason the palette guards do:
//! these tests run under `cargo test-fast` (via `--workspace`, CLAUDE.md rule
//! 5) and in the CI `build` job, which is one of the four **required** checks.
//! `desktop-web`, where a vitest guard would run, is advisory — so a guard
//! there can be merged past, which is the failure mode a credential boundary
//! cannot afford. The runtime behaviour of `normalizeDesktopSettings` *is* also
//! asserted in `desktop/src/lib/bridge.test.ts`, because a text scan cannot
//! prove what a function does; check 3 above is what makes that vitest test
//! hard to quietly defeat.
//!
//! **What none of this proves.** There is no `SecretStore` yet — PRD #803 M5
//! named the seam and deliberately did not build it — so "a credential can
//! only enter through `SecretStore`" is not assertable here. What is assertable
//! is stronger while it holds and weaker in scope: *no route in exists at all*.
//! When #802 builds the store, check 1 is what forces the credential to go
//! through it.
//!
//! **Fails closed**, like `desktop_palette`: every filesystem error fails the
//! test rather than removing a file from the scan, and a symlink is refused
//! rather than followed. Same budget as the other file-reading modules here —
//! no network, no git, no sleep, no subprocess.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::slash_path;

/// The Rust settings schema.
const SETTINGS_RS: &str = "desktop/src-tauri/src/settings.rs";

/// The TypeScript half of the same schema, plus the normaliser and the fixture
/// bridge's storage.
const BRIDGE_TS: &str = "desktop/src/lib/bridge.ts";

/// The frontend tree the `localStorage` scan walks.
const DESKTOP_SRC: &str = "desktop/src";

/// A field type a settings struct may use, and why it cannot carry a
/// credential.
///
/// **Read this list as the boundary, not as bookkeeping.** Adding an entry is
/// the act of asserting that values of that type cannot hold arbitrary text —
/// so `String`, `Option<String>`, `PathBuf`, `Vec<String>` and any map do not
/// belong on it, whatever the field is called. If PRD #802 needs a credential,
/// the answer is the `SecretStore` seam, not a row here.
const ALLOWED_FIELD_TYPES: [(&str, FieldKind, &str); 5] = [
    (
        "u32",
        FieldKind::Scalar,
        "an integer; there is no text for a credential to be",
    ),
    (
        "AppearanceMode",
        FieldKind::Scalar,
        "a closed enum: its deserializer folds an unknown token to the default \
         and refuses anything over MAX_APPEARANCE_TOKEN_BYTES, so the value \
         this crate reads out and writes back is one of three tokens. Note \
         what that is NOT: a hand-edited file can hold any string here until \
         the next save, which is why settings.rs follows a sentinel through \
         the load as well",
    ),
    (
        "ZoomLevel",
        FieldKind::Scalar,
        "a newtype over f64 whose deserializer snaps to ZOOM_LEVELS, so the \
         value this crate reads out and writes back is one of ten numbers",
    ),
    (
        "AppearanceSettings",
        FieldKind::Section,
        "a section struct, whose own fields this check walks",
    ),
    (
        "ZoomSettings",
        FieldKind::Section,
        "a section struct, whose own fields this check walks",
    ),
];

/// Whether an allowlisted type is a leaf or another struct in the same file.
///
/// The distinction is what stops the allowlist being a way around itself: a
/// [`FieldKind::Section`] entry must resolve to a struct this scan actually
/// reads, so `voice: VoiceConfig` cannot be waved through by adding
/// `VoiceConfig` to the list and leaving its fields unscanned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Scalar,
    Section,
}

/// Serde attribute fragments that take a field out of the serialised document,
/// and therefore out of the reach of `settings.rs`'s sentinel sweep.
const HIDING_ATTRS: [&str; 2] = ["skip", "flatten"];

/// The `Storage` methods that name a key through an argument.
const KEY_METHODS: [&str; 3] = ["getItem", "setItem", "removeItem"];

/// The `Storage` members that name no key, so reaching one is not a finding:
/// `clear()` empties the store without naming anything, `key(n)` reads a name
/// out by index, and `length` counts.
const KEYLESS_MEMBERS: [&str; 3] = ["clear", "key", "length"];

/// The exact declared shape of the TypeScript settings DTOs.
///
/// `(interface, field, type)`. Pinned rather than pattern-matched because a
/// name scan on this side would repeat the mistake #827 is about: `endpoint:
/// string` passes any name check and is a free-text field. A diff here is the
/// review prompt.
const PINNED_TS_FIELDS: [(&str, &str, &str); 5] = [
    ("DesktopSettingsDto", "version", "number"),
    (
        "DesktopSettingsDto",
        "appearance",
        "{ mode: AppearanceMode }",
    ),
    ("DesktopSettingsDto", "zoom", "{ level: number }"),
    (
        "DesktopSettingsSnapshotDto",
        "settings",
        "DesktopSettingsDto",
    ),
    // The one legitimate string: where the document lives. It reaches the
    // webview deliberately — the settings footer answers "where did that go?"
    // — and `DesktopSettingsSnapshot`'s Rust doc comment records why that is
    // not in tension with the error paths refusing to name a path.
    ("DesktopSettingsSnapshotDto", "path?", "string"),
];

/// The keys the desktop app stores under, and the literal each one is built
/// from.
///
/// `(constant, literal, scoped)` — `scoped` records whether the literal is
/// wrapped in `modeScopedKey`, because the settings key deliberately is not
/// (a theme choice is global) and that is a property worth pinning rather than
/// rediscovering.
///
/// Whether each of these *belongs* in `localStorage` is issue #824's question,
/// not this check's. What this check refuses is a **new** one arriving
/// unnoticed — including one holding a credential, which is the half of PRD
/// #803's rule that had no enforcement at all.
const PINNED_STORAGE_KEYS: [(&str, &str, bool); 6] = [
    (
        "FIXTURE_SETTINGS_KEY",
        "dot-agent-deck.desktop-settings",
        false,
    ),
    (
        "OVERVIEW_COLUMNS_STORAGE_KEY",
        "dot-agent-deck.desktop.overview-columns.v1",
        true,
    ),
    (
        "PROJECTS_STORAGE_KEY",
        "dot-agent-deck.desktop.projects.v1",
        true,
    ),
    (
        "WORKFLOW_STORAGE_KEY",
        "dot-agent-deck.desktop.workflow-preview.v1",
        true,
    ),
    (
        "PROMPTS_STORAGE_KEY",
        "dot-agent-deck.desktop.prompts.v1",
        true,
    ),
    (
        "STORAGE_KEY",
        "dot-agent-deck.desktop.agent-profiles.v1",
        true,
    ),
];

/// The namespace every stored key shares, used to find a literal that looks
/// like a storage key wherever it was written.
const STORAGE_NAMESPACE: &str = "dot-agent-deck.";

/// The workspace root, from this crate's manifest dir rather than the process
/// cwd, so the tests do not depend on how the runner was invoked.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|error| panic!("could not read {rel}: {error}"))
}

/// One field of one settings struct, with the attributes written above it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RustField {
    struct_name: String,
    name: String,
    ty: String,
    /// The `#[...]` lines directly above the field, joined.
    attrs: String,
    /// 1-indexed, for a clickable failure.
    line: usize,
}

/// What a scan of the settings structs found.
///
/// The `unparsed` half is the important one and the reason this is not just a
/// `Vec<RustField>`: a line inside a settings struct that this scanner cannot
/// read is a **finding**, never a skip. Greptile caught the fail-open version
/// of this on PR #943 — `field_line` stripped only a bare `pub `, so
/// `pub(crate) endpoint: String,` parsed to the nonsense name
/// `pub(crate) endpoint`, was discarded, and the credential-boundary check
/// passed with a free-text field in the schema. A guard that quietly ignores
/// what it does not understand reports safety it never established, which is
/// the same principle as the fail-closed filesystem walk.
#[derive(Debug, Default, PartialEq, Eq)]
struct SchemaScan {
    fields: Vec<RustField>,
    /// `(struct, 1-indexed line, the line's text)`.
    unparsed: Vec<(String, usize, String)>,
}

/// Every field of every `*Settings` struct in `source`, plus every line inside
/// one that could not be read as a field.
///
/// A line scanner rather than a parse: this file is `rustfmt`-formatted, so a
/// field is one line and a struct body ends at a `}` in column zero. Two
/// consequences are worth knowing, and each is answered by making something a
/// failure rather than a skip:
///
/// - a struct **not** named `*Settings` is not scanned, which is why an
///   unrecognised field *type* is a failure — so a `voice: VoiceConfig` field
///   cannot escape by naming its struct something else;
/// - a field spelled in a way this scanner does not handle is not silently
///   dropped, which is why [`SchemaScan::unparsed`] exists.
fn rust_settings_fields(source: &str) -> SchemaScan {
    let mut scan = SchemaScan::default();
    let mut current: Option<String> = None;
    let mut attrs = String::new();

    for (index, raw) in source.lines().enumerate() {
        let line = raw.trim();
        match &current {
            None => {
                if let Some(name) = struct_header(raw) {
                    current = Some(name);
                    attrs.clear();
                }
            }
            Some(struct_name) => {
                if raw == "}" {
                    current = None;
                    attrs.clear();
                    continue;
                }
                if line.starts_with("#[") {
                    attrs.push_str(line);
                    continue;
                }
                if line.starts_with("//") || line.is_empty() {
                    continue;
                }
                match field_line(line) {
                    Some((name, ty)) => scan.fields.push(RustField {
                        struct_name: struct_name.clone(),
                        name,
                        ty,
                        attrs: std::mem::take(&mut attrs),
                        line: index + 1,
                    }),
                    None => {
                        attrs.clear();
                        scan.unparsed
                            .push((struct_name.clone(), index + 1, line.to_string()));
                    }
                }
            }
        }
    }
    scan
}

/// The name of the settings struct a line declares, if it declares one.
fn struct_header(raw: &str) -> Option<String> {
    let rest = strip_visibility(raw).strip_prefix("struct ")?;
    let name = rest.strip_suffix(" {")?;
    name.ends_with("Settings").then(|| name.to_string())
}

/// `decl` with any leading Rust visibility removed.
///
/// Handles every form the language has: none, `pub`, and the restricted
/// `pub(crate)` / `pub(super)` / `pub(in some::path)`. The restricted forms are
/// what PR #943's fail-open turned on — `pub(crate) endpoint: String,` has no
/// `pub ` prefix, so stripping that one literal left the visibility attached to
/// the field name and the whole line was discarded as unreadable.
fn strip_visibility(decl: &str) -> &str {
    let Some(rest) = decl.strip_prefix("pub") else {
        return decl;
    };
    // `pub(...)`: skip the balanced group. Nothing in Rust nests parentheses in
    // a visibility, so the first `)` closes it.
    let rest = match rest.strip_prefix('(') {
        Some(restricted) => match restricted.find(')') {
            Some(close) => &restricted[close + 1..],
            // An unclosed `pub(` is not a visibility this can read; hand the
            // line back unchanged so it lands in `unparsed` rather than being
            // half-interpreted.
            None => return decl,
        },
        None => rest,
    };
    // `pub` must be a whole word: `public_thing: u32` is a field, not a
    // visibility followed by a type.
    match rest.strip_prefix(' ') {
        Some(body) => body.trim_start(),
        None if rest.is_empty() => rest,
        None => decl,
    }
}

/// `pub name: Type,` split into its name and type, or `None` for anything this
/// scanner cannot read — which the caller records as a finding rather than
/// dropping.
fn field_line(line: &str) -> Option<(String, String)> {
    let body = strip_visibility(line).strip_suffix(',')?;
    let (name, ty) = body.split_once(':')?;
    let name = name.trim();
    let ty = ty.trim();
    if name.is_empty() || ty.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_string(), ty.to_string()))
}

/// The body of a `{ … }` block whose header line contains `header`, with the
/// header line excluded and the closing brace found by depth.
///
/// `None` when the header is absent, which every caller treats as a failure:
/// a check that silently passes because it could not find what it was looking
/// for is worse than no check.
fn block_after(source: &str, header: &str) -> Option<String> {
    let start = source.find(header)?;
    let open = source[start..].find('{')? + start;
    let mut depth = 0usize;
    for (offset, c) in source[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(source[open + 1..open + offset].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

/// The declared members of a TypeScript interface body: `(name, type)`, with
/// `?` kept on the name so an optional field is a different pin from a
/// required one.
///
/// Doc comments are skipped by their leading `*` / `//`, and a field whose type
/// is an inline object is kept whole — the whole point of the pin is that
/// `{ mode: AppearanceMode }` becoming `{ mode: string }` is a diff.
///
/// # Every member is read or reported; none is skipped
///
/// **A member terminator is optional in TypeScript**, and `;`, `,` and a bare
/// newline are all valid. Greptile caught the fail-open version of this on PR
/// #943: requiring a trailing `;` meant `apiKey: string` with no terminator was
/// silently dropped, `found` was unchanged, and the exact-shape assertion passed
/// with an unpinned free-text field on the DTO.
///
/// So all three terminators are accepted, and anything left that this function
/// cannot read comes back as `(the line, String::new())` — a member with an
/// empty type, which matches nothing in [`PINNED_TS_FIELDS`] and therefore
/// fails the pin instead of vanishing from it. A guard's unreadable input has
/// to be louder than its readable input, not quieter.
fn ts_interface_fields(body: &str) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut depth = 0usize;
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line.starts_with("//")
            || line.starts_with('*')
            || line.starts_with("/*")
        {
            continue;
        }
        // A multi-line inline object type would otherwise read as several
        // members. Nothing in these two interfaces is written that way today,
        // and a change that introduced one lands in the unreadable bucket
        // below rather than being half-read.
        if depth > 0 {
            depth += line.matches('{').count();
            depth -= line.matches('}').count().min(depth);
            continue;
        }
        let opens = line.matches('{').count();
        let closes = line.matches('}').count();
        if opens > closes {
            depth += opens - closes;
            fields.push((line.to_string(), String::new()));
            continue;
        }
        // `;`, `,` or nothing at all — all three are valid TypeScript.
        let member = line.strip_suffix(';').unwrap_or(line);
        let member = member.strip_suffix(',').unwrap_or(member);
        match member.split_once(':') {
            Some((name, ty)) if !name.trim().is_empty() && !ty.trim().is_empty() => {
                fields.push((name.trim().to_string(), ty.trim().to_string()));
            }
            // Unreadable: reported as itself with no type, so the pin fails.
            _ => fields.push((line.to_string(), String::new())),
        }
    }
    fields
}

/// Every `localStorage.<member>` in `source`, classified, with 1-indexed lines.
///
/// The member name is read first and decides everything, which is the part
/// worth understanding: `getItem`/`setItem`/`removeItem` are the only members
/// that name a key through an argument, so those get their argument extracted
/// verbatim (up to the first top-level `,` or `)`) and everything else is a
/// finding. The argument is deliberately **not** resolved — a computed key is
/// reported as the expression it is, because an unrecognised expression is
/// exactly what this check refuses.
///
/// Classifying by member rather than scanning forward for a `(` is what closes
/// the property-access route: `localStorage.apiKey = secret` stores a key with
/// no call at all, and the previous shape of this function read `apiKey` as a
/// method name and then hunted for the next `(` anywhere later in the file —
/// which reported a nonsense argument, or fell out of the loop entirely and
/// stopped scanning the rest of the file.
fn storage_uses(source: &str) -> Vec<StorageUse> {
    let mut found = Vec::new();
    let mut offset = 0usize;
    while offset <= source.len() {
        let Some(hit) = source[offset..].find("localStorage.") else {
            break;
        };
        let at = offset + hit + "localStorage.".len();
        let rest = &source[at..];
        let member: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$'))
            .collect();
        let line = source[..at].matches('\n').count() + 1;
        // Always advance by at least one character, and always by a whole one,
        // so a malformed `localStorage..` can neither loop forever nor slice
        // through the middle of a multi-byte character.
        let advance = if member.is_empty() {
            rest.chars().next().map_or(1, char::len_utf8)
        } else {
            member.len()
        };
        offset = (at + advance).min(source.len());

        if KEYLESS_MEMBERS.contains(&member.as_str()) {
            continue;
        }
        if !KEY_METHODS.contains(&member.as_str()) {
            found.push(StorageUse::Member { line, member });
            continue;
        }
        // A key-naming method has to be called right here. A bare reference
        // (`onClick={localStorage.removeItem}`) is the aliasing shape by
        // another route, so it lands in the same bucket.
        match rest[member.len()..].trim_start().strip_prefix('(') {
            Some(args) => found.push(StorageUse::Keyed {
                line,
                op: member,
                argument: first_argument(args),
            }),
            None => found.push(StorageUse::Member { line, member }),
        }
    }
    found
}

/// One use of `localStorage.<member>` in the shipped code.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StorageUse {
    /// A call that names a key, with the key expression exactly as written.
    Keyed {
        line: usize,
        op: String,
        argument: String,
    },
    /// Anything else reached through the `.`, which is a finding rather than
    /// something to analyse.
    ///
    /// **This is the property-access route, and it is not hypothetical:**
    /// `localStorage.apiKey = secret` stores a key just as `setItem` does, and
    /// no scan of call arguments can see it — the member name *is* the key. A
    /// bare method reference lands here too, for the same reason an alias does.
    Member { line: usize, member: String },
}

/// Every occurrence of `localStorage` in `source` that is **not** immediately
/// a member access, with 1-indexed lines and the ten bytes that follow.
///
/// This is what closes the aliasing route, and without it the key pin would be
/// a formality: `const store = window.localStorage;` followed by
/// `store.setItem(anything, secret)` names no key [`storage_uses`] can see,
/// and neither does `const { setItem } = window.localStorage`. So the shipped
/// code may only ever spell it `localStorage.<op>(…)`, and any other use of the
/// identifier is refused rather than analysed.
///
/// Run on comment-masked source, so the five doc comments under `desktop/src`
/// that discuss `localStorage` in prose are not findings.
fn storage_aliases(source: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    for (index, _) in source.match_indices("localStorage") {
        let after = &source[index + "localStorage".len()..];
        if after.starts_with('.') {
            continue;
        }
        // `window.localStorage` is the ordinary spelling; the access check
        // reads the `.` that follows, so only a non-access use lands here.
        let line = source[..index].matches('\n').count() + 1;
        let tail: String = after.chars().take(10).collect();
        found.push((line, format!("localStorage{tail}")));
    }
    found
}

/// Replace every JavaScript comment in `src` with spaces, keeping newlines so
/// line numbers survive.
///
/// String contents are kept, and the masker has to know where a string starts
/// and ends so that a `//` inside one does not swallow the rest of the file —
/// the same reasoning as `desktop_palette::mask_comments`, which this is a
/// trimmed-down sibling of (that one also returns per-line comment text, which
/// nothing here needs). An unterminated quote recovers at the newline rather
/// than masking the remainder, so an apostrophe in prose cannot turn a real
/// finding into a silent pass.
fn mask_comments(src: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        Line,
        Block,
        Str(char),
    }

    let mut out = String::with_capacity(src.len());
    let mut state = State::Code;
    let mut escaped = false;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            State::Code => {
                if c == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    state = State::Block;
                    out.push_str("  ");
                } else if c == '/' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = State::Line;
                    out.push_str("  ");
                } else {
                    if matches!(c, '"' | '\'' | '`') {
                        state = State::Str(c);
                        escaped = false;
                    }
                    out.push(c);
                }
            }
            State::Line => {
                if c == '\n' {
                    state = State::Code;
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            State::Block => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = State::Code;
                    out.push_str("  ");
                } else {
                    out.push(if c == '\n' { '\n' } else { ' ' });
                }
            }
            State::Str(quote) => {
                out.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote || (c == '\n' && quote != '`') {
                    state = State::Code;
                }
            }
        }
    }
    out
}

/// The first argument of a call whose opening paren has already been consumed.
fn first_argument(rest: &str) -> String {
    let mut depth = 0usize;
    for (offset, c) in rest.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => return rest[..offset].trim().to_string(),
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => return rest[..offset].trim().to_string(),
            _ => {}
        }
    }
    rest.trim().to_string()
}

/// The non-test TypeScript sources under `root`, walked fail-closed.
///
/// Test files are excluded because they legitimately clear and poke storage
/// under arbitrary keys; the shipped code is what the pin is about. Symlinks
/// are refused rather than followed, and every I/O error is returned — the
/// reasoning is `desktop_palette::sources`', and it applies identically: a
/// required check that inspects nothing and passes reports safety it never
/// established.
fn frontend_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut visited = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let real = fs::canonicalize(&dir)
            .map_err(|error| format!("could not resolve {}: {error}", dir.display()))?;
        if !visited.insert(real) {
            continue;
        }
        let entries = fs::read_dir(&dir)
            .map_err(|error| format!("could not read the directory {}: {error}", dir.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                format!("could not read an entry in {}: {error}", dir.display())
            })?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| {
                format!(
                    "could not determine the type of {}: {error}",
                    path.display()
                )
            })?;
            if kind.is_symlink() {
                return Err(format!(
                    "{} is a symlink; the settings-secret guard will not follow one, \
                     because a directory link can leave desktop/src or loop back into it \
                     and a file link would be reported under a path it does not have.",
                    path.display()
                ));
            }
            if kind.is_dir() {
                stack.push(path);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("ts" | "tsx")
            ) {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default();
                if !name.contains(".test.") {
                    out.push(path);
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Every `dot-agent-deck.`-prefixed string literal in `source`, with a
/// `modeScopedKey(...)` wrapper preserved as part of the entry.
///
/// The wrapper is kept because it changes what the key *is*: a scoped key
/// carries a `.live`/`.fixture` suffix at runtime and an unscoped one does not,
/// so pinning the bare literal would let a key silently change scope. Test
/// files are already out of the walk, so a literal found here is one the
/// shipped app stores under.
fn namespaced_literals(source: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (index, _) in source.match_indices(STORAGE_NAMESPACE) {
        // Only a literal counts: the prefix has to open with a quote.
        let Some(quote) = source[..index].chars().next_back() else {
            continue;
        };
        if !matches!(quote, '"' | '\'' | '`') {
            continue;
        }
        let Some(end) = source[index..].find(quote) else {
            continue;
        };
        let literal = &source[index..index + end];
        let before = source[..index - 1].trim_end();
        if before.ends_with("modeScopedKey(") {
            found.insert(format!("modeScopedKey({literal})"));
        } else {
            found.insert(literal.to_string());
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check 1, and the boundary this whole module exists for: **no field in
    /// the settings schema has a type that can carry arbitrary text.**
    ///
    /// This is the check that makes "a credential cannot be stored here" a
    /// statement about types rather than about names, and the one PRD #802
    /// will trip. A failure has exactly two honest resolutions: put the value
    /// behind the `SecretStore` seam PRD #803 M5 names, or add the new type to
    /// [`ALLOWED_FIELD_TYPES`] with a reason saying why it cannot hold a
    /// credential — and "the field is not called `api_key`" is not such a
    /// reason.
    #[test]
    fn every_settings_field_has_a_type_that_cannot_carry_a_credential() {
        let source = read(SETTINGS_RS);
        let SchemaScan { fields, unparsed } = rust_settings_fields(&source);
        assert!(
            fields.len() >= 5,
            "the scan found only {} field(s), so it has stopped reading the schema \
             rather than found it clean: {fields:#?}",
            fields.len()
        );

        let scanned: BTreeSet<&str> = fields.iter().map(|f| f.struct_name.as_str()).collect();
        let mut offenders = Vec::new();
        // Unreadable first: a line this scanner cannot parse is the one case
        // where it knows nothing about a field, so it cannot be a pass.
        for (struct_name, line, text) in &unparsed {
            offenders.push(format!(
                "{SETTINGS_RS}:{line}: this line inside {struct_name} could not be read as \
                 a field, so its type was never checked: `{text}`"
            ));
        }
        for field in &fields {
            let allowed = ALLOWED_FIELD_TYPES
                .iter()
                .find(|(ty, _, _)| *ty == field.ty);
            match allowed {
                None => offenders.push(format!(
                    "{SETTINGS_RS}:{}: {}.{} is a `{}`, which is not on ALLOWED_FIELD_TYPES",
                    field.line, field.struct_name, field.name, field.ty
                )),
                // A section entry has to name a struct this scan actually
                // read, or the allowlist becomes a way around itself.
                Some((ty, FieldKind::Section, _)) if !scanned.contains(ty) => {
                    offenders.push(format!(
                        "{SETTINGS_RS}:{}: {}.{} is a `{ty}`, allowlisted as a section, but \
                         no `{ty}` struct was scanned — rename it to end in `Settings` or \
                         teach `rust_settings_fields` about it",
                        field.line, field.struct_name, field.name
                    ));
                }
                Some(_) => {}
            }
            if let Some(hiding) = HIDING_ATTRS
                .iter()
                .find(|fragment| field.attrs.contains(**fragment))
            {
                offenders.push(format!(
                    "{SETTINGS_RS}:{}: {}.{} carries a serde `{hiding}`, which takes it out \
                     of the serialised document that settings.rs's sentinel sweep walks",
                    field.line, field.struct_name, field.name
                ));
            }
        }

        assert!(
            offenders.is_empty(),
            "the desktop settings schema gained a field that could hold credential \
             material (issue #827):\n\n{}\n\nA credential belongs behind the `SecretStore` \
             seam (PRD #803 M5), never in this document. If the new type genuinely cannot \
             carry text, add it to ALLOWED_FIELD_TYPES with the reason — and read that \
             list's doc comment first, because a field NAME is not a reason.",
            offenders.join("\n")
        );
    }

    /// The scanner's own logic, proven rather than assumed: a clean result on
    /// the real file only means something if a dirty one would be caught.
    #[test]
    fn the_field_type_scan_catches_a_free_text_field_however_it_is_named() {
        let source = "\
pub struct VoiceSettings {
    pub backend: AppearanceMode,
    pub endpoint: String,
    #[serde(skip_serializing_if = \"Option::is_none\")]
    pub value: u32,
    #[serde(flatten)]
    pub extra: AppearanceSettings,
}
";
        let SchemaScan { fields, unparsed } = rust_settings_fields(source);
        assert!(unparsed.is_empty(), "{unparsed:#?}");
        assert_eq!(fields.len(), 4, "{fields:#?}");
        assert_eq!(fields[1].name, "endpoint");
        assert_eq!(fields[1].ty, "String");
        // The attribute travels with the field it sits above, and not with the
        // next one.
        assert!(fields[2].attrs.contains("skip_serializing_if"));
        assert!(fields[3].attrs.contains("flatten"));
        assert!(fields[0].attrs.is_empty());

        // The three offences this scan reports, in one fixture: an unlisted
        // type, and the two hiding attributes.
        assert!(
            !ALLOWED_FIELD_TYPES.iter().any(|(ty, _, _)| *ty == "String"),
            "`String` must never be on the allowlist — it is the thing the check is for"
        );
        for fragment in HIDING_ATTRS {
            assert!(
                fields.iter().any(|f| f.attrs.contains(fragment)),
                "the fixture should exercise `{fragment}`"
            );
        }
    }

    /// The two fail-opens Greptile found on PR #943, kept as regression tests
    /// because both were the same defect: a scanner that **silently skipped**
    /// what it could not parse, in a check whose whole job is to refuse what it
    /// does not recognise. Each let a free-text field into the schema with
    /// every gate green.
    #[test]
    fn a_field_this_scanner_cannot_read_is_a_finding_rather_than_a_skip() {
        // Rust side: `pub(crate)` has no `pub ` prefix, so the old
        // `strip_prefix("pub ")` left the visibility glued to the name, the
        // line parsed to nothing, and the field was dropped.
        for visibility in [
            "",
            "pub ",
            "pub(crate) ",
            "pub(super) ",
            "pub(in crate::a) ",
        ] {
            let source =
                format!("pub struct VoiceSettings {{\n    {visibility}endpoint: String,\n}}\n");
            let SchemaScan { fields, unparsed } = rust_settings_fields(&source);
            assert!(unparsed.is_empty(), "{visibility:?}: {unparsed:#?}");
            assert_eq!(
                fields
                    .iter()
                    .map(|f| (f.name.as_str(), f.ty.as_str()))
                    .collect::<Vec<_>>(),
                [("endpoint", "String")],
                "a `{visibility}` field must still be read, and read correctly"
            );
        }

        // `pub` must be a whole word, or a field legitimately named
        // `public_thing` would be mangled.
        let SchemaScan { fields, .. } =
            rust_settings_fields("pub struct VoiceSettings {\n    pub public_id: u32,\n}\n");
        assert_eq!(fields[0].name, "public_id");

        // And anything genuinely unreadable is reported, not dropped: an
        // unclosed visibility, a multi-line type, and a line with no colon.
        let SchemaScan { fields, unparsed } = rust_settings_fields(
            "pub struct VoiceSettings {\n    pub(crate endpoint: String,\n    pub keys: Vec<\n    nonsense\n}\n",
        );
        assert!(fields.is_empty(), "{fields:#?}");
        assert_eq!(unparsed.len(), 3, "{unparsed:#?}");
        assert!(unparsed.iter().all(|(name, _, _)| name == "VoiceSettings"));

        // TypeScript side: a member terminator is optional, so requiring `;`
        // dropped a perfectly valid `apiKey: string`. All three spellings are
        // read now, and an unreadable line comes back with an empty type so it
        // cannot match a pin.
        assert_eq!(
            ts_interface_fields("  apiKey: string\n  token: string,\n  version: number;\n"),
            [
                ("apiKey".to_string(), "string".to_string()),
                ("token".to_string(), "string".to_string()),
                ("version".to_string(), "number".to_string()),
            ]
        );
        assert_eq!(
            ts_interface_fields(
                "  [key: string]: unknown;\n  nested: {\n    secret: string;\n  };\n"
            ),
            [
                // An index signature is read as a member — and one whose name
                // is not in the pin, so it fails it.
                ("[key".to_string(), "string]: unknown".to_string()),
                ("nested: {".to_string(), String::new()),
            ]
        );
        assert_eq!(
            ts_interface_fields("  justAName\n"),
            [("justAName".to_string(), String::new())]
        );
    }

    /// Check 2: the TypeScript DTO's shape, pinned field by field.
    ///
    /// The gap this closes: the Rust tripwire reads the Rust schema, so a
    /// frontend-only `apiKey` on `DesktopSettingsDto` was declared where
    /// nothing read it. A new field here is a deliberate diff, the same friction
    /// `default_document_shape_is_pinned` applies on the other side.
    #[test]
    fn the_typescript_settings_dtos_carry_exactly_the_pinned_fields() {
        let source = read(BRIDGE_TS);
        let mut found = Vec::new();
        for interface in ["DesktopSettingsDto", "DesktopSettingsSnapshotDto"] {
            let body = block_after(&source, &format!("export interface {interface} "))
                .unwrap_or_else(|| {
                    panic!("no `export interface {interface}` block in {BRIDGE_TS}")
                });
            for (name, ty) in ts_interface_fields(&body) {
                found.push((interface.to_string(), name, ty));
            }
        }

        let expected: Vec<(String, String, String)> = PINNED_TS_FIELDS
            .iter()
            .map(|(i, n, t)| (i.to_string(), n.to_string(), t.to_string()))
            .collect();
        assert_eq!(
            found, expected,
            "the TypeScript settings DTOs changed shape (issue #827). A credential must \
             not be here in any form: this side is handed to the webview verbatim and, in \
             the fixture preview, written to localStorage. If the new field is legitimate, \
             update PINNED_TS_FIELDS — and make sure its Rust counterpart passed \
             `every_settings_field_has_a_type_that_cannot_carry_a_credential`."
        );
    }

    /// Check 3: `normalizeDesktopSettings` builds its result rather than
    /// copying its input, so an extra field cannot ride through it.
    ///
    /// It is the frontend's whole ingress — the fixture bridge writes its
    /// output to `localStorage`, and the live bridge coerces every IPC reply
    /// through it — so if it spread its argument, a credential under any name
    /// would be stored verbatim. The *behaviour* is asserted in
    /// `bridge.test.ts`; this is the structural half, in a gate that cannot be
    /// merged past.
    #[test]
    fn the_settings_normaliser_constructs_its_result_and_never_spreads_its_input() {
        let source = read(BRIDGE_TS);
        let body = block_after(
            &source,
            "export function normalizeDesktopSettings(value: unknown): DesktopSettingsDto ",
        )
        .unwrap_or_else(|| panic!("no `normalizeDesktopSettings` in {BRIDGE_TS}"));

        assert!(
            !body.contains("..."),
            "`normalizeDesktopSettings` gained a spread, which would make it a \
             passthrough for any field its caller was handed — including a credential \
             (issue #827). Name every field it returns:\n{body}"
        );

        // And the object it does return carries exactly the pinned key set, so
        // a fourth key is a diff here as well as in PINNED_TS_FIELDS.
        let returned = block_after(&body, "return ")
            .unwrap_or_else(|| panic!("`normalizeDesktopSettings` has no object literal return"));
        let mut keys: Vec<&str> = returned
            .lines()
            .filter_map(|line| line.trim().split_once(':'))
            .map(|(key, _)| key.trim())
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            ["appearance", "version", "zoom"],
            "unexpected normaliser result shape:\n{returned}"
        );
    }

    /// Check 4: the `localStorage` key set, pinned.
    ///
    /// `desktop.toml` is only half of PRD #803's rule and the other half had
    /// no enforcement: nothing stopped a fifth key appearing, credential-shaped
    /// or otherwise, and the developer doc asked for it in prose only. Every
    /// access has to name a pinned constant, and every namespaced literal has
    /// to be one of the pinned literals — so neither an inline key nor a
    /// computed one gets in quietly.
    ///
    /// Which keys *belong* in `localStorage` is issue #824's question. This
    /// check is indifferent to the answer and would pass unchanged either way.
    #[test]
    fn the_localstorage_key_set_is_pinned_and_holds_no_credential() {
        let root = repo_root().join(DESKTOP_SRC);
        let sources = frontend_sources(&root).expect("walk desktop/src");
        assert!(
            sources.len() > 10,
            "the walk found only {} non-test source(s), so it is not reading the tree",
            sources.len()
        );

        let allowed_exprs: BTreeSet<String> = PINNED_STORAGE_KEYS
            .iter()
            .map(|(constant, _, _)| (*constant).to_string())
            .collect();
        let mut offenders = Vec::new();
        let mut literals = BTreeSet::new();

        for path in &sources {
            let raw = fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("could not read {}: {error}", path.display()));
            // Masked, so prose about `localStorage` in a doc comment is neither
            // a finding nor mistaken for a real access.
            let text = mask_comments(&raw);
            let rel = slash_path(path.strip_prefix(repo_root()).unwrap_or(path));

            for (line, use_) in storage_aliases(&text) {
                offenders.push(format!(
                    "{rel}:{line}: `{use_}` — `localStorage` may only be spelled \
                     `localStorage.<op>(...)`, because an alias or a destructured \
                     accessor names no key this check can read"
                ));
            }

            for use_ in storage_uses(&text) {
                match use_ {
                    StorageUse::Keyed { line, op, argument }
                        if !allowed_exprs.contains(&argument) =>
                    {
                        offenders.push(format!(
                            "{rel}:{line}: localStorage.{op}({argument}) — `{argument}` is \
                             not one of the pinned storage-key constants"
                        ));
                    }
                    StorageUse::Keyed { .. } => {}
                    StorageUse::Member { line, member } => offenders.push(format!(
                        "{rel}:{line}: localStorage.{member} — only \
                         getItem/setItem/removeItem (and the keyless clear/key/length) may \
                         be reached through `localStorage.`; a property access stores a key \
                         whose name no argument scan can read"
                    )),
                }
            }

            for literal in namespaced_literals(&text) {
                literals.insert(literal);
            }
        }

        assert!(
            offenders.is_empty(),
            "the desktop app gained a localStorage key the pin does not know about \
             (issue #827):\n\n{}\n\nPRD #803's rule is that a secret goes in neither \
             `desktop.toml` nor `localStorage`. If this key is legitimate, add it to \
             PINNED_STORAGE_KEYS — and read the criterion in \
             `docs/develop/desktop-gui.md` first, because an app preference belongs in \
             `desktop.toml` and project content is waiting on #819.",
            offenders.join("\n")
        );

        let expected: BTreeSet<String> = PINNED_STORAGE_KEYS
            .iter()
            .map(|(_, literal, scoped)| {
                if *scoped {
                    format!("modeScopedKey({literal})")
                } else {
                    (*literal).to_string()
                }
            })
            .collect();
        assert_eq!(
            literals, expected,
            "the set of `{STORAGE_NAMESPACE}`-prefixed storage literals under \
             {DESKTOP_SRC} no longer matches PINNED_STORAGE_KEYS (issue #827). The \
             `modeScopedKey(...)` wrapper is part of the pin: the settings key is \
             deliberately unscoped because a theme choice is global, and every \
             project-draft key deliberately is scoped."
        );
    }

    /// The `localStorage` scanner's own logic. Same reasoning as the field-type
    /// one: a pass on the real tree means nothing unless a violation would be
    /// caught, and every shape below is one a contributor could plausibly
    /// write.
    #[test]
    fn the_localstorage_scan_reads_the_key_expression_it_is_handed() {
        let source = r#"
window.localStorage.setItem(PROMPTS_STORAGE_KEY, JSON.stringify({ prompts }));
const raw = window.localStorage.getItem("dot-agent-deck.desktop.api-key.v1");
window.localStorage.removeItem(`${base}.${mode}`);
window.localStorage.clear();
localStorage.setItem(modeScopedKey("dot-agent-deck.desktop.new.v1"), token);
"#;
        let keyed: Vec<(usize, String, String)> = storage_uses(source)
            .into_iter()
            .filter_map(|use_| match use_ {
                StorageUse::Keyed { line, op, argument } => Some((line, op, argument)),
                StorageUse::Member { .. } => None,
            })
            .collect();
        assert_eq!(
            keyed
                .iter()
                .map(|(line, op, arg)| (*line, op.as_str(), arg.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (2, "setItem", "PROMPTS_STORAGE_KEY"),
                (3, "getItem", "\"dot-agent-deck.desktop.api-key.v1\""),
                (4, "removeItem", "`${base}.${mode}`"),
                (
                    6,
                    "setItem",
                    "modeScopedKey(\"dot-agent-deck.desktop.new.v1\")"
                ),
            ],
            "`clear()` names no key, so it must not appear as a keyed access"
        );

        // An inline literal, a computed key and a `modeScopedKey(...)` call are
        // all refused, because none of them is a pinned constant — which is
        // what makes a new key hard to add by accident.
        let pinned: BTreeSet<&str> = PINNED_STORAGE_KEYS
            .iter()
            .map(|(constant, _, _)| *constant)
            .collect();
        for (_, _, argument) in &keyed {
            assert_eq!(
                pinned.contains(argument.as_str()),
                argument == "PROMPTS_STORAGE_KEY",
                "unexpected verdict for {argument}"
            );
        }

        // The property-access route, which is the hole that classifying by
        // member name closes: the member IS the key, so no argument scan could
        // ever see it. A bare method reference lands in the same bucket, and
        // the keyless members in neither.
        assert_eq!(
            storage_uses(
                "localStorage.apiKey = secret;\n\
                 const f = localStorage.removeItem;\n\
                 void localStorage.length;\n\
                 localStorage.key(0);\n"
            ),
            vec![
                StorageUse::Member {
                    line: 1,
                    member: "apiKey".to_string()
                },
                StorageUse::Member {
                    line: 2,
                    member: "removeItem".to_string()
                },
            ]
        );

        // A malformed member neither loops forever nor slices through a
        // multi-byte character.
        assert_eq!(
            storage_uses("localStorage..é\nlocalStorage."),
            vec![
                StorageUse::Member {
                    line: 1,
                    member: String::new()
                },
                StorageUse::Member {
                    line: 2,
                    member: String::new()
                },
            ]
        );

        // And the literal sweep sees both spellings, with the wrapper kept so
        // scoped and unscoped keys are different pins.
        assert_eq!(
            namespaced_literals(source),
            [
                "dot-agent-deck.desktop.api-key.v1".to_string(),
                "modeScopedKey(dot-agent-deck.desktop.new.v1)".to_string(),
            ]
            .into_iter()
            .collect::<BTreeSet<_>>()
        );
    }

    /// The alias rule and the comment mask, which only make sense together:
    /// the rule is what stops the key pin being a formality, and the mask is
    /// what stops the five doc comments under `desktop/src` that discuss
    /// `localStorage` in prose from being findings.
    #[test]
    fn an_aliased_or_destructured_localstorage_is_refused_and_prose_is_not() {
        let hostile = r#"
const store = window.localStorage;
store.setItem("dot-agent-deck.desktop.voice.v1", token);
const { setItem } = window.localStorage;
"#;
        let masked = mask_comments(hostile);
        let aliases = storage_aliases(&masked);
        assert_eq!(aliases.len(), 2, "{aliases:#?}");
        assert_eq!(aliases[0].0, 2);
        assert_eq!(aliases[1].0, 4);
        // And the aliased write names no key the use scan can see, which is
        // exactly why the alias itself has to be the finding.
        assert!(
            storage_uses(&masked).is_empty(),
            "the aliased call should be invisible to the use scan: {:#?}",
            storage_uses(&masked)
        );

        // Prose is not a finding, and a commented-out access is not an access.
        let prose = r#"
/**
 * `FixtureDeckBridge` keeps settings in `localStorage`. The live bridge
 * never reads localStorage — see PRD #803.
 */
// window.localStorage.setItem(ROGUE_KEY, secret);
window.localStorage.getItem(PROMPTS_STORAGE_KEY);
"#;
        let masked = mask_comments(prose);
        assert!(
            storage_aliases(&masked).is_empty(),
            "{:#?}",
            storage_aliases(&masked)
        );
        assert_eq!(
            storage_uses(&masked),
            vec![StorageUse::Keyed {
                line: 7,
                op: "getItem".to_string(),
                argument: "PROMPTS_STORAGE_KEY".to_string()
            }]
        );

        // A `//` inside a string must not swallow the code after it, or the
        // mask would turn a real finding into a silent pass.
        let tricky = "const url = \"https://example.test\";\nconst s = window.localStorage;\n";
        assert_eq!(storage_aliases(&mask_comments(tricky)).len(), 1);
    }

    /// A missing header is a failure, not a pass — the property every check
    /// above depends on, since all four locate their subject by text.
    #[test]
    fn a_check_whose_subject_has_moved_cannot_pass_by_finding_nothing() {
        assert_eq!(
            block_after("nothing here", "export interface Whatever "),
            None
        );
        assert_eq!(block_after("fn a() { unclosed", "fn a"), None);
        assert_eq!(
            block_after("fn a() { b: 1 }", "fn a").as_deref(),
            Some(" b: 1 ")
        );
        // A struct whose name does not end in `Settings` is not scanned, which
        // is the scan's one documented blind spot — and the reason an
        // unrecognised field *type* is a failure rather than a skip, so
        // `voice: VoiceConfig` still goes red from the field that references
        // it. (`NotSettings` would be a poor fixture for this: it ends in
        // `Settings`.)
        assert_eq!(
            rust_settings_fields("pub struct VoiceConfig {\n    pub a: String,\n}\n"),
            SchemaScan::default()
        );
    }

    /// A symlink is refused rather than followed, for the reasons
    /// `desktop_palette` records: a directory link can leave the tree or loop,
    /// and a file link would be reported under a path it does not have.
    #[cfg(unix)]
    #[test]
    fn a_symlink_under_the_frontend_tree_is_refused_rather_than_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::os::unix::fs::symlink(dir.path(), dir.path().join("sub/loop")).expect("symlink");
        let message = frontend_sources(dir.path()).expect_err("a symlink must fail the walk");
        assert!(message.contains("symlink"), "{message}");
    }
}
