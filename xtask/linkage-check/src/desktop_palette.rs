//! PRD #743 M2: no hard-coded colour under `desktop/src` outside the palette.
//!
//! The desktop app follows the OS light/dark appearance, and a colour written
//! straight into a rule cannot follow anything — it stays light in dark mode.
//! M1 collapsed 150 hex literals onto a semantic token set on `:root`; this is
//! what stops the next one being added. It has to exist, because the decay is
//! otherwise invisible: **nothing renders dark mode in CI**, so a component that
//! hard-codes `#f8f6f0` looks perfect on every screenshot anyone takes and is
//! broken only for the users who run their machine dark.
//!
//! It lives here rather than in a vitest file for one reason: these tests run
//! under `cargo test-fast` (via `--workspace`, CLAUDE.md rule 5) and in the CI
//! `build` job, which is one of the four **required** checks. `desktop-web`,
//! where a vitest guard would run, is advisory — so a guard there could be
//! merged past, which is exactly the failure mode this is meant to prevent.
//!
//! What counts as a colour:
//!
//! - hex literals — `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`;
//! - the colour functions `rgb() rgba() hsl() hsla() hwb() lab() lch() oklab()
//!   oklch() color()`, **unless** the arguments name a token, so the
//!   `rgb(var(--scrim-rgb) / .42)` form the scrims and shadows use is fine;
//! - the bare keywords `white` and `black`.
//!
//! The functional forms are included deliberately. The brief allowed leaving
//! `rgba(0, 0, 0, .4)` alone as "a backdrop shadow, arguably not a theme
//! colour", but a drop shadow tuned for a light surface is invisible on a dark
//! one, so it is a theme colour in the way that matters — and with a per-line
//! opt-out available, catching too much costs a comment while catching too
//! little costs a bug nobody sees. `transparent`, `currentColor` and `inherit`
//! carry no value of their own and are never flagged.
//!
//! Two things are allowed to hold a literal:
//!
//! - a **custom-property declaration inside a `:root` block of
//!   `desktop/src/styles.css` itself** — that is the palette. The exemption is
//!   a custom property specifically, so `:root { color: #1d2522; }` (which is
//!   how the pre-M1 file duplicated `--ink`) still fails; it covers any
//!   `:root`-prefixed selector so M3's `:root[data-theme="dark"]` and its
//!   `prefers-color-scheme` block land without touching this file; and it is
//!   that one path, not any file *named* `styles.css`, because the palette is
//!   supposed to live in exactly one place;
//! - any line carrying `theme-invariant: <reason>` **in a comment, with a
//!   reason**. That is not laziness: the xterm palette is genuinely outside the
//!   theme (PRD #743 keeps the terminals dark in both appearances), and PR #779
//!   has light hexes in flight on another branch, so whichever of the two lands
//!   second needs a note rather than a wall. Both qualifiers are enforced — the
//!   marker is read from the line's comment text and nowhere else, so a string
//!   literal quoting it exempts nothing, and a bare `theme-invariant:` with
//!   nothing after it is a `TODO` in disguise rather than an opt-out.
//!
//! Comments are masked before scanning, which is load-bearing rather than
//! tidy: `PR #416` and `(PRD #743)` are three valid hex digits behind a `#` and
//! appear in both `src/lib/bridge.ts` and the opt-out markers themselves.
//!
//! String contents are **not** masked — `"#141817"` is precisely what the scan
//! is for — so the same three digits behind a `#` come back inside a string
//! literal, and `describe("AgentOverview across a fleet (PRD #742 M4)")` failed
//! `cargo test-fast` as a hard-coded colour. Any issue number of 3, 4, 6 or 8
//! digits does. [`is_issue_reference`] is the narrow exemption: a decimal run
//! introduced by one of [`ISSUE_CUES`], or wrapped in its own parentheses as
//! `(#742)`. Both halves of the cue form are load-bearing — decimal alone would
//! exempt `#000` and `#333`, and a cue word alone would let `PRD #fff` through
//! — and a general "any word then a space" rule was rejected because
//! `1px solid #333` would satisfy it. The parenthesised form needs **both**
//! brackets for the same reason; [`is_issue_reference`]'s own doc enumerates
//! the CSS each looser spelling would have let through.
//!
//! **Both guards fail closed.** Every filesystem error — a directory that
//! cannot be listed, an entry that cannot be typed, a file that cannot be read
//! — fails the test instead of removing files from the scan, and a symlink is
//! refused rather than followed. A required CI check that can inspect nothing
//! and still pass is worse than no check at all, because it reports safety it
//! never established.
//!
//! Same budget as the other file-reading modules here (`pin_lockstep`,
//! `junit_strip`): no network, no git, no sleep, no subprocess.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::paths::slash_path;

/// The marker that exempts the line it appears on.
const OPT_OUT: &str = "theme-invariant:";

/// Colour functions whose literal (non-`var()`) form is a hard-coded colour.
const COLOUR_FNS: [&str; 10] = [
    "rgba", "rgb", "hsla", "hsl", "hwb", "oklab", "oklch", "lab", "lch", "color",
];

/// Bare CSS colour keywords worth catching. Kept to the two that actually
/// appear as values and cannot be mistaken for anything else.
const COLOUR_WORDS: [&str; 2] = ["white", "black"];

/// Words that introduce a GitHub issue or pull-request number.
///
/// Masking comments covers `// PR #416`, but a `#` inside a **string literal**
/// is deliberately kept — `"#141817"` is exactly what the scan is for — and a
/// three-digit issue number is three valid hex digits. `describe("... (PRD
/// #742 M4)")` therefore failed as a hard-coded colour, and so would any issue
/// whose number is 3, 4, 6 or 8 digits long. See [`is_issue_reference`].
///
/// The nouns are the original set; the verbs are GitHub's own closing keywords
/// minus the past participles (see below), plus `refs`/`ref`/`see`. They are
/// here because a reference introduced by one of them is just as ordinary as
/// `PRD #742` and was still being flagged, which forced the same `#`-dropping
/// workaround this exemption exists to retire.
///
/// **`fixed` is deliberately absent, and it is the one that shows where the
/// boundary is.** Every cue added is one more word that could in principle
/// stand immediately left of a real colour, and `fixed` is a CSS *value*:
/// `background` is an any-order shorthand, so `background: fixed #333` is valid
/// and would be laundered. Its two siblings `closed` and `resolved` are left
/// out with it, so the list states a rule someone can restate — the base and
/// `-s` forms, never the past participle — rather than an arbitrary set with
/// one hole punched in it. `fixes #742` is the spelling people actually write
/// in a source comment; `fixed #742` is a commit-message spelling and costs a
/// CSS keyword to admit.
const ISSUE_CUES: [&str; 16] = [
    "issue", "issues", "pr", "prd", "pull", "gh", "github", "fix", "fixes", "close", "closes",
    "resolve", "resolves", "ref", "refs", "see",
];

/// The workspace root, from this crate's manifest dir rather than the process
/// cwd, so the tests do not depend on how the runner was invoked.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("xtask/linkage-check sits two levels below the workspace root")
        .to_path_buf()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Finding {
    /// Path as given to the scan, so the message points somewhere clickable.
    file: String,
    /// 1-indexed.
    line: usize,
    literal: String,
    source: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Mask {
    Code,
    LineComment,
    BlockComment,
    Str(char),
}

/// One source file split into the code the scanner reads and the comment text
/// the opt-out marker is read from.
struct Masked {
    /// Every comment replaced by spaces, newlines kept so line numbers survive.
    /// String contents are kept — a literal inside `"#141817"` is exactly what
    /// the scan is looking for — which is also why the masker must know where
    /// strings start and end, so a `//` or `/*` inside one does not swallow the
    /// rest of the file.
    code: String,
    /// The comment text on each line, concatenated, indexed from 0. Kept
    /// separately so the `theme-invariant:` marker is read from a **comment**
    /// and nowhere else: on the raw line, a string literal that merely mentions
    /// the marker would exempt a hard-coded colour sitting beside it.
    comments: Vec<String>,
}

impl Masked {
    /// The comment text on a 1-indexed line, or `""` where the line has none.
    fn comment_on(&self, line: usize) -> &str {
        self.comments
            .get(line - 1)
            .map(String::as_str)
            .unwrap_or_default()
    }
}

/// Split `src` into masked code and per-line comment text.
///
/// `line_comments` is false for CSS, where `//` is not a comment.
fn mask_comments(src: &str, line_comments: bool) -> Masked {
    let mut code = String::with_capacity(src.len());
    let mut comments: Vec<String> = vec![String::new()];
    let mut state = Mask::Code;
    let mut escaped = false;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        // Line bookkeeping first and in every state, so a comment spanning
        // several lines contributes to each of them separately.
        if c == '\n' {
            comments.push(String::new());
        }
        match state {
            Mask::Code => {
                if c == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    state = Mask::BlockComment;
                    code.push_str("  ");
                } else if line_comments && c == '/' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = Mask::LineComment;
                    code.push_str("  ");
                } else {
                    if c == '"' || c == '\'' || c == '`' {
                        state = Mask::Str(c);
                        escaped = false;
                    }
                    code.push(c);
                }
            }
            Mask::LineComment => {
                if c == '\n' {
                    state = Mask::Code;
                    code.push('\n');
                } else {
                    code.push(' ');
                    comments.last_mut().expect("a current line").push(c);
                }
            }
            Mask::BlockComment => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = Mask::Code;
                    code.push_str("  ");
                } else if c == '\n' {
                    code.push('\n');
                } else {
                    code.push(' ');
                    comments.last_mut().expect("a current line").push(c);
                }
            }
            Mask::Str(quote) => {
                code.push(c);
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == quote {
                    state = Mask::Code;
                } else if c == '\n' && quote != '`' {
                    // Unterminated quote (an apostrophe in prose, most likely).
                    // Recover at the newline rather than masking the rest of
                    // the file, which would turn a typo into a silent pass.
                    state = Mask::Code;
                }
            }
        }
    }
    Masked { code, comments }
}

/// The reason attached to a `theme-invariant:` marker in `comment`, if it has
/// one.
///
/// A marker with nothing after it is **not** an opt-out: `theme-invariant:` on
/// its own is a `TODO` wearing a disguise, and the guard's whole value is that
/// every exemption says why it exists. The first marker carrying a non-empty
/// reason wins, so a line may hold more than one.
fn opt_out_reason(comment: &str) -> Option<&str> {
    comment
        .split(OPT_OUT)
        .skip(1)
        .map(str::trim)
        .find(|reason| !reason.is_empty())
}

/// The 1-indexed lines of `masked` that sit inside a `:root…{ … }` block, at any
/// nesting depth, so a `:root` inside `@media (prefers-color-scheme: dark)`
/// counts too.
fn root_block_lines(masked: &str) -> BTreeSet<usize> {
    let mut lines = BTreeSet::new();
    let mut depth = 0usize;
    let mut selector_start = 0usize;
    let mut line = 1usize;
    let mut open: Option<(usize, usize)> = None; // (depth of the block, first line)

    for (i, c) in masked.char_indices() {
        match c {
            '\n' => line += 1,
            '{' => {
                depth += 1;
                if open.is_none() && masked[selector_start..i].trim().starts_with(":root") {
                    open = Some((depth, line));
                }
                selector_start = i + 1;
            }
            '}' => {
                if let Some((block_depth, first)) = open
                    && block_depth == depth
                {
                    lines.extend(first..=line);
                    open = None;
                }
                depth = depth.saturating_sub(1);
                selector_start = i + 1;
            }
            ';' => selector_start = i + 1,
            _ => {}
        }
    }
    lines
}

fn is_hex(c: char) -> bool {
    c.is_ascii_hexdigit()
}

/// Identifier characters for CSS/TS, used for the "not part of a longer word"
/// checks. `-` counts, which is what keeps `white-space` and `background-color`
/// out of the results.
fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Every colour literal on one already-masked line.
fn literals_in(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut found = Vec::new();

    for (i, &c) in chars.iter().enumerate() {
        if c == '#' {
            let run = chars[i + 1..]
                .iter()
                .take_while(|c| is_hex(**c))
                .count()
                .min(9);
            let next_is_ident = chars.get(i + 1 + run).copied().is_some_and(is_ident);
            if matches!(run, 3 | 4 | 6 | 8) && !next_is_ident && !is_issue_reference(&chars, i, run)
            {
                found.push(chars[i..=i + run].iter().collect());
            }
            continue;
        }

        if i > 0 && is_ident(chars[i - 1]) {
            continue;
        }
        let rest: String = chars[i..].iter().collect();

        if let Some(name) = COLOUR_FNS
            .iter()
            .find(|n| rest.starts_with(**n) && rest[n.len()..].starts_with('('))
        {
            let args = balanced_args(&rest[name.len()..]);
            if !args.contains("var(") {
                found.push(format!("{name}({args})"));
            }
            continue;
        }

        if let Some(word) = COLOUR_WORDS.iter().find(|w| {
            rest.starts_with(**w) && !chars.get(i + w.len()).copied().is_some_and(is_ident)
        }) {
            // A value, not a property or an object key: `background: white;` and
            // `"white"` count, `white: "#d8ddd8"` and `whiteSpace` do not.
            let after = rest[word.len()..].trim_start();
            let terminated =
                after.is_empty() || after.starts_with([';', '}', ')', ',', '!', '"', '\'', ']']);
            let before = chars[..i].iter().rev().find(|c| !c.is_whitespace());
            let valuish = before.is_none_or(|c| matches!(c, ':' | ',' | '(' | '[' | '"' | '\''));
            if terminated && valuish {
                found.push((*word).to_string());
            }
        }
    }
    found
}

/// Whether the `#` at `at`, carrying a `run`-digit hex run, is an issue or
/// pull-request reference rather than a colour.
///
/// **Both halves are required, and each is what stops the other from being a
/// hole.**
///
/// * *The run is entirely decimal.* An issue number is decimal; a colour is
///   written for its hex. This is what keeps a cue word from laundering a real
///   literal — `PRD #fff` is still a finding, because `#fff` is not a number
///   anyone files.
/// * *A cue word introduces it.* Decimal alone would exempt `#000`, `#333` and
///   `#123456`, which are among the most commonly hard-coded colours there are.
///   Requiring one of [`ISSUE_CUES`] immediately to the left keeps every
///   CSS-value position flagged: `color: #333`, `1px solid #000` and
///   `"#000"` have no cue word in front of them and still fail.
///
/// Deliberately NOT "any word plus a space": `solid`, `inset` and `outset` are
/// ordinary alphabetic words that really do precede a colour in a shorthand, so
/// a general word rule would exempt `1px solid #333`. A fixed list is a little
/// arbitrary, and that is the price of not widening the hole.
///
/// The cue may be followed by any run of spaces or by none at all, so
/// `PRD #742`, `PRD  #742` and `PRD#742` all read the same way.
///
/// # The one cue-less spelling, and why it is exactly one
///
/// `(#742)` — a bare parenthesised reference with no word to its left — is as
/// conventional as any of the cues, and until now it failed: nothing alphabetic
/// precedes it, so the walk below finds `start == end`. It is admitted, and the
/// **closing** `)` is required as well as the opening `(`, which is what keeps
/// it from being a hole rather than an exemption:
///
/// * a *line-start* rule would exempt the second line of a wrapped
///   `linear-gradient(\n  #333,\n  #000\n)`, which is ordinary formatted CSS;
/// * a *string-start* rule would exempt `"#333"`, which is the single most
///   common hard-coded colour there is and one of the cases the tests pin;
/// * an *opening-paren* rule alone would exempt the first stop of a one-line
///   `linear-gradient(#333, #000)`.
///
/// Requiring `(` … `)` around the whole run leaves none of those: each ends in
/// `,` or a space rather than `)`. What is left is the shape `fn(#NNN)` — a
/// call whose sole argument is an all-decimal hex — and the two spellings of it
/// are `url(#742)`, an SVG fragment reference where exempting it is the right
/// answer anyway, and CSS Color 5's single-argument `contrast-color(#333)`,
/// which is the residual. That one is accepted knowingly rather than
/// overlooked: it is Safari-only at the time of writing, appears nowhere under
/// `desktop/src`, and a `theme-invariant:` marker is the escape hatch if it
/// ever needs one. Nothing wider than that is claimed here — this is the shape
/// to re-examine before widening the rule again.
fn is_issue_reference(chars: &[char], at: usize, run: usize) -> bool {
    if !chars[at + 1..=at + run].iter().all(char::is_ascii_digit) {
        return false;
    }
    if at > 0 && chars[at - 1] == '(' && chars.get(at + run + 1) == Some(&')') {
        return true;
    }
    let mut end = at;
    while end > 0 && chars[end - 1] == ' ' {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && chars[start - 1].is_ascii_alphabetic() {
        start -= 1;
    }
    if start == end {
        return false;
    }
    let word: String = chars[start..end]
        .iter()
        .collect::<String>()
        .to_ascii_lowercase();
    ISSUE_CUES.contains(&word.as_str())
}

/// The text between `(` and its matching `)`, or to end of input if the call is
/// unclosed on this line — an unclosed call still gets judged, so a wrapped
/// `rgba(` cannot slip through by putting its digits on the next line.
fn balanced_args(from_paren: &str) -> String {
    let mut depth = 0usize;
    let mut out = String::new();
    for c in from_paren.chars() {
        match c {
            '(' => {
                depth += 1;
                if depth == 1 {
                    continue;
                }
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return out;
                }
            }
            _ => {}
        }
        out.push(c);
    }
    out
}

/// The light palette's selector.
const LIGHT_BLOCK: &str = ":root";

/// The OS-default dark path. The `:not` is load-bearing: without it an explicit
/// Light choice would lose to a dark OS.
const DARK_MEDIA_BLOCK: &str = r#":root:not([data-theme="light"])"#;

/// The explicit-Dark path, which has to win on a light OS.
const DARK_ATTR_BLOCK: &str = r#":root[data-theme="dark"]"#;

/// One `:root…{ … }` block: what it is called, what it declares, and **where
/// it is**.
///
/// The byte range is what makes the nesting checkable. A selector found by text
/// alone says nothing about which rule it is inside, and the difference between
/// `:root:not([data-theme="light"])` inside the `@media` block and the same
/// selector dedented out of it is the difference between a dark palette that
/// applies on a dark OS and one that applies always — see
/// [`check_dark_palette`].
#[derive(Debug, Clone)]
struct RootBlock {
    selector: String,
    declarations: Vec<(String, String)>,
    /// The block body, `{`-exclusive to `}`-exclusive, in the masked source.
    body: std::ops::Range<usize>,
}

/// Every `:root…{ … }` block in `masked`, in source order. Whitespace in the
/// selector is normalised so a reformat cannot change what a block is called.
///
/// Runs on the *masked* source so a `--token: #hex` sitting in a comment is not
/// mistaken for a declaration.
fn root_blocks(masked: &str) -> Vec<RootBlock> {
    let mut blocks = Vec::new();
    let mut depth = 0usize;
    let mut selector_start = 0usize;
    let mut open: Option<(usize, String, usize)> = None; // (depth, selector, body start)

    for (i, c) in masked.char_indices() {
        match c {
            '{' => {
                depth += 1;
                let selector = masked[selector_start..i].trim();
                if open.is_none() && selector.starts_with(":root") {
                    let normalised = selector.split_whitespace().collect::<Vec<_>>().join(" ");
                    open = Some((depth, normalised, i + 1));
                }
                selector_start = i + 1;
            }
            '}' => {
                if let Some((block_depth, selector, body_start)) = open.clone()
                    && block_depth == depth
                {
                    blocks.push(RootBlock {
                        selector,
                        declarations: declarations(&masked[body_start..i]),
                        body: body_start..i,
                    });
                    open = None;
                }
                depth = depth.saturating_sub(1);
                selector_start = i + 1;
            }
            ';' => selector_start = i + 1,
            _ => {}
        }
    }
    blocks
}

/// The body of the `@media (prefers-color-scheme: dark)` rule in `masked`,
/// opening-brace-exclusive to matching-close-exclusive.
///
/// Found by tracking braces rather than by text, because the whole point of
/// having it is to answer "is that block *inside* this one?".
fn dark_media_body(masked: &str) -> Option<std::ops::Range<usize>> {
    let mut depth = 0usize;
    let mut prelude_start = 0usize;
    let mut open: Option<(usize, usize)> = None; // (depth, body start)

    for (i, c) in masked.char_indices() {
        match c {
            '{' => {
                depth += 1;
                if open.is_none() && is_dark_media_prelude(&masked[prelude_start..i]) {
                    open = Some((depth, i + 1));
                }
                prelude_start = i + 1;
            }
            '}' => {
                if let Some((block_depth, body_start)) = open
                    && block_depth == depth
                {
                    return Some(body_start..i);
                }
                depth = depth.saturating_sub(1);
                prelude_start = i + 1;
            }
            ';' => prelude_start = i + 1,
            _ => {}
        }
    }
    None
}

/// Whether a rule prelude is the dark `prefers-color-scheme` media query.
/// Whitespace-insensitive, so `@media(prefers-color-scheme:dark)` and a
/// reformat across two lines both match.
fn is_dark_media_prelude(prelude: &str) -> bool {
    let squashed: String = prelude
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    squashed.starts_with("@media") && squashed.contains("prefers-color-scheme:dark")
}

/// The `--custom-property: value` pairs in one block body, in source order.
/// Anything that is not a custom property is skipped, so `:root`'s own `color`
/// and `font-family` do not show up as tokens.
fn declarations(body: &str) -> Vec<(String, String)> {
    body.split(';')
        .filter_map(|decl| decl.split_once(':'))
        .filter_map(|(name, value)| {
            let name = name.trim();
            name.starts_with("--").then(|| {
                (
                    name.to_string(),
                    value.split_whitespace().collect::<Vec<_>>().join(" "),
                )
            })
        })
        .collect()
}

/// Whether a token's value is a colour, and so needs a dark counterpart.
///
/// By shape rather than by a hard-coded name list, so a layout token added to
/// `:root` later is exempt without anyone having to remember to exempt it.
/// Covers the two forms the palette uses: `#rrggbb` and the channel form
/// `R G B`.
fn looks_like_colour(value: &str) -> bool {
    if value.starts_with('#') {
        return true;
    }
    let parts: Vec<&str> = value.split_whitespace().collect();
    parts.len() == 3 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// Compare the palette's three `:root` blocks. `Err` is the operator-facing
/// report; `Ok` means the dark palette is complete and its two deliveries agree.
///
/// This exists because **nothing else can see either failure.** The colour guard
/// above only asks whether a colour came from a token, not whether that token
/// has a dark value; no test renders dark mode; and the two dark blocks are
/// necessarily separate rules, so a hand edit to one of them is invisible in
/// review unless the reviewer diffs them against each other by eye.
fn check_dark_palette(css: &str) -> Result<(), String> {
    let masked = mask_comments(css, false);
    let blocks = root_blocks(&masked.code);
    let find = |selector: &str| blocks.iter().find(|block| block.selector == selector);

    let mut problems = Vec::new();
    let (light, media, attr) = match (
        find(LIGHT_BLOCK),
        find(DARK_MEDIA_BLOCK),
        find(DARK_ATTR_BLOCK),
    ) {
        (Some(l), Some(m), Some(a)) => (l, m, a),
        (l, m, a) => {
            for (name, found) in [
                (LIGHT_BLOCK, l.is_some()),
                (DARK_MEDIA_BLOCK, m.is_some()),
                (DARK_ATTR_BLOCK, a.is_some()),
            ] {
                if !found {
                    problems.push(format!("no `{name}` block in desktop/src/styles.css"));
                }
            }
            return Err(report_dark(&problems));
        }
    };

    // 0. The two dark blocks have to be where their selectors imply they are.
    //    Finding `:root:not([data-theme="light"])` somewhere in the file says
    //    nothing about which rule encloses it, and a block dedented out of the
    //    `@media` wrapper applies its dark values in **light** mode with every
    //    other assertion here still green — the exact class this guard exists
    //    to close, at the one seam that is structural rather than textual.
    match dark_media_body(&masked.code) {
        None => problems.push(
            "no `@media (prefers-color-scheme: dark)` rule in desktop/src/styles.css".to_string(),
        ),
        Some(media_body) => {
            if !(media_body.start <= media.body.start && media.body.end <= media_body.end) {
                problems.push(format!(
                    "the `{DARK_MEDIA_BLOCK}` block is not inside the \
                     `@media (prefers-color-scheme: dark)` rule, so its dark values apply \
                     in light mode too"
                ));
            }
            if media_body.start <= attr.body.start && attr.body.end <= media_body.end {
                problems.push(format!(
                    "the `{DARK_ATTR_BLOCK}` block is inside the \
                     `@media (prefers-color-scheme: dark)` rule, so an explicit Dark choice \
                     does nothing on a light OS"
                ));
            }
        }
    }

    let as_map = |decls: &[(String, String)]| -> BTreeMap<String, String> {
        decls.iter().cloned().collect()
    };
    let (light_map, media_map, attr_map) = (
        as_map(&light.declarations),
        as_map(&media.declarations),
        as_map(&attr.declarations),
    );

    // 1. The two deliveries must agree, value for value.
    for (token, value) in &media_map {
        match attr_map.get(token) {
            Some(other) if other == value => {}
            Some(other) => problems.push(format!(
                "`{token}` is `{value}` under the media query but `{other}` under \
                 `[data-theme=\"dark\"]`"
            )),
            None => problems.push(format!(
                "`{token}` is declared under the media query but missing from \
                 `[data-theme=\"dark\"]`"
            )),
        }
    }
    for token in attr_map.keys() {
        if !media_map.contains_key(token) {
            problems.push(format!(
                "`{token}` is declared under `[data-theme=\"dark\"]` but missing from \
                 the media query"
            ));
        }
    }

    // 2. Every light colour token needs a dark counterpart.
    for (token, value) in &light_map {
        if looks_like_colour(value) && !media_map.contains_key(token) {
            problems.push(format!(
                "`{token}` has a light value (`{value}`) and no dark one"
            ));
        }
    }

    // 3. And the dark blocks may not invent a token light does not have, which
    //    would be a token that resolves to nothing in light mode.
    for token in media_map.keys() {
        if !light_map.contains_key(token) {
            problems.push(format!(
                "`{token}` is declared dark-only, so it resolves to nothing in light mode"
            ));
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(report_dark(&problems))
    }
}

/// The operator-facing half of [`check_dark_palette`].
fn report_dark(problems: &[String]) -> String {
    let mut msg = format!(
        "desktop/src/styles.css: {} problem{} with the dark palette (PRD #743 M3)\n\n",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for problem in problems {
        let _ = writeln!(msg, "  {problem}");
    }
    msg.push_str(
        "\nThe dark palette is delivered twice and both paths are required: the\n\
         `@media (prefers-color-scheme: dark)` block under its\n\
         `:root:not([data-theme=\"light\"])` guard is the OS default, and\n\
         `:root[data-theme=\"dark\"]` is an explicit Dark choice winning on a light OS.\n\
         The two must therefore declare the same tokens with the same values, and each\n\
         has to stay where its selector implies -- the guarded one inside the media\n\
         rule, the attribute one outside it.\n\
         \n\
         Every colour token on `:root` is re-declared in both, even where the dark\n\
         value is deliberately identical to the light one -- the terminals and the live\n\
         status colours are the worked examples. Re-stating them is what makes the dark\n\
         block a complete list of decisions instead of a diff, and it is why a missing\n\
         token is treated as an omission rather than as an intentional carry-over.\n",
    );
    msg
}

/// A place a token's colour is measured against. The two forms are the only
/// two `desktop/src` actually paints a background with: a solid palette
/// token, or `rgb(var(--channel) / alpha)` composited over another surface
/// (the lease badge, the topbar wash — see the comment beside `--faint` and
/// `--status-error` in `styles.css` for the call sites each pair covers).
#[derive(Debug, Clone)]
enum Surface {
    Token(&'static str),
    Composited {
        channel: &'static str,
        alpha: f64,
        on: Box<Surface>,
    },
}

/// A short label for a [`Surface`], used only in failure output.
fn describe_surface(surface: &Surface) -> String {
    match surface {
        Surface::Token(name) => format!("`{name}`"),
        Surface::Composited { channel, alpha, on } => {
            format!(
                "`rgb(var({channel}) / {alpha})` on {}",
                describe_surface(on)
            )
        }
    }
}

/// Parse `#rgb` or `#rrggbb` into `[r, g, b]`. `None` on anything else,
/// which callers turn into a clear failure rather than a panic — a malformed
/// hex literal is exactly the kind of thing this guard exists to catch.
fn parse_hex(value: &str) -> Option<[u8; 3]> {
    let s = value.trim().strip_prefix('#')?;
    let digit = |c: char| c.to_digit(16);
    match s.len() {
        3 => {
            let mut chars = s.chars();
            let (r, g, b) = (
                digit(chars.next()?)?,
                digit(chars.next()?)?,
                digit(chars.next()?)?,
            );
            Some([(r * 16 + r) as u8, (g * 16 + g) as u8, (b * 16 + b) as u8])
        }
        6 => {
            let byte = |i: usize| u8::from_str_radix(s.get(i..i + 2)?, 16).ok();
            Some([byte(0)?, byte(2)?, byte(4)?])
        }
        _ => None,
    }
}

/// Parse the channel-triplet form a `-rgb` token holds, `"R G B"` (as used at
/// each of its `rgb(var(--x-rgb) / alpha)` call sites).
fn parse_channel_triplet(value: &str) -> Option<[u8; 3]> {
    let mut parts = value.split_whitespace();
    let triplet = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(triplet)
}

/// WCAG 2.x relative luminance of one sRGB channel, 0-255 in, 0.0-1.0 out.
fn srgb_channel_luminance(c: u8) -> f64 {
    let s = f64::from(c) / 255.0;
    if s <= 0.04045 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2.x relative luminance of an sRGB colour.
fn relative_luminance(rgb: [u8; 3]) -> f64 {
    0.2126 * srgb_channel_luminance(rgb[0])
        + 0.7152 * srgb_channel_luminance(rgb[1])
        + 0.0722 * srgb_channel_luminance(rgb[2])
}

/// WCAG 2.x contrast ratio between two colours. Order-independent, per the
/// spec's own `(L1 + 0.05) / (L2 + 0.05)` with `L1` the lighter of the two.
fn contrast_ratio(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Source-over composite of `fg` at `alpha` onto an opaque `bg`, the model
/// `rgb(var(--x) / alpha)` paints with.
fn composite_over(fg: [u8; 3], alpha: f64, bg: [u8; 3]) -> [u8; 3] {
    std::array::from_fn(|i| {
        (f64::from(fg[i]) * alpha + f64::from(bg[i]) * (1.0 - alpha))
            .round()
            .clamp(0.0, 255.0) as u8
    })
}

/// Resolve a [`Surface`] to an RGB colour against one block's token map.
/// `None` means a token the surface needs is missing from that block, which
/// the caller reports rather than panicking on.
fn resolve_surface(surface: &Surface, tokens: &BTreeMap<String, String>) -> Option<[u8; 3]> {
    match surface {
        Surface::Token(name) => parse_hex(tokens.get(*name)?),
        Surface::Composited { channel, alpha, on } => {
            let fg = parse_channel_triplet(tokens.get(*channel)?)?;
            let bg = resolve_surface(on, tokens)?;
            Some(composite_over(fg, *alpha, bg))
        }
    }
}

/// Every (token, surface, minimum-ratio) pair issues #821 and #822 measured
/// and PR #1231's body tabulates, in the same order. Found by grepping
/// `desktop/src` for each token and walking its selector up to the nearest
/// rule that paints a background; the composited ones are the lease badge
/// (`--scrim-rgb` over a card) and the topbar/canvas wash (`--paper-rgb`).
/// The bar is 4.5 for normal text and 3.0 for the two WCAG 1.4.11 (non-text)
/// cases: a filled status dot, and that same fill against its own ring.
fn contrast_pairs() -> Vec<(&'static str, Surface, f64)> {
    use Surface::{Composited, Token};
    let topbar_wash = || Composited {
        channel: "--paper-rgb",
        alpha: 0.97,
        on: Box::new(Token("--canvas")),
    };
    let lease = |on: &'static str| Composited {
        channel: "--scrim-rgb",
        alpha: 0.04,
        on: Box::new(Token(on)),
    };
    vec![
        // --faint (issue #821)
        ("--faint", Token("--paper-strong"), 4.5),
        ("--faint", Token("--paper"), 4.5),
        ("--faint", Token("--canvas"), 4.5),
        ("--faint", Token("--paper-sunken"), 4.5),
        ("--faint", Token("--paper-sunken-strong"), 4.5),
        ("--faint", topbar_wash(), 4.5),
        ("--faint", lease("--paper-strong"), 4.5),
        ("--faint", lease("--paper"), 4.5),
        ("--faint", Token("--red-soft"), 4.5),
        ("--faint", lease("--red-soft"), 4.5),
        ("--faint", Token("--green-soft"), 4.5),
        // `.agent-state-mark` (idle): --faint as a FILL, not text, ringed
        // 2px `--paper-strong` — the non-text WCAG 1.4.11 case.
        ("--faint", Token("--paper-strong"), 3.0),
        // --muted (issue #821)
        ("--muted", Token("--paper-strong"), 4.5),
        ("--muted", Token("--paper"), 4.5),
        ("--muted", Token("--canvas"), 4.5),
        ("--muted", Token("--paper-sunken"), 4.5),
        ("--muted", Token("--paper-sunken-strong"), 4.5),
        ("--muted", topbar_wash(), 4.5),
        (
            "--muted",
            Composited {
                channel: "--paper-rgb",
                alpha: 0.55,
                on: Box::new(Token("--canvas")),
            },
            4.5,
        ),
        ("--muted", Token("--teal-soft"), 4.5),
        ("--muted", Token("--teal-wash"), 4.5),
        ("--muted", Token("--green-soft"), 4.5),
        ("--muted", Token("--red-soft"), 4.5),
        // --ink-soft (moved with #821 to keep a visible step above --muted --
        // see the comment on the light block's text tokens)
        ("--ink-soft", Token("--paper"), 4.5),
        ("--ink-soft", Token("--paper-sunken-strong"), 4.5),
        // --status-error (issue #822)
        // `.terminal-input-status.is-blocked`: 9px TEXT on --shell.
        ("--status-error", Token("--shell"), 4.5),
        // `.connection-lamp.connection-error`: the same token as a FILL, at
        // two of its four grounds, each a non-text WCAG 1.4.11 case. NOT the
        // topbar/canvas ground (`rgb(var(--paper-rgb) / .97)` composited on
        // --canvas): that one is 2.86:1, below 3.0, and stays so on purpose
        // -- the only values that clear 4.5:1 as text on the light --shell and
        // 3:1 as a fill on --paper at once sit in a relative-luminance band of
        // 0.2714-0.2739, at both bars with no headroom, and #822 took the
        // headroom on the text (see the comment beside --status-error). It is not
        // unenforced, though: WCAG 1.4.11 asks for 3:1 against the colour
        // actually ADJACENT to the fill, which for this lamp is its own
        // ring, not the page two rules away -- and that pair is the next
        // one in this list, and it is required.
        ("--status-error", Token("--shell"), 3.0),
        ("--status-error", Token("--paper-strong"), 3.0),
        // The lamp fill against its own 2px `--shell-line` ring -- the
        // colour actually adjacent to it, and the pair that carries its
        // WCAG 1.4.11 standing (see the comment beside --status-error).
        ("--status-error", Token("--shell-line"), 3.0),
    ]
}

/// Every pair [`contrast_pairs`] names clears its bar, in the light block and
/// in both dark deliveries.
///
/// This is the check [`check_dark_palette`] is not: that guard asks whether a
/// colour came from a token and whether the two dark deliveries agree with
/// each other, never whether a value clears any particular bar. Reverting
/// `--faint`, `--muted`, `--ink-soft` or `--status-error` to its pre-#821/#822
/// value leaves every other gate in this file green -- issue #821 measured
/// `--faint` at 2.39-3.05:1 and `--muted` at 3.95-5.04:1 against the 4.5:1
/// text bar, and #822 measured `--status-error` at 4.08:1 on light `--shell`,
/// all of which `check_dark_palette` and the tokenisation guard above are
/// structurally blind to. This is the guard that is not.
fn check_contrast_floor(css: &str) -> Result<(), String> {
    let masked = mask_comments(css, false);
    let blocks = root_blocks(&masked.code);
    let find = |selector: &str| blocks.iter().find(|block| block.selector == selector);
    let (Some(light), Some(media), Some(attr)) = (
        find(LIGHT_BLOCK),
        find(DARK_MEDIA_BLOCK),
        find(DARK_ATTR_BLOCK),
    ) else {
        // A missing block is [`check_dark_palette`]'s failure to report, in
        // more detail than this guard has any business duplicating; there is
        // nothing this guard can check without all three.
        return Ok(());
    };
    let as_map =
        |decls: &[(String, String)]| decls.iter().cloned().collect::<BTreeMap<String, String>>();
    let light_map = as_map(&light.declarations);
    let dark_deliveries = [
        ("the media query", as_map(&media.declarations)),
        (r#"`[data-theme="dark"]`"#, as_map(&attr.declarations)),
    ];

    let mut problems = Vec::new();
    for (token, surface, bar) in contrast_pairs() {
        let where_ = describe_surface(&surface);
        match (
            light_map.get(token).and_then(|v| parse_hex(v)),
            resolve_surface(&surface, &light_map),
        ) {
            (Some(fg), Some(bg)) => {
                let ratio = contrast_ratio(fg, bg);
                if ratio < bar {
                    problems.push(format!(
                        "light: `{token}` on {where_} is {ratio:.2}:1, below the {bar:.1}:1 bar"
                    ));
                }
            }
            _ => problems.push(format!(
                "light: could not resolve `{token}` on {where_} -- a token the pair needs is \
                 missing or not a recognised colour"
            )),
        }
        for (label, dark_map) in &dark_deliveries {
            match (
                dark_map.get(token).and_then(|v| parse_hex(v)),
                resolve_surface(&surface, dark_map),
            ) {
                (Some(fg), Some(bg)) => {
                    let ratio = contrast_ratio(fg, bg);
                    if ratio < bar {
                        problems.push(format!(
                            "dark ({label}): `{token}` on {where_} is {ratio:.2}:1, below the \
                             {bar:.1}:1 bar"
                        ));
                    }
                }
                _ => problems.push(format!(
                    "dark ({label}): could not resolve `{token}` on {where_} -- a token the \
                     pair needs is missing or not a recognised colour"
                )),
            }
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(report_contrast_floor(&problems))
    }
}

/// The operator-facing half of [`check_contrast_floor`].
fn report_contrast_floor(problems: &[String]) -> String {
    let mut msg = format!(
        "desktop/src/styles.css: {} contrast regression{} against the minimums issues #821 \
         and #822 recorded\n\n",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for problem in problems {
        let _ = writeln!(msg, "  {problem}");
    }
    msg.push_str(
        "\nEach pair above is measured and written down already -- in the comments beside\n\
         `--faint`, `--muted`, `--ink-soft` and `--status-error` in desktop/src/styles.css,\n\
         and in changelog.d/821.bugfix.md / changelog.d/822.bugfix.md. This guard exists\n\
         because nothing else here computes a contrast ratio: the tokenisation guard above\n\
         only asks whether a colour came from a token, and the dark-agreement guard only\n\
         asks whether the two dark deliveries match EACH OTHER, so reverting any of the\n\
         four tokens to its pre-fix value left both green. This one does not.\n",
    );
    msg
}

/// Scan one file's text. `palette` marks the file whose `:root` blocks hold the
/// token declarations.
fn scan_text(label: &str, src: &str, is_css: bool, palette: bool) -> Vec<Finding> {
    let masked = mask_comments(src, !is_css);
    let exempt = if palette {
        root_block_lines(&masked.code)
    } else {
        BTreeSet::new()
    };

    let mut findings = Vec::new();
    for (idx, (raw, masked_line)) in src.lines().zip(masked.code.lines()).enumerate() {
        let line = idx + 1;
        // The marker counts only in a comment, and only with a reason after it.
        // Read off the raw line it would also fire from a string literal that
        // merely quotes it, which would exempt a hard-coded colour beside it.
        if opt_out_reason(masked.comment_on(line)).is_some() {
            continue;
        }
        // Inside the palette, a custom-property declaration is the point. A
        // plain `color:` there is not, and was a real defect before M1.
        if exempt.contains(&line) && raw.trim_start().starts_with("--") {
            continue;
        }
        for literal in literals_in(masked_line) {
            findings.push(Finding {
                file: label.to_string(),
                line,
                literal,
                source: raw.trim().to_string(),
            });
        }
    }
    findings
}

/// Every `.css`, `.ts` and `.tsx` file under `root`, sorted, so the report is
/// stable run to run.
///
/// **Fails closed.** Every I/O error is returned rather than skipped: a
/// required CI check that can quietly inspect *nothing* and still pass is the
/// worst property a guard can have, and `read_dir` failing on one subdirectory
/// used to take every file under it out of the scan with no trace.
///
/// **Symlinks are refused rather than followed.** A directory link could take
/// the walk outside `desktop/src` (mislabelling every finding) or back into an
/// ancestor (looping forever), and a file link would let a colour live in a
/// file the guard reports under a path it does not really have. There is no
/// symlink under `desktop/src` today, so this is an error rather than a silent
/// skip — silently skipping is how content leaves the scan unnoticed, which is
/// the failure this whole function was rewritten to prevent. The visited set is
/// the second belt: canonicalised, so no arrangement of directory entries can
/// make the walk revisit one.
fn sources(root: &Path) -> Result<Vec<PathBuf>, String> {
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
                    "{} is a symlink; the palette guard will not follow one, because a \
                     directory link can leave desktop/src or loop back into it and a file \
                     link would be reported under a path it does not have. Replace it, or \
                     teach `sources` what this particular link means.",
                    path.display()
                ));
            }
            if kind.is_dir() {
                stack.push(path);
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("css" | "ts" | "tsx")
            ) {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Scan a `desktop/src` tree. Fails closed on any traversal or read error —
/// see [`sources`].
fn scan_tree(src_root: &Path) -> Result<Vec<Finding>, String> {
    // The palette is one specific file. Matching on the *file name* exempted
    // any nested `styles.css` as well, so a `:root` block in a subdirectory
    // stylesheet could hold literals the palette guard is meant to catch —
    // and the palette is supposed to live in exactly one place.
    let palette_path = src_root.join("styles.css");
    let mut findings = Vec::new();
    for path in sources(src_root)? {
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let rel = slash_path(path.strip_prefix(src_root).unwrap_or(&path));
        let is_css = path.extension().is_some_and(|e| e == "css");
        findings.extend(scan_text(
            &format!("desktop/src/{rel}"),
            &text,
            is_css,
            // Path equality compares **components**, not printed forms, so this
            // is already separator-insensitive: on Windows the walked
            // `…\\desktop/src\\styles.css` and `palette_path` compare equal
            // even though `display()` disagrees with the literal above. Do not
            // turn it into a comparison of `rel` against a string — that would
            // be the same defect as the one `slash_path` fixes, except it would
            // silently *disable* the palette exemption on Windows rather than
            // printing an odd path, and no test here would notice.
            path == palette_path,
        ));
    }
    Ok(findings)
}

/// The failure message: where, what, and what to do about it.
fn report(findings: &[Finding]) -> String {
    let mut msg = format!(
        "{} hard-coded colour{} under desktop/src (PRD #743):\n\n",
        findings.len(),
        if findings.len() == 1 { "" } else { "s" }
    );
    for f in findings {
        let _ = writeln!(msg, "  {}:{}: `{}`", f.file, f.line, f.literal);
        let _ = writeln!(msg, "      {}\n", f.source);
    }
    msg.push_str(
        "A colour written into a rule cannot follow the light/dark appearance -- it stays\n\
         light in dark mode, and nothing renders dark mode in CI, so nobody sees it break.\n\
         \n\
         Fix it one of two ways:\n\
         \n\
         1. Use a token. The palette is the `:root` block at the top of\n\
         \x20  desktop/src/styles.css -- reuse the token that names this role, or add one\n\
         \x20  there (family plus depth, e.g. `--teal-deep`) and reference it as\n\
         \x20  `var(--teal-deep)`. For a colour needed at partial alpha, use the channel\n\
         \x20  form: `rgb(var(--scrim-rgb) / .42)`.\n\
         \n\
         2. If the colour genuinely must not follow the theme, put\n\
         \x20  `theme-invariant: <why>` in a comment on that line. Give a real reason --\n\
         \x20  the xterm palette in TerminalViewport.tsx is the worked example, and the\n\
         \x20  reason there is that PRD #743 keeps the terminals dark in both appearances.\n",
    );
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// The guard doing its job. Everything else here proves this assertion is
    /// not vacuous.
    #[test]
    fn the_real_desktop_source_holds_no_untokenised_colour() {
        let root = repo_root().join("desktop/src");
        assert!(root.is_dir(), "{} is missing", root.display());
        let findings = scan_tree(&root).expect("the desktop source tree must be fully readable");
        assert!(findings.is_empty(), "{}", report(&findings));
    }

    /// And that it is looking at something: a tree it scans clean must still
    /// have been read.
    #[test]
    fn the_real_scan_covers_every_source_file() {
        let root = repo_root().join("desktop/src");
        let files = sources(&root).expect("the desktop source tree must be fully readable");
        assert!(
            files.len() > 15,
            "only {} files scanned under desktop/src; the walk is not reaching \
             the subdirectories",
            files.len()
        );
        for expected in ["styles.css", "App.tsx", "TerminalViewport.tsx", "bridge.ts"] {
            assert!(
                files.iter().any(|p| p.ends_with(expected)),
                "{expected} was not scanned"
            );
        }
    }

    /// The dark palette is complete and its two deliveries agree.
    ///
    /// Nothing else in the repo can see either failure: the colour guard asks
    /// only whether a colour came from a token, no test renders dark mode, and
    /// the two dark blocks are necessarily separate rules.
    #[test]
    fn the_real_dark_palette_is_complete_and_its_two_blocks_agree() {
        let css = fs::read_to_string(repo_root().join("desktop/src/styles.css"))
            .expect("read styles.css");
        if let Err(report) = check_dark_palette(&css) {
            panic!("{report}");
        }
    }

    /// And that the assertion above is not vacuous. Each mutation is one of the
    /// three ways this can actually go wrong in a hand edit.
    #[test]
    fn the_dark_palette_check_catches_drift_an_omission_and_a_dark_only_token() {
        let css = fs::read_to_string(repo_root().join("desktop/src/styles.css"))
            .expect("read styles.css");

        // A value edited in one delivery and not the other. `--canvas` appears
        // once in light and once in each dark block, so replacing the last
        // occurrence hits `[data-theme="dark"]` alone.
        let at = css.rfind("--canvas:").expect("a --canvas declaration");
        let end = css[at..].find(';').expect("terminated declaration") + at;
        let drifted = format!("{}--canvas: #123456{}", &css[..at], &css[end..]);
        let report = check_dark_palette(&drifted).expect_err("drift must be caught");
        assert!(report.contains("--canvas"), "{report}");
        assert!(report.contains("data-theme"), "{report}");

        // A new light token with no dark counterpart.
        let omitted = css.replacen("  --canvas:", "  --brand-new: #abcdef;\n  --canvas:", 1);
        let report = check_dark_palette(&omitted).expect_err("an omission must be caught");
        assert!(report.contains("--brand-new"), "{report}");
        assert!(report.contains("no dark one"), "{report}");

        // A layout token, though, is exempt by shape and needs no dark value.
        let layout = css.replacen("  --canvas:", "  --gutter: 12px;\n  --canvas:", 1);
        assert!(
            check_dark_palette(&layout).is_ok(),
            "a non-colour token must not be required to have a dark value"
        );

        // A token that exists only in dark resolves to nothing in light.
        let dark_only = css.replacen(
            r#":root[data-theme="dark"] {"#,
            ":root[data-theme=\"dark\"] {\n  --dark-only: #abcdef;",
            1,
        );
        let report = check_dark_palette(&dark_only).expect_err("a dark-only token must be caught");
        assert!(report.contains("--dark-only"), "{report}");
    }

    /// The contrast floor is what [`check_dark_palette`] is not: every pair
    /// issues #821 and #822 measured clears its bar today, in both light and
    /// dark.
    #[test]
    fn the_real_palette_clears_the_821_822_contrast_floor() {
        let css = fs::read_to_string(repo_root().join("desktop/src/styles.css"))
            .expect("read styles.css");
        if let Err(report) = check_contrast_floor(&css) {
            panic!("{report}");
        }
    }

    /// And that the assertion above is not vacuous: reverting `--faint` to
    /// the exact value #821 replaced -- in the LIGHT block only, the one
    /// delivery this guard actually protects, since dark already cleared the
    /// bar before #821 touched anything -- is caught.
    #[test]
    fn the_contrast_floor_catches_a_revert_of_faint_to_its_pre_821_value() {
        let css = fs::read_to_string(repo_root().join("desktop/src/styles.css"))
            .expect("read styles.css");
        let reverted = css.replacen("--faint: #5a605b;", "--faint: #8d948f;", 1);
        let report =
            check_contrast_floor(&reverted).expect_err("a reverted --faint must be caught");
        assert!(report.contains("--faint"), "{report}");
        assert!(report.contains("below the 4.5:1 bar"), "{report}");
        // `--faint` lands on eleven light surfaces at the 4.5:1 bar (issue
        // #821); the old value failed all eleven, so this is not a report
        // with one line.
        assert!(
            report.matches("light: `--faint`").count() > 1,
            "expected the pre-#821 value to fail more than one surface: {report}"
        );
    }

    /// Same shape, for `--status-error` (#822): reverting the LIGHT
    /// declaration alone -- the one `replacen(.., 1)` hits first, since all
    /// three blocks currently share this token's value -- is caught, and
    /// leaving the two dark deliveries at the fixed value does not paper
    /// over it.
    #[test]
    fn the_contrast_floor_catches_a_revert_of_status_error_to_its_pre_822_value() {
        let css = fs::read_to_string(repo_root().join("desktop/src/styles.css"))
            .expect("read styles.css");
        let reverted = css.replacen("--status-error: #d57971;", "--status-error: #d0685f;", 1);
        let report =
            check_contrast_floor(&reverted).expect_err("a reverted --status-error must be caught");
        assert!(report.contains("--status-error"), "{report}");
        assert!(report.contains("`--shell`"), "{report}");
    }

    /// The composite-surface arithmetic itself, independent of the real file:
    /// a lease badge at `rgb(var(--scrim-rgb) / .04)` over a white card
    /// darkens the card slightly, and the ratio moves accordingly.
    #[test]
    fn resolve_surface_composites_a_channel_token_over_its_base() {
        let mut tokens = BTreeMap::new();
        tokens.insert("--scrim-rgb".to_string(), "0 0 0".to_string());
        tokens.insert("--card".to_string(), "#ffffff".to_string());
        let surface = Surface::Composited {
            channel: "--scrim-rgb",
            alpha: 0.04,
            on: Box::new(Surface::Token("--card")),
        };
        let rgb = resolve_surface(&surface, &tokens).expect("both tokens are present");
        // 4% black over white is #f5f5f5 (255 * .96, rounded).
        assert_eq!(rgb, [245, 245, 245]);
    }

    /// The contrast-ratio arithmetic against WCAG's own worked values: pure
    /// black on pure white is exactly 21:1, and a colour against itself is
    /// always 1:1.
    #[test]
    fn contrast_ratio_matches_wcags_own_worked_values() {
        assert!((contrast_ratio([0, 0, 0], [255, 255, 255]) - 21.0).abs() < 0.001);
        assert!((contrast_ratio([100, 150, 200], [100, 150, 200]) - 1.0).abs() < 0.001);
    }

    /// The `:not([data-theme="light"])` guard is the whole reason an explicit
    /// Light choice survives a dark OS, and it is one character class away from
    /// being wrong in a way nothing else would notice.
    ///
    /// That the guarded block really is *inside* the media rule is asserted by
    /// [`check_dark_palette`] against byte ranges, and covered on the real file
    /// by [`tests::the_real_dark_palette_is_complete_and_its_two_blocks_agree`];
    /// what is left here is the selector's spelling.
    #[test]
    fn the_media_block_keeps_its_explicit_light_escape_hatch() {
        let css = fs::read_to_string(repo_root().join("desktop/src/styles.css"))
            .expect("read styles.css");
        assert!(
            css.contains(DARK_MEDIA_BLOCK),
            "the dark media query must be scoped to `{DARK_MEDIA_BLOCK}`, or a user who \
             chose Light gets a dark app on a dark OS"
        );
    }

    /// A three-block palette in the shape `styles.css` really has, small enough
    /// that the nesting is the only thing under test.
    const NESTED_FIXTURE: &str = "\
:root {\n  --teal: #0f7167;\n}\n\
@media (prefers-color-scheme: dark) {\n\
\x20 :root:not([data-theme=\"light\"]) {\n\
\x20   --teal: #72c3b3;\n\
\x20 }\n\
}\n\
:root[data-theme=\"dark\"] {\n\
\x20 --teal: #72c3b3;\n\
}\n";

    /// The structural hole, and the reason the check is on byte ranges rather
    /// than on selector text.
    ///
    /// Dedent the guarded block out of the `@media { … }` wrapper and every
    /// other assertion here stays green — the selector is still spelled right,
    /// the two dark blocks still agree, every light token still has a dark
    /// counterpart — while the dark values now apply in **light** mode. Text
    /// cannot see that; a byte range can.
    #[test]
    fn the_dark_palette_check_catches_a_block_dedented_out_of_the_media_rule() {
        assert!(
            check_dark_palette(NESTED_FIXTURE).is_ok(),
            "the fixture must pass before it is broken"
        );

        // Same tokens, same values, same selector spelling, same media rule —
        // only the nesting moved.
        let dedented = "\
:root {\n  --teal: #0f7167;\n}\n\
@media (prefers-color-scheme: dark) {\n\
\x20 .placeholder { border-color: var(--teal); }\n\
}\n\
:root:not([data-theme=\"light\"]) {\n\
\x20 --teal: #72c3b3;\n\
}\n\
:root[data-theme=\"dark\"] {\n\
\x20 --teal: #72c3b3;\n\
}\n";
        assert!(
            dedented.contains(DARK_MEDIA_BLOCK) && dedented.contains("@media"),
            "the fixture must still hold both, or it is testing the wrong thing"
        );

        let report = check_dark_palette(dedented)
            .expect_err("a dark block outside the media rule must be caught");
        assert!(report.contains("not inside the"), "{report}");
        assert!(report.contains("light mode"), "{report}");
    }

    /// The mirror image, which fails the other way round: nested inside the
    /// media rule, `:root[data-theme="dark"]` never fires on a light OS, so an
    /// explicit Dark choice silently does nothing there.
    #[test]
    fn the_dark_palette_check_catches_the_attribute_block_nested_inside_the_media_rule() {
        let swallowed = "\
:root {\n  --teal: #0f7167;\n}\n\
@media (prefers-color-scheme: dark) {\n\
\x20 :root:not([data-theme=\"light\"]) {\n\
\x20   --teal: #72c3b3;\n\
\x20 }\n\
\x20 :root[data-theme=\"dark\"] {\n\
\x20   --teal: #72c3b3;\n\
\x20 }\n\
}\n";
        let report = check_dark_palette(swallowed)
            .expect_err("an attribute block inside the media rule must be caught");
        assert!(report.contains("is inside the"), "{report}");
        assert!(report.contains("light OS"), "{report}");
    }

    /// The xterm palette is the opt-out's only user today, and it is the case
    /// the mechanism exists for -- so pin that it is really exempted by the
    /// marker rather than passing for some other reason.
    #[test]
    fn the_xterm_palette_is_exempt_only_because_it_is_marked() {
        let path = repo_root().join("desktop/src/components/TerminalViewport.tsx");
        let text = fs::read_to_string(&path).expect("read TerminalViewport.tsx");
        assert!(scan_text("t.tsx", &text, false, false).is_empty());

        let unmarked = text.replace(OPT_OUT, "see above:");
        let findings = scan_text("t.tsx", &unmarked, false, false);
        assert_eq!(
            findings.len(),
            21,
            "expected the 21 xterm slots to be the only exempted colours, got {findings:#?}"
        );
    }

    fn tree(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        for (name, body) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir");
            }
            fs::write(&path, body).expect("write");
        }
        dir
    }

    /// [`scan_tree`] where the walk is expected to succeed, which is every test
    /// except the ones asserting that it does not.
    fn scanned(root: &Path) -> Vec<Finding> {
        scan_tree(root).expect("the fixture tree must be fully readable")
    }

    const PALETTE: &str = ":root {\n  --teal: #0f7167;\n  --scrim-rgb: 29 37 34;\n}\n";

    #[test]
    fn a_tokenised_tree_passes() {
        let dir = tree(&[(
            "styles.css",
            &format!(
                "{PALETTE}\
                 .a {{ color: var(--teal); background: rgb(var(--scrim-rgb) / .42); }}\n\
                 .b {{ border: 1px solid transparent; white-space: nowrap; }}\n"
            ),
        )]);
        assert_eq!(scanned(dir.path()), vec![]);
    }

    #[test]
    fn a_bare_hex_in_css_fails_and_says_where() {
        let dir = tree(&[(
            "styles.css",
            &format!("{PALETTE}.a {{ color: #5e9c90; }}\n"),
        )]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 5);
        assert_eq!(findings[0].literal, "#5e9c90");
        assert_eq!(findings[0].source, ".a { color: #5e9c90; }");

        let msg = report(&findings);
        assert!(msg.contains("desktop/src/styles.css:5"), "{msg}");
        assert!(msg.contains("#5e9c90"), "{msg}");
        assert!(msg.contains("theme-invariant:"), "{msg}");
    }

    #[test]
    fn a_bare_hex_in_tsx_fails() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("c/Tile.tsx", "const s = { color: \"#fff\" };\n"),
        ]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].file, "desktop/src/c/Tile.tsx");
        assert_eq!(findings[0].literal, "#fff");
    }

    #[test]
    fn the_opt_out_comment_exempts_its_line_and_only_its_line() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "T.tsx",
                "const a = \"#141817\"; // theme-invariant: xterm stays dark\n\
                 const b = \"#141817\";\n",
            ),
        ]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 2);
    }

    #[test]
    fn custom_properties_in_the_palette_block_are_allowed() {
        let dir = tree(&[("styles.css", PALETTE)]);
        assert_eq!(scanned(dir.path()), vec![]);
    }

    /// The pre-M1 defect: `:root` declared `--ink` *and* repeated its value as a
    /// bare `color:`. The block is not a blanket exemption.
    #[test]
    fn a_bare_declaration_inside_the_palette_block_still_fails() {
        let dir = tree(&[(
            "styles.css",
            ":root {\n  color: #1d2522;\n  --ink: #1d2522;\n}\n",
        )]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 2);
    }

    /// M3 adds `:root[data-theme="dark"]` and a `prefers-color-scheme` block.
    /// Both must land without editing this guard.
    #[test]
    fn the_dark_palette_blocks_m3_will_add_are_exempt_too() {
        let dir = tree(&[(
            "styles.css",
            "@media (prefers-color-scheme: dark) {\n\
             \x20 :root:not([data-theme=\"light\"]) {\n\
             \x20   --teal: #72c3b3;\n\
             \x20 }\n\
             }\n\
             :root[data-theme=\"dark\"] {\n\
             \x20 --teal: #72c3b3;\n\
             }\n",
        )]);
        assert_eq!(scanned(dir.path()), vec![]);
    }

    /// The exemption is `styles.css`'s, so the palette stays in one file and a
    /// stray `:root` elsewhere is not a way around the guard.
    #[test]
    fn a_root_block_in_another_stylesheet_is_not_exempt() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("other.css", ":root { --sneaky: #5e9c90; }\n"),
        ]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].file, "desktop/src/other.css");
    }

    /// `PR #416` and `(PRD #743)` are three hex digits behind a `#`, and both
    /// are really in the tree.
    #[test]
    fn issue_references_in_comments_are_not_colours() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "b.ts",
                "// PR #416 review M1: keys are scoped by runtime mode.\n\
                 /* per PRD #162 a new daemon build gets the same treatment. */\n\
                 const x = 1;\n",
            ),
            (
                "c.css",
                "/* Palette notes, PRD #743 M1. */\n.a { color: red; }\n",
            ),
        ]);
        assert_eq!(scanned(dir.path()), vec![]);
    }

    /// The string-literal half of the same problem, which the comment masking
    /// above does not reach. `#742` in a `describe` title is an issue number;
    /// `#742` in a CSS value is a colour; and the guard has to tell them apart
    /// without letting a real literal through behind a cue word.
    #[test]
    fn issue_references_in_string_literals_are_not_colours_but_real_hexes_still_fail() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "ok.tsx",
                // Every run here is 3, 4, 6 or 8 digits long, which is the only
                // window `is_issue_reference` is ever consulted in. A five-digit
                // run scans clean whatever word precedes it, so a fixture line
                // carrying one asserts nothing about the cue list — this test
                // held such a line (`#74253`) until PRD #742 M11.
                "describe(\"AgentOverview across a fleet (PRD #742 M4)\", () => {});\n\
                 it(\"regression for issue #1046 and PR #416\", () => {});\n\
                 const a = \"PRD#742 M6 — no space is still a reference\";\n\
                 const b = \"see github #7425 for the rest\";\n\
                 const c = \"fixes #742, closes #416, resolves #1046, refs #7425\";\n\
                 const d = \"fix #742, close #416, resolve #1046, ref #7425\";\n\
                 const e = \"the parenthesised form (#742) carries no cue at all\";\n\
                 const f = \"pull #742, issues #416, gh #1046\";\n",
            ),
        ]);
        assert_eq!(scanned(dir.path()), vec![], "issue numbers are not colours");

        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "bad.tsx",
                // 1. the plain hard-coded colour a string literal holds;
                // 2. an all-decimal colour, which the decimal half alone would
                //    have exempted;
                // 3. the same, in the shorthand position a general
                //    \"word then a space\" rule would have exempted;
                // 4. a cue word in front of a run that is not a number;
                // 5. `fixed`, the cue deliberately left out of the list because
                //    `background` is an any-order shorthand and this is valid
                //    CSS — the one line that pins where the boundary was drawn;
                // 6. the first stop of a one-line gradient, which an
                //    opening-paren-alone rule would have exempted, and its
                //    second stop, which nothing was ever going to exempt.
                "const s = { color: \"#fff\" };\n\
                 const t = { color: \"#333\" };\n\
                 const u = { border: \"1px solid #000\" };\n\
                 const v = { color: \"PRD #fff\" };\n\
                 const w = { background: \"fixed #333\" };\n\
                 const x = { background: \"linear-gradient(#333, #000)\" };\n",
            ),
        ]);
        let findings = scanned(dir.path());
        let hits: Vec<(usize, String)> = findings
            .iter()
            .map(|f| (f.line, f.literal.clone()))
            .collect();
        assert_eq!(
            hits,
            vec![
                (1, "#fff".to_string()),
                (2, "#333".to_string()),
                (3, "#000".to_string()),
                (4, "#fff".to_string()),
                (5, "#333".to_string()),
                (6, "#333".to_string()),
                (6, "#000".to_string()),
            ],
            "{findings:#?}"
        );
    }

    /// The parenthesised exemption is anchored on **both** brackets, and the
    /// case that pays for the closing one is ordinary formatted CSS: a wrapped
    /// `linear-gradient` puts a bare `#333,` at the start of a line, which a
    /// line-start rule would have waved through.
    #[test]
    fn a_bare_reference_is_exempt_only_when_its_own_parentheses_close_it() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "wrapped.css",
                ".a {\n  \
                 background: linear-gradient(\n    \
                 #333,\n    \
                 #000\n  \
                 );\n\
                 }\n",
            ),
        ]);
        let hits: Vec<(usize, String)> = scanned(dir.path())
            .iter()
            .map(|f| (f.line, f.literal.clone()))
            .collect();
        assert_eq!(
            hits,
            vec![(3, "#333".to_string()), (4, "#000".to_string())],
            "a colour at the start of a wrapped value line is still a colour"
        );

        // And the form that is exempt really is, in the position it is written
        // in: closed by its own `)`, with an ordinary word to its left.
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "cited.tsx",
                "describe(\"the fleet snapshot (#742)\", () => {});\n\
                 it(\"and a second one (#1046)\", () => {});\n",
            ),
        ]);
        assert_eq!(scanned(dir.path()), vec![]);
    }

    #[test]
    fn literal_colour_functions_fail_but_the_token_form_passes() {
        let dir = tree(&[(
            "styles.css",
            &format!(
                "{PALETTE}\
                 .a {{ background: rgba(24, 30, 28, .42); }}\n\
                 .b {{ background: rgb(var(--scrim-rgb) / .42); }}\n\
                 .c {{ color: hsl(160 40% 30%); }}\n\
                 .d {{ box-shadow: 0 1px 2px rgb(0 0 0 / .2); }}\n"
            ),
        )]);
        let findings = scanned(dir.path());
        let lines: Vec<usize> = findings.iter().map(|f| f.line).collect();
        assert_eq!(lines, vec![5, 7, 8], "{findings:#?}");
        assert_eq!(findings[0].literal, "rgba(24, 30, 28, .42)");
    }

    #[test]
    fn bare_colour_keywords_fail_without_catching_look_alikes() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "k.css",
                ".a { background: white; }\n\
                 .b { color: black }\n\
                 .c { white-space: nowrap; border: 1px solid transparent; }\n\
                 .d { color: currentColor; }\n",
            ),
            (
                "k.tsx",
                "const a = { whiteSpace: \"nowrap\", blackList: 1 };\n\
                 const b = { white: \"1\", black: \"2\" };\n\
                 const c = \"white\";\n",
            ),
        ]);
        let findings = scanned(dir.path());
        let hits: Vec<(String, usize)> =
            findings.iter().map(|f| (f.file.clone(), f.line)).collect();
        assert_eq!(
            hits,
            vec![
                ("desktop/src/k.css".to_string(), 1),
                ("desktop/src/k.css".to_string(), 2),
                ("desktop/src/k.tsx".to_string(), 3),
            ],
            "{findings:#?}"
        );
    }

    /// A `//` or `/*` inside a string must not swallow the rest of the file,
    /// which would silently disable the guard from that point on.
    #[test]
    fn a_comment_marker_inside_a_string_does_not_blind_the_scanner() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "u.ts",
                "const url = \"https://example.test/a\";\n\
                 const glob = \"/*\";\n\
                 const c = \"#5e9c90\";\n",
            ),
        ]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 3);
    }

    /// The marker is read from the line's **comment**, so a string that merely
    /// quotes it exempts nothing. Before this, a line holding both a hex and a
    /// string mentioning `theme-invariant:` passed — including this file's own
    /// documentation of the marker, copied into a component.
    #[test]
    fn a_marker_inside_a_string_literal_does_not_exempt_the_line() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "S.tsx",
                "const doc = \"write theme-invariant: <reason>\"; const bad = \"#5e9c90\";\n\
                 const good = \"#5e9c90\"; // theme-invariant: really outside the theme\n",
            ),
        ]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 1);
    }

    /// `theme-invariant:` with nothing after it is a `TODO` in disguise. The
    /// guard's value is that every exemption says why it exists, so a reason is
    /// required rather than encouraged.
    #[test]
    fn a_marker_with_no_reason_does_not_exempt_the_line() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "R.tsx",
                "const a = \"#141817\"; // theme-invariant:\n\
                 const b = \"#141817\"; /* theme-invariant:   */\n\
                 const c = \"#141817\"; // theme-invariant: xterm stays dark\n",
            ),
        ]);
        let lines: Vec<usize> = scanned(dir.path()).iter().map(|f| f.line).collect();
        assert_eq!(lines, vec![1, 2]);
    }

    /// The palette is one file, not one file *name*. Matching on the name alone
    /// made any nested `styles.css` a place literals could live unchallenged.
    #[test]
    fn a_nested_stylesheet_named_styles_css_is_not_the_palette() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("themes/styles.css", ":root { --sneaky: #5e9c90; }\n"),
        ]);
        let findings = scanned(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].file, "desktop/src/themes/styles.css");
        assert_eq!(findings[0].literal, "#5e9c90");
    }

    /// The palette exemption is a **path** comparison, so it survives a root
    /// spelled differently from the paths the walk builds out of it. That is
    /// the property that keeps it working on Windows, where the root carries
    /// one separator and the walked tail another; a `.` component is the
    /// equivalent divergence this host can construct.
    ///
    /// Worth pinning because the failure mode is silent in the dangerous
    /// direction: the palette would stop being exempt and its own tokens would
    /// be reported as hard-coded colours.
    #[test]
    fn the_palette_exemption_survives_a_root_spelled_differently_from_the_walk() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("themes/styles.css", ":root { --sneaky: #5e9c90; }\n"),
        ]);
        let root = dir.path().join(".");
        let findings = scan_tree(&root).expect("the fixture tree must be fully readable");
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].file, "desktop/src/themes/styles.css");
    }

    /// Fail closed: a directory the walk cannot list used to take every file
    /// under it out of the scan silently, so the guard reported a clean tree it
    /// had never read.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_fails_the_scan_rather_than_shrinking_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("sub/Hidden.tsx", "const a = \"#5e9c90\";\n"),
        ]);
        let sub = dir.path().join("sub");
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o000)).expect("chmod");
        let result = scan_tree(dir.path());
        fs::set_permissions(&sub, fs::Permissions::from_mode(0o700)).expect("restore");

        match result {
            Err(message) => assert!(
                message.contains("could not read the directory") && message.contains("sub"),
                "{message}"
            ),
            Ok(findings) => {
                // A privileged process can list it anyway, and then the hidden
                // colour must still be found rather than skipped.
                assert_eq!(findings.len(), 1, "{findings:#?}");
                eprintln!(
                    "SKIP: this process can list a 0o000 directory (running privileged), so an \
                     unreadable one cannot be constructed here"
                );
            }
        }
    }

    /// Same property one level down: a file that cannot be read is a failure,
    /// not a file with no colours in it.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_fails_the_scan_rather_than_being_skipped() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("Locked.tsx", "const a = \"#5e9c90\";\n"),
        ]);
        let locked = dir.path().join("Locked.tsx");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        let result = scan_tree(dir.path());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o600)).expect("restore");

        match result {
            Err(message) => assert!(
                message.contains("could not read") && message.contains("Locked.tsx"),
                "{message}"
            ),
            Ok(findings) => {
                assert_eq!(findings.len(), 1, "{findings:#?}");
                eprintln!(
                    "SKIP: this process can read a 0o000 file (running privileged), so an \
                     unreadable one cannot be constructed here"
                );
            }
        }
    }

    /// A symlink is refused rather than followed, and this one points back at
    /// its own ancestor — the arrangement that would otherwise walk forever.
    /// The scan returning at all is half of what is asserted here.
    #[cfg(unix)]
    #[test]
    fn a_symlink_back_into_the_tree_is_refused_rather_than_followed() {
        let dir = tree(&[("styles.css", PALETTE), ("sub/A.tsx", "const a = 1;\n")]);
        std::os::unix::fs::symlink(dir.path(), dir.path().join("sub/loop")).expect("symlink");

        let message = scan_tree(dir.path()).expect_err("a symlink must fail the scan");
        assert!(message.contains("symlink"), "{message}");
        assert!(message.contains("loop"), "{message}");
    }

    #[test]
    fn hex_like_runs_that_are_not_colours_are_ignored() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            (
                "i.css",
                "#root { margin: 0; }\n\
                 .a { grid-area: a12345678901; }\n\
                 .b { background-image: url(\"/x.png#deadbeefcafe\"); }\n",
            ),
        ]);
        assert_eq!(scanned(dir.path()), vec![]);
    }
}
