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
//!   `rgb(var(--ink-rgb) / .42)` form the scrims and shadows use is fine;
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
//!   `desktop/src/styles.css`** — that is the palette. The exemption is a
//!   custom property specifically, so `:root { color: #1d2522; }` (which is how
//!   the pre-M1 file duplicated `--ink`) still fails, and it covers any
//!   `:root`-prefixed selector so M3's `:root[data-theme="dark"]` and its
//!   `prefers-color-scheme` block land without touching this file;
//! - any line carrying `theme-invariant: <reason>` in a comment. That is not
//!   laziness: the xterm palette is genuinely outside the theme (PRD #743 keeps
//!   the terminals dark in both appearances), and PR #779 has light hexes in
//!   flight on another branch, so whichever of the two lands second needs a note
//!   rather than a wall.
//!
//! Comments are masked before scanning, which is load-bearing rather than
//! tidy: `PR #416` and `(PRD #743)` are three valid hex digits behind a `#` and
//! appear in both `src/lib/bridge.ts` and the opt-out markers themselves.
//!
//! Same budget as the other file-reading modules here (`pin_lockstep`,
//! `junit_strip`): no network, no git, no sleep, no subprocess.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// The marker that exempts the line it appears on.
const OPT_OUT: &str = "theme-invariant:";

/// Colour functions whose literal (non-`var()`) form is a hard-coded colour.
const COLOUR_FNS: [&str; 10] = [
    "rgba", "rgb", "hsla", "hsl", "hwb", "oklab", "oklch", "lab", "lch", "color",
];

/// Bare CSS colour keywords worth catching. Kept to the two that actually
/// appear as values and cannot be mistaken for anything else.
const COLOUR_WORDS: [&str; 2] = ["white", "black"];

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

/// Replace every comment with spaces, keeping newlines so line numbers survive.
/// String contents are kept — a literal inside `"#141817"` is exactly what this
/// is looking for — which is also why the scanner must know where strings start
/// and end, so a `//` or `/*` inside one does not swallow the rest of the file.
///
/// `line_comments` is false for CSS, where `//` is not a comment.
fn mask_comments(src: &str, line_comments: bool) -> String {
    let mut out = String::with_capacity(src.len());
    let mut state = Mask::Code;
    let mut escaped = false;
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match state {
            Mask::Code => {
                if c == '/' && chars.peek() == Some(&'*') {
                    chars.next();
                    state = Mask::BlockComment;
                    out.push_str("  ");
                } else if line_comments && c == '/' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = Mask::LineComment;
                    out.push_str("  ");
                } else {
                    if c == '"' || c == '\'' || c == '`' {
                        state = Mask::Str(c);
                        escaped = false;
                    }
                    out.push(c);
                }
            }
            Mask::LineComment => {
                if c == '\n' {
                    state = Mask::Code;
                    out.push('\n');
                } else {
                    out.push(' ');
                }
            }
            Mask::BlockComment => {
                if c == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    state = Mask::Code;
                    out.push_str("  ");
                } else {
                    out.push(if c == '\n' { '\n' } else { ' ' });
                }
            }
            Mask::Str(quote) => {
                out.push(c);
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
    out
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
            if matches!(run, 3 | 4 | 6 | 8) && !next_is_ident {
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

/// Scan one file's text. `palette` marks the file whose `:root` blocks hold the
/// token declarations.
fn scan_text(label: &str, src: &str, is_css: bool, palette: bool) -> Vec<Finding> {
    let masked = mask_comments(src, !is_css);
    let exempt = if palette {
        root_block_lines(&masked)
    } else {
        BTreeSet::new()
    };

    let mut findings = Vec::new();
    for (idx, (raw, masked_line)) in src.lines().zip(masked.lines()).enumerate() {
        let line = idx + 1;
        if raw.contains(OPT_OUT) {
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
fn sources(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
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
    out
}

/// Scan a `desktop/src` tree.
fn scan_tree(src_root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for path in sources(src_root) {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let rel = path
            .strip_prefix(src_root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let is_css = path.extension().is_some_and(|e| e == "css");
        let palette = path.file_name().is_some_and(|n| n == "styles.css");
        findings.extend(scan_text(
            &format!("desktop/src/{rel}"),
            &text,
            is_css,
            palette,
        ));
    }
    findings
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
         \x20  form: `rgb(var(--ink-rgb) / .42)`.\n\
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
        let findings = scan_tree(&root);
        assert!(findings.is_empty(), "{}", report(&findings));
    }

    /// And that it is looking at something: a tree it scans clean must still
    /// have been read.
    #[test]
    fn the_real_scan_covers_every_source_file() {
        let root = repo_root().join("desktop/src");
        let files = sources(&root);
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

    const PALETTE: &str = ":root {\n  --teal: #0f7167;\n  --ink-rgb: 29 37 34;\n}\n";

    #[test]
    fn a_tokenised_tree_passes() {
        let dir = tree(&[(
            "styles.css",
            &format!(
                "{PALETTE}\
                 .a {{ color: var(--teal); background: rgb(var(--ink-rgb) / .42); }}\n\
                 .b {{ border: 1px solid transparent; white-space: nowrap; }}\n"
            ),
        )]);
        assert_eq!(scan_tree(dir.path()), vec![]);
    }

    #[test]
    fn a_bare_hex_in_css_fails_and_says_where() {
        let dir = tree(&[(
            "styles.css",
            &format!("{PALETTE}.a {{ color: #5e9c90; }}\n"),
        )]);
        let findings = scan_tree(dir.path());
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
        let findings = scan_tree(dir.path());
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
        let findings = scan_tree(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 2);
    }

    #[test]
    fn custom_properties_in_the_palette_block_are_allowed() {
        let dir = tree(&[("styles.css", PALETTE)]);
        assert_eq!(scan_tree(dir.path()), vec![]);
    }

    /// The pre-M1 defect: `:root` declared `--ink` *and* repeated its value as a
    /// bare `color:`. The block is not a blanket exemption.
    #[test]
    fn a_bare_declaration_inside_the_palette_block_still_fails() {
        let dir = tree(&[(
            "styles.css",
            ":root {\n  color: #1d2522;\n  --ink: #1d2522;\n}\n",
        )]);
        let findings = scan_tree(dir.path());
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
        assert_eq!(scan_tree(dir.path()), vec![]);
    }

    /// The exemption is `styles.css`'s, so the palette stays in one file and a
    /// stray `:root` elsewhere is not a way around the guard.
    #[test]
    fn a_root_block_in_another_stylesheet_is_not_exempt() {
        let dir = tree(&[
            ("styles.css", PALETTE),
            ("other.css", ":root { --sneaky: #5e9c90; }\n"),
        ]);
        let findings = scan_tree(dir.path());
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
        assert_eq!(scan_tree(dir.path()), vec![]);
    }

    #[test]
    fn literal_colour_functions_fail_but_the_token_form_passes() {
        let dir = tree(&[(
            "styles.css",
            &format!(
                "{PALETTE}\
                 .a {{ background: rgba(24, 30, 28, .42); }}\n\
                 .b {{ background: rgb(var(--ink-rgb) / .42); }}\n\
                 .c {{ color: hsl(160 40% 30%); }}\n\
                 .d {{ box-shadow: 0 1px 2px rgb(0 0 0 / .2); }}\n"
            ),
        )]);
        let findings = scan_tree(dir.path());
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
        let findings = scan_tree(dir.path());
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
        let findings = scan_tree(dir.path());
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].line, 3);
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
        assert_eq!(scan_tree(dir.path()), vec![]);
    }
}
