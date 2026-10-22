//! What kind of shell command an agent ran, read from the command text its
//! hook reported — for `agent.operation` spans.
//!
//! **The command line is never sent.** [`classify`] reads the text once and
//! returns three words at most: the runner (an enum), the program's basename
//! and its first non-option argument. Each word is kept only when it has the
//! shape of a command word ([`is_command_word`]: lowercase letters, digits,
//! `.`, `_` and `-`, at most 32 characters), so a path, a URL, a quoted
//! string, a token or anything with an uppercase letter or a `=` in it never
//! leaves this function; and the first argument of a program that prints or
//! searches text ([`WORDLESS_PROGRAMS`]) is never kept at all. The text itself
//! is not stored, logged or returned.
//!
//! Only the first line counts, as the deck's cards show it. It is split on
//! `&&`, `||`, `;` and `|` outside quotes, and the first segment that runs
//! something other than a shell builtin that only sets up the next one
//! ([`SETUP_PROGRAMS`]) is the one classified — after its leading `VAR=value`
//! assignments and an `env`, `sudo`, `command`, `time` or `nice` in front.

/// Where a command ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// On the agent's own machine.
    Local,
    /// Inside an RWX sandbox: `rwx sandbox exec … -- <command>`.
    RwxSandbox,
    /// An RWX run: `rwx run …`.
    RwxRun,
}

impl Runner {
    pub fn as_str(self) -> &'static str {
        match self {
            Runner::Local => "local",
            Runner::RwxSandbox => "rwx_sandbox",
            Runner::RwxRun => "rwx_run",
        }
    }
}

/// What [`classify`] keeps of a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub runner: Runner,
    /// The program's basename, when it passes [`is_command_word`].
    pub program: Option<String>,
    /// Its first non-option argument, when it passes [`is_command_word`] and
    /// the program is not one of [`WORDLESS_PROGRAMS`].
    pub word: Option<String>,
}

/// The tool names agents give their shell tool: Claude Code's `Bash`,
/// OpenCode's and Pi's `bash`, Codex's `shell` (and the names its newer
/// releases use), and Devin's `exec`.
pub const SHELL_TOOLS: &[&str] = &[
    "Bash",
    "bash",
    "shell",
    "exec",
    "exec_command",
    "shell_command",
    "local_shell",
];

/// Whether `tool_name` is an agent's shell tool.
pub fn is_shell_tool(tool_name: Option<&str>) -> bool {
    tool_name.is_some_and(|name| SHELL_TOOLS.contains(&name))
}

/// Builtins that only prepare the next command; a segment running one is
/// skipped.
pub const SETUP_PROGRAMS: &[&str] = &["cd", "export", "source", ".", "set", "true"];

/// Programs whose first argument is the text they print or search for: only
/// the program is kept.
pub const WORDLESS_PROGRAMS: &[&str] = &["echo", "printf", "cat", "grep", "rg", "sed", "awk"];

/// Words that run the rest of the segment as a command.
const PREFIX_PROGRAMS: &[&str] = &["env", "sudo", "command", "time", "nice"];

/// Shells whose `-c` argument is the command they run.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];

/// The longest command word kept.
pub const MAX_WORD_CHARS: usize = 32;

/// `^[a-z0-9][a-z0-9._-]{0,31}$`.
pub fn is_command_word(word: &str) -> bool {
    let mut bytes = word.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && word.len() <= MAX_WORD_CHARS
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// Classify the command a shell tool call reported. `None` for no text gives
/// a local operation with nothing else known.
pub fn classify(command: Option<&str>) -> Operation {
    let line = command.and_then(|c| c.lines().next()).unwrap_or("");
    classify_line(line, 0)
}

/// How deep `sh -c '…'` nesting is followed.
const MAX_NESTING: usize = 2;

fn classify_line(line: &str, depth: usize) -> Operation {
    for segment in segments(line) {
        let words = command_words(&segment);
        let Some(first) = words.first() else {
            continue;
        };
        let program = basename(first);
        if SETUP_PROGRAMS.contains(&program) {
            continue;
        }
        let rest = &words[1..];
        if depth < MAX_NESTING
            && SHELLS.contains(&program)
            && let Some(flag) = rest.first()
            && flag.starts_with('-')
            && flag.contains('c')
        {
            let inner = rest[1..].join(" ");
            return classify_line(&inner, depth + 1);
        }
        if program == "rwx" {
            match rest.first().map(String::as_str) {
                Some("sandbox") if rest.get(1).map(String::as_str) == Some("exec") => {
                    let inner = rest
                        .iter()
                        .position(|w| w == "--")
                        .map(|i| rest[i + 1..].join(" "))
                        .unwrap_or_default();
                    let mut op = if depth < MAX_NESTING {
                        classify_line(&inner, depth + 1)
                    } else {
                        named(program, rest)
                    };
                    op.runner = Runner::RwxSandbox;
                    if inner.is_empty() {
                        op.program = None;
                        op.word = None;
                    }
                    return op;
                }
                Some("run") => {
                    let mut op = named(program, rest);
                    op.runner = Runner::RwxRun;
                    return op;
                }
                _ => {}
            }
        }
        return named(program, rest);
    }
    Operation {
        runner: Runner::Local,
        program: None,
        word: None,
    }
}

/// A local operation running `program` with `args`, its words filtered.
fn named(program: &str, args: &[String]) -> Operation {
    let word = if WORDLESS_PROGRAMS.contains(&program) {
        None
    } else {
        args.iter()
            .find(|a| !a.starts_with('-') && !a.starts_with('+'))
            .filter(|w| is_command_word(w))
            .cloned()
    };
    Operation {
        runner: Runner::Local,
        program: Some(program.to_string()).filter(|p| is_command_word(p)),
        word,
    }
}

/// The last `/`-separated part of `word`.
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// A segment's words from its program on: leading assignments and the prefix
/// programs in front of it, with their options and assignments, dropped.
fn command_words(segment: &[String]) -> Vec<String> {
    let mut words = segment.iter().peekable();
    while words
        .peek()
        .is_some_and(|w| crate::event::is_env_assignment(w))
    {
        words.next();
    }
    while let Some(prefix) = words.peek().map(|w| basename(w))
        && PREFIX_PROGRAMS.contains(&prefix)
    {
        words.next();
        while let Some(w) = words.peek() {
            if w.starts_with('-') || crate::event::is_env_assignment(w) {
                let option = words.next();
                // `nice -n 10`, `sudo -u root`: the option's own value.
                if matches!(option.map(String::as_str), Some("-n" | "-u" | "-g"))
                    && words.peek().is_some()
                {
                    words.next();
                }
            } else {
                break;
            }
        }
    }
    words.cloned().collect()
}

/// `line` split into segments on `&&`, `||`, `;` and `|` outside quotes, each
/// a list of words with the shell's quotes and backslashes removed.
fn segments(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    let end_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for q in chars.by_ref() {
                    if q == '\'' {
                        break;
                    }
                    word.push(q);
                }
            }
            '"' => {
                in_word = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                word.push(escaped);
                            }
                        }
                        other => word.push(other),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(escaped) = chars.next() {
                    word.push(escaped);
                }
            }
            ';' | '|' | '&' => {
                let doubled = chars.peek() == Some(&c);
                if c == '&' && !doubled {
                    // A lone `&` (a redirection's or a background job's)
                    // is kept in the segment.
                    in_word = true;
                    word.push(c);
                    continue;
                }
                end_word(&mut word, &mut in_word, &mut words);
                if doubled {
                    chars.next();
                }
                out.push(std::mem::take(&mut words));
            }
            c if c.is_whitespace() => end_word(&mut word, &mut in_word, &mut words),
            other => {
                in_word = true;
                word.push(other);
            }
        }
    }
    end_word(&mut word, &mut in_word, &mut words);
    out.push(words);
    out.retain(|segment| !segment.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(runner: Runner, program: Option<&str>, word: Option<&str>) -> Operation {
        Operation {
            runner,
            program: program.map(str::to_string),
            word: word.map(str::to_string),
        }
    }

    fn local(program: &str, word: Option<&str>) -> Operation {
        op(Runner::Local, Some(program), word)
    }

    #[test]
    fn the_program_and_its_first_word() {
        for (command, expected) in [
            ("cargo test --workspace", local("cargo", Some("test"))),
            ("git -C repo status", local("git", Some("repo"))),
            ("cargo +nightly fmt", local("cargo", Some("fmt"))),
            ("/usr/bin/make", local("make", None)),
            ("./scripts/release.sh --dry-run", local("release.sh", None)),
            ("npm run build:prod", local("npm", Some("run"))),
            ("ls", local("ls", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    #[test]
    fn setup_segments_assignments_and_prefixes_are_skipped() {
        for (command, expected) in [
            ("cd repo && cargo build", local("cargo", Some("build"))),
            (
                "export X=1; source env.sh; make test",
                local("make", Some("test")),
            ),
            (
                "RUST_LOG=debug CI=1 cargo nextest run",
                local("cargo", Some("nextest")),
            ),
            ("env -i FOO=1 cargo check", local("cargo", Some("check"))),
            (
                "sudo -u root systemctl restart x",
                local("systemctl", Some("restart")),
            ),
            ("time nice -n 10 cargo bench", local("cargo", Some("bench"))),
            ("true || gh pr checks 1", local("gh", Some("pr"))),
            ("cargo test 2>&1 | tail -n 5", local("cargo", Some("test"))),
            ("command -v rg", local("rg", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    #[test]
    fn a_shell_dash_c_is_classified_by_what_it_runs() {
        assert_eq!(
            classify(Some("/bin/bash -lc 'cd x && cargo test'")),
            local("cargo", Some("test"))
        );
        // Codex's argv, joined: its quotes are already gone.
        assert_eq!(
            classify(Some("/bin/sh -lc touch out.txt")),
            local("touch", Some("out.txt"))
        );
    }

    #[test]
    fn rwx_runners() {
        assert_eq!(
            classify(Some("rwx sandbox exec --image x -- cargo test --workspace")),
            op(Runner::RwxSandbox, Some("cargo"), Some("test"))
        );
        assert_eq!(
            classify(Some("cd repo && rwx sandbox exec -- bash -c 'make ci'")),
            op(Runner::RwxSandbox, Some("make"), Some("ci"))
        );
        assert_eq!(
            classify(Some("rwx sandbox exec --image x")),
            op(Runner::RwxSandbox, None, None)
        );
        assert_eq!(
            classify(Some("rwx run .rwx/ci.yml --wait")),
            op(Runner::RwxRun, Some("rwx"), Some("run"))
        );
        assert_eq!(classify(Some("rwx login")), local("rwx", Some("login")));
    }

    #[test]
    fn only_the_first_line_counts() {
        assert_eq!(
            classify(Some("cargo build\nrm -rf secret-dir")),
            local("cargo", Some("build"))
        );
    }

    /// The shape filter applies to the program and the word separately.
    #[test]
    fn anything_not_shaped_like_a_command_word_is_absent() {
        for (command, expected) in [
            ("curl https://user:pw@host/x", local("curl", None)),
            (
                "git commit -m 'Fix the SECRET thing'",
                local("git", Some("commit")),
            ),
            ("python3 /home/me/private/script.py", local("python3", None)),
            ("Rscript analysis.R", op(Runner::Local, None, None)),
            (
                "kubectl get secret/db-PASSWORD",
                local("kubectl", Some("get")),
            ),
            ("node -e 'console.log(1)'", local("node", None)),
            ("tool TOKEN=abc", local("tool", None)),
            (
                "make aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                local("make", None),
            ),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
        assert!(is_command_word(&"a".repeat(32)));
        assert!(!is_command_word(&"a".repeat(33)));
        assert!(!is_command_word("-x"));
        assert!(!is_command_word("_x"));
        assert!(!is_command_word(""));
    }

    #[test]
    fn text_tools_never_send_their_first_word() {
        for program in WORDLESS_PROGRAMS {
            assert_eq!(
                classify(Some(&format!("{program} hunter2 file"))),
                local(program, None),
                "{program}"
            );
        }
        assert_eq!(
            classify(Some("FOO=1 /bin/grep -r password .")),
            local("grep", None)
        );
    }

    #[test]
    fn no_command_or_only_setup_is_a_bare_local_operation() {
        let bare = op(Runner::Local, None, None);
        assert_eq!(classify(None), bare);
        assert_eq!(classify(Some("")), bare);
        assert_eq!(classify(Some("cd /tmp && export A=b")), bare);
        assert_eq!(classify(Some("   ")), bare);
    }

    #[test]
    fn quoted_operators_do_not_split() {
        assert_eq!(
            classify(Some("cd x; 'my tool' run")),
            op(Runner::Local, None, Some("run"))
        );
        assert_eq!(
            classify(Some(r#"git log --format="%H;%s" | head"#)),
            local("git", Some("log"))
        );
    }

    #[test]
    fn shell_tools_by_agent() {
        for name in ["Bash", "bash", "shell", "exec"] {
            assert!(is_shell_tool(Some(name)), "{name}");
        }
        for name in ["Read", "Edit", "apply_patch", "write"] {
            assert!(!is_shell_tool(Some(name)), "{name}");
        }
        assert!(!is_shell_tool(None));
    }
}
