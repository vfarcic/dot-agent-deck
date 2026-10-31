//! PRD #1666 S8: edit one list in a JSONC file without disturbing anything
//! else in it — OpenCode's `tui.json`, whose `plugin` list carries the deck's
//! TUI plugin (design §7.2), and (PRD #2050) OpenCode 2.x's `cli.json`, whose
//! `plugins` list can carry a copy of that entry migrated from `tui.json`.
//!
//! Pure: text in, text out, no I/O. The dialect is OpenCode's: `//` and
//! `/* */` comments, and trailing commas in objects and arrays. Every edit is
//! a splice into the original text, so comments, spacing and member order
//! survive, and every result is parsed again and compared with what the edit
//! meant to do, so a scanner mistake is a refusal, never a damaged file.

use serde_json::{Map, Value};
use std::ops::Range;

/// Why a file was left alone. The wording is shown to the user after
/// "Left <file> unchanged: ".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal(why.into()))
}

/// The top-level list the 1.x `tui.json` edits work on.
const TUI_LIST: &str = "plugin";

/// One element of the root's list: its span and its value.
#[derive(Debug, Clone)]
struct Element {
    span: Range<usize>,
    value: Value,
    /// Where the comma after it is, if one is.
    comma: Option<usize>,
}

/// What the scan learned about a document's root object.
#[derive(Debug)]
struct Root {
    value: Map<String, Value>,
    /// Byte offset just past the root's `{`.
    open: usize,
    /// The list being edited: offset just past its `[`, and its elements.
    plugin: Option<(usize, Vec<Element>)>,
}

/// The `key` list a document holds at its top level, after the checks every
/// edit makes.
fn scan(text: &str, key: &str) -> Result<Root, Refusal> {
    if text.contains("{env:") || text.contains("{file:") {
        return refuse(
            "it uses {env:…} or {file:…} substitution, so the list OpenCode reads is not the one the file shows",
        );
    }
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        pos: 0,
    };
    parser.skip_trivia()?;
    if parser.peek() != Some(b'{') {
        return refuse("its top level is not a JSON object");
    }
    let root = parser.root_object(key)?;
    parser.skip_trivia()?;
    if parser.pos != parser.bytes.len() {
        return refuse("it is not valid JSONC (text after the top-level object)");
    }
    if root.plugin.is_none()
        && root
            .value
            .get("tui")
            .and_then(Value::as_object)
            .is_some_and(|tui| tui.contains_key(key))
    {
        return refuse(format!(
            "its {key} list is under \"tui\" rather than at the top level, so where to add an entry is ambiguous"
        ));
    }
    Ok(root)
}

/// Whether `text`'s root `plugin` list holds an entry `is_ours` accepts.
pub fn has_entry(text: &str, is_ours: impl Fn(&str) -> bool) -> Result<bool, Refusal> {
    let root = scan(text, TUI_LIST)?;
    Ok(root.plugin.is_some_and(|(_, elements)| {
        elements
            .iter()
            .any(|e| e.value.as_str().is_some_and(&is_ours))
    }))
}

/// `text` with `entry` added as the first element of the root's `plugin`
/// list, creating the list as the root's first member when there is none.
pub fn insert_entry(text: &str, entry: &str) -> Result<String, Refusal> {
    let root = scan(text, TUI_LIST)?;
    let literal = serde_json::to_string(entry).map_err(|e| Refusal(e.to_string()))?;
    let (at, insertion) = match &root.plugin {
        Some((open, elements)) => {
            let tail = if elements.is_empty() { "" } else { ", " };
            (*open, format!("{literal}{tail}"))
        }
        None => {
            let has_members = !root.value.is_empty();
            let rest = &text[root.open..];
            let indent = rest
                .strip_prefix('\n')
                .or_else(|| rest.strip_prefix("\r\n"))
                .map(|after| {
                    let width = after.len() - after.trim_start_matches([' ', '\t']).len();
                    format!("\n{}", &after[..width])
                });
            let member = format!("\"plugin\": [{literal}]");
            match (has_members, indent) {
                (true, Some(indent)) => (root.open, format!("{indent}{member},")),
                (true, None) => (root.open, format!("{member},")),
                (false, _) => (root.open, member),
            }
        }
    };
    let mut result = String::with_capacity(text.len() + insertion.len());
    result.push_str(&text[..at]);
    result.push_str(&insertion);
    result.push_str(&text[at..]);

    let mut expected = root.value.clone();
    match expected.get_mut("plugin").and_then(Value::as_array_mut) {
        Some(list) => list.insert(0, Value::String(entry.to_string())),
        None => {
            expected.insert(
                "plugin".to_string(),
                Value::Array(vec![Value::String(entry.to_string())]),
            );
        }
    }
    verify(&result, TUI_LIST, &expected)?;
    Ok(result)
}

/// `text` with every element of the root's `plugin` list that `is_ours`
/// accepts removed, each with one adjacent comma; `None` when there is none.
pub fn remove_entries(
    text: &str,
    is_ours: impl Fn(&str) -> bool,
) -> Result<Option<String>, Refusal> {
    remove_list_entries(text, TUI_LIST, is_ours)
}

/// [`remove_entries`] for the root's `key` list (`plugins` in OpenCode 2.x's
/// `cli.json`). Only string elements are ever matched, so an object entry
/// (`{ "package": …, "options": … }`) is always kept.
pub fn remove_list_entries(
    text: &str,
    key: &str,
    is_ours: impl Fn(&str) -> bool,
) -> Result<Option<String>, Refusal> {
    let original = scan(text, key)?;
    let mut current = text.to_string();
    let mut removed = false;
    // One element per pass, re-scanning after each splice; bounded by the
    // list's length.
    let budget = original.plugin.as_ref().map_or(0, |(_, e)| e.len());
    for _ in 0..budget {
        let root = scan(&current, key)?;
        let Some((_, elements)) = root.plugin else {
            break;
        };
        let Some(n) = elements
            .iter()
            .position(|e| e.value.as_str().is_some_and(&is_ours))
        else {
            break;
        };
        let range = removal_range(&current, &elements, n);
        current.replace_range(range, "");
        removed = true;
    }
    if !removed {
        return Ok(None);
    }
    let mut expected = original.value.clone();
    if let Some(list) = expected.get_mut(key).and_then(Value::as_array_mut) {
        list.retain(|v| !v.as_str().is_some_and(&is_ours));
    }
    verify(&current, key, &expected)?;
    Ok(Some(current))
}

/// The bytes to cut to remove element `n` with one comma next to it.
///
/// PRD #2050: in a list written one element per line (OpenCode 2.x writes
/// `cli.json` that way), the line break and indentation that belonged to the
/// element go with it, so no blank line is left: an element followed by
/// another takes the line break after its comma and the next line's
/// indentation, and a list's only element takes the line break and
/// indentation before it. Only spaces, tabs and one line break are ever
/// taken, so a comment next to the element is never joined to another line.
fn removal_range(text: &str, elements: &[Element], n: usize) -> Range<usize> {
    let line_break_after = |at: usize| {
        let rest = &text[at..];
        let after = rest
            .strip_prefix('\n')
            .or_else(|| rest.strip_prefix("\r\n"))?;
        Some(text.len() - after.trim_start_matches([' ', '\t']).len())
    };
    let line_break_before = |at: usize| {
        let before = text[..at].trim_end_matches([' ', '\t']);
        let before = before
            .strip_suffix('\n')
            .map(|b| b.strip_suffix('\r').unwrap_or(b))?;
        let before = before.trim_end_matches([' ', '\t']);
        before.ends_with('[').then_some(before.len())
    };
    let range = cut_range(text, elements, n);
    if n + 1 < elements.len() {
        match line_break_after(range.end) {
            Some(end) => range.start..end,
            None => range,
        }
    } else if elements.len() == 1 {
        match line_break_before(range.start) {
            Some(start) => start..range.end,
            None => range,
        }
    } else {
        range
    }
}

/// [`removal_range`] before the line tidying: the element and one comma.
fn cut_range(text: &str, elements: &[Element], n: usize) -> Range<usize> {
    let element = &elements[n];
    let spaces = |from: usize| {
        from + text[from..].len() - text[from..].trim_start_matches([' ', '\t']).len()
    };
    match element.comma {
        // A later element follows: take this one, its comma and the spaces
        // after it, which is exactly what an insert added.
        Some(comma) if n + 1 < elements.len() => element.span.start..spaces(comma + 1),
        // A trailing comma and an earlier element: cut from the earlier
        // element's comma, keeping the trailing one.
        Some(_) if n > 0 => match elements[n - 1].comma {
            Some(previous) => previous..element.span.end,
            None => element.span.clone(),
        },
        // The only element, with a trailing comma.
        Some(comma) => element.span.start..comma + 1,
        None if n > 0 => match elements[n - 1].comma {
            Some(previous) => previous..element.span.end,
            None => element.span.clone(),
        },
        None => element.span.clone(),
    }
}

fn verify(result: &str, key: &str, expected: &Map<String, Value>) -> Result<(), Refusal> {
    match scan(result, key) {
        Ok(root) if root.value == *expected => Ok(()),
        _ => refuse("the edit could not be made without changing something else in the file"),
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    pos: usize,
}

const DEPTH_LIMIT: usize = 128;

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn invalid<T>(&self) -> Result<T, Refusal> {
        refuse(format!("it is not valid JSONC (at byte {})", self.pos))
    }

    /// Skip whitespace and comments.
    fn skip_trivia(&mut self) -> Result<(), Refusal> {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n' | b'\r') => self.pos += 1,
                Some(b'/') => match self.bytes.get(self.pos + 1) {
                    Some(b'/') => {
                        while let Some(c) = self.peek() {
                            if c == b'\n' {
                                break;
                            }
                            self.pos += 1;
                        }
                    }
                    Some(b'*') => {
                        let Some(end) = self.text[self.pos + 2..].find("*/") else {
                            return refuse("it is not valid JSONC (an unterminated comment)");
                        };
                        self.pos += 2 + end + 2;
                    }
                    _ => return self.invalid(),
                },
                _ => return Ok(()),
            }
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), Refusal> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            self.invalid()
        }
    }

    fn root_object(&mut self, list: &str) -> Result<Root, Refusal> {
        self.expect(b'{')?;
        let open = self.pos;
        let mut value = Map::new();
        let mut plugin = None;
        self.members(|parser, key| {
            if key == list {
                if plugin.is_some() || value.contains_key(list) {
                    return refuse(format!(
                        "it lists \"{list}\" more than once at the top level"
                    ));
                }
                parser.skip_trivia()?;
                if parser.peek() != Some(b'[') {
                    return refuse(format!("its \"{list}\" is not a list"));
                }
                let (open, elements) = parser.plugin_array()?;
                let list = elements.iter().map(|e| e.value.clone()).collect();
                plugin = Some((open, elements));
                value.insert(key, Value::Array(list));
            } else {
                let parsed = parser.value(1)?;
                value.insert(key, parsed);
            }
            Ok(())
        })?;
        Ok(Root {
            value,
            open,
            plugin,
        })
    }

    /// Parse an object's members after its `{`, through its `}`, handing each
    /// key to `member` with the parser positioned before its value.
    fn members(
        &mut self,
        mut member: impl FnMut(&mut Self, String) -> Result<(), Refusal>,
    ) -> Result<(), Refusal> {
        loop {
            self.skip_trivia()?;
            match self.peek() {
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(());
                }
                Some(b'"') => {}
                _ => return self.invalid(),
            }
            let key = self.string()?;
            self.skip_trivia()?;
            self.expect(b':')?;
            self.skip_trivia()?;
            member(self, key)?;
            self.skip_trivia()?;
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {}
                _ => return self.invalid(),
            }
        }
    }

    fn plugin_array(&mut self) -> Result<(usize, Vec<Element>), Refusal> {
        self.expect(b'[')?;
        let open = self.pos;
        let mut elements = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.peek() == Some(b']') {
                self.pos += 1;
                return Ok((open, elements));
            }
            let start = self.pos;
            let value = self.value(2)?;
            let end = self.pos;
            self.skip_trivia()?;
            let comma = match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                    Some(self.pos - 1)
                }
                Some(b']') => None,
                _ => return self.invalid(),
            };
            elements.push(Element {
                span: start..end,
                value,
                comma,
            });
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, Refusal> {
        if depth > DEPTH_LIMIT {
            return refuse("it nests too deeply to edit");
        }
        self.skip_trivia()?;
        match self.peek() {
            Some(b'{') => {
                self.pos += 1;
                let mut map = Map::new();
                self.members(|parser, key| {
                    let value = parser.value(depth + 1)?;
                    map.insert(key, value);
                    Ok(())
                })?;
                Ok(Value::Object(map))
            }
            Some(b'[') => {
                self.pos += 1;
                let mut list = Vec::new();
                loop {
                    self.skip_trivia()?;
                    if self.peek() == Some(b']') {
                        self.pos += 1;
                        return Ok(Value::Array(list));
                    }
                    list.push(self.value(depth + 1)?);
                    self.skip_trivia()?;
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b']') => {}
                        _ => return self.invalid(),
                    }
                }
            }
            Some(b'"') => self.string().map(Value::String),
            Some(_) => {
                let start = self.pos;
                while let Some(c) = self.peek() {
                    if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let token = &self.text[start..self.pos];
                match token {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" => Ok(Value::Null),
                    _ => match serde_json::from_str::<serde_json::Number>(token) {
                        Ok(number) if !token.is_empty() => Ok(Value::Number(number)),
                        _ => {
                            self.pos = start;
                            self.invalid()
                        }
                    },
                }
            }
            None => self.invalid(),
        }
    }

    /// A JSON string literal, decoded by `serde_json`.
    fn string(&mut self) -> Result<String, Refusal> {
        let start = self.pos;
        self.expect(b'"')?;
        loop {
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => self.pos += 2,
                Some(b'\n') | None => return self.invalid(),
                Some(_) => self.pos += 1,
            }
        }
        if self.pos > self.bytes.len() {
            return self.invalid();
        }
        serde_json::from_str(&self.text[start..self.pos]).or_else(|_| {
            self.pos = start;
            self.invalid()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours(entry: &str) -> bool {
        entry == "/deck/tui.js" || entry == "file:///deck/tui.js"
    }

    #[test]
    fn insert_keeps_every_other_byte() {
        let before =
            "{\n  // c\n  \"theme\": \"dark\", /* k */\n  \"plugin\": [\"a\", \"b\",],\n}\n";
        assert_eq!(
            insert_entry(before, "/deck/tui.js").unwrap(),
            before.replacen('[', "[\"/deck/tui.js\", ", 1)
        );
        assert_eq!(
            insert_entry("{\"plugin\":[]}", "/deck/tui.js").unwrap(),
            "{\"plugin\":[\"/deck/tui.js\"]}"
        );
    }

    #[test]
    fn insert_without_a_list_adds_the_first_member() {
        assert_eq!(
            insert_entry("{}", "/deck/tui.js").unwrap(),
            "{\"plugin\": [\"/deck/tui.js\"]}"
        );
        assert_eq!(
            insert_entry("{\n  \"theme\": 1\n}", "/deck/tui.js").unwrap(),
            "{\n  \"plugin\": [\"/deck/tui.js\"],\n  \"theme\": 1\n}"
        );
    }

    #[test]
    fn ambiguous_or_invalid_files_are_refused() {
        for text in [
            "",
            "[]",
            "{broken",
            "{/* open",
            "{\"plugin\":[],\"plugin\":[]}",
            "{\"plugin\":\"x\"}",
            "{\"tui\":{\"plugin\":[]}}",
            "{\"theme\":\"{env:X}\"}",
            "{\"theme\":\"{file:x}\"}",
            "{} {}",
            "{\"a\":tru}",
        ] {
            assert!(insert_entry(text, "/deck/tui.js").is_err(), "{text:?}");
        }
    }

    #[test]
    fn remove_takes_one_adjacent_comma() {
        for (before, after) in [
            ("{\"plugin\":[\"/deck/tui.js\"]}", "{\"plugin\":[]}"),
            ("{\"plugin\":[\"/deck/tui.js\",]}", "{\"plugin\":[]}"),
            (
                "{\"plugin\":[\"/deck/tui.js\", \"u\"]}",
                "{\"plugin\":[\"u\"]}",
            ),
            (
                "{\"plugin\":[\"u\", \"/deck/tui.js\"]}",
                "{\"plugin\":[\"u\"]}",
            ),
            (
                "{\"plugin\":[\"u\", \"file:///deck/tui.js\",]}",
                "{\"plugin\":[\"u\",]}",
            ),
            (
                "{/*k*/\"plugin\":[\"/deck/tui.js\",\"file:///deck/tui.js\",\"dot-agent-deck-x\",]}",
                "{/*k*/\"plugin\":[\"dot-agent-deck-x\",]}",
            ),
        ] {
            assert_eq!(
                remove_entries(before, ours).unwrap().as_deref(),
                Some(after)
            );
        }
        assert_eq!(remove_entries("{\"plugin\":[\"u\"]}", ours).unwrap(), None);
        assert!(remove_entries("{broken", ours).is_err());
    }

    /// PRD #2050: OpenCode 2.x's `cli.json` holds a `plugins` list, in the
    /// two-space layout OpenCode writes; removing the deck's entry keeps every
    /// other entry — strings and `{ package, options }` objects — and every
    /// other member and byte, and leaves the `plugin` key of a 1.x file alone.
    #[test]
    fn remove_from_a_named_list_keeps_everything_else() {
        let before = "{\n  \"$schema\": \"https://opencode.ai/cli.json\",\n  \"plugins\": [\n    \"/deck/tui.js\",\n    \"user-plugin\",\n    {\n      \"package\": \"p\",\n      \"options\": {\"a\": 1}\n    }\n  ],\n  \"theme\": {\"name\": \"x\"}\n}\n";
        let after = "{\n  \"$schema\": \"https://opencode.ai/cli.json\",\n  \"plugins\": [\n    \"user-plugin\",\n    {\n      \"package\": \"p\",\n      \"options\": {\"a\": 1}\n    }\n  ],\n  \"theme\": {\"name\": \"x\"}\n}\n";
        assert_eq!(
            remove_list_entries(before, "plugins", ours)
                .unwrap()
                .as_deref(),
            Some(after)
        );
        let only = "{\n  \"plugins\": [\n    \"/deck/tui.js\"\n  ]\n}\n";
        assert_eq!(
            remove_list_entries(only, "plugins", ours)
                .unwrap()
                .as_deref(),
            Some("{\n  \"plugins\": [\n  ]\n}\n")
        );
        assert_eq!(
            remove_list_entries("{\"plugin\":[\"/deck/tui.js\"]}", "plugins", ours).unwrap(),
            None,
            "only the named list is edited"
        );
        assert_eq!(
            remove_list_entries("{\"plugins\":[\"u\"]}", "plugins", ours).unwrap(),
            None
        );
        assert!(remove_list_entries("{\"plugins\":\"x\"}", "plugins", ours).is_err());
        // The last of several, a trailing comma, and a comment beside an
        // element: only spaces, tabs and one line break are ever taken.
        assert_eq!(
            remove_list_entries(
                "{\n  \"plugins\": [\n    \"u\",\n    \"/deck/tui.js\"\n  ]\n}",
                "plugins",
                ours
            )
            .unwrap()
            .as_deref(),
            Some("{\n  \"plugins\": [\n    \"u\"\n  ]\n}")
        );
        assert_eq!(
            remove_list_entries("{\"plugins\": [\n  \"/deck/tui.js\",\n]}", "plugins", ours)
                .unwrap()
                .as_deref(),
            Some("{\"plugins\": [\n]}")
        );
        assert_eq!(
            remove_list_entries(
                "{\"plugins\": [ // mine\n  \"/deck/tui.js\",\n  \"u\"\n]}",
                "plugins",
                ours
            )
            .unwrap()
            .as_deref(),
            Some("{\"plugins\": [ // mine\n  \"u\"\n]}")
        );
    }

    #[test]
    fn an_entry_is_recognised_in_either_spelling() {
        assert!(has_entry("{\"plugin\":[\"file:///deck/tui.js\"]}", ours).unwrap());
        assert!(!has_entry("{\"plugin\":[\"/deck/tui.jsx\"]}", ours).unwrap());
        assert!(!has_entry("{}", ours).unwrap());
    }
}
