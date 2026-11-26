//! PRD #2374: the agent-card catalog — the fields and actions an agent's card
//! may carry, with the descriptions agents are told about.
//!
//! The deck ships defaults ([`default_fields`], [`default_actions`]); the user
//! adds to, replaces or removes them in the `[catalog]` table of their user
//! config (`~/.config/dot-agent-deck/config.toml`, or `DOT_AGENT_DECK_CONFIG`).
//! A `[catalog]` in a project's `.dot-agent-deck.toml` is ignored in this
//! slice: catalog prompts become buttons that type text into an agent, and the
//! project-trust store approves exact orchestration-role text, not a catalog.
//!
//! The parsing, merging and editing of TOML text is pure; the file is touched
//! in two places: [`resolve_from_user_config`] (and `_at`) reads it, and
//! [`edit_user_catalog_at`] reads it and, for an edit, replaces it, one edit at
//! a time in this process (a mutex). The catalog is parsed from the raw TOML separately from the rest of
//! the config, so a bad catalog never breaks the user's other settings and a
//! bad setting never hides the catalog. Validation is non-fatal: an offending
//! entry is skipped, the rest applies, and each problem is one string in
//! [`ResolvedCatalog::errors`].

use serde::{Deserialize, Serialize};

/// The most fields a resolved catalog keeps; extras are dropped with an error.
pub const MAX_CATALOG_FIELDS: usize = 64;
/// The most actions a resolved catalog keeps; extras are dropped with an error.
pub const MAX_CATALOG_ACTIONS: usize = 32;
/// Longest `description`, in characters.
pub const MAX_DESCRIPTION_CHARS: usize = 300;
/// Longest catalog `label`, in characters.
pub const MAX_CATALOG_LABEL_CHARS: usize = 40;
/// Longest action `prompt`, in characters.
pub const MAX_PROMPT_CHARS: usize = 2000;
/// Longest field or action name, in characters.
pub const MAX_NAME_CHARS: usize = 40;
/// Longest icon name, in characters.
pub const MAX_ICON_CHARS: usize = 32;

/// Field names a report and a catalog may not use: they name the card's own
/// slots.
pub const RESERVED_FIELD_NAMES: &[&str] = &["name", "status", "summary", "needs-you"];

/// Action names a report and a catalog may not use: the deck's own actions.
/// Anything starting with `question` is reserved as well
/// ([`is_reserved_action_name`]), because question options become actions
/// named `question/<id>/<option>`.
pub const RESERVED_ACTION_NAMES: &[&str] = &["open", "reply", "close", "close-keep-worktree"];

/// The fixed set of value types a field can have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    Text,
    LongText,
    Number,
    Boolean,
    Url,
    List,
    PullRequest,
    Issue,
}

impl FieldType {
    pub const ALL: [FieldType; 8] = [
        FieldType::Text,
        FieldType::LongText,
        FieldType::Number,
        FieldType::Boolean,
        FieldType::Url,
        FieldType::List,
        FieldType::PullRequest,
        FieldType::Issue,
    ];

    /// The spelling the config, the wire and the dispatch text use.
    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::Text => "text",
            FieldType::LongText => "long_text",
            FieldType::Number => "number",
            FieldType::Boolean => "boolean",
            FieldType::Url => "url",
            FieldType::List => "list",
            FieldType::PullRequest => "pull_request",
            FieldType::Issue => "issue",
        }
    }

    pub fn parse(raw: &str) -> Option<FieldType> {
        FieldType::ALL.into_iter().find(|t| t.as_str() == raw)
    }
}

/// Where a field or an action is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    /// A table column (a later slice), the card and the detail overlay.
    Compact,
    /// The card and the detail overlay.
    Card,
    /// The detail overlay only.
    Detail,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Placement::Compact => "compact",
            Placement::Card => "card",
            Placement::Detail => "detail",
        }
    }

    pub fn parse(raw: &str) -> Option<Placement> {
        [Placement::Compact, Placement::Card, Placement::Detail]
            .into_iter()
            .find(|p| p.as_str() == raw)
    }
}

/// Where a resolved entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Default,
    User,
}

/// One resolved catalog field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogField {
    pub name: String,
    pub label: String,
    pub description: String,
    #[serde(rename = "type")]
    pub field_type: FieldType,
    pub icon: Option<String>,
    pub placement: Placement,
    pub source: CatalogSource,
}

/// One resolved catalog action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogAction {
    pub name: String,
    pub label: String,
    pub description: String,
    pub prompt: String,
    pub icon: Option<String>,
    pub placement: Placement,
    pub confirm: bool,
    pub always: bool,
    pub source: CatalogSource,
}

/// The catalog after defaults, the user's `remove`, replacements and
/// additions, with every validation problem met on the way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedCatalog {
    pub fields: Vec<CatalogField>,
    pub actions: Vec<CatalogAction>,
    pub errors: Vec<String>,
    /// The user config file read, when one was. Set by
    /// [`resolve_from_user_config`]; [`resolve`] leaves it `None`.
    #[serde(default)]
    pub user_config: Option<String>,
}

impl ResolvedCatalog {
    pub fn field(&self, name: &str) -> Option<&CatalogField> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn action(&self, name: &str) -> Option<&CatalogAction> {
        self.actions.iter().find(|a| a.name == name)
    }
}

/// `^[a-z0-9][a-z0-9-]{0,39}$`, the syntax of field, action and question
/// names.
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME_CHARS
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// `^[a-z0-9-]{1,32}$`.
pub fn is_valid_icon(icon: &str) -> bool {
    !icon.is_empty()
        && icon.len() <= MAX_ICON_CHARS
        && icon
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

pub fn is_reserved_field_name(name: &str) -> bool {
    RESERVED_FIELD_NAMES.contains(&name)
}

pub fn is_reserved_action_name(name: &str) -> bool {
    RESERVED_ACTION_NAMES.contains(&name) || name.starts_with("question")
}

/// The label a field or action gets when none is given: the name with `-` as
/// spaces and its first letter upper-cased.
pub fn default_label(name: &str) -> String {
    let spaced = name.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Whether `text` holds a control character, allowing `\n` and `\t` when
/// `multiline`.
pub fn has_disallowed_control(text: &str, multiline: bool) -> bool {
    text.chars()
        .any(|c| c.is_control() && !(multiline && (c == '\n' || c == '\t')))
}

fn field(
    name: &str,
    field_type: FieldType,
    placement: Placement,
    icon: &str,
    description: &str,
) -> CatalogField {
    CatalogField {
        name: name.to_string(),
        label: default_label(name),
        description: description.to_string(),
        field_type,
        icon: Some(icon.to_string()),
        placement,
        source: CatalogSource::Default,
    }
}

fn action(name: &str, label: &str, prompt: &str, icon: &str, description: &str) -> CatalogAction {
    CatalogAction {
        name: name.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        prompt: prompt.to_string(),
        icon: Some(icon.to_string()),
        placement: Placement::Card,
        confirm: false,
        // No default is always offered (#2418 D15 removed Explain, the one
        // that was); a user's catalog can still set `always`.
        always: false,
        source: CatalogSource::Default,
    }
}

/// The deck's default fields, in catalog order.
pub fn default_fields() -> Vec<CatalogField> {
    vec![
        field(
            "progress",
            FieldType::Text,
            Placement::Card,
            "activity",
            "What you are doing now, in a few words.",
        ),
        field(
            "next-step",
            FieldType::Text,
            Placement::Card,
            "arrow-right",
            "What you will do next, or what you are waiting for.",
        ),
        field(
            "details",
            FieldType::LongText,
            Placement::Detail,
            "file-text",
            "Anything the user should know that does not fit the summary.",
        ),
        field(
            "pull-request",
            FieldType::PullRequest,
            Placement::Compact,
            "git-pull-request",
            "The pull request you opened or are working on.",
        ),
        field(
            "issue",
            FieldType::Issue,
            Placement::Card,
            "circle-dot",
            "The issue you are working on.",
        ),
        field(
            "links",
            FieldType::List,
            Placement::Detail,
            "link",
            "Other links the user may want.",
        ),
    ]
}

/// Names that were deck defaults once and are no longer (issue #2418 D15:
/// `explain`). A user's `remove` entry naming one still says what it always
/// said, so it is ignored rather than reported as naming no default.
pub const RETIRED_DEFAULT_NAMES: &[&str] = &["explain"];

/// The deck's default actions, in catalog order.
pub fn default_actions() -> Vec<CatalogAction> {
    vec![
        action(
            "go-ahead",
            "Go ahead",
            "Go ahead.",
            "play",
            "Offer when you are waiting for the user's approval to continue.",
        ),
        action(
            "merge",
            "Merge it",
            "Merge it, following this project's own merge procedure, then update your card.",
            "git-merge",
            "Offer when your work is ready to merge.",
        ),
    ]
}

/// The defaults alone, as a resolved catalog with no errors.
pub fn defaults() -> ResolvedCatalog {
    ResolvedCatalog {
        fields: default_fields(),
        actions: default_actions(),
        errors: Vec::new(),
        user_config: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fields,
    Actions,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Fields => "catalog.fields",
            Kind::Actions => "catalog.actions",
        }
    }
}

/// `catalog.fields "risk"` or `catalog.fields[2]` when there is no usable
/// name.
fn entry_label(kind: Kind, index: usize, name: Option<&str>) -> String {
    match name {
        Some(name) if !name.is_empty() && !has_disallowed_control(name, false) => {
            format!(
                "{} {:?}",
                kind.table(),
                truncate_chars(name, MAX_NAME_CHARS + 8)
            )
        }
        _ => format!("{}[{index}]", kind.table()),
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn type_list() -> String {
    FieldType::ALL
        .iter()
        .map(|t| t.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Read a string key; an error string when it is present and not a string.
fn string_key(table: &toml::Table, key: &str, label: &str) -> Result<Option<String>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{label}: \"{key}\" must be a string")),
    }
}

fn bool_key(table: &toml::Table, key: &str, label: &str) -> Result<Option<bool>, String> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Boolean(b)) => Ok(Some(*b)),
        Some(_) => Err(format!("{label}: \"{key}\" must be true or false")),
    }
}

fn check_text(
    value: &str,
    key: &str,
    max: usize,
    multiline: bool,
    label: &str,
) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{label}: \"{key}\" is empty"));
    }
    if value.chars().count() > max {
        return Err(format!(
            "{label}: \"{key}\" is longer than {max} characters"
        ));
    }
    if has_disallowed_control(value, multiline) {
        return Err(format!(
            "{label}: \"{key}\" contains a control character{}",
            if multiline { "" } else { " or a line break" }
        ));
    }
    Ok(())
}

fn common_checks(
    table: &toml::Table,
    kind: Kind,
    allowed: &[&str],
    label: &str,
) -> Result<String, String> {
    if let Some(unknown) = table.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(format!(
            "{label}: unknown key {unknown:?}; expected one of {}",
            allowed.join(", ")
        ));
    }
    let Some(name) = string_key(table, "name", label)? else {
        return Err(format!("{label}: missing required key \"name\""));
    };
    if !is_valid_name(&name) {
        return Err(format!(
            "{label}: bad name; use 1 to {MAX_NAME_CHARS} lowercase letters, digits and '-', starting with a letter or digit"
        ));
    }
    let reserved = match kind {
        Kind::Fields => is_reserved_field_name(&name),
        Kind::Actions => is_reserved_action_name(&name),
    };
    if reserved {
        return Err(format!("{label}: {name:?} is a reserved name"));
    }
    match string_key(table, "description", label)? {
        None => return Err(format!("{label}: missing required key \"description\"")),
        Some(d) => check_text(&d, "description", MAX_DESCRIPTION_CHARS, false, label)?,
    }
    if let Some(l) = string_key(table, "label", label)? {
        check_text(&l, "label", MAX_CATALOG_LABEL_CHARS, false, label)?;
    }
    if let Some(icon) = string_key(table, "icon", label)?
        && !is_valid_icon(&icon)
    {
        return Err(format!(
            "{label}: bad icon {:?}; use 1 to {MAX_ICON_CHARS} lowercase letters, digits and '-'",
            truncate_chars(&icon, MAX_ICON_CHARS + 8)
        ));
    }
    if let Some(p) = string_key(table, "placement", label)?
        && Placement::parse(&p).is_none()
    {
        return Err(format!(
            "{label}: unknown placement {:?}; expected one of compact, card, detail",
            truncate_chars(&p, 40)
        ));
    }
    Ok(name)
}

const FIELD_KEYS: &[&str] = &["name", "description", "type", "label", "icon", "placement"];
const ACTION_KEYS: &[&str] = &[
    "name",
    "description",
    "prompt",
    "label",
    "icon",
    "placement",
    "confirm",
    "always",
];

fn parse_field(table: &toml::Table, label: &str) -> Result<CatalogField, String> {
    let name = common_checks(table, Kind::Fields, FIELD_KEYS, label)?;
    let label = entry_label(Kind::Fields, 0, Some(&name));
    let field_type = match string_key(table, "type", &label)? {
        None => return Err(format!("{label}: missing required key \"type\"")),
        Some(t) => FieldType::parse(&t).ok_or_else(|| {
            format!(
                "{label}: unknown type {:?}; expected one of {}",
                truncate_chars(&t, 40),
                type_list()
            )
        })?,
    };
    Ok(CatalogField {
        label: string_key(table, "label", &label)?.unwrap_or_else(|| default_label(&name)),
        description: string_key(table, "description", &label)?.unwrap_or_default(),
        field_type,
        icon: string_key(table, "icon", &label)?,
        placement: string_key(table, "placement", &label)?
            .and_then(|p| Placement::parse(&p))
            .unwrap_or(Placement::Card),
        source: CatalogSource::User,
        name,
    })
}

fn parse_action(table: &toml::Table, label: &str) -> Result<CatalogAction, String> {
    let name = common_checks(table, Kind::Actions, ACTION_KEYS, label)?;
    let label = entry_label(Kind::Actions, 0, Some(&name));
    let prompt = match string_key(table, "prompt", &label)? {
        None => return Err(format!("{label}: missing required key \"prompt\"")),
        Some(p) => {
            check_text(&p, "prompt", MAX_PROMPT_CHARS, true, &label)?;
            p
        }
    };
    Ok(CatalogAction {
        label: string_key(table, "label", &label)?.unwrap_or_else(|| default_label(&name)),
        description: string_key(table, "description", &label)?.unwrap_or_default(),
        prompt,
        icon: string_key(table, "icon", &label)?,
        placement: string_key(table, "placement", &label)?
            .and_then(|p| Placement::parse(&p))
            .unwrap_or(Placement::Card),
        confirm: bool_key(table, "confirm", &label)?.unwrap_or(false),
        always: bool_key(table, "always", &label)?.unwrap_or(false),
        source: CatalogSource::User,
        name,
    })
}

/// The user's entries of one kind, parsed and validated; problems go to
/// `errors` and the entry is skipped. A second entry with a name already seen
/// in the file is skipped too.
fn user_entries<T>(
    catalog: &toml::Table,
    kind: Kind,
    parse: impl Fn(&toml::Table, &str) -> Result<T, String>,
    name_of: impl Fn(&T) -> &str,
    errors: &mut Vec<String>,
) -> Vec<T> {
    let key = match kind {
        Kind::Fields => "fields",
        Kind::Actions => "actions",
    };
    let Some(value) = catalog.get(key) else {
        return Vec::new();
    };
    let Some(array) = value.as_array() else {
        errors.push(format!(
            "{}: must be an array of tables ([[{}]])",
            kind.table(),
            kind.table()
        ));
        return Vec::new();
    };
    let mut out: Vec<T> = Vec::new();
    for (index, item) in array.iter().enumerate() {
        let Some(table) = item.as_table() else {
            errors.push(format!("{}[{index}]: must be a table", kind.table()));
            continue;
        };
        let name = table.get("name").and_then(|v| v.as_str());
        let label = entry_label(kind, index, name);
        match parse(table, &label) {
            Ok(entry) => {
                if out.iter().any(|e| name_of(e) == name_of(&entry)) {
                    errors.push(format!(
                        "{label}: duplicate name in the user catalog; the later entry is ignored"
                    ));
                } else {
                    out.push(entry);
                }
            }
            Err(error) => errors.push(error),
        }
    }
    out
}

/// Apply user entries over `base`: a same-name entry replaces the base entry
/// whole, in place; a new name is appended in file order.
fn merge<T>(base: &mut Vec<T>, user: Vec<T>, name_of: impl Fn(&T) -> &str) {
    for entry in user {
        match base.iter().position(|b| name_of(b) == name_of(&entry)) {
            Some(at) => base[at] = entry,
            None => base.push(entry),
        }
    }
}

/// The `[catalog]` tables of a user config that is not valid TOML as a whole:
/// every line from a table header whose first key is `catalog`
/// (`[catalog]`, `[[catalog.fields]]`, `[catalog.x]`, …) up to the next header
/// whose first key is not, so a broken setting in another table does not hide
/// the catalog. `None` when the file has no such header.
///
/// The file is split with `toml`'s own lexer (`toml_parser`), not a scanner
/// of ours, so a string, a multi-line string (closing quote runs and escapes
/// included) or a comment ends exactly where the real parser ends it, and a
/// `[line]` inside one is never read as a header. Headers are recognised only
/// at the start of a line and outside multi-line arrays. A `catalog` written
/// as root-level dotted keys or an inline table is not found here; the caller
/// then reports the file and applies the defaults.
fn catalog_sections(raw: &str) -> Option<String> {
    use toml_parser::lexer::TokenKind;
    let tokens = toml_parser::Source::new(raw).lex().into_vec();
    let mut depth: i32 = 0;
    let mut in_catalog = false;
    let mut found = false;
    let mut out = String::new();
    let mut start = 0;
    while start < tokens.len() {
        // One line: up to and including its newline. A multi-line string is
        // a single token, so a newline inside one never ends a line here.
        let end = tokens[start..]
            .iter()
            .position(|t| matches!(t.kind(), TokenKind::Newline | TokenKind::Eof))
            .map_or(tokens.len(), |at| start + at + 1);
        let line = &tokens[start..end];
        start = end;
        // A bare-key header (`[catalog]`) cannot be an array element, so it
        // also ends an array left open by a typo above it.
        if let Some((first, quoted)) = header_first_key(raw, line)
            && (depth == 0 || !(quoted || is_value_word(&first)))
        {
            depth = 0;
            in_catalog = first == "catalog";
            found |= in_catalog;
        } else {
            for token in line {
                match token.kind() {
                    TokenKind::LeftSquareBracket => depth += 1,
                    TokenKind::RightSquareBracket => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
        }
        if in_catalog && let (Some(first), Some(last)) = (line.first(), line.last()) {
            out.push_str(&raw[first.span().start()..last.span().end()]);
        }
    }
    found.then_some(out)
}

/// Whether a bare word in brackets could be an array element rather than a
/// key: a number or a boolean.
fn is_value_word(word: &str) -> bool {
    matches!(word, "true" | "false" | "inf" | "nan")
        || word
            .chars()
            .all(|c| c.is_ascii_digit() || c == '_' || c == '-')
}

/// The first dotted key of a line that is a table header (`[a.b]` /
/// `[[a.b]]`, with an optional trailing comment), unquoted, and whether it was
/// quoted; `None` when the line is not a header. `line` is one line of
/// `toml_parser` tokens of `raw`.
fn header_first_key(raw: &str, line: &[toml_parser::lexer::Token]) -> Option<(String, bool)> {
    use toml_parser::lexer::TokenKind;
    let text = |t: &toml_parser::lexer::Token| &raw[t.span().start()..t.span().end()];
    let mut tokens = line
        .iter()
        .filter(|t| t.kind() != TokenKind::Whitespace)
        .peekable();
    let open = tokens.next()?;
    if open.kind() != TokenKind::LeftSquareBracket {
        return None;
    }
    let double = tokens.peek().is_some_and(|t| {
        t.kind() == TokenKind::LeftSquareBracket && t.span().start() == open.span().end()
    });
    if double {
        tokens.next();
    }
    // `key ( . key )*`, where a key is a bare word or a one-line string.
    let mut first: Option<(String, bool)> = None;
    let mut want_key = true;
    let close = loop {
        let token = tokens.next()?;
        match (token.kind(), want_key) {
            (TokenKind::Atom, true) => {
                let word = text(token);
                let bare = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
                if !word.chars().all(bare) {
                    return None;
                }
                first.get_or_insert((word.to_string(), false));
            }
            (TokenKind::BasicString | TokenKind::LiteralString, true) => {
                let quoted = text(token);
                let quote = if token.kind() == TokenKind::BasicString {
                    '"'
                } else {
                    '\''
                };
                // An unterminated string stops at the newline.
                if quoted.len() < 2 || !quoted.ends_with(quote) {
                    return None;
                }
                first.get_or_insert((quoted[1..quoted.len() - 1].to_string(), true));
            }
            (TokenKind::Dot, false) => {}
            (TokenKind::RightSquareBracket, false) => break token,
            _ => return None,
        }
        want_key = !want_key;
    };
    if double {
        let second = tokens.next()?;
        if second.kind() != TokenKind::RightSquareBracket
            || second.span().start() != close.span().end()
        {
            return None;
        }
    }
    let mut rest = tokens.map(|t| t.kind());
    let mut next = rest.next();
    if next == Some(TokenKind::Comment) {
        next = rest.next();
    }
    matches!(next, None | Some(TokenKind::Newline | TokenKind::Eof)).then_some(first?)
}

/// Resolve the catalog from the user config's raw TOML (`None`: no file).
///
/// Start from the defaults; apply `remove` (a name matching no default is an
/// error, and ignored); then each user entry whose name matches an existing
/// entry of the same kind replaces it whole, and a new name is appended in
/// file order. Fields and actions are separate namespaces.
pub fn resolve(user_config_toml: Option<&str>) -> ResolvedCatalog {
    let mut resolved = defaults();
    let Some(raw) = user_config_toml else {
        return resolved;
    };
    let document: toml::Table = match toml::from_str(raw) {
        Ok(doc) => doc,
        Err(error) => {
            let first_line = error.to_string();
            let first_line = first_line.lines().next().unwrap_or_default().to_string();
            let first_line = truncate_chars(&first_line, 200);
            // A typo elsewhere in the file need not cost the user their
            // catalog: read the `[catalog…]` tables on their own. Either way
            // the broken file is reported, so nothing here is silent.
            match catalog_sections(raw).and_then(|text| toml::from_str::<toml::Table>(&text).ok()) {
                Some(doc) => {
                    resolved.errors.push(format!(
                        "catalog: the user config is not valid TOML ({first_line}); its [catalog] \
                         tables were read on their own and apply, but fix the file"
                    ));
                    doc
                }
                None => {
                    resolved.errors.push(format!(
                        "catalog: the user config could not be read as TOML, so the deck defaults apply: {first_line}"
                    ));
                    return resolved;
                }
            }
        }
    };
    let Some(catalog) = document.get("catalog") else {
        return resolved;
    };
    let Some(catalog) = catalog.as_table() else {
        resolved
            .errors
            .push("catalog: [catalog] must be a table".to_string());
        return resolved;
    };
    let errors = &mut resolved.errors;
    if let Some(unknown) = catalog
        .keys()
        .find(|k| !["remove", "fields", "actions"].contains(&k.as_str()))
    {
        errors.push(format!(
            "catalog: unknown key {unknown:?}; expected one of remove, fields, actions"
        ));
    }
    // 2. `remove`.
    match catalog.get("remove") {
        None => {}
        Some(toml::Value::Array(names)) => {
            for name in names {
                let Some(name) = name.as_str() else {
                    errors.push("catalog.remove: every entry must be a string".to_string());
                    continue;
                };
                let before = resolved.fields.len() + resolved.actions.len();
                resolved.fields.retain(|f| f.name != name);
                resolved.actions.retain(|a| a.name != name);
                if resolved.fields.len() + resolved.actions.len() == before
                    && !RETIRED_DEFAULT_NAMES.contains(&name)
                {
                    errors.push(format!(
                        "catalog.remove: {:?} names no deck default field or action",
                        truncate_chars(name, MAX_NAME_CHARS + 8)
                    ));
                }
            }
        }
        Some(_) => errors.push("catalog.remove: must be an array of names".to_string()),
    }
    // 3. Replacements and additions.
    let fields = user_entries(catalog, Kind::Fields, parse_field, |f| &f.name, errors);
    let actions = user_entries(catalog, Kind::Actions, parse_action, |a| &a.name, errors);
    merge(&mut resolved.fields, fields, |f| &f.name);
    merge(&mut resolved.actions, actions, |a| &a.name);
    if resolved.fields.len() > MAX_CATALOG_FIELDS {
        for dropped in resolved.fields.drain(MAX_CATALOG_FIELDS..) {
            resolved.errors.push(format!(
                "catalog.fields {:?}: dropped; a catalog holds at most {MAX_CATALOG_FIELDS} fields",
                dropped.name
            ));
        }
    }
    if resolved.actions.len() > MAX_CATALOG_ACTIONS {
        for dropped in resolved.actions.drain(MAX_CATALOG_ACTIONS..) {
            resolved.errors.push(format!(
                "catalog.actions {:?}: dropped; a catalog holds at most {MAX_CATALOG_ACTIONS} actions",
                dropped.name
            ));
        }
    }
    resolved
}

/// The longest user config the catalog reader reads.
const MAX_USER_CONFIG_BYTES: u64 = 1024 * 1024;

/// Resolve the catalog from the user config file
/// (`~/.config/dot-agent-deck/config.toml`, or `DOT_AGENT_DECK_CONFIG`). A
/// missing file is the defaults with no error; an unreadable one is the
/// defaults with one.
pub fn resolve_from_user_config() -> ResolvedCatalog {
    resolve_from_user_config_at(&crate::config::user_config_path())
}

/// [`resolve_from_user_config`] for the file at `path`, read as an edit reads
/// it (`read_user_config`): a FIFO, a device or a folder there is refused
/// without waiting, and a link whose target is missing is an unreadable
/// config rather than a missing one.
pub fn resolve_from_user_config_at(path: &std::path::Path) -> ResolvedCatalog {
    let display = path.display().to_string();
    let mut resolved = match read_user_config(path) {
        Ok(ConfigRead { raw: Some(raw), .. }) => resolve(Some(&raw)),
        Ok(ConfigRead { raw: None, .. }) => {
            let mut resolved = defaults();
            resolved.user_config = None;
            return resolved;
        }
        Err(reason) => {
            let mut resolved = defaults();
            resolved.errors.push(format!(
                "catalog: the user config {display} could not be read, so the deck defaults apply: {reason}"
            ));
            resolved
        }
    };
    resolved.user_config = Some(display);
    resolved
}

/// Issue #2399 (PR #2491 scope extension, PRD #2374 decision 2026-11-24): one
/// change the desktop app's card-action editor asks the daemon to make to the
/// `[catalog]` actions of the user config the daemon reads. The request names
/// no file and no key outside one action: the daemon writes only its own user
/// config's catalog actions (`[[catalog.actions]]` and `catalog.remove`).
///
/// - `add`: a new user action; its name must not be a deck default's or an
///   existing user action's.
/// - `update`: replace the user action of that name, or override the deck
///   default of that name with a user entry of the same name.
/// - `remove`: delete a user action that overrides no default (a default is
///   hidden, not removed).
/// - `hide`: hide a deck default (its name in `catalog.remove`, and any user
///   override of it gone).
/// - `restore`: bring a deck default back as the deck ships it (its name out
///   of `catalog.remove`, and any user override of it gone).
///
/// `update`, `remove`, `hide` and `restore` may carry `base` (issue #2399
/// re-review N5): the action of that name as the editor read it, or `null`
/// when it read none. With a `base`, the edit is refused when the file's
/// action of that name is not that one any more — another program or another
/// editor changed it since the editor's read — and nothing is written. An
/// edit with no `base` (an app from before it) is applied to the action as
/// the file has it. Additive: a daemon from before it ignores the key.
///
/// [`apply_action_edit`] applies one; the daemon's `card-catalog-edit` request
/// carries it ([`edit_user_catalog_at`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogActionEdit {
    Add {
        action: CatalogActionInput,
    },
    Update {
        action: CatalogActionInput,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "base_as_read"
        )]
        base: Option<Option<EditableCatalogAction>>,
    },
    Remove {
        name: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "base_as_read"
        )]
        base: Option<Option<EditableCatalogAction>>,
    },
    Hide {
        name: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "base_as_read"
        )]
        base: Option<Option<EditableCatalogAction>>,
    },
    Restore {
        name: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "base_as_read"
        )]
        base: Option<Option<EditableCatalogAction>>,
    },
}

impl CatalogActionEdit {
    /// The name the edit acts on, and the action as the editor read it, when
    /// it says (`Some(None)`: it read none).
    fn base(&self) -> (&str, Option<Option<&EditableCatalogAction>>) {
        match self {
            Self::Add { action } => (&action.name, None),
            Self::Update { action, base } => (&action.name, base.as_ref().map(Option::as_ref)),
            Self::Remove { name, base }
            | Self::Hide { name, base }
            | Self::Restore { name, base } => (name, base.as_ref().map(Option::as_ref)),
        }
    }
}

/// A `base` that is present: `null` is "the editor read no such action"
/// (`Some(None)`), told apart from a missing key (`None`, no check).
fn base_as_read<'de, D>(deserializer: D) -> Result<Option<Option<EditableCatalogAction>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<EditableCatalogAction>::deserialize(deserializer).map(Some)
}

/// One action as the editor's form sends it: every key a `[[catalog.actions]]`
/// entry may carry, and nothing else (an unknown key is refused).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogActionInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub description: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<Placement>,
    #[serde(default)]
    pub confirm: bool,
    #[serde(default)]
    pub always: bool,
}

/// One action as the editor lists it: the resolved action (its `source` says
/// whether the deck's default or the user's entry is in effect), whether a
/// deck default has its name, whether the user's entry overrides that
/// default, and whether the user's `remove` hides it (a hidden default is
/// listed with the deck's own values, so it can be restored).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditableCatalogAction {
    #[serde(flatten)]
    pub action: CatalogAction,
    pub default: bool,
    pub overridden: bool,
    pub hidden: bool,
}

/// The catalog actions the editor shows: deck defaults (hidden ones included)
/// in catalog order, then the user's own in file order; never the deck's own
/// actions (reply, open, close, close keeping the worktree). `errors` are the
/// catalog's problems, as [`ResolvedCatalog::errors`]; `user_config` is the
/// file read, when there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditableCardCatalog {
    pub actions: Vec<EditableCatalogAction>,
    pub errors: Vec<String>,
    #[serde(default)]
    pub user_config: Option<String>,
}

/// The editable catalog of a user config's raw TOML (`None`: no file): every
/// deck default in catalog order — the user's override in its place when there
/// is one, the deck's own values when the user's `remove` hides it — then the
/// user's own actions in file order. The deck's own actions are never listed:
/// a catalog cannot name them. `user_config` is left `None`; the caller that
/// read a file names it.
pub fn editable_catalog(user_config_toml: Option<&str>) -> EditableCardCatalog {
    let resolved = resolve(user_config_toml);
    let defaults = default_actions();
    let mut actions: Vec<EditableCatalogAction> = defaults
        .iter()
        .map(|default| match resolved.action(&default.name) {
            Some(in_effect) => EditableCatalogAction {
                overridden: in_effect.source == CatalogSource::User,
                action: in_effect.clone(),
                default: true,
                hidden: false,
            },
            None => EditableCatalogAction {
                action: default.clone(),
                default: true,
                overridden: false,
                hidden: true,
            },
        })
        .collect();
    actions.extend(
        resolved
            .actions
            .iter()
            .filter(|a| !defaults.iter().any(|d| d.name == a.name))
            .map(|a| EditableCatalogAction {
                action: a.clone(),
                default: false,
                overridden: false,
                hidden: false,
            }),
    );
    EditableCardCatalog {
        actions,
        errors: resolved.errors,
        user_config: None,
    }
}

/// A name as an edit's refusal quotes it: escaped, and cut short.
fn quoted_name(name: &str) -> String {
    format!("{:?}", truncate_chars(name, MAX_NAME_CHARS + 8))
}

/// `input` as the `[[catalog.actions]]` entry it becomes, for the catalog's
/// own validation ([`parse_action`]).
fn input_table(input: &CatalogActionInput) -> toml::Table {
    let mut table = toml::Table::new();
    table.insert("name".into(), input.name.clone().into());
    if let Some(label) = &input.label {
        table.insert("label".into(), label.clone().into());
    }
    table.insert("description".into(), input.description.clone().into());
    table.insert("prompt".into(), input.prompt.clone().into());
    if let Some(icon) = &input.icon {
        table.insert("icon".into(), icon.clone().into());
    }
    if let Some(placement) = input.placement {
        table.insert("placement".into(), placement.as_str().into());
    }
    if input.confirm {
        table.insert("confirm".into(), true.into());
    }
    if input.always {
        table.insert("always".into(), true.into());
    }
    table
}

/// The action `input` describes, validated exactly as a `[[catalog.actions]]`
/// entry in the file is: name syntax, reserved names, the description, label
/// and prompt (required where required, length limits, no control characters
/// beyond a prompt's line breaks and tabs), the icon's syntax.
fn validated(input: &CatalogActionInput) -> Result<CatalogAction, String> {
    let label = entry_label(Kind::Actions, 0, Some(&input.name));
    parse_action(&input_table(input), &label)
}

/// Write `input`'s keys into the entry `table`: a key the input sets keeps its
/// place and comments in the file. A key it leaves unset is removed — unless
/// keeping it changes nothing the catalog reads, as `always = false` or
/// `placement = "card"` when the form leaves them at that default: that key
/// stays, with its comment (issue #2399 review 7). Any key an action entry may
/// not carry is removed (it would make the catalog skip the entry the user
/// just saved).
fn write_entry(table: &mut toml_edit::Table, input: &CatalogActionInput) {
    let wanted = input_table(input);
    let means = validated(input).ok();
    let changes_nothing = |key: &str, item: &toml_edit::Item| {
        let value: toml::Value = match item.as_value() {
            Some(toml_edit::Value::String(s)) => s.value().clone().into(),
            Some(toml_edit::Value::Boolean(b)) => (*b.value()).into(),
            _ => return false,
        };
        let mut with = wanted.clone();
        with.insert(key.to_string(), value);
        let label = entry_label(Kind::Actions, 0, Some(&input.name));
        means.is_some() && parse_action(&with, &label).ok() == means
    };
    let stale: Vec<String> = table
        .iter()
        .filter(|(key, item)| !wanted.contains_key(*key) && !changes_nothing(key, item))
        .map(|(key, _)| key.to_string())
        .collect();
    for key in stale {
        table.remove(&key);
    }
    for key in ACTION_KEYS {
        let Some(value) = wanted.get(*key) else {
            continue;
        };
        let mut value = match value {
            toml::Value::String(s) => toml_edit::Value::from(s.as_str()),
            toml::Value::Boolean(b) => toml_edit::Value::from(*b),
            _ => continue,
        };
        match table.get_mut(key) {
            Some(slot) => {
                if let Some(old) = slot.as_value() {
                    *value.decor_mut() = old.decor().clone();
                }
                *slot = toml_edit::Item::Value(value);
            }
            None => {
                table.insert(key, toml_edit::Item::Value(value));
            }
        }
    }
}

/// The `[catalog]` table of `doc`, made (with no header of its own) when the
/// file has none. Refused when the file writes `catalog` some other way — an
/// inline table or dotted keys — which this editor does not rewrite.
fn catalog_table(doc: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::Table, String> {
    if !doc.contains_key("catalog") {
        let mut table = toml_edit::Table::new();
        table.set_implicit(true);
        doc.insert("catalog", toml_edit::Item::Table(table));
    }
    match doc.get_mut("catalog").and_then(|c| c.as_table_mut()) {
        Some(table) if !table.is_dotted() => Ok(table),
        _ => Err(
            "the user config writes its catalog in a form this editor does not change (not a \
             [catalog] table); edit the file by hand"
                .to_string(),
        ),
    }
}

/// The user's `[[catalog.actions]]` entries; made empty when `create` and
/// there are none. Refused when `catalog.actions` is written some other way.
fn action_tables(
    catalog: &mut toml_edit::Table,
    create: bool,
) -> Result<Option<&mut toml_edit::ArrayOfTables>, String> {
    if !catalog.contains_key("actions") {
        if !create {
            return Ok(None);
        }
        catalog.insert(
            "actions",
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }
    match catalog.get_mut("actions") {
        Some(toml_edit::Item::ArrayOfTables(tables)) => Ok(Some(tables)),
        _ => Err(
            "the user config's catalog actions are not written as [[catalog.actions]] tables; \
             edit the file by hand"
                .to_string(),
        ),
    }
}

fn entry_name(table: &toml_edit::Table) -> Option<&str> {
    table.get("name").and_then(|n| n.as_str())
}

/// Every user entry named `name` dropped, and `catalog.actions` with them when
/// none is left.
fn drop_user_entries(catalog: &mut toml_edit::Table, name: &str) -> Result<usize, String> {
    let Some(tables) = action_tables(catalog, false)? else {
        return Ok(0);
    };
    let before = tables.len();
    tables.retain(|t| entry_name(t) != Some(name));
    let dropped = before - tables.len();
    if tables.is_empty() {
        catalog.remove("actions");
    }
    Ok(dropped)
}

/// `name` added to (`hide`) or taken out of `catalog.remove`; the key goes when
/// it is left empty. Refused when `catalog.remove` is not an array of names.
fn set_removed(catalog: &mut toml_edit::Table, name: &str, hide: bool) -> Result<(), String> {
    if !catalog.contains_key("remove") {
        if !hide {
            return Ok(());
        }
        catalog.insert("remove", toml_edit::value(toml_edit::Array::new()));
    }
    let Some(names) = catalog.get_mut("remove").and_then(|r| r.as_array_mut()) else {
        return Err(
            "the user config's catalog.remove is not an array of names; edit the file by hand"
                .to_string(),
        );
    };
    if names.iter().any(|n| n.as_str().is_none()) {
        return Err(
            "the user config's catalog.remove holds something other than names; edit the file \
             by hand"
                .to_string(),
        );
    }
    if hide {
        if !names.iter().any(|n| n.as_str() == Some(name)) {
            names.push(name);
        }
    } else {
        names.retain(|n| n.as_str() != Some(name));
        if names.is_empty() {
            catalog.remove("remove");
        }
    }
    Ok(())
}

/// The user config's raw TOML (`None`: no file) with `edit` applied to its
/// `[catalog]` actions — `[[catalog.actions]]` and `catalog.remove` — and
/// nothing else changed: every other setting and table, the catalog's fields
/// and comments are kept as written. `Err` with the reason, and nothing to
/// write, when the edit is invalid (the catalog's own validation) or cannot
/// apply, when the file is not valid TOML, or when the result would be larger
/// than the catalog reader reads or would add a catalog problem.
pub fn apply_action_edit(
    user_config_toml: Option<&str>,
    edit: &CatalogActionEdit,
) -> Result<String, String> {
    let mut doc = match user_config_toml {
        None => toml_edit::DocumentMut::new(),
        Some(raw) => raw.parse::<toml_edit::DocumentMut>().map_err(|e| {
            let message = e.message().to_string();
            let first = message.lines().next().unwrap_or_default();
            format!(
                "the user config is not valid TOML ({}); fix it by hand first, because saving \
                 would replace everything it holds",
                truncate_chars(first, 200)
            )
        })?,
    };
    let before = resolve(user_config_toml);
    if let (name, Some(base)) = edit.base() {
        let listed = editable_catalog(user_config_toml);
        if listed.actions.iter().find(|a| a.action.name == name) != base {
            return Err(format!(
                "{} changed since the editor read it (another program or editor saved the user \
                 config); nothing was saved: press Refresh to read it again, then make the \
                 change again",
                quoted_name(name)
            ));
        }
    }
    let defaults = default_actions();
    let is_default = |name: &str| defaults.iter().any(|d| d.name == name);
    let catalog = catalog_table(&mut doc)?;
    // The action the edit must leave in effect, checked after the write.
    let mut expect: Option<CatalogAction> = None;
    match edit {
        CatalogActionEdit::Add { action } => {
            let parsed = validated(action)?;
            if is_default(&action.name) {
                return Err(format!(
                    "{} is a deck default; edit it instead of adding it",
                    quoted_name(&action.name)
                ));
            }
            let tables = action_tables(catalog, true)?.expect("made when missing");
            if tables.iter().any(|t| entry_name(t) == Some(&action.name)) {
                return Err(format!(
                    "an action named {} already exists; edit it instead",
                    quoted_name(&action.name)
                ));
            }
            if before.actions.len() >= MAX_CATALOG_ACTIONS {
                return Err(format!(
                    "the catalog already holds {MAX_CATALOG_ACTIONS} actions, the most it can; \
                     remove or hide one first"
                ));
            }
            let mut table = toml_edit::Table::new();
            write_entry(&mut table, action);
            tables.push(table);
            expect = Some(parsed);
        }
        CatalogActionEdit::Update { action, .. } => {
            let parsed = validated(action)?;
            let default = is_default(&action.name);
            if default {
                // An edited default is in effect, so it is no longer hidden.
                set_removed(catalog, &action.name, false)?;
            }
            let exists = action_tables(catalog, false)?
                .is_some_and(|tables| tables.iter().any(|t| entry_name(t) == Some(&action.name)));
            if exists {
                update_entry(catalog, action)?;
            } else if default {
                let tables = action_tables(catalog, true)?.expect("made when missing");
                let mut table = toml_edit::Table::new();
                write_entry(&mut table, action);
                tables.push(table);
            } else {
                return Err(format!(
                    "no action named {} in your catalog to edit",
                    quoted_name(&action.name)
                ));
            }
            expect = Some(parsed);
        }
        CatalogActionEdit::Remove { name, .. } => {
            if is_default(name) {
                return Err(format!(
                    "{} is a deck default; hide it instead of removing it",
                    quoted_name(name)
                ));
            }
            if drop_user_entries(catalog, name)? == 0 {
                return Err(format!(
                    "no action named {} in your catalog to remove",
                    quoted_name(name)
                ));
            }
        }
        CatalogActionEdit::Hide { name, .. } => {
            if !is_default(name) {
                return Err(format!(
                    "{} is not a deck default; only a deck default can be hidden (remove your \
                     own action instead)",
                    quoted_name(name)
                ));
            }
            drop_user_entries(catalog, name)?;
            set_removed(catalog, name, true)?;
        }
        CatalogActionEdit::Restore { name, .. } => {
            if !is_default(name) {
                return Err(format!(
                    "{} is not a deck default; only a deck default can be restored",
                    quoted_name(name)
                ));
            }
            drop_user_entries(catalog, name)?;
            set_removed(catalog, name, false)?;
        }
    }
    let saved = doc.to_string();
    if saved.len() as u64 > MAX_USER_CONFIG_BYTES {
        return Err(format!(
            "the user config would be larger than {MAX_USER_CONFIG_BYTES} bytes, more than the \
             deck reads"
        ));
    }
    let after = resolve(Some(&saved));
    let added = problems_added(&before.errors, &after.errors);
    if !added.is_empty() {
        return Err(added.join("; "));
    }
    if let Some(expected) = expect
        && after.action(&expected.name) != Some(&expected)
    {
        return Err(format!(
            "the edit to {} would not take effect; edit the file by hand",
            quoted_name(&expected.name)
        ));
    }
    Ok(saved)
}

/// The problems in `after` that `before` did not have, compared without the
/// position an entry with no usable name is reported by (`catalog.actions[2]`):
/// an edit that adds or removes an entry before it moves that position, and
/// the problem is still the one the file already had (PR #2491 review). Each
/// problem in `before` accounts for one in `after`, so a second entry with the
/// same problem is new. The problems returned are worded as `after` has them.
fn problems_added<'a>(before: &[String], after: &'a [String]) -> Vec<&'a str> {
    let mut had: Vec<String> = before.iter().map(|e| without_positions(e)).collect();
    after
        .iter()
        .filter(|e| {
            let problem = without_positions(e);
            match had.iter().position(|h| *h == problem) {
                Some(at) => {
                    had.swap_remove(at);
                    false
                }
                None => true,
            }
        })
        .map(String::as_str)
        .collect()
}

/// `problem` with the index taken out of every `catalog.fields[N]` and
/// `catalog.actions[N]` in it, as [`entry_label`] writes an unnamed entry.
fn without_positions(problem: &str) -> String {
    let mut out = String::with_capacity(problem.len());
    let mut rest = problem;
    while let Some(open) = [Kind::Fields, Kind::Actions]
        .iter()
        .filter_map(|kind| {
            let prefix = format!("{}[", kind.table());
            rest.find(&prefix).map(|at| at + prefix.len())
        })
        .min()
    {
        out.push_str(&rest[..open]);
        rest = &rest[open..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits > 0 && rest[digits..].starts_with(']') {
            rest = &rest[digits..];
        }
    }
    out.push_str(rest);
    out
}

/// Replace the first user entry named `action.name` with `action`, in place.
fn update_entry(catalog: &mut toml_edit::Table, action: &CatalogActionInput) -> Result<(), String> {
    let tables = action_tables(catalog, false)?.expect("an entry was found");
    let mut seen = false;
    // A later entry of the same name is already ignored by the catalog (and
    // reported); it goes, so the saved entry is the one in effect.
    tables.retain(|t| {
        if entry_name(t) != Some(&action.name) {
            return true;
        }
        !std::mem::replace(&mut seen, true)
    });
    let table = tables
        .iter_mut()
        .find(|t| entry_name(t) == Some(&action.name))
        .expect("an entry was found");
    write_entry(table, action);
    Ok(())
}

/// Which folder a path names, so a later look can tell whether it is the same
/// one: the device and inode on Unix (elsewhere every folder compares equal).
/// The file itself is compared by its [`crate::config::FileStamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

impl FileIdentity {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Self { dev: 0, ino: 0 }
        }
    }

    /// The folder `target` sits in, when it is there.
    fn of_folder(target: &std::path::Path) -> Option<Self> {
        let folder = match target.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => std::path::Path::new("."),
        };
        std::fs::metadata(folder).ok().map(|m| Self::of(&m))
    }
}

/// The user config as an edit read it: the file a write must replace (the
/// path resolved through its links, once), its text (`None`: there is no
/// file), which file it was — with its mode, owner and links — and which
/// folder held it.
struct ConfigRead {
    target: std::path::PathBuf,
    raw: Option<String>,
    file: Option<crate::config::FileStamp>,
    folder: Option<FileIdentity>,
}

/// Open `target` for reading without waiting and without following a link at
/// its last step: `O_NONBLOCK`, because opening a FIFO with no writer waits
/// for one, and `O_NOFOLLOW`, because `target` is already the end of the link
/// chain, so a link there now is a change since it was resolved.
fn open_config_file(target: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    options.open(target)
}

/// The text of the regular file open in `file` and its stamp, or the
/// reason it cannot be read as the catalog reader reads it: not a regular
/// file (a FIFO, a device, a folder), larger than [`MAX_USER_CONFIG_BYTES`],
/// or not UTF-8.
fn read_open_config(file: std::fs::File) -> Result<(String, crate::config::FileStamp), String> {
    use std::io::Read as _;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("it is not a regular file".to_string());
    }
    if meta.len() > MAX_USER_CONFIG_BYTES {
        return Err(format!("it is larger than {MAX_USER_CONFIG_BYTES} bytes"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_USER_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_USER_CONFIG_BYTES {
        return Err(format!("it is larger than {MAX_USER_CONFIG_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "it is not UTF-8 text".to_string())?;
    Ok((text, crate::config::FileStamp::of(&meta)))
}

/// Read the user config at `path` for the catalog: resolve its links once,
/// open the file they lead to without waiting ([`open_config_file`]) and read
/// it ([`read_open_config`]). No file is `raw: None` only when there is
/// genuinely nothing there (a first run, its folder perhaps missing too); a
/// link whose target is missing, or a folder above it that is such a link, is
/// a reason, not an absent config, because the file it leads to may be the
/// user's real config on a volume that is not mounted.
fn read_user_config(path: &std::path::Path) -> Result<ConfigRead, String> {
    let target = crate::config::resolve_symlink_chain(path);
    let folder = FileIdentity::of_folder(&target);
    match open_config_file(&target) {
        Ok(file) => {
            let (raw, file) = read_open_config(file)?;
            Ok(ConfigRead {
                target,
                raw: Some(raw),
                file: Some(file),
                folder,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            crate::config::config_not_found_verdict(path)?;
            Ok(ConfigRead {
                target,
                raw: None,
                file: None,
                folder,
            })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Whether the user config at `path` is still exactly what `read` found — the
/// same file behind the same links, in the same folder, with the same mode,
/// owner, group, number of links and bytes, or still no file — checked by the
/// strict save just before it renames the new file into place. A change that
/// is visible when this runs refuses the edit rather than being overwritten:
/// a save by a writer that does not take the config lock (a hand edit, an
/// older deck), a `chmod`, a new hard link. A change made after it, before
/// the rename, is not caught.
fn still_as_read(path: &std::path::Path, read: &ConfigRead) -> Result<(), String> {
    const CHANGED: &str = crate::config::CONFIG_CHANGED_SINCE_READ;
    if crate::config::resolve_symlink_chain(path) != read.target
        || FileIdentity::of_folder(&read.target) != read.folder
    {
        return Err(CHANGED.to_string());
    }
    match &read.raw {
        Some(raw) => {
            let file = open_config_file(&read.target).map_err(|_| CHANGED.to_string())?;
            match read_open_config(file) {
                Ok((now, identity)) if &now == raw && Some(identity) == read.file => Ok(()),
                _ => Err(CHANGED.to_string()),
            }
        }
        None => match std::fs::symlink_metadata(&read.target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            _ => Err(CHANGED.to_string()),
        },
    }
}

/// One edit of a user config at a time in this process, so two editors'
/// read-change-write cycles never interleave. Another process is kept out by
/// the config lock ([`crate::config::lock_config_edit`]) when it is a deck
/// writer, and a writer that takes no lock by [`still_as_read`].
static EDIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`EDIT_LOCK`], tried until `deadline` (issue #2399 final audit, section
/// 2): a request that cannot have its turn by then is answered busy rather
/// than queued behind the one waiting for the config lock. `None`: busy.
fn edit_turn_by(deadline: std::time::Instant) -> Option<std::sync::MutexGuard<'static, ()>> {
    loop {
        match EDIT_LOCK.try_lock() {
            Ok(turn) => return Some(turn),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(crate::config::CONFIG_LOCK_POLL);
            }
            Err(std::sync::TryLockError::WouldBlock) => return None,
        }
    }
}

/// Read the user config at `path` and, with `edit`, apply it
/// ([`apply_action_edit`]) and replace the file by rename
/// ([`crate::config::write_config_strictly`]); answer the editable catalog
/// afterwards, naming the file. An edit holds the config lock
/// ([`crate::config::lock_config_edit`]) from before its read until the file
/// is replaced, so `config set` and another daemon's editor wait for it
/// rather than save in between. Refused, with the file left as it was:
///
/// - a file that cannot be read — not a regular file, a link whose target is
///   missing, not UTF-8, larger than the catalog reader reads, not valid TOML
///   (a read with no edit still answers, with the problem in `errors`);
/// - an edit whose `base` is not the action as the file has it now
///   ([`apply_action_edit`]);
/// - the lock, held by another writer until the request's deadline, unsafe
///   ([`crate::config::ConfigLockProblem::Unsafe`]), or not available at all;
/// - a file that changed between the read and the save, as far as the last
///   check sees ([`still_as_read`]), or whose links now lead elsewhere;
/// - a file that cannot be replaced by a rename (another hard link, an owner
///   the swap would change, a folder that takes no new file, a folder on its
///   path other users can change): there is no in-place fallback.
///
/// One deadline, [`crate::config::CONFIG_LOCK_WAIT`] from when the request
/// starts, bounds both waits a request can make: for its turn among this
/// process's requests ([`edit_turn_by`]) and for the config lock. A request,
/// read or edit, that has not had its turn by then is answered busy, to try
/// again. Reading and writing the files has no deadline of its own.
///
/// A missing file, and its folder, are created by an edit. The daemon passes
/// its own user config's path; no path comes from a client. Every `Err` means
/// the config file was not written (a folder an edit created for a missing
/// file may remain, and so does the lock file beside the config).
pub fn edit_user_catalog_at(
    path: &std::path::Path,
    edit: Option<&CatalogActionEdit>,
) -> Result<EditableCardCatalog, String> {
    edit_user_catalog_at_with(path, edit, || {})
}

/// [`edit_user_catalog_at`] with `between` run after the file is read and the
/// edit applied, before the save: a test's other writer.
fn edit_user_catalog_at_with(
    path: &std::path::Path,
    edit: Option<&CatalogActionEdit>,
    between: impl FnOnce(),
) -> Result<EditableCardCatalog, String> {
    let deadline = std::time::Instant::now() + crate::config::CONFIG_LOCK_WAIT;
    let display = path.display().to_string();
    let Some(_one_at_a_time) = edit_turn_by(deadline) else {
        return Err(format!(
            "the user config {display} is busy: another change to it is still waiting to be \
             saved; nothing was read or saved, try again"
        ));
    };
    let named = |mut catalog: EditableCardCatalog| {
        catalog.user_config = Some(display.clone());
        catalog
    };
    let not_saved = |reason: String| format!("the user config {display} was not saved: {reason}");
    let lock = || {
        crate::config::lock_config_edit_by(path, deadline).map_err(|problem| {
            not_saved(match problem {
                crate::config::ConfigLockProblem::Busy(why)
                | crate::config::ConfigLockProblem::Unsafe(why)
                | crate::config::ConfigLockProblem::Unavailable(why) => why,
            })
        })
    };
    // The lock is taken before the read when the config's folder is there; a
    // config with no folder yet is locked once the folder is made, and the
    // last check then confirms there is still no file.
    let mut locked = match edit {
        Some(_)
            if FileIdentity::of_folder(&crate::config::resolve_symlink_chain(path)).is_some() =>
        {
            Some(lock()?)
        }
        _ => None,
    };
    let mut read = match read_user_config(path) {
        Ok(read) => read,
        Err(reason) => {
            return match edit {
                None => {
                    let mut catalog = editable_catalog(None);
                    catalog.errors.push(format!(
                        "catalog: the user config {display} could not be read, so the deck \
                         defaults apply: {reason}"
                    ));
                    Ok(named(catalog))
                }
                Some(_) => Err(format!(
                    "the user config {display} could not be read ({reason}); fix it by hand \
                     first, because saving would replace everything it holds"
                )),
            };
        }
    };
    let Some(edit) = edit else {
        return Ok(named(editable_catalog(read.raw.as_deref())));
    };
    let saved = apply_action_edit(read.raw.as_deref(), edit)?;
    between();
    if read.raw.as_deref() != Some(saved.as_str()) {
        if read.folder.is_none()
            && let Some(parent) = read.target.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                format!("could not create the folder of the user config {display}: {e}")
            })?;
            read.folder = FileIdentity::of_folder(&read.target);
        }
        if locked.is_none() {
            locked = Some(lock()?);
        }
        crate::config::write_config_strictly(
            path,
            &read.target,
            saved.as_bytes(),
            read.file,
            || still_as_read(path, &read),
        )
        .map_err(not_saved)?;
    }
    drop(locked);
    Ok(named(editable_catalog(Some(&saved))))
}

/// The text block appended to a dispatched unit's task (both launch shapes)
/// and printed by `card catalog`: what an agent card is and which fields and
/// actions the deck knows. `bin` is the deck binary's name.
pub fn dispatch_text(catalog: &ResolvedCatalog, bin: &str) -> String {
    let mut text = String::new();
    text.push_str("## Your agent card\n\n");
    text.push_str(&format!(
        "The deck shows you as a card in its overview. Keep it current with `{bin} card publish` \
         (`{bin} card publish --help` for the format; `{bin} card catalog` prints this list \
         again). Each publish replaces your card and notifies no one; it is not your completion \
         report. Agents you delegate to can publish their own cards.\n"
    ));
    if !catalog.fields.is_empty() {
        text.push_str("\nFields (report the ones that apply, by name):\n");
        for field in &catalog.fields {
            text.push_str(&format!(
                "- `{}` ({}): {}\n",
                field.name,
                field.field_type.as_str(),
                field.description
            ));
        }
    }
    let suggestable: Vec<&CatalogAction> = catalog.actions.iter().filter(|a| !a.always).collect();
    if !suggestable.is_empty() {
        text.push_str(
            "\nActions you can suggest (the user's button sends you the prompt shown):\n",
        );
        for action in suggestable {
            text.push_str(&format!("- `{}`: {}\n", action.name, action.description));
        }
    }
    text.push_str(
        "\nYou may also report fields and actions that are not listed; the user sees them as custom.\n",
    );
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_f(c: &ResolvedCatalog) -> Vec<&str> {
        c.fields.iter().map(|f| f.name.as_str()).collect()
    }

    fn names_a(c: &ResolvedCatalog) -> Vec<&str> {
        c.actions.iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn defaults_resolve_in_catalog_order_with_no_errors() {
        let c = resolve(None);
        assert_eq!(
            names_f(&c),
            [
                "progress",
                "next-step",
                "details",
                "pull-request",
                "issue",
                "links"
            ]
        );
        assert_eq!(names_a(&c), ["go-ahead", "merge"]);
        assert!(c.errors.is_empty());
        assert_eq!(c.field("next-step").unwrap().label, "Next step");
        assert_eq!(c.field("details").unwrap().placement, Placement::Detail);
        assert_eq!(
            c.field("pull-request").unwrap().field_type,
            FieldType::PullRequest
        );
        // Issue #2418 (D15): Explain is no longer a default, and no default
        // is always offered.
        assert!(c.action("explain").is_none());
        assert!(c.actions.iter().all(|a| !a.always));
        assert_eq!(c.action("merge").unwrap().label, "Merge it");
        // A config with no `[catalog]` is the defaults too.
        assert_eq!(resolve(Some("default_command = \"claude\"\n")), c);
    }

    #[test]
    fn user_entries_replace_whole_append_and_remove() {
        let c = resolve(Some(
            r#"
[catalog]
remove = ["merge", "links"]

[[catalog.fields]]
name = "risk"
description = "How risky the change is."
type = "text"
icon = "alert-triangle"

[[catalog.fields]]
name = "progress"
description = "Replaced."
type = "long_text"
placement = "detail"

[[catalog.actions]]
name = "ship"
description = "Offer when ready to release."
prompt = "Release it."
label = "Ship it"
confirm = true
"#,
        ));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(
            names_f(&c),
            [
                "progress",
                "next-step",
                "details",
                "pull-request",
                "issue",
                "risk"
            ]
        );
        let progress = c.field("progress").unwrap();
        assert_eq!(progress.description, "Replaced.");
        assert_eq!(progress.field_type, FieldType::LongText);
        assert_eq!(progress.placement, Placement::Detail);
        assert_eq!(progress.icon, None, "replaced whole, not merged key by key");
        assert_eq!(progress.source, CatalogSource::User);
        assert_eq!(c.field("risk").unwrap().label, "Risk");
        assert_eq!(names_a(&c), ["go-ahead", "ship"]);
        let ship = c.action("ship").unwrap();
        assert!(ship.confirm);
        assert!(!ship.always);
        assert_eq!(ship.placement, Placement::Card);
    }

    /// Issue #2418 (D15): Explain left the defaults, but a user who wants it
    /// back adds it to their own catalog, `always` included.
    #[test]
    fn a_user_catalog_can_add_an_always_offered_action() {
        let c = resolve(Some(
            r#"
[[catalog.actions]]
name = "explain"
description = "Ask the agent to explain what it did and what it needs."
prompt = "Explain in a few sentences what you did and what you need from me, then update your card."
always = true
"#,
        ));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(names_a(&c), ["go-ahead", "merge", "explain"]);
        let explain = c.action("explain").unwrap();
        assert!(explain.always);
        assert_eq!(explain.source, CatalogSource::User);
    }

    /// Issue #2418 round 2 review: a config written while Explain was a
    /// default, removing it, still resolves without an error, and Explain
    /// stays off; a name that never was a default is still reported.
    #[test]
    fn removing_the_retired_explain_default_is_a_silent_no_op() {
        let c = resolve(Some("[catalog]\nremove = [\"explain\"]\n"));
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c, resolve(None));
        assert!(c.action("explain").is_none());

        let c = resolve(Some(
            "[catalog]\nremove = [\"explain\", \"merge\", \"nope\"]\n",
        ));
        assert_eq!(names_a(&c), ["go-ahead"]);
        assert_eq!(
            c.errors,
            ["catalog.remove: \"nope\" names no deck default field or action"]
        );
    }

    #[test]
    fn invalid_entries_are_skipped_with_one_error_each() {
        let c = resolve(Some(
            r#"
[catalog]
remove = ["nope"]

[[catalog.fields]]
name = "risk"
description = "x"
type = "colour"

[[catalog.fields]]
name = "Bad Name"
description = "x"
type = "text"

[[catalog.fields]]
name = "status"
description = "x"
type = "text"

[[catalog.fields]]
name = "nodesc"
type = "text"

[[catalog.fields]]
name = "where"
description = "x"
type = "text"
placement = "sidebar"

[[catalog.fields]]
name = "extra"
description = "x"
type = "text"
colour = "red"

[[catalog.fields]]
name = "ok"
description = "fine"
type = "number"

[[catalog.fields]]
name = "ok"
description = "again"
type = "text"

[[catalog.actions]]
name = "noprompt"
description = "x"

[[catalog.actions]]
name = "question-x"
description = "x"
prompt = "p"
"#,
        ));
        let joined = c.errors.join("\n");
        assert!(joined.contains("catalog.remove: \"nope\""), "{joined}");
        assert!(
            joined.contains(
                "catalog.fields \"risk\": unknown type \"colour\"; expected one of text, long_text, number, boolean, url, list, pull_request, issue"
            ),
            "{joined}"
        );
        assert!(
            joined.contains("catalog.fields \"Bad Name\": bad name"),
            "{joined}"
        );
        assert!(joined.contains("\"status\" is a reserved name"), "{joined}");
        assert!(
            joined.contains("catalog.fields \"nodesc\": missing required key \"description\""),
            "{joined}"
        );
        assert!(joined.contains("unknown placement \"sidebar\""), "{joined}");
        assert!(joined.contains("unknown key \"colour\""), "{joined}");
        assert!(
            joined.contains("catalog.fields \"ok\": duplicate name"),
            "{joined}"
        );
        assert!(
            joined.contains("catalog.actions \"noprompt\": missing required key \"prompt\""),
            "{joined}"
        );
        assert!(
            joined.contains("catalog.actions \"question-x\": \"question-x\" is a reserved name"),
            "{joined}"
        );
        assert_eq!(c.errors.len(), 10, "{joined}");
        // The one valid entry applied, as its first spelling.
        assert_eq!(c.field("ok").unwrap().field_type, FieldType::Number);
        assert_eq!(names_a(&c), ["go-ahead", "merge"]);
    }

    /// PRD #2374 (review finding): a syntax error in another table does not
    /// hide the catalog. Its `[catalog]` tables are read on their own and
    /// apply, and the broken file is still reported as a catalog error, so the
    /// result is never silently different from what the user wrote.
    #[test]
    fn a_typo_elsewhere_in_the_config_keeps_the_catalog_and_says_so() {
        let config = "\
default_command = \"claude
[bell]
enabled = tru
list = [1, 2
[catalog]
remove = [
  \"merge\", # gone
]

[[catalog.fields]]
name = \"risk\"
description = \"How risky the change is.\"
type = \"text\"

[[catalog.actions]]
name = \"ship\"
description = \"Offer when ready.\"
prompt = \"\"\"Ship it.
[not a header]
Then update your card.\"\"\"

[doorbell]
claude = nope

[[catalog.actions]]
name = \"hold\"
description = 'Offer when it should wait.'
prompt = '''Hold.
[[also not a header]]'''
";
        let c = resolve(Some(config));
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert!(
            c.errors[0].starts_with("catalog: the user config is not valid TOML (")
                && c.errors[0].ends_with(
                    "its [catalog] tables were read on their own and apply, but fix the file"
                ),
            "{:?}",
            c.errors
        );
        assert_eq!(
            c.field("risk").unwrap().description,
            "How risky the change is."
        );
        assert_eq!(names_a(&c), ["go-ahead", "ship", "hold"]);
        assert_eq!(
            c.action("ship").unwrap().prompt,
            "Ship it.\n[not a header]\nThen update your card."
        );
        assert_eq!(
            c.action("hold").unwrap().prompt,
            "Hold.\n[[also not a header]]"
        );

        // A broken line inside the catalog itself still falls back to the
        // defaults, with the error that says so.
        let c = resolve(Some("x = \"\n[catalog]\nremove = [\"merge\"\n"));
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert!(c.errors[0].starts_with("catalog: the user config could not be read as TOML"));
        assert_eq!(names_a(&c), ["go-ahead", "merge"]);
    }

    /// PRD #2374 (review finding, round 5): a multi-line string may close with
    /// four or five quotes, the extra one or two belonging to its value. The
    /// fallback must end the string where TOML does, so a `[catalog]` written
    /// inside a later string stays inert and the defaults apply. Covers the
    /// reviewer's literal-string reproduction and its basic-string twin.
    #[test]
    fn catalog_text_inside_a_string_is_never_read_as_the_catalog() {
        let literal = "\
broken = tru
memo = '''x'''' # ''''
payload = '''
[catalog]
remove = [\"merge\"]
[not_catalog]
'''
";
        let basic = "\
broken = tru
memo = \"\"\"x\"\"\"\" # \"\"\"\"
payload = \"\"\"
[catalog]
remove = [\"merge\"]
[not_catalog]
\"\"\"
";
        // Five closing quotes, and an escaped quote run inside the value.
        let basic_five = "\
broken = tru
memo = \"\"\"a \\\"\"\" b\"\"\"\"\" # \"\"\"
payload = \"\"\"
[catalog]
remove = [\"merge\"]
\"\"\"
";
        let literal_five = "\
broken = tru
memo = '''x''''' # '''
payload = '''
[catalog]
remove = [\"merge\"]
'''
";
        for config in [literal, basic, basic_five, literal_five] {
            let without_typo = config.replacen("broken = tru\n", "", 1);
            let doc: toml::Table = toml::from_str(&without_typo).expect("valid once fixed");
            assert!(doc.get("catalog").is_none(), "{config}");
            assert_eq!(catalog_sections(config), None, "{config}");
            let c = resolve(Some(config));
            assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
            assert!(
                c.errors[0].starts_with("catalog: the user config could not be read as TOML"),
                "{:?}",
                c.errors
            );
            assert_eq!(names_a(&c), ["go-ahead", "merge"], "{config}");
        }

        // The same closing runs before, and inside, a real catalog leave it
        // readable, with the extra quotes kept in the value.
        let config = "\
broken = tru
memo = '''x'''' # ''''
note = \"\"\"y\"\"\"\"\" # [catalog]
[catalog]
remove = [\"merge\"]
[[catalog.actions]]
name = \"ship\"
description = \"Offer when ready.\"
prompt = \"\"\"Ship \"it\"\"\"\"\"
[later]
x = '''
[catalog]
remove = [\"go-ahead\"]
'''
";
        let c = resolve(Some(config));
        assert_eq!(c.errors.len(), 1, "{:?}", c.errors);
        assert!(c.errors[0].ends_with("but fix the file"), "{:?}", c.errors);
        assert_eq!(names_a(&c), ["go-ahead", "ship"]);
        assert_eq!(c.action("ship").unwrap().prompt, "Ship \"it\"\"");
    }

    #[test]
    fn a_toml_parse_error_keeps_the_defaults_with_one_error() {
        let c = resolve(Some("[catalog\nremove = 1"));
        assert_eq!(c.errors.len(), 1);
        assert!(c.errors[0].starts_with("catalog: the user config could not be read as TOML"));
        assert_eq!(names_a(&c), ["go-ahead", "merge"]);
        let c = resolve(Some("catalog = 3\n"));
        assert_eq!(c.errors, ["catalog: [catalog] must be a table"]);
    }

    #[test]
    fn more_than_the_limit_is_dropped_with_errors() {
        let mut toml = String::from("[catalog]\n");
        for i in 0..70 {
            toml.push_str(&format!(
                "[[catalog.fields]]\nname = \"f{i}\"\ndescription = \"d\"\ntype = \"text\"\n"
            ));
        }
        let c = resolve(Some(&toml));
        assert_eq!(c.fields.len(), MAX_CATALOG_FIELDS);
        assert_eq!(c.errors.len(), 6 + 70 - MAX_CATALOG_FIELDS);
    }

    #[test]
    fn dispatch_text_lists_fields_and_suggestable_actions() {
        let c = resolve(None);
        let text = dispatch_text(&c, "dad");
        assert!(text.starts_with("## Your agent card\n\nThe deck shows you as a card"));
        assert!(text.contains("`dad card publish`"));
        assert!(text.contains(
            "Fields (report the ones that apply, by name):\n- `progress` (text): What you are doing now, in a few words.\n"
        ));
        assert!(text.contains("- `pull-request` (pull_request): "));
        assert!(text.contains(
            "Actions you can suggest (the user's button sends you the prompt shown):\n- `go-ahead`: Offer when you are waiting for the user's approval to continue.\n- `merge`: Offer when your work is ready to merge.\n"
        ));
        assert!(
            !text.contains("`explain`"),
            "Explain is not a default (#2418)"
        );
        // An action the user's catalog marks `always` is offered on every
        // card, so it is not one the agent suggests.
        let with_always = resolve(Some(
            "[[catalog.actions]]\nname = \"explain\"\ndescription = \"d\"\nprompt = \"Explain.\"\nalways = true\n",
        ));
        assert!(with_always.errors.is_empty(), "{:?}", with_always.errors);
        let text = dispatch_text(&with_always, "dad");
        assert!(text.contains("- `merge`: "));
        assert!(!text.contains("`explain`"), "always actions are not listed");
        assert!(text.ends_with(
            "\nYou may also report fields and actions that are not listed; the user sees them as custom.\n"
        ));
        let empty = ResolvedCatalog {
            fields: vec![],
            actions: vec![],
            errors: vec![],
            user_config: None,
        };
        let text = dispatch_text(&empty, "dad");
        assert!(!text.contains("Fields ("));
        assert!(!text.contains("Actions you can"));
    }

    #[test]
    fn names_and_labels() {
        assert!(is_valid_name("a"));
        assert!(is_valid_name("0-x"));
        assert!(!is_valid_name("-x"));
        assert!(!is_valid_name("A"));
        assert!(!is_valid_name(&"a".repeat(41)));
        assert!(is_valid_name(&"a".repeat(40)));
        assert!(is_reserved_action_name("questionnaire"));
        assert!(is_reserved_action_name("close-keep-worktree"));
        assert!(!is_reserved_action_name("merge"));
        assert_eq!(default_label("pull-request"), "Pull request");
        assert!(is_valid_icon("git-merge"));
        assert!(!is_valid_icon("Git"));
    }

    // Issue #2399 (PR #2491 scope extension): the desktop's card-action editor
    // changes the daemon's user catalog through these. TDD (RED): written
    // before the implementation; `editable_catalog`, `apply_action_edit` and
    // `edit_user_catalog_at` are stubs until it lands.

    fn input(name: &str) -> CatalogActionInput {
        CatalogActionInput {
            name: name.to_string(),
            label: None,
            description: format!("Offer when {name} applies."),
            prompt: format!("Do {name}, then update your card."),
            icon: None,
            placement: None,
            confirm: false,
            always: false,
        }
    }

    fn user_action_tables(saved: &str) -> Vec<toml::Table> {
        let table: toml::Table = toml::from_str(saved).unwrap();
        table
            .get("catalog")
            .and_then(|c| c.get("actions"))
            .and_then(|a| a.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_table().cloned()).collect())
            .unwrap_or_default()
    }

    fn removed_names(saved: &str) -> Vec<String> {
        let table: toml::Table = toml::from_str(saved).unwrap();
        table
            .get("catalog")
            .and_then(|c| c.get("remove"))
            .and_then(|r| r.as_array())
            .map(|r| {
                r.iter()
                    .filter_map(|n| n.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    const EDITED_CONFIG: &str = "\
# my deck
default_command = \"claude\" # the usual

[bell]
on_idle = true

[catalog]
# keep this catalog comment
remove = [\"merge\"] # not merge

[[catalog.fields]]
name = \"risk\"
description = \"How risky the change is.\"
type = \"text\"

[[catalog.actions]]
# the ship comment
name = \"ship\"
description = \"Offer when it is ready.\"
prompt = \"Ship it.\"

[from_a_newer_build]
answer = 42
";

    /// Issue #2399: adding, updating and removing a user action through the
    /// editor changes only `[[catalog.actions]]`; every other setting, table,
    /// the catalog's fields and `remove`, and the user's comments are kept.
    #[test]
    fn an_action_edit_keeps_every_other_setting_comment_and_the_catalog_fields() {
        let before: toml::Table = toml::from_str(EDITED_CONFIG).unwrap();
        let deploy = CatalogActionInput {
            label: Some("Deploy it".into()),
            description: "Offer when the change can be deployed.".into(),
            prompt: "Deploy it.\nThen update your card.".into(),
            icon: Some("rocket".into()),
            confirm: true,
            ..input("deploy")
        };
        let saved = apply_action_edit(
            Some(EDITED_CONFIG),
            &CatalogActionEdit::Add { action: deploy },
        )
        .expect("a valid add is written");
        assert!(
            saved.starts_with("# my deck\ndefault_command = \"claude\" # the usual\n"),
            "{saved}"
        );
        for kept in [
            "# keep this catalog comment",
            "remove = [\"merge\"] # not merge",
            "# the ship comment",
        ] {
            assert!(saved.contains(kept), "{kept:?} kept:\n{saved}");
        }
        let after: toml::Table = toml::from_str(&saved).unwrap();
        for key in ["default_command", "bell", "from_a_newer_build"] {
            assert_eq!(after[key], before[key], "{key} kept:\n{saved}");
        }
        assert_eq!(after["catalog"]["fields"], before["catalog"]["fields"]);
        assert_eq!(after["catalog"]["remove"], before["catalog"]["remove"]);
        let resolved = resolve(Some(&saved));
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        assert_eq!(names_a(&resolved), ["go-ahead", "ship", "deploy"]);
        let added = resolved.action("deploy").unwrap();
        assert_eq!(added.label, "Deploy it");
        assert_eq!(added.prompt, "Deploy it.\nThen update your card.");
        assert_eq!(added.icon.as_deref(), Some("rocket"));
        assert!(added.confirm && !added.always);
        assert_eq!(added.source, CatalogSource::User);

        // Update: the entry of that name is replaced; its neighbours are not.
        let saved = apply_action_edit(
            Some(&saved),
            &CatalogActionEdit::Update {
                base: None,
                action: CatalogActionInput {
                    label: Some("Ship it now".into()),
                    prompt: "Ship it now, then update your card.".into(),
                    always: true,
                    ..input("ship")
                },
            },
        )
        .expect("a valid update is written");
        let resolved = resolve(Some(&saved));
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        assert_eq!(names_a(&resolved), ["go-ahead", "ship", "deploy"]);
        let ship = resolved.action("ship").unwrap();
        assert_eq!(ship.label, "Ship it now");
        assert_eq!(ship.prompt, "Ship it now, then update your card.");
        assert!(ship.always);
        assert_eq!(resolved.action("deploy").unwrap().label, "Deploy it");
        assert_eq!(user_action_tables(&saved).len(), 2, "{saved}");

        // Remove: the user's own action goes; everything else stays.
        let saved = apply_action_edit(
            Some(&saved),
            &CatalogActionEdit::Remove {
                base: None,
                name: "deploy".into(),
            },
        )
        .expect("removing a user action is written");
        let resolved = resolve(Some(&saved));
        assert_eq!(names_a(&resolved), ["go-ahead", "ship"]);
        let after: toml::Table = toml::from_str(&saved).unwrap();
        for key in ["default_command", "bell", "from_a_newer_build"] {
            assert_eq!(after[key], before[key], "{key} kept:\n{saved}");
        }
        assert_eq!(after["catalog"]["fields"], before["catalog"]["fields"]);
        assert!(saved.contains("# keep this catalog comment"), "{saved}");

        // With no file, an add writes a file holding just that action.
        let fresh = apply_action_edit(
            None,
            &CatalogActionEdit::Add {
                action: input("ship"),
            },
        )
        .expect("an add with no file is written");
        let resolved = resolve(Some(&fresh));
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        assert_eq!(names_a(&resolved), ["go-ahead", "merge", "ship"]);
    }

    /// Issue #2399: hiding a deck default writes its name into
    /// `catalog.remove` and drops any user override of it (which would bring
    /// it back); editing a default writes a same-name user override; restoring
    /// undoes both, so the deck's own definition applies again.
    #[test]
    fn hiding_editing_and_restoring_a_default_go_through_remove_and_overrides() {
        let original = "\
[[catalog.actions]]
name = \"go-ahead\"
description = \"Offer when you may continue.\"
prompt = \"Proceed.\"
";
        let hidden = apply_action_edit(
            Some(original),
            &CatalogActionEdit::Hide {
                base: None,
                name: "go-ahead".into(),
            },
        )
        .expect("hiding a default is written");
        assert_eq!(removed_names(&hidden), ["go-ahead"], "{hidden}");
        assert!(
            user_action_tables(&hidden)
                .iter()
                .all(|t| t.get("name").and_then(|n| n.as_str()) != Some("go-ahead")),
            "the override would bring the hidden default back:\n{hidden}"
        );
        let resolved = resolve(Some(&hidden));
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        assert_eq!(names_a(&resolved), ["merge"]);

        let restored = apply_action_edit(
            Some(&hidden),
            &CatalogActionEdit::Restore {
                base: None,
                name: "go-ahead".into(),
            },
        )
        .expect("restoring a hidden default is written");
        assert!(removed_names(&restored).is_empty(), "{restored}");
        assert_eq!(resolve(Some(&restored)).actions, default_actions());

        let overridden = apply_action_edit(
            Some(&restored),
            &CatalogActionEdit::Update {
                base: None,
                action: CatalogActionInput {
                    label: Some("Merge it".into()),
                    prompt: "Merge with a squash commit, then update your card.".into(),
                    icon: Some("git-merge".into()),
                    ..input("merge")
                },
            },
        )
        .expect("editing a default writes an override");
        let resolved = resolve(Some(&overridden));
        assert_eq!(names_a(&resolved), ["go-ahead", "merge"]);
        let merge = resolved.action("merge").unwrap();
        assert_eq!(merge.source, CatalogSource::User);
        assert_eq!(
            merge.prompt,
            "Merge with a squash commit, then update your card."
        );

        let back = apply_action_edit(
            Some(&overridden),
            &CatalogActionEdit::Restore {
                base: None,
                name: "merge".into(),
            },
        )
        .expect("restoring an overridden default is written");
        assert!(user_action_tables(&back).is_empty(), "{back}");
        assert!(removed_names(&back).is_empty(), "{back}");
        let resolved = resolve(Some(&back));
        assert_eq!(resolved.actions, default_actions());
    }

    /// Issue #2399: an edit the catalog would reject, or one that cannot
    /// apply, is refused with a reason naming the problem, and nothing is
    /// produced to write. The values the editor sends are validated as a
    /// `[[catalog.actions]]` entry in the file is.
    #[test]
    fn an_invalid_or_impossible_edit_is_refused_with_its_reason() {
        let base = "default_command = \"claude\"\n";
        let mut many = String::from(base);
        for n in 0..(MAX_CATALOG_ACTIONS - default_actions().len()) {
            many.push_str(&format!(
                "\n[[catalog.actions]]\nname = \"a{n}\"\ndescription = \"d\"\nprompt = \"p\"\n"
            ));
        }
        assert!(resolve(Some(&many)).errors.is_empty());
        let add = |action: CatalogActionInput| CatalogActionEdit::Add { action };
        let cases: Vec<(&str, String, CatalogActionEdit, &str)> = vec![
            ("bad name", base.into(), add(input("Ship It")), "name"),
            (
                "reserved name",
                base.into(),
                add(input("close")),
                "reserved",
            ),
            (
                "reserved question name",
                base.into(),
                add(input("question-x")),
                "reserved",
            ),
            (
                "empty description",
                base.into(),
                add(CatalogActionInput {
                    description: " ".into(),
                    ..input("ship")
                }),
                "description",
            ),
            (
                "over-long prompt",
                base.into(),
                add(CatalogActionInput {
                    prompt: "p".repeat(MAX_PROMPT_CHARS + 1),
                    ..input("ship")
                }),
                "prompt",
            ),
            (
                "empty prompt",
                base.into(),
                add(CatalogActionInput {
                    prompt: String::new(),
                    ..input("ship")
                }),
                "prompt",
            ),
            (
                "over-long label",
                base.into(),
                add(CatalogActionInput {
                    label: Some("l".repeat(MAX_CATALOG_LABEL_CHARS + 1)),
                    ..input("ship")
                }),
                "label",
            ),
            (
                "multi-line label",
                base.into(),
                add(CatalogActionInput {
                    label: Some("Ship\nit".into()),
                    ..input("ship")
                }),
                "label",
            ),
            (
                "control character in the prompt",
                base.into(),
                add(CatalogActionInput {
                    prompt: "Ship\u{1b}[2Jit".into(),
                    ..input("ship")
                }),
                "prompt",
            ),
            (
                "bad icon",
                base.into(),
                add(CatalogActionInput {
                    icon: Some("Rocket!".into()),
                    ..input("ship")
                }),
                "icon",
            ),
            (
                "add over a default",
                base.into(),
                add(input("go-ahead")),
                "go-ahead",
            ),
            (
                "update of nothing",
                base.into(),
                CatalogActionEdit::Update {
                    base: None,
                    action: input("nope"),
                },
                "nope",
            ),
            (
                "remove a default",
                base.into(),
                CatalogActionEdit::Remove {
                    base: None,
                    name: "merge".into(),
                },
                "merge",
            ),
            (
                "remove of nothing",
                base.into(),
                CatalogActionEdit::Remove {
                    base: None,
                    name: "nope".into(),
                },
                "nope",
            ),
            (
                "hide a user action",
                base.into(),
                CatalogActionEdit::Hide {
                    base: None,
                    name: "ship".into(),
                },
                "ship",
            ),
            (
                "restore of nothing",
                base.into(),
                CatalogActionEdit::Restore {
                    base: None,
                    name: "nope".into(),
                },
                "nope",
            ),
            ("one past the limit", many.clone(), add(input("ship")), "32"),
            (
                "a file that is not TOML",
                "default_command = \n".into(),
                add(input("ship")),
                "TOML",
            ),
        ];
        for (case, file, edit, needle) in cases {
            match apply_action_edit(Some(&file), &edit) {
                Ok(saved) => panic!("{case}: refused, not written as:\n{saved}"),
                Err(reason) => assert!(
                    reason.to_lowercase().contains(&needle.to_lowercase()),
                    "{case}: the reason names {needle:?}: {reason}"
                ),
            }
        }
    }

    /// Issue #2399: the editor lists the deck defaults (a hidden one with the
    /// deck's own values) and then the user's own actions, each saying where
    /// the action in effect came from, whether it is a default, overridden or
    /// hidden; never the deck's own reply, open and close.
    #[test]
    fn the_editable_catalog_lists_each_action_with_where_it_came_from() {
        let none = editable_catalog(None);
        assert!(none.errors.is_empty(), "{:?}", none.errors);
        assert_eq!(none.user_config, None);
        assert_eq!(
            none.actions
                .iter()
                .map(|a| a.action.name.as_str())
                .collect::<Vec<_>>(),
            ["go-ahead", "merge"]
        );
        assert!(none.actions.iter().all(|a| a.default
            && !a.overridden
            && !a.hidden
            && a.action.source == CatalogSource::Default));

        let file = "\
[catalog]
remove = [\"merge\"]

[[catalog.actions]]
name = \"go-ahead\"
description = \"Offer when you may continue.\"
prompt = \"Proceed.\"

[[catalog.actions]]
name = \"ship\"
label = \"Ship it\"
description = \"Offer when it is ready.\"
prompt = \"Ship it.\"
icon = \"rocket\"
";
        let listed = editable_catalog(Some(file));
        assert!(listed.errors.is_empty(), "{:?}", listed.errors);
        let names: Vec<&str> = listed
            .actions
            .iter()
            .map(|a| a.action.name.as_str())
            .collect();
        assert_eq!(names, ["go-ahead", "merge", "ship"]);
        for reserved in RESERVED_ACTION_NAMES {
            assert!(!names.contains(reserved), "{reserved} is the deck's own");
        }
        let by = |name: &str| {
            listed
                .actions
                .iter()
                .find(|a| a.action.name == name)
                .unwrap()
        };
        let go = by("go-ahead");
        assert_eq!(go.action.source, CatalogSource::User);
        assert_eq!(go.action.prompt, "Proceed.");
        assert!(go.default && go.overridden && !go.hidden);
        let merge = by("merge");
        assert_eq!(merge.action.source, CatalogSource::Default);
        assert_eq!(
            &merge.action,
            default_actions()
                .iter()
                .find(|a| a.name == "merge")
                .unwrap()
        );
        assert!(merge.default && !merge.overridden && merge.hidden);
        let ship = by("ship");
        assert_eq!(ship.action.source, CatalogSource::User);
        assert_eq!(ship.action.label, "Ship it");
        assert_eq!(ship.action.icon.as_deref(), Some("rocket"));
        assert!(!ship.default && !ship.overridden && !ship.hidden);

        // The editor's request carries the action's own keys and nothing
        // else: a path, a command or any other key is refused at parsing.
        let wire = serde_json::json!({"kind": "add", "action": {
            "name": "ship", "description": "d", "prompt": "p", "path": "/etc/passwd"
        }});
        assert!(serde_json::from_value::<CatalogActionEdit>(wire).is_err());
        let wire = serde_json::json!({"kind": "hide", "name": "merge"});
        assert_eq!(
            serde_json::from_value::<CatalogActionEdit>(wire).unwrap(),
            CatalogActionEdit::Hide {
                base: None,
                name: "merge".into()
            }
        );
    }

    /// Issue #2399: the daemon's write replaces the user config whole, by
    /// rename (a reader sees the old file or the new one, never half), and
    /// leaves nothing beside it; a file it cannot read — not UTF-8, larger
    /// than the catalog reader reads, not valid TOML — is refused and left
    /// byte for byte as it was, as is a valid file whose edit is refused. A
    /// read with no edit writes nothing; a missing file is created by an add.
    #[cfg(unix)]
    #[test]
    fn the_catalog_file_is_replaced_whole_and_one_it_cannot_read_is_never_written() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();

        let read = edit_user_catalog_at(&path, None).expect("a read answers");
        assert_eq!(
            read.user_config.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "a read writes nothing"
        );

        let refused = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("Bad Name"),
            }),
        );
        assert!(refused.is_err(), "{refused:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);

        let edited = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect("a valid add is written");
        assert!(edited.actions.iter().any(|a| a.action.name == "deploy"));
        assert_ne!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "the file is replaced by rename, not rewritten in place"
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with("# my deck\n"), "{saved}");
        assert!(resolve(Some(&saved)).action("deploy").is_some());
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != crate::config::CONFIG_LOCK_NAME)
            .collect();
        assert_eq!(left, ["config.toml"], "no temp file left beside it");

        let unreadable: Vec<(&str, Vec<u8>)> = vec![
            ("not UTF-8", b"default_command = \"\xff\xfe\"\n".to_vec()),
            ("not TOML", b"default_command = \n[catalog]\n".to_vec()),
            (
                "too large",
                format!("# {}\n", "x".repeat(MAX_USER_CONFIG_BYTES as usize)).into_bytes(),
            ),
        ];
        for (case, bytes) in unreadable {
            std::fs::write(&path, &bytes).unwrap();
            let result = edit_user_catalog_at(
                &path,
                Some(&CatalogActionEdit::Add {
                    action: input("ship"),
                }),
            );
            assert!(result.is_err(), "{case}: refused: {result:?}");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                bytes,
                "{case}: never written"
            );
        }

        let missing = dir.path().join("new").join("config.toml");
        let created = edit_user_catalog_at(
            &missing,
            Some(&CatalogActionEdit::Add {
                action: input("ship"),
            }),
        )
        .expect("an add with no file creates it");
        assert!(created.actions.iter().any(|a| a.action.name == "ship"));
        let written = std::fs::read_to_string(&missing).unwrap();
        assert!(
            resolve(Some(&written)).action("ship").is_some(),
            "{written}"
        );
    }

    /// Issue #2399 review (finding 2): an update is validated exactly as an
    /// add is — a user action that exists, and a deck default's override,
    /// each refused with every malformed value — an add of a name the user
    /// already has is refused, and the largest values the catalog accepts are
    /// accepted by both, so the editor refuses no more than the file does.
    #[test]
    fn an_update_is_validated_as_an_add_is_and_the_limits_are_accepted() {
        let update = |action: CatalogActionInput| CatalogActionEdit::Update { action, base: None };
        for name in ["ship", "go-ahead"] {
            let cases: Vec<(&str, CatalogActionInput, &str)> = vec![
                (
                    "empty description",
                    CatalogActionInput {
                        description: String::new(),
                        ..input(name)
                    },
                    "description",
                ),
                (
                    "over-long description",
                    CatalogActionInput {
                        description: "d".repeat(MAX_DESCRIPTION_CHARS + 1),
                        ..input(name)
                    },
                    "description",
                ),
                (
                    "empty prompt",
                    CatalogActionInput {
                        prompt: String::new(),
                        ..input(name)
                    },
                    "prompt",
                ),
                (
                    "over-long prompt",
                    CatalogActionInput {
                        prompt: "p".repeat(MAX_PROMPT_CHARS + 1),
                        ..input(name)
                    },
                    "prompt",
                ),
                (
                    "control character in the prompt",
                    CatalogActionInput {
                        prompt: "Ship\u{1b}[2Jit".into(),
                        ..input(name)
                    },
                    "prompt",
                ),
                (
                    "over-long label",
                    CatalogActionInput {
                        label: Some("l".repeat(MAX_CATALOG_LABEL_CHARS + 1)),
                        ..input(name)
                    },
                    "label",
                ),
                (
                    "multi-line label",
                    CatalogActionInput {
                        label: Some("Ship\nit".into()),
                        ..input(name)
                    },
                    "label",
                ),
                (
                    "bad icon",
                    CatalogActionInput {
                        icon: Some("../../etc".into()),
                        ..input(name)
                    },
                    "icon",
                ),
            ];
            for (case, action, needle) in cases {
                match apply_action_edit(Some(EDITED_CONFIG), &update(action)) {
                    Ok(saved) => panic!("{name}: {case}: refused, not written as:\n{saved}"),
                    Err(reason) => assert!(
                        reason.to_lowercase().contains(needle),
                        "{name}: {case}: the reason names {needle:?}: {reason}"
                    ),
                }
            }
        }
        for (case, edit, needle) in [
            ("a bad name", update(input("Ship It")), "ship it"),
            (
                "an add of a name the user has",
                CatalogActionEdit::Add {
                    action: input("ship"),
                },
                "ship",
            ),
        ] {
            match apply_action_edit(Some(EDITED_CONFIG), &edit) {
                Ok(saved) => panic!("{case}: refused, not written as:\n{saved}"),
                Err(reason) => assert!(
                    reason.to_lowercase().contains(needle),
                    "{case}: the reason names {needle:?}: {reason}"
                ),
            }
        }

        // The largest values the catalog reads are accepted, by an add and by
        // an update, and read back whole.
        let largest = |name: &str| CatalogActionInput {
            label: Some("L".repeat(MAX_CATALOG_LABEL_CHARS)),
            description: "d".repeat(MAX_DESCRIPTION_CHARS),
            prompt: format!("{}\n\tend", "p".repeat(MAX_PROMPT_CHARS - 5)),
            icon: Some("i".repeat(MAX_ICON_CHARS)),
            ..input(name)
        };
        assert_eq!(largest("x").prompt.chars().count(), MAX_PROMPT_CHARS);
        let added = apply_action_edit(
            Some(EDITED_CONFIG),
            &CatalogActionEdit::Add {
                action: largest("deploy"),
            },
        )
        .expect("an add at every limit is written");
        let updated = apply_action_edit(Some(&added), &update(largest("ship")))
            .expect("an update at every limit is written");
        let resolved = resolve(Some(&updated));
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
        for name in ["deploy", "ship"] {
            let action = resolved.action(name).unwrap();
            assert_eq!(action.prompt, largest(name).prompt, "{name}");
            assert_eq!(action.label, "L".repeat(MAX_CATALOG_LABEL_CHARS), "{name}");
        }
    }

    const COMMENTED_CONFIG: &str = "\
[catalog]
remove = [\"progress\"] # no progress field

[[catalog.fields]]
# the risk field comment
name = \"risk\"
description = \"How risky the change is.\"
type = \"text\"

[[catalog.actions]]
# the ship comment
name = \"ship\"
description = \"Offer when it is ready.\" # when to ship
prompt = \"Ship it.\"

[[catalog.actions]]
# the deploy comment
name = \"deploy\"
description = \"Offer when it can be deployed.\"
prompt = \"Deploy it.\"
";

    /// Issue #2399 review (finding 3): the writer's destructive paths keep
    /// what they do not change. Updating an action keeps its own comments and
    /// its neighbour's; hiding and restoring a default keep a field the user
    /// removed (`progress`) removed, and every comment; removing an action
    /// keeps its neighbour's comment and the fields'.
    #[test]
    fn update_hide_restore_and_remove_keep_comments_and_an_unrelated_removal() {
        let kept = |saved: &str, what: &str, comments: &[&str]| {
            for comment in comments {
                assert!(
                    saved.contains(comment),
                    "{what}: {comment:?} kept:\n{saved}"
                );
            }
            assert_eq!(
                removed_names(saved).first().map(String::as_str),
                Some("progress"),
                "{what}: the field stays removed:\n{saved}"
            );
            let resolved = resolve(Some(saved));
            assert!(resolved.errors.is_empty(), "{what}: {:?}", resolved.errors);
            assert!(
                resolved.field("progress").is_none(),
                "{what}: progress stays hidden"
            );
            assert!(
                resolved.field("risk").is_some(),
                "{what}: the user's field stays"
            );
        };
        let all = [
            "remove = [\"progress\"",
            "# no progress field",
            "# the risk field comment",
            "# the ship comment",
            "# when to ship",
            "# the deploy comment",
        ];
        let updated = apply_action_edit(
            Some(COMMENTED_CONFIG),
            &CatalogActionEdit::Update {
                base: None,
                action: CatalogActionInput {
                    label: Some("Ship it now".into()),
                    description: "Offer when it is ready.".into(),
                    prompt: "Ship it now.".into(),
                    ..input("ship")
                },
            },
        )
        .expect("the update is written");
        kept(&updated, "update", &all);
        assert_eq!(
            resolve(Some(&updated)).action("ship").unwrap().prompt,
            "Ship it now."
        );

        let overridden = apply_action_edit(
            Some(&updated),
            &CatalogActionEdit::Update {
                base: None,
                action: CatalogActionInput {
                    prompt: "Carry on.".into(),
                    ..input("go-ahead")
                },
            },
        )
        .expect("a default's override is written");
        kept(&overridden, "override", &all);

        let hidden = apply_action_edit(
            Some(&overridden),
            &CatalogActionEdit::Hide {
                base: None,
                name: "go-ahead".into(),
            },
        )
        .expect("hiding is written");
        kept(&hidden, "hide", &all);
        assert_eq!(removed_names(&hidden), ["progress", "go-ahead"], "{hidden}");

        let restored = apply_action_edit(
            Some(&hidden),
            &CatalogActionEdit::Restore {
                base: None,
                name: "go-ahead".into(),
            },
        )
        .expect("restoring is written");
        kept(&restored, "restore", &all);
        assert_eq!(removed_names(&restored), ["progress"], "{restored}");
        assert_eq!(
            resolve(Some(&restored)).action("go-ahead").unwrap().source,
            CatalogSource::Default
        );

        let removed = apply_action_edit(
            Some(&restored),
            &CatalogActionEdit::Remove {
                base: None,
                name: "ship".into(),
            },
        )
        .expect("removing is written");
        kept(
            &removed,
            "remove",
            &[
                "# no progress field",
                "# the risk field comment",
                "# the deploy comment",
            ],
        );
        assert!(resolve(Some(&removed)).action("ship").is_none());
    }

    /// Issue #2399 review (finding 4): a user config the daemon cannot read
    /// for a reason of the file system — here its path is a directory, which
    /// opens and then fails to read — is refused for an edit with the reason,
    /// and nothing is put in its place; a read still answers, the deck
    /// defaults with the problem in `errors`.
    #[cfg(unix)]
    #[test]
    fn a_config_the_file_system_will_not_read_is_refused_and_never_replaced() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), "kept").unwrap();
        let refused = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("ship"),
            }),
        )
        .expect_err("an edit of a file that cannot be read is refused");
        assert!(refused.contains("could not be read"), "{refused}");
        assert!(path.is_dir(), "nothing replaced it");
        assert_eq!(
            std::fs::read_to_string(path.join("inside")).unwrap(),
            "kept"
        );
        let read = edit_user_catalog_at(&path, None).expect("a read still answers");
        assert_eq!(read.actions.len(), default_actions().len());
        assert!(
            read.errors.iter().any(|e| e.contains("could not be read")),
            "{:?}",
            read.errors
        );
        assert!(path.is_dir(), "a read writes nothing");
    }

    /// Issue #2399 review 7: saving an action whose form leaves a setting at
    /// its default keeps an entry's explicit default-valued key, and its
    /// comment, when the edit does not change what that key means: editing
    /// only the prompt of an entry that says `always = false # rationale`
    /// keeps that line.
    #[test]
    fn an_edit_keeps_an_unchanged_default_valued_key_and_its_comment() {
        let raw = "\
[[catalog.actions]]
name = \"ship\"
label = \"Ship\" # the label as the deck would write it
description = \"Offer when it is ready.\"
prompt = \"Ship it.\"
confirm = false # ask? no, it is cheap
always = false # rationale: only when ready
placement = \"card\" # on the card, on purpose
";
        let saved = apply_action_edit(
            Some(raw),
            &CatalogActionEdit::Update {
                base: None,
                action: CatalogActionInput {
                    description: "Offer when it is ready.".into(),
                    prompt: "Ship it now.".into(),
                    ..input("ship")
                },
            },
        )
        .expect("the edit applies");
        for kept in [
            "label = \"Ship\" # the label as the deck would write it",
            "confirm = false # ask? no, it is cheap",
            "always = false # rationale: only when ready",
            "placement = \"card\" # on the card, on purpose",
        ] {
            assert!(saved.contains(kept), "{kept:?} kept in:\n{saved}");
        }
        assert!(saved.contains("prompt = \"Ship it now.\""), "{saved}");
        // A key whose meaning the edit changes still goes: `confirm = true`
        // is not the form's `false`.
        let changed = apply_action_edit(
            Some(&raw.replace("confirm = false", "confirm = true")),
            &CatalogActionEdit::Update {
                base: None,
                action: CatalogActionInput {
                    description: "Offer when it is ready.".into(),
                    prompt: "Ship it.".into(),
                    ..input("ship")
                },
            },
        )
        .expect("the edit applies");
        assert!(!changed.contains("confirm"), "{changed}");
        assert!(!resolve(Some(&changed)).action("ship").unwrap().confirm);
    }

    /// Issue #2399 review 1 / audit 4: a catalog edit is published only by
    /// rename; where the swap would split a hard-linked config from its other
    /// name, the edit is refused, never written in place the way `config set`
    /// falls back to, and both names keep the file as it was.
    #[cfg(unix)]
    #[test]
    fn a_catalog_edit_is_refused_rather_than_written_in_place() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let other = dir.path().join("dotfiles-config.toml");
        std::fs::hard_link(&path, &other).unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        let refused = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect_err("a hard-linked config is not written in place");
        assert!(refused.contains("hard link"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        assert_eq!(std::fs::read_to_string(&other).unwrap(), EDITED_CONFIG);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != crate::config::CONFIG_LOCK_NAME)
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["config.toml", "dotfiles-config.toml"],
            "no temp file"
        );
    }

    /// Issue #2399 audit 3: a FIFO at the config path is refused at once, for
    /// a read and for an edit, without waiting for a writer that never comes
    /// (the reads run on a thread the test abandons if it hangs, so a
    /// regression fails here rather than wedging the test process); the
    /// card reader refuses it the same way.
    #[cfg(unix)]
    #[test]
    fn a_fifo_at_the_config_path_is_refused_without_waiting() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = path.clone();
        std::thread::spawn(move || {
            let read = edit_user_catalog_at(&probe, None);
            let edit = edit_user_catalog_at(
                &probe,
                Some(&CatalogActionEdit::Add {
                    action: input("ship"),
                }),
            );
            let cards = resolve_from_user_config_at(&probe);
            let _ = tx.send((read, edit, cards));
        });
        let (read, edit, cards) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the reads return instead of waiting for a writer");
        let read = read.expect("a read answers");
        assert!(
            read.errors.iter().any(|e| e.contains("not a regular file")),
            "{:?}",
            read.errors
        );
        let edit = edit.expect_err("an edit is refused");
        assert!(edit.contains("not a regular file"), "{edit}");
        assert!(
            cards
                .errors
                .iter()
                .any(|e| e.contains("not a regular file")),
            "{:?}",
            cards.errors
        );
        use std::os::unix::fs::FileTypeExt as _;
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }

    /// Issue #2399 audit 3: a config path that is a link whose target is
    /// missing is not an absent config: an edit is refused and creates
    /// nothing at the link's target, and a read reports the problem.
    #[cfg(unix)]
    #[test]
    fn a_dangling_config_link_is_refused_not_treated_as_a_new_config() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let target = dir.path().join("unmounted").join("config.toml");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let refused = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("ship"),
            }),
        )
        .expect_err("a dangling link is refused");
        assert!(
            refused.contains("link whose target is missing"),
            "{refused}"
        );
        assert!(!target.exists(), "nothing created at the target");
        assert!(!target.parent().unwrap().exists());
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        let read = edit_user_catalog_at(&path, None).expect("a read answers");
        assert!(
            read.errors
                .iter()
                .any(|e| e.contains("link whose target is missing")),
            "{:?}",
            read.errors
        );
    }

    /// Issue #2399 audit 2: another writer that saves the user config between
    /// the editor's read and its save — `config set`, another daemon reading
    /// the same file, a hand edit — is never overwritten: the edit is refused
    /// with nothing written, and the other writer's file is what remains. The
    /// same holds for a file that appears where there was none.
    #[test]
    fn an_edit_is_refused_when_the_file_changed_since_it_was_read() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let theirs = EDITED_CONFIG.replace("on_idle = true", "on_idle = false");
        let refused = edit_user_catalog_at_with(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
            || std::fs::write(&path, &theirs).unwrap(),
        )
        .expect_err("a file saved by someone else meanwhile is not overwritten");
        assert!(
            refused.contains("changed since the editor read it"),
            "{refused}"
        );
        assert!(refused.contains("was not saved"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), theirs);

        let fresh = dir.path().join("fresh.toml");
        let refused = edit_user_catalog_at_with(
            &fresh,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
            || std::fs::write(&fresh, "# made meanwhile\n").unwrap(),
        )
        .expect_err("a file that appeared meanwhile is not overwritten");
        assert!(refused.contains("changed since"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(&fresh).unwrap(),
            "# made meanwhile\n"
        );
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "no temp file left: {left:?}");

        // Unchanged meanwhile, the same edit is saved.
        edit_user_catalog_at_with(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
            || {},
        )
        .expect("an edit of an unchanged file is saved");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("on_idle = false"), "{saved}");
        assert!(resolve(Some(&saved)).action("deploy").is_some());
    }

    /// Issue #2399 audit 5: the save replaces exactly the file the edit read.
    /// When the config's link is pointed at another file between the read and
    /// the save — the same bytes there included — the edit is refused and
    /// neither file is written; a linked config that stays put is saved
    /// through its link, which stays a link.
    #[cfg(unix)]
    #[test]
    fn an_edit_is_refused_when_the_config_link_was_retargeted() {
        let dir = crate::test_temp::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let other = dir.path().join("other.toml");
        std::fs::write(&real, EDITED_CONFIG).unwrap();
        std::fs::write(&other, EDITED_CONFIG).unwrap();
        let path = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let refused = edit_user_catalog_at_with(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
            || {
                std::fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(&other, &path).unwrap();
            },
        )
        .expect_err("a retargeted link is refused");
        assert!(refused.contains("changed since"), "{refused}");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), EDITED_CONFIG);
        assert_eq!(std::fs::read_to_string(&other).unwrap(), EDITED_CONFIG);

        // The same file swapped for an identical copy is another file too.
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let refused = edit_user_catalog_at_with(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
            || {
                let copy = dir.path().join("copy.toml");
                std::fs::write(&copy, EDITED_CONFIG).unwrap();
                std::fs::rename(&copy, &real).unwrap();
            },
        )
        .expect_err("a replaced file is refused even with the same bytes");
        assert!(refused.contains("changed since"), "{refused}");

        edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect("an edit through a link that stays put is saved");
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let saved = std::fs::read_to_string(&real).unwrap();
        assert!(resolve(Some(&saved)).action("deploy").is_some(), "{saved}");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), EDITED_CONFIG);
    }

    /// Issue #2399 re-audit 2: the catalog editor and `config set`'s writer
    /// take one lock beside the config for their whole read, merge and save.
    /// With the editor held just after its read, `config set` waits instead of
    /// saving under it, and saves once the editor has published, merging into
    /// the editor's file: both changes land, neither refused nor lost.
    #[test]
    fn a_catalog_edit_and_config_set_interleaved_after_their_reads_both_land() {
        use std::time::Duration;
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let (read_tx, read_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let editor_path = path.clone();
        let editor = std::thread::spawn(move || {
            edit_user_catalog_at_with(
                &editor_path,
                Some(&CatalogActionEdit::Add {
                    action: input("deploy"),
                }),
                || {
                    read_tx.send(()).unwrap();
                    let _ = go_rx.recv_timeout(Duration::from_secs(20));
                },
            )
        });
        read_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the editor read the file");
        let (set_tx, set_rx) = std::sync::mpsc::channel();
        let setter_path = path.clone();
        let setter = std::thread::spawn(move || {
            let config = crate::config::DashboardConfig {
                default_command: "zsh".into(),
                ..Default::default()
            };
            let saved = config.save_to(&setter_path);
            let _ = set_tx.send(());
            saved
        });
        let waited = set_rx.recv_timeout(Duration::from_millis(500)).is_err();
        go_tx.send(()).unwrap();
        let edited = editor.join().unwrap();
        let set = setter.join().unwrap();
        edited.expect("the editor's save lands");
        set.expect("config set's save lands after it");
        assert!(waited, "config set waited for the editor's lock");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(resolve(Some(&saved)).action("deploy").is_some(), "{saved}");
        assert!(saved.contains("default_command = \"zsh\""), "{saved}");
    }

    /// The child of
    /// [`a_fifo_swapped_in_before_the_save_is_refused_and_the_lock_released`]:
    /// nothing unless `DAD_CATALOG_FIFO_CHILD` names a config. It edits that
    /// config with a FIFO put in the file's place after the read, prints the
    /// outcome, then puts a regular file back and edits again.
    #[cfg(unix)]
    #[test]
    fn fifo_swap_child() {
        let Some(path) = std::env::var_os("DAD_CATALOG_FIFO_CHILD") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let swapped = edit_user_catalog_at_with(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
            || {
                std::fs::remove_file(&path).unwrap();
                let c = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            },
        );
        println!("SWAPPED-REFUSED: {}", swapped.is_err());
        if let Err(reason) = &swapped {
            println!("SWAPPED-REASON: {}", reason.replace('\n', " "));
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let again = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        );
        println!("AGAIN-SAVED: {}", again.is_ok());
    }

    /// Issue #2399 re-audit new 1: a FIFO put in the config's place after the
    /// editor read a regular file is refused by the save's write probe without
    /// waiting for a reader, and the edit lock is released: the same process
    /// then saves an edit. It runs in a child process that is killed after a
    /// bound, so a probe that waits fails here instead of wedging the tests.
    #[cfg(unix)]
    #[test]
    fn a_fifo_swapped_in_before_the_save_is_refused_and_the_lock_released() {
        use std::io::Read as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "card_catalog::tests::fifo_swap_child",
                "--nocapture",
            ])
            .env("DAD_CATALOG_FIFO_CHILD", &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break Some(status);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the save waited on the FIFO put in the config's place");
        }
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let mut err = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut err)
            .unwrap();
        assert!(status.unwrap().success(), "{out}\n{err}");
        assert!(out.contains("SWAPPED-REFUSED: true"), "{out}\n{err}");
        assert!(out.contains("AGAIN-SAVED: true"), "{out}\n{err}");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(resolve(Some(&saved)).action("deploy").is_some(), "{saved}");
    }

    /// Issue #2399 re-audit 5: a save is refused where another user could
    /// swap the config by changing a folder on its path — here the config's
    /// own folder is group-writable — and the file is left as it was. A
    /// sticky world-writable folder (as `/tmp` is) and a private one are
    /// saved to.
    #[cfg(unix)]
    #[test]
    fn an_edit_is_refused_where_another_user_can_change_a_folder_on_the_path() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let folder = dir.path().join("shared");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let add = |name: &str| {
            edit_user_catalog_at(
                &path,
                Some(&CatalogActionEdit::Add {
                    action: input(name),
                }),
            )
        };
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o770)).unwrap();
        let refused = add("deploy").expect_err("a group-writable folder is refused");
        assert!(refused.contains("other users"), "{refused}");
        assert!(refused.contains("shared"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);

        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o1777)).unwrap();
        add("deploy").expect("a sticky folder is saved to");
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        add("release").expect("a private folder is saved to");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(resolve(Some(&saved)).action("release").is_some(), "{saved}");
    }

    /// Issue #2399 re-audit new 3: the last check before the rename compares
    /// the config's mode, owner and link count with the read, not only its
    /// file and bytes. A `chmod 0600` or a new hard link made between the read
    /// and that check refuses the save, and the file keeps the mode and links
    /// it was given.
    #[cfg(unix)]
    #[test]
    fn a_mode_or_link_change_before_the_last_check_refuses_the_save() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let read = read_user_config(&path).unwrap();
        let refused = crate::config::write_config_strictly(
            &path,
            &read.target,
            b"new = 1\n",
            read.file,
            || {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
                still_as_read(&path, &read)
            },
        )
        .expect_err("a chmod since the read is refused");
        assert!(refused.contains("changed since"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);

        let read = read_user_config(&path).unwrap();
        let twin = dir.path().join("twin.toml");
        let refused = crate::config::write_config_strictly(
            &path,
            &read.target,
            b"new = 1\n",
            read.file,
            || {
                std::fs::hard_link(&path, &twin).unwrap();
                still_as_read(&path, &read)
            },
        )
        .expect_err("a hard link made since the read is refused");
        assert!(refused.contains("changed since"), "{refused}");
        assert_eq!(std::fs::read_to_string(&twin).unwrap(), EDITED_CONFIG);
        assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 2);
    }

    /// Issue #2399 re-review N5: an edit carrying the action as the editor
    /// read it (`base`) is refused when the file's action of that name is not
    /// that one any more, and applied when it is; `null` means the editor read
    /// none, and an edit with no `base` is applied as before. The key is read
    /// from the wire as the desktop sends it.
    #[test]
    fn an_edit_whose_base_is_not_the_file_action_is_refused() {
        let raw = format!("{EDITED_CONFIG}\n");
        let listed = editable_catalog(Some(&raw));
        let ship = listed
            .actions
            .iter()
            .find(|a| a.action.name == "ship")
            .unwrap()
            .clone();
        let theirs = raw.replace("prompt = \"Ship it.\"", "prompt = \"Ship it now.\"");
        assert_ne!(theirs, raw);
        let relabel = |base| CatalogActionEdit::Update {
            base,
            action: CatalogActionInput {
                label: Some("Ship!".into()),
                description: "Offer when it is ready.".into(),
                prompt: "Ship it.".into(),
                ..input("ship")
            },
        };
        let refused = apply_action_edit(Some(&theirs), &relabel(Some(Some(ship.clone()))))
            .expect_err("a stale base is refused");
        assert!(
            refused.contains("changed since the editor read it") && refused.contains("Refresh"),
            "{refused}"
        );
        let saved = apply_action_edit(Some(&raw), &relabel(Some(Some(ship.clone()))))
            .expect("a base that is the file's action is applied");
        assert_eq!(resolve(Some(&saved)).action("ship").unwrap().label, "Ship!");
        apply_action_edit(Some(&theirs), &relabel(None)).expect("no base: applied as before");

        let removed = CatalogActionEdit::Remove {
            name: "ship".into(),
            base: Some(Some(ship.clone())),
        };
        apply_action_edit(Some(&theirs), &removed).expect_err("a stale remove is refused");
        let ghost = CatalogActionEdit::Remove {
            name: "ghost".into(),
            base: Some(None),
        };
        let refused =
            apply_action_edit(Some(&raw), &ghost).expect_err("no action named ghost to remove");
        assert!(refused.contains("no action named"), "{refused}");
        let with_ghost = format!(
            "{raw}\n[[catalog.actions]]\nname = \"ghost\"\ndescription = \"Boo.\"\nprompt = \"Boo.\"\n"
        );
        let refused = apply_action_edit(Some(&with_ghost), &ghost)
            .expect_err("an action that appeared since the read is refused");
        assert!(refused.contains("changed since"), "{refused}");

        let hidden = listed
            .actions
            .iter()
            .find(|a| a.action.name == "merge")
            .unwrap()
            .clone();
        assert!(hidden.hidden);
        let restore = CatalogActionEdit::Restore {
            name: "merge".into(),
            base: Some(Some(hidden)),
        };
        let shown_again = raw.replace("remove = [\"merge\"] # not merge\n", "");
        apply_action_edit(Some(&shown_again), &restore)
            .expect_err("a restore of a default no longer hidden is refused");
        apply_action_edit(Some(&raw), &restore).expect("a restore as read is applied");

        let wire: CatalogActionEdit = serde_json::from_value(serde_json::json!({
            "kind": "hide", "name": "merge", "base": null,
        }))
        .unwrap();
        assert_eq!(
            wire,
            CatalogActionEdit::Hide {
                name: "merge".into(),
                base: Some(None)
            }
        );
        let wire: CatalogActionEdit =
            serde_json::from_value(serde_json::json!({"kind": "hide", "name": "merge"})).unwrap();
        assert_eq!(
            wire,
            CatalogActionEdit::Hide {
                name: "merge".into(),
                base: None
            }
        );
        let round: CatalogActionEdit =
            serde_json::from_value(serde_json::to_value(relabel(Some(Some(ship)))).unwrap())
                .unwrap();
        assert!(matches!(
            round,
            CatalogActionEdit::Update {
                base: Some(Some(_)),
                ..
            }
        ));
    }

    /// Issue #2399 re-audit 2: while another deck writer holds the config
    /// lock past the bounded wait, an edit is refused with that reason, its
    /// wait for the lock bounded by the request's deadline, and the file is
    /// left as it was.
    #[test]
    fn an_edit_refuses_when_the_config_stays_locked() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let held = crate::config::lock_config_edit(&path, std::time::Duration::ZERO)
            .expect("the test takes the lock");
        let started = std::time::Instant::now();
        let refused = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect_err("a held lock refuses the edit");
        assert!(
            started.elapsed() < crate::config::CONFIG_LOCK_WAIT * 2,
            "the wait is bounded"
        );
        assert!(refused.contains("stayed locked"), "{refused}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        drop(held);
        edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect("saved once the lock is free");
    }

    /// The lock file beside `path`'s config: where both deck writers lock.
    fn lock_beside(path: &std::path::Path) -> std::path::PathBuf {
        path.parent().unwrap().join(crate::config::CONFIG_LOCK_NAME)
    }

    /// Issue #2399 final review F2 / audit A3: a lock file beside the config
    /// that the deck did not make for this user alone — a link, a FIFO, a file
    /// others can read or write, one with a second hard link — is unsafe:
    /// another user could replace it under a writer, or hold it. The catalog
    /// editor and `config set` each refuse, naming the lock file, and leave
    /// the config and the lock entry exactly as they were (nothing is
    /// `chmod`ed or replaced).
    #[cfg(unix)]
    #[test]
    fn an_unsafe_lock_file_beside_the_config_refuses_both_writers() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        type Make = fn(&std::path::Path, &std::path::Path);
        let cases: [(&str, Make); 5] = [
            ("a symlink", |dir, lock| {
                let elsewhere = dir.join("elsewhere.lock");
                std::fs::write(&elsewhere, "").unwrap();
                std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                std::os::unix::fs::symlink(&elsewhere, lock).unwrap();
            }),
            ("a FIFO", |_, lock| {
                let c = std::ffi::CString::new(lock.to_string_lossy().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }),
            ("mode 0644", |_, lock| {
                std::fs::write(lock, "").unwrap();
                std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o644)).unwrap();
            }),
            ("mode 0666", |_, lock| {
                std::fs::write(lock, "").unwrap();
                std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o666)).unwrap();
            }),
            ("a second hard link", |dir, lock| {
                std::fs::write(lock, "").unwrap();
                std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o600)).unwrap();
                std::fs::hard_link(lock, dir.join("twin.lock")).unwrap();
            }),
        ];
        for (what, make) in cases {
            let dir = crate::test_temp::tempdir().unwrap();
            let path = dir.path().join("config.toml");
            std::fs::write(&path, EDITED_CONFIG).unwrap();
            let lock = lock_beside(&path);
            make(dir.path(), &lock);
            let before = std::fs::symlink_metadata(&lock).unwrap();

            let refused = edit_user_catalog_at(
                &path,
                Some(&CatalogActionEdit::Add {
                    action: input("deploy"),
                }),
            )
            .expect_err(&format!("{what}: the editor refuses"));
            assert!(
                refused.contains(crate::config::CONFIG_LOCK_NAME),
                "{what}: {refused}"
            );
            assert!(refused.contains("not safe"), "{what}: {refused}");

            let config = crate::config::DashboardConfig {
                default_command: "zsh".into(),
                ..Default::default()
            };
            let refused = config
                .save_to(&path)
                .expect_err(&format!("{what}: config set refuses"));
            assert!(
                refused.contains(crate::config::CONFIG_LOCK_NAME),
                "{what}: {refused}"
            );
            assert!(refused.contains("not safe"), "{what}: {refused}");

            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                EDITED_CONFIG,
                "{what}"
            );
            let after = std::fs::symlink_metadata(&lock).unwrap();
            assert_eq!(
                (after.ino(), after.mode(), after.uid(), after.nlink()),
                (before.ino(), before.mode(), before.uid(), before.nlink()),
                "{what}: the lock entry is left as it was"
            );
        }
    }

    /// Issue #2399 final review F2: a lock file the writers make themselves
    /// is a private regular file of this user's with one link, and both
    /// writers use it again.
    #[cfg(unix)]
    #[test]
    fn the_lock_file_the_writers_make_is_private_and_used_again() {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect("the editor saves");
        let meta = std::fs::symlink_metadata(lock_beside(&path)).unwrap();
        assert!(meta.is_file());
        assert_eq!(meta.permissions().mode() & 0o077, 0, "{:o}", meta.mode());
        assert_eq!(meta.uid(), unsafe { libc::geteuid() });
        assert_eq!(meta.nlink(), 1);
        crate::config::DashboardConfig {
            default_command: "zsh".into(),
            ..Default::default()
        }
        .save_to(&path)
        .expect("config set saves with the same lock");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(resolve(Some(&saved)).action("deploy").is_some(), "{saved}");
        assert!(saved.contains("default_command = \"zsh\""), "{saved}");
    }

    /// Issue #2399 final review section 4: while another deck writer holds
    /// the config lock, `config set`'s save waits the bounded time and then
    /// refuses with the reason, saving nothing; once the lock is free it
    /// saves.
    #[test]
    fn config_set_refuses_when_the_config_stays_locked() {
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let held = crate::config::lock_config_edit(&path, std::time::Duration::ZERO)
            .expect("the test takes the lock");
        let config = crate::config::DashboardConfig {
            default_command: "zsh".into(),
            ..Default::default()
        };
        let started = std::time::Instant::now();
        let refused = config.save_to(&path).expect_err("a held lock refuses");
        let waited = started.elapsed();
        assert!(
            waited >= crate::config::CONFIG_LOCK_WAIT - std::time::Duration::from_millis(100),
            "config set waited for the other writer ({waited:?})"
        );
        assert!(
            waited < crate::config::CONFIG_LOCK_WAIT + std::time::Duration::from_secs(1),
            "the wait is bounded ({waited:?})"
        );
        assert!(refused.contains("try again"), "{refused}");
        assert!(
            refused.contains(crate::config::CONFIG_LOCK_NAME),
            "{refused}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        drop(held);
        config.save_to(&path).expect("saved once the lock is free");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_command = \"zsh\"")
        );
    }

    /// Issue #2399 final audit, section 2: the editor's bound covers the
    /// whole request, not each wait in turn. While another writer holds the
    /// config lock, an edit arrives and waits for it; another edit and a read
    /// arrive meanwhile and wait for their turn against their own request's
    /// deadline rather than queueing behind the first, so each answers within
    /// the bound plus a small margin, the edits refused, saying to try again.
    /// The read answers either way (it takes no file lock once it has its
    /// turn). A read that cannot have its turn in time is answered busy.
    #[test]
    fn a_held_lock_bounds_every_editor_request_that_arrives_meanwhile() {
        use std::time::{Duration, Instant};
        let dir = crate::test_temp::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EDITED_CONFIG).unwrap();
        let held = crate::config::lock_config_edit(&path, Duration::ZERO)
            .expect("the test takes the lock");
        let request = |name: Option<&'static str>| {
            let path = path.clone();
            std::thread::spawn(move || {
                let edit = name.map(|name| CatalogActionEdit::Add {
                    action: input(name),
                });
                let began = Instant::now();
                let answer = edit_user_catalog_at(&path, edit.as_ref());
                (name, answer, began.elapsed())
            })
        };
        let first = request(Some("deploy"));
        std::thread::sleep(Duration::from_millis(300));
        let others = [request(Some("release")), request(None)];
        let bound = crate::config::CONFIG_LOCK_WAIT + Duration::from_secs(1);
        for handle in std::iter::once(first).chain(others) {
            let (name, answer, took) = handle.join().unwrap();
            let what = name.unwrap_or("the read");
            assert!(took < bound, "{what} answered after {took:?}");
            match (name, answer) {
                (Some(_), Ok(_)) => panic!("{what} saved while the lock was held"),
                (_, Err(refused)) => assert!(refused.contains("try again"), "{what}: {refused}"),
                (None, Ok(_)) => {}
            }
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), EDITED_CONFIG);
        drop(held);

        let turn = EDIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let began = Instant::now();
        let busy = edit_user_catalog_at(&path, None).expect_err("a read with no turn is busy");
        let took = began.elapsed();
        drop(turn);
        assert!(took < bound, "the read answered after {took:?}");
        assert!(
            busy.contains("busy") && busy.contains("try again"),
            "{busy}"
        );
    }

    /// Issue #2399 final audit A2: the folder check walks the config's path
    /// as configured as well as where it leads. `shared/alias/config.toml`,
    /// with `shared` group-writable and `alias` a link to a private folder,
    /// is refused, naming `shared` — whoever can change `shared` can point
    /// `alias` elsewhere — and the file is left as it was.
    #[cfg(unix)]
    #[test]
    fn an_edit_is_refused_where_a_folder_holding_a_link_on_the_path_is_shared() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let home = dir.path().join("home");
        let private = home.join("private");
        let shared = home.join("shared");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&private, shared.join("alias")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        std::fs::write(private.join("config.toml"), EDITED_CONFIG).unwrap();
        let path = shared.join("alias").join("config.toml");

        let refused = edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect_err("a shared folder holding a link on the path is refused");
        assert!(refused.contains("other users"), "{refused}");
        assert!(refused.contains("shared"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(private.join("config.toml")).unwrap(),
            EDITED_CONFIG
        );

        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        edit_user_catalog_at(
            &path,
            Some(&CatalogActionEdit::Add {
                action: input("deploy"),
            }),
        )
        .expect("saved once the folder is private");
    }

    /// Issue #2399 final audit A2, round 4: a folder holding a link is judged
    /// even when the path reaches it only through another link's expansion.
    /// `outer -> shared/alias` and `shared/alias -> private`, with `shared`
    /// group-writable: `outer/config.toml` is refused naming `shared`, though
    /// neither the path as written nor where it ends passes through `shared`,
    /// and the file is left as it was.
    #[cfg(unix)]
    #[test]
    fn an_edit_is_refused_where_a_shared_folder_holds_a_link_inside_another_links_expansion() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let home = dir.path().join("home");
        let private = home.join("private");
        let shared = home.join("shared");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&private, shared.join("alias")).unwrap();
        std::os::unix::fs::symlink(shared.join("alias"), home.join("outer")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        std::fs::write(private.join("config.toml"), EDITED_CONFIG).unwrap();
        let path = home.join("outer").join("config.toml");
        let add = || {
            edit_user_catalog_at(
                &path,
                Some(&CatalogActionEdit::Add {
                    action: input("deploy"),
                }),
            )
        };

        let refused = add().expect_err("a shared folder inside a link's expansion is refused");
        assert!(refused.contains("other users"), "{refused}");
        assert!(refused.contains("shared"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(private.join("config.toml")).unwrap(),
            EDITED_CONFIG
        );

        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        add().expect("saved once the folder is private");
    }

    /// Issue #2399 final audit A2, round 4: a folder holding a hop of the
    /// final file's own link chain is judged, not only the chain's two ends.
    /// `via-hop.toml -> shared/hop.toml -> private/config.toml`, with `shared`
    /// group-writable, is refused naming `shared`, and the file is left as it
    /// was.
    #[cfg(unix)]
    #[test]
    fn an_edit_is_refused_where_a_shared_folder_holds_a_hop_of_the_files_link_chain() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::test_temp::tempdir().unwrap();
        let home = dir.path().join("home");
        let private = home.join("private");
        let shared = home.join("shared");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(private.join("config.toml"), EDITED_CONFIG).unwrap();
        std::os::unix::fs::symlink(private.join("config.toml"), shared.join("hop.toml")).unwrap();
        std::os::unix::fs::symlink(shared.join("hop.toml"), home.join("via-hop.toml")).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o770)).unwrap();
        let path = home.join("via-hop.toml");
        let add = || {
            edit_user_catalog_at(
                &path,
                Some(&CatalogActionEdit::Add {
                    action: input("deploy"),
                }),
            )
        };

        let refused = add().expect_err("a shared folder holding a hop of the chain is refused");
        assert!(refused.contains("other users"), "{refused}");
        assert!(refused.contains("shared"), "{refused}");
        assert_eq!(
            std::fs::read_to_string(private.join("config.toml")).unwrap(),
            EDITED_CONFIG
        );

        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        add().expect("saved once the folder is private");
    }

    /// Issue #2399 final audit A2, round 5 (C1): the folder the config is in
    /// is judged even when it is above the home. With `$HOME` at
    /// `team/alice` and `team` group-writable, an existing
    /// `team/config.toml`, an absent `team/new.toml` and `../config.toml`
    /// from the home are each refused naming `team`, and nothing is written;
    /// with `team` `0750` all three are saved to.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn an_edit_is_refused_where_the_configs_own_folder_above_the_home_is_shared() {
        use std::os::unix::fs::PermissionsExt as _;
        struct Restore {
            home: Option<std::ffi::OsString>,
            cwd: std::path::PathBuf,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.cwd);
                // SAFETY: the test holds STATE_DIR_ENV_LOCK and is serial, so
                // no other test reads or writes HOME meanwhile.
                unsafe {
                    match self.home.take() {
                        Some(home) => std::env::set_var("HOME", home),
                        None => std::env::remove_var("HOME"),
                    }
                }
            }
        }
        let _env = crate::config::STATE_DIR_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = crate::test_temp::tempdir().unwrap();
        let fixture = dir.path().join("fixture");
        let team = fixture.join("team");
        let home = team.join("alice");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let existing = team.join("config.toml");
        let absent = team.join("new.toml");
        std::fs::write(&existing, EDITED_CONFIG).unwrap();
        let _restore = Restore {
            home: std::env::var_os("HOME"),
            cwd: std::env::current_dir().unwrap(),
        };
        // SAFETY: see Restore::drop.
        unsafe { std::env::set_var("HOME", &home) };
        std::env::set_current_dir(&home).unwrap();
        let relative = std::path::Path::new("../config.toml");
        let add = |path: &std::path::Path, name: &str| {
            edit_user_catalog_at(
                path,
                Some(&CatalogActionEdit::Add {
                    action: input(name),
                }),
            )
        };

        std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o770)).unwrap();
        for (path, case) in [
            (existing.as_path(), "an existing config"),
            (absent.as_path(), "an absent config"),
            (relative, "a relative config"),
        ] {
            let refused = add(path, "deploy").expect_err(&format!(
                "{case} in a shared folder above the home is refused"
            ));
            assert!(refused.contains("other users"), "{case}: {refused}");
            assert!(
                refused.contains(&team.display().to_string()),
                "{case}: {refused}"
            );
            assert_eq!(
                std::fs::read_to_string(&existing).unwrap(),
                EDITED_CONFIG,
                "{case}"
            );
            assert!(!absent.exists(), "{case}");
        }

        std::fs::set_permissions(&team, std::fs::Permissions::from_mode(0o750)).unwrap();
        add(&existing, "deploy").expect("an existing config is saved to");
        add(&absent, "deploy").expect("an absent config is created");
        add(relative, "release").expect("a relative config is saved to");
        let saved = std::fs::read_to_string(&existing).unwrap();
        assert!(resolve(Some(&saved)).action("release").is_some(), "{saved}");
        let created = std::fs::read_to_string(&absent).unwrap();
        assert!(
            resolve(Some(&created)).action("deploy").is_some(),
            "{created}"
        );
    }

    /// PR #2491 review (Qodo 2): a problem the catalog already had is not a
    /// problem the edit added, even when the edit moves the entry it is
    /// reported against. An unnamed malformed entry is reported by its
    /// position, so removing an action before it changes that position; the
    /// removal is still saved and the old problem is still reported, while an
    /// edit that adds a genuinely new problem is still refused.
    #[test]
    fn a_problem_the_catalog_already_had_does_not_block_an_edit_that_moves_it() {
        let original = "\
[[catalog.actions]]
name = \"deploy\"
description = \"Offer when the change can be deployed.\"
prompt = \"Deploy it.\"

[[catalog.actions]]
description = \"An entry the user never named.\"
prompt = \"Do the unnamed thing.\"
";
        let before = resolve(Some(original));
        assert_eq!(before.errors.len(), 1, "{:?}", before.errors);
        assert!(
            before.errors[0].starts_with("catalog.actions[1]"),
            "{:?}",
            before.errors
        );

        let saved = apply_action_edit(
            Some(original),
            &CatalogActionEdit::Remove {
                base: None,
                name: "deploy".into(),
            },
        )
        .expect("removing an action before an existing problem is saved");
        let after = resolve(Some(&saved));
        assert!(after.action("deploy").is_none(), "{saved}");
        assert_eq!(after.errors.len(), 1, "{:?}", after.errors);
        assert!(
            after.errors[0].starts_with("catalog.actions[0]"),
            "the existing problem is still reported: {:?}",
            after.errors
        );
        assert!(
            saved.contains("Do the unnamed thing."),
            "the malformed entry is kept for its owner to fix:\n{saved}"
        );

        // A problem the edit genuinely adds is still found: the comparison
        // ignores where an unnamed entry sits, not how many problems there
        // are, so a second unnamed entry with the same problem is new, as is
        // a problem of another kind.
        let had = vec!["catalog.actions[1]: missing name".to_string()];
        let moved = vec!["catalog.actions[0]: missing name".to_string()];
        assert!(problems_added(&had, &moved).is_empty());
        let doubled = vec![
            "catalog.actions[0]: missing name".to_string(),
            "catalog.actions[3]: missing name".to_string(),
        ];
        assert_eq!(
            problems_added(&had, &doubled),
            ["catalog.actions[3]: missing name"]
        );
        let other = vec![
            "catalog.actions[0]: missing name".to_string(),
            "catalog.actions \"ship\": bad icon".to_string(),
        ];
        assert_eq!(
            problems_added(&had, &other),
            ["catalog.actions \"ship\": bad icon"]
        );
        assert_eq!(
            problems_added(&[], &moved),
            ["catalog.actions[0]: missing name"]
        );
        // A fields problem keeps its kind: an actions problem does not hide it.
        assert_eq!(
            problems_added(&had, &["catalog.fields[1]: missing name".to_string()]),
            ["catalog.fields[1]: missing name"]
        );
    }
}
