//! PRD #1666 S8: edit one list in a JSONC file without disturbing anything
//! else in it — OpenCode's `tui.json`, whose `plugin` list carries the deck's
//! TUI plugin (design §7.2).
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

/// One element of the root's `plugin` array: its span and its value.
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
    /// The `plugin` array: offset just past its `[`, and its elements.
    plugin: Option<(usize, Vec<Element>)>,
}

/// The `plugin` list a document holds, after the checks every edit makes.
fn scan(text: &str) -> Result<Root, Refusal> {
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
    let root = parser.root_object()?;
    parser.skip_trivia()?;
    if parser.pos != parser.bytes.len() {
        return refuse("it is not valid JSONC (text after the top-level object)");
    }
    if root.plugin.is_none()
        && root
            .value
            .get("tui")
            .and_then(Value::as_object)
            .is_some_and(|tui| tui.contains_key("plugin"))
    {
        return refuse(
            "its plugin list is under \"tui\" rather than at the top level, so where to add an entry is ambiguous",
        );
    }
    Ok(root)
}

/// Whether `text`'s root `plugin` list holds an entry `is_ours` accepts.
pub fn has_entry(text: &str, is_ours: impl Fn(&str) -> bool) -> Result<bool, Refusal> {
    let root = scan(text)?;
    Ok(root.plugin.is_some_and(|(_, elements)| {
        elements
            .iter()
            .any(|e| e.value.as_str().is_some_and(&is_ours))
    }))
}

/// `text` with `entry` added as the first element of the root's `plugin`
/// list, creating the list as the root's first member when there is none.
pub fn insert_entry(text: &str, entry: &str) -> Result<String, Refusal> {
    let root = scan(text)?;
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
    verify(&result, &expected)?;
    Ok(result)
}

/// `text` with every element of the root's `plugin` list that `is_ours`
/// accepts removed, each with one adjacent comma; `None` when there is none.
pub fn remove_entries(
    text: &str,
    is_ours: impl Fn(&str) -> bool,
) -> Result<Option<String>, Refusal> {
    let original = scan(text)?;
    let mut current = text.to_string();
    let mut removed = false;
    // One element per pass, re-scanning after each splice; bounded by the
    // list's length.
    let budget = original.plugin.as_ref().map_or(0, |(_, e)| e.len());
    for _ in 0..budget {
        let root = scan(&current)?;
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
    if let Some(list) = expected.get_mut("plugin").and_then(Value::as_array_mut) {
        list.retain(|v| !v.as_str().is_some_and(&is_ours));
    }
    verify(&current, &expected)?;
    Ok(Some(current))
}

/// The bytes to cut to remove element `n` with one comma next to it.
fn removal_range(text: &str, elements: &[Element], n: usize) -> Range<usize> {
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

fn verify(result: &str, expected: &Map<String, Value>) -> Result<(), Refusal> {
    match scan(result) {
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

    fn root_object(&mut self) -> Result<Root, Refusal> {
        self.expect(b'{')?;
        let open = self.pos;
        let mut value = Map::new();
        let mut plugin = None;
        self.members(|parser, key| {
            if key == "plugin" {
                if plugin.is_some() || value.contains_key("plugin") {
                    return refuse("it lists \"plugin\" more than once at the top level");
                }
                parser.skip_trivia()?;
                if parser.peek() != Some(b'[') {
                    return refuse("its \"plugin\" is not a list");
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

    #[test]
    fn an_entry_is_recognised_in_either_spelling() {
        assert!(has_entry("{\"plugin\":[\"file:///deck/tui.js\"]}", ours).unwrap());
        assert!(!has_entry("{\"plugin\":[\"/deck/tui.jsx\"]}", ours).unwrap());
        assert!(!has_entry("{}", ours).unwrap());
    }
}
