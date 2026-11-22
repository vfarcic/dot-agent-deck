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
//! Everything here is pure except [`resolve_from_user_config`], which reads the
//! file. The catalog is parsed from the raw TOML separately from the rest of
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
    let path = crate::config::user_config_path();
    let display = path.display().to_string();
    let read = std::fs::metadata(&path).and_then(|meta| {
        if meta.len() > MAX_USER_CONFIG_BYTES {
            return Err(std::io::Error::other(format!(
                "it is larger than {MAX_USER_CONFIG_BYTES} bytes"
            )));
        }
        std::fs::read_to_string(&path)
    });
    let mut resolved = match read {
        Ok(raw) => resolve(Some(&raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut resolved = defaults();
            resolved.user_config = None;
            return resolved;
        }
        Err(error) => {
            let mut resolved = defaults();
            resolved.errors.push(format!(
                "catalog: the user config {display} could not be read, so the deck defaults apply: {error}"
            ));
            resolved
        }
    };
    resolved.user_config = Some(display);
    resolved
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
}
