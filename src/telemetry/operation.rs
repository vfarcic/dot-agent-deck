//! What kind of shell command an agent ran, read from the command text its
//! hook reported — for `agent.operation` spans.
//!
//! **The command line is not sent.** [`classify`] reads the text once and
//! returns two values at most: the program's basename and its first
//! non-option argument. Each word is kept only when it has the
//! shape of a command word ([`is_command_word`]: lowercase letters, digits,
//! `.`, `_` and `-`, at most 32 characters), which leaves out a path, a URL, a
//! quoted string with a space in it, and anything with an uppercase letter or
//! a `=` in it. **That shape is not a secret filter**: a short lowercase
//! password, file name or host name has it. So the first argument of a program
//! whose arguments are usually data — text, patterns, files, hosts, request
//! bodies and credentials ([`WORDLESS_PROGRAMS`]) — is never kept, and a
//! wrapper whose options cannot be told apart from the command it runs gives
//! neither a program nor a word, nor does a program that is not kept. The text
//! itself is not stored, logged or returned.
//!
//! Only the first line counts, as the deck's cards show it, and only up to a
//! shell comment: an unquoted `#` starting a word ends it. It is split on
//! `&&`, `||`, `;` and `|` outside quotes into words, and those words keep the
//! boundaries the shell gives them: a quoted argument stays one word wherever
//! it is unwrapped from, and a shell's `-c` script is read as a script, its
//! later arguments not. A shell reading its script from
//! standard input (`bash -s …`) gives its name only. The first segment that runs
//! something other than a shell builtin that only sets up the next one
//! ([`SETUP_PROGRAMS`]) is the one classified — after its leading `VAR=value`
//! assignments and an `env`, `sudo`, `command`, `time` or `nice` in front,
//! with their options and the options' values.

/// What [`classify`] keeps of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operation {
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

/// The shell tool whose command the hooks report as an argv joined with
/// spaces (Codex's `shell`): see [`classify_joined_argv`].
pub const JOINED_ARGV_TOOL: &str = "shell";

/// Whether `tool_name` is an agent's shell tool.
pub fn is_shell_tool(tool_name: Option<&str>) -> bool {
    tool_name.is_some_and(|name| SHELL_TOOLS.contains(&name))
}

/// Classify the command a shell tool call reported, read as its tool reports
/// it: [`classify_joined_argv`] for [`JOINED_ARGV_TOOL`], else [`classify`].
pub fn classify_tool(tool_name: Option<&str>, command: Option<&str>) -> Operation {
    if tool_name == Some(JOINED_ARGV_TOOL) {
        classify_joined_argv(command)
    } else {
        classify(command)
    }
}

/// Builtins that only prepare the next command; a segment running one is
/// skipped.
pub const SETUP_PROGRAMS: &[&str] = &["cd", "export", "source", ".", "set", "true"];

/// Programs whose arguments are usually data — text they print, a pattern, a
/// file, a host, a request body or a credential: only the program is kept.
pub const WORDLESS_PROGRAMS: &[&str] = &[
    "echo",
    "printf",
    "cat",
    "grep",
    "rg",
    "sed",
    "awk",
    "curl",
    "wget",
    "redis-cli",
    "base64",
    "head",
    "tail",
    "xargs",
    "ssh",
    "scp",
    "sftp",
    "rsync",
];

/// Shells whose `-c` argument is the command they run.
const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];

/// The single-letter options a shell takes without a value. `-c` is read
/// separately, and `-o` / `-O` take one.
const SHELL_FLAGS: &str = "abefhiklmnprstuvxBCDEHPT";

/// The options of a program that runs the rest of its words as a command.
struct Options {
    /// Short options that take a value: `-u root`, or attached, `-uroot`.
    short_values: &'static str,
    /// Short options that take none.
    short_flags: &'static str,
    /// Long options that take a value: `--chdir dir` or `--chdir=dir`.
    long_values: &'static [&'static str],
    /// Long options that take none, or only an attached `=value`.
    long_flags: &'static [&'static str],
    /// Whether a number is an option: `nice -10`.
    numbers: bool,
}

/// `env`. `-S` / `--split-string`, which carries the command inside its
/// value, is deliberately unknown.
const ENV: Options = Options {
    short_values: "uCPa",
    short_flags: "i0v",
    long_values: &["unset", "chdir", "argv0"],
    long_flags: &[
        "ignore-environment",
        "null",
        "debug",
        "block-signal",
        "default-signal",
        "ignore-signal",
        "list-signal-handling",
    ],
    numbers: false,
};

/// `sudo`. `-e` / `--edit`, which takes files rather than a command, is
/// deliberately unknown.
const SUDO: Options = Options {
    short_values: "ugChprtUTDR",
    short_flags: "AbBEHiKklnNPSsvV",
    long_values: &[
        "user",
        "group",
        "close-from",
        "host",
        "prompt",
        "role",
        "type",
        "other-user",
        "command-timeout",
        "chdir",
        "chroot",
    ],
    long_flags: &[
        "askpass",
        "background",
        "bell",
        "preserve-env",
        "set-home",
        "login",
        "remove-timestamp",
        "reset-timestamp",
        "list",
        "non-interactive",
        "no-update",
        "preserve-groups",
        "stdin",
        "shell",
        "validate",
        "version",
    ],
    numbers: false,
};

/// `command`. Its `-v` / `-V` look a name up rather than run it: see
/// [`command_words`].
const COMMAND: Options = Options {
    short_values: "",
    short_flags: "pvV",
    long_values: &[],
    long_flags: &[],
    numbers: false,
};

/// GNU `time`, and the shell keyword's `-p`.
const TIME: Options = Options {
    short_values: "fo",
    short_flags: "apqv",
    long_values: &["format", "output"],
    long_flags: &["append", "portability", "quiet", "verbose"],
    numbers: false,
};

/// `nice`.
const NICE: Options = Options {
    short_values: "n",
    short_flags: "",
    long_values: &["adjustment"],
    long_flags: &[],
    numbers: true,
};

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

/// [`classify`] for a command reported as an argv joined with single spaces,
/// as Codex's `shell` tool's is. Which of its spaces were inside an argument
/// is lost, so no word is read from it at all: only the program, and only
/// where it is certain. That is the first word of the line, or, for a shell
/// run as `<shell> -c <script>` (Codex runs its commands as
/// `["/bin/sh", "-lc", "<script>"]`), the first word of the script. A program
/// that only sets up, prefixes or wraps the next one gives nothing, since that
/// one's words may belong to a later argument; so does a line with an empty
/// argument in it, or a shell with any other options.
pub fn classify_joined_argv(command: Option<&str>) -> Operation {
    let line = command.and_then(|c| c.lines().next()).unwrap_or("");
    let words: Vec<&str> = line.split(' ').collect();
    // An empty argument, or one starting or ending with a space, leaves an
    // empty word: nothing can be placed.
    if words.iter().any(|w| w.is_empty()) {
        return Operation::default();
    }
    let program = match words.as_slice() {
        [shell, flags, script @ ..] if SHELLS.contains(&basename(shell)) => {
            let dash_c = flags.strip_prefix('-').is_some_and(|cluster| {
                cluster.contains('c')
                    && cluster.chars().all(|c| c == 'c' || SHELL_FLAGS.contains(c))
            });
            match script {
                [program, ..] if dash_c => basename(program),
                _ => return Operation::default(),
            }
        }
        [program, ..] => basename(program),
        [] => return Operation::default(),
    };
    let unsure = SETUP_PROGRAMS.contains(&program)
        || SHELLS.contains(&program)
        || ["env", "sudo", "command", "time", "nice", "exec", "eval"].contains(&program);
    if unsure {
        return Operation::default();
    }
    named(program, &[])
}

/// How deep `sh -c '…'` nesting is followed.
const MAX_NESTING: usize = 2;

fn classify_line(line: &str, depth: usize) -> Operation {
    segments(line)
        .iter()
        .find_map(|segment| classify_words(segment, depth))
        .unwrap_or(Operation::default())
}

/// Classify one segment's words. `None` when the segment only sets up the
/// next one.
fn classify_words(segment: &[String], depth: usize) -> Option<Operation> {
    let Some(words) = command_words(segment) else {
        return Some(Operation::default());
    };
    let first = words.first()?;
    let program = basename(first);
    if SETUP_PROGRAMS.contains(&program) {
        return None;
    }
    let rest = &words[1..];
    if SHELLS.contains(&program) {
        return Some(match shell_operand(rest) {
            None => Operation::default(),
            Some(ShellInput::File(at)) => named(program, &rest[at..]),
            Some(ShellInput::Stdin) => named(program, &[]),
            Some(ShellInput::Command(_)) if depth >= MAX_NESTING => named(program, &[]),
            Some(ShellInput::Command(at)) => match rest.get(at) {
                Some(script) => classify_line(script, depth + 1),
                None => Operation::default(),
            },
        });
    }
    Some(named(program, rest))
}

/// An operation running `program` with `args`, its words filtered. No
/// word is kept for a program that is not kept: what it is cannot be told.
fn named(program: &str, args: &[String]) -> Operation {
    let program = Some(program.to_string()).filter(|p| is_command_word(p));
    let word = match &program {
        Some(program) if !WORDLESS_PROGRAMS.contains(&program.as_str()) => args
            .iter()
            .find(|a| !a.starts_with('-') && !a.starts_with('+'))
            .filter(|w| is_command_word(w))
            .cloned(),
        _ => None,
    };
    Operation { program, word }
}

/// The last `/`-separated part of `word`.
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// A segment's words from its program on: leading assignments and the prefix
/// programs in front of it, with their options, the options' values and
/// assignments, dropped. `None` when a prefix's options cannot be told apart
/// from the command it runs.
fn command_words(segment: &[String]) -> Option<&[String]> {
    let mut words = segment;
    loop {
        let assignments = words
            .iter()
            .take_while(|w| crate::event::is_env_assignment(w))
            .count();
        words = &words[assignments..];
        let Some(first) = words.first() else {
            return Some(words);
        };
        let prefix = basename(first);
        let options = match prefix {
            "env" => &ENV,
            "sudo" => &SUDO,
            "command" => &COMMAND,
            "time" => &TIME,
            "nice" => &NICE,
            _ => return Some(words),
        };
        let len = options_len(&words[1..], options)?;
        // `command -v cargo` looks `cargo` up and runs nothing: the program
        // is `command`.
        if prefix == "command"
            && words[1..=len]
                .iter()
                .any(|w| !w.starts_with("--") && w.starts_with('-') && w.contains(['v', 'V']))
        {
            return Some(words);
        }
        words = &words[1 + len..];
    }
}

/// How many of `words` are `options` with their values, up to the command
/// they run (past a `--`). `None` when one is not an option `options` knows,
/// or its value is missing.
fn options_len(words: &[String], options: &Options) -> Option<usize> {
    let mut i = 0;
    while let Some(word) = words.get(i) {
        if word == "--" {
            return Some(i + 1);
        }
        if let Some(long) = word.strip_prefix("--") {
            let (name, attached) = match long.split_once('=') {
                Some((name, _)) => (name, true),
                None => (long, false),
            };
            if options.long_values.contains(&name) {
                i += if attached { 1 } else { 2 };
            } else if options.long_flags.contains(&name) {
                i += 1;
            } else {
                return None;
            }
            continue;
        }
        let Some(cluster) = word.strip_prefix('-').filter(|c| !c.is_empty()) else {
            break;
        };
        if options.numbers && cluster.bytes().all(|b| b.is_ascii_digit()) {
            i += 1;
            continue;
        }
        let mut takes_next = false;
        for (at, c) in cluster.char_indices() {
            if options.short_values.contains(c) {
                // A value attached to the option is the rest of this word.
                takes_next = at + c.len_utf8() == cluster.len();
                break;
            }
            if !options.short_flags.contains(c) {
                return None;
            }
        }
        i += if takes_next { 2 } else { 1 };
    }
    (i <= words.len()).then_some(i)
}

/// Where a shell reads the commands it runs.
enum ShellInput {
    /// `-c`: from its operand at this index, a script.
    Command(usize),
    /// From the file its operand at this index names.
    File(usize),
    /// From its standard input — `-s`, or no operand — its operands being the
    /// script's arguments.
    Stdin,
}

/// A shell's words after its name, read as its options: where it reads the
/// commands it runs. `None` when an option is not one a shell has, or its
/// value is missing.
fn shell_operand(args: &[String]) -> Option<ShellInput> {
    let mut command = false;
    let mut stdin = false;
    let mut i = 0;
    while let Some(word) = args.get(i) {
        if word == "--" || word == "-" {
            i += 1;
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            i += match long {
                "rcfile" | "init-file" => 2,
                "login" | "norc" | "noprofile" | "noediting" | "posix" | "restricted"
                | "verbose" | "debugger" | "dump-strings" | "dump-po-strings" | "help"
                | "version" | "pretty-print" => 1,
                _ => return None,
            };
            continue;
        }
        let (minus, cluster) = match (word.strip_prefix('-'), word.strip_prefix('+')) {
            (Some(cluster), _) => (true, cluster),
            (None, Some(cluster)) => (false, cluster),
            (None, None) => break,
        };
        let mut values = 0;
        for c in cluster.chars() {
            match c {
                'c' if minus => command = true,
                's' => stdin = true,
                'o' | 'O' => values += 1,
                c if SHELL_FLAGS.contains(c) => {}
                _ => return None,
            }
        }
        i += 1 + values;
    }
    if i > args.len() {
        return None;
    }
    Some(if command {
        ShellInput::Command(i)
    } else if stdin || i == args.len() {
        ShellInput::Stdin
    } else {
        ShellInput::File(i)
    })
}

/// `line` split into segments on `&&`, `||`, `;` and `|` outside quotes, each
/// a list of words with the shell's quotes and backslashes removed. A comment
/// ends the line: an unquoted `#` that starts a word, or follows one of the
/// shell's operator characters (`&`, `<`, `>`, `(`, `)`), and everything after
/// it is dropped before any separator in it is read.
fn segments(line: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    // Whether the last character read was an unquoted operator character
    // kept in the word.
    let mut after_operator = false;
    let mut chars = line.chars().peekable();
    let end_word = |word: &mut String, in_word: &mut bool, words: &mut Vec<String>| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        let operator_before = std::mem::take(&mut after_operator);
        match c {
            '#' if !in_word || operator_before => break,
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
                    after_operator = true;
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
                after_operator = matches!(other, '<' | '>' | '(' | ')');
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

    fn op(program: Option<&str>, word: Option<&str>) -> Operation {
        Operation {
            program: program.map(str::to_string),
            word: word.map(str::to_string),
        }
    }

    fn expected(program: &str, word: Option<&str>) -> Operation {
        op(Some(program), word)
    }

    #[test]
    fn the_program_and_its_first_word() {
        for (command, expected) in [
            ("cargo test --workspace", expected("cargo", Some("test"))),
            ("git -C repo status", expected("git", Some("repo"))),
            ("cargo +nightly fmt", expected("cargo", Some("fmt"))),
            ("/usr/bin/make", expected("make", None)),
            (
                "./scripts/release.sh --dry-run",
                expected("release.sh", None),
            ),
            ("npm run build:prod", expected("npm", Some("run"))),
            ("ls", expected("ls", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    #[test]
    fn setup_segments_assignments_and_prefixes_are_skipped() {
        for (command, expected) in [
            ("cd repo && cargo build", expected("cargo", Some("build"))),
            (
                "export X=1; source env.sh; make test",
                expected("make", Some("test")),
            ),
            (
                "RUST_LOG=debug CI=1 cargo nextest run",
                expected("cargo", Some("nextest")),
            ),
            ("env -i FOO=1 cargo check", expected("cargo", Some("check"))),
            (
                "sudo -u root systemctl restart x",
                expected("systemctl", Some("restart")),
            ),
            (
                "time nice -n 10 cargo bench",
                expected("cargo", Some("bench")),
            ),
            ("true || gh pr checks 1", expected("gh", Some("pr"))),
            (
                "cargo test 2>&1 | tail -n 5",
                expected("cargo", Some("test")),
            ),
            ("command -v rg", expected("command", Some("rg"))),
            ("command -p make test", expected("make", Some("test"))),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    #[test]
    fn a_shell_dash_c_is_classified_by_what_it_runs() {
        assert_eq!(
            classify(Some("/bin/bash -lc 'cd x && cargo test'")),
            expected("cargo", Some("test"))
        );
        // Unquoted, the script is `touch`; `out.txt` is the script's `$0`.
        assert_eq!(
            classify(Some("/bin/sh -lc touch out.txt")),
            expected("touch", None)
        );
    }

    /// A quoted argument stays one word however it is unwrapped, and a
    /// shell's later arguments are not its script.
    #[test]
    fn word_boundaries_survive_unwrapping() {
        for (command, expected) in [
            ("cargo 'private payload'", expected("cargo", None)),
            ("env cargo 'abc;node secret'", expected("cargo", None)),
            ("sudo -u root cargo \"a && b\"", expected("cargo", None)),
            (
                r#"env bash -c 'node "private payload"'"#,
                expected("node", None),
            ),
            (
                "FOO=1 env -u X sudo cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "bash -c 'cargo test' private payload",
                expected("cargo", Some("test")),
            ),
            ("bash -c cargo private", expected("cargo", None)),
            ("sh -c 'node \"private payload\"' x", expected("node", None)),
            (r#"sh -c 'sh -c "make ci"'"#, expected("make", Some("ci"))),
            (r#"sh -c 'sh -c "sh -c make"'"#, expected("sh", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// A prefix's options and their values are skipped, not read as the
    /// program; a form whose options are not known gives nothing.
    #[test]
    fn prefix_and_shell_options_take_their_values() {
        let unresolved = Operation::default();
        for (command, expected) in [
            (
                "time -f hunter2 cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "time -o out.txt -a cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "time --format=%e cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "time --output out -p cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "env --chdir /home/private cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "env -C /home/private cargo test",
                expected("cargo", Some("test")),
            ),
            ("env -u hunter2 cargo test", expected("cargo", Some("test"))),
            ("env -iu x Y=1 cargo test", expected("cargo", Some("test"))),
            (
                "env --unset=x -- cargo test",
                expected("cargo", Some("test")),
            ),
            ("env -S 'cargo test'", unresolved.clone()),
            ("env --split-string='cargo test'", unresolved.clone()),
            ("env --bogus x cargo test", unresolved.clone()),
            (
                "nice --adjustment 10 cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "nice --adjustment=10 cargo test",
                expected("cargo", Some("test")),
            ),
            ("nice -n 10 cargo test", expected("cargo", Some("test"))),
            ("nice -n10 cargo test", expected("cargo", Some("test"))),
            ("nice -10 cargo test", expected("cargo", Some("test"))),
            (
                "sudo -u root -g wheel -C 5 systemctl restart x",
                expected("systemctl", Some("restart")),
            ),
            (
                "sudo --user=root -H make install",
                expected("make", Some("install")),
            ),
            (
                "sudo -uroot make install",
                expected("make", Some("install")),
            ),
            ("sudo -e /etc/hosts", unresolved.clone()),
            ("sudo -u", unresolved.clone()),
            (
                "bash --rcfile /home/private -c 'cargo test'",
                expected("cargo", Some("test")),
            ),
            (
                "bash --init-file private -lc 'cargo test'",
                expected("cargo", Some("test")),
            ),
            (
                "bash -o pipefail -c 'make ci'",
                expected("make", Some("ci")),
            ),
            (
                "bash -eo pipefail -c 'make ci'",
                expected("make", Some("ci")),
            ),
            ("bash +O extglob -c 'make ci'", expected("make", Some("ci"))),
            ("bash --login -c 'make ci'", expected("make", Some("ci"))),
            ("bash --bogus -c 'make ci'", unresolved.clone()),
            ("bash -c", unresolved.clone()),
            ("bash --rcfile", unresolved.clone()),
            ("bash ci.sh -c x", expected("bash", Some("ci.sh"))),
            ("bash --rcfile private", expected("bash", None)),
            ("sh", expected("sh", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// Codex's `shell` tool reports its argv joined with spaces, which loses
    /// where each argument ends: its program only, and only where certain.
    #[test]
    fn a_joined_argv_gives_its_program_at_most() {
        let bare = Operation::default();
        for (command, expected) in [
            ("/bin/sh -lc touch out.txt", expected("touch", None)),
            (
                "/bin/bash -lc cargo test --workspace",
                expected("cargo", None),
            ),
            // `["/bin/sh", "-c", "cargo", "private payload"]`: the script is
            // `cargo`, the rest its `$0` and `$1`.
            ("/bin/sh -c cargo private payload", expected("cargo", None)),
            // `["/bin/sh", "-c", "true", "x && hunter2 run"]`.
            ("/bin/sh -c true x && hunter2 run", bare.clone()),
            ("/bin/bash -lc cd x && cargo test", bare.clone()),
            ("/bin/sh -c FOO=1 cargo test", bare.clone()),
            ("/bin/sh -c env cargo test", bare.clone()),
            ("/bin/sh -c sh -c make ci", bare.clone()),
            ("/bin/sh -c 'cargo test'", bare.clone()),
            ("/bin/sh -c  cargo", bare.clone()),
            ("bash --rcfile private -c make ci", bare.clone()),
            ("bash -o pipefail -c make ci", bare.clone()),
            ("bash ci.sh", bare.clone()),
            ("bash -c", bare.clone()),
            ("cargo private payload", expected("cargo", None)),
            ("env hunter2 cargo", bare.clone()),
            ("rwx sandbox exec -- cargo private", expected("rwx", None)),
            (
                "/bin/sh -lc rwx sandbox exec -- cargo test",
                expected("rwx", None),
            ),
            ("", bare.clone()),
        ] {
            assert_eq!(classify_joined_argv(Some(command)), expected, "{command}");
        }
        assert_eq!(
            classify_tool(Some("shell"), Some("/bin/sh -lc make ci")),
            expected("make", None)
        );
        assert_eq!(
            classify_tool(Some("Bash"), Some("/bin/sh -lc make ci")),
            expected("make", None)
        );
        assert_eq!(
            classify_tool(Some("Bash"), Some("/bin/sh -lc 'make ci'")),
            expected("make", Some("ci"))
        );
    }

    #[test]
    fn only_the_first_line_counts() {
        assert_eq!(
            classify(Some("cargo build\nrm -rf secret-dir")),
            expected("cargo", Some("build"))
        );
    }

    /// The shape filter applies to the program and the word separately.
    #[test]
    fn anything_not_shaped_like_a_command_word_is_absent() {
        for (command, expected) in [
            ("curl https://user:pw@host/x", expected("curl", None)),
            (
                "git commit -m 'Fix the SECRET thing'",
                expected("git", Some("commit")),
            ),
            (
                "python3 /home/me/private/script.py",
                expected("python3", None),
            ),
            ("Rscript analysis.R", Operation::default()),
            (
                "kubectl get secret/db-PASSWORD",
                expected("kubectl", Some("get")),
            ),
            ("node -e 'console.log(1)'", expected("node", None)),
            ("tool TOKEN=abc", expected("tool", None)),
            (
                "make aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                expected("make", None),
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
                expected(program, None),
                "{program}"
            );
        }
        assert_eq!(
            classify(Some("FOO=1 /bin/grep -r password .")),
            expected("grep", None)
        );
        for (command, program) in [
            ("curl -d hunter2 https://example.invalid", "curl"),
            (
                "curl --oauth2-bearer hunter2 https://example.invalid",
                "curl",
            ),
            ("curl -u 'hunter2' https://example.invalid", "curl"),
            ("wget hunter2.example", "wget"),
            ("redis-cli -a hunter2 ping", "redis-cli"),
            ("ssh internal-prod", "ssh"),
            ("scp internal-prod:x .", "scp"),
            ("sftp internal-prod", "sftp"),
            ("rsync -a internal-prod:x .", "rsync"),
            ("base64 private.txt", "base64"),
            ("head private.txt", "head"),
            ("tail -f private.log", "tail"),
            ("xargs hunter2", "xargs"),
            ("sh -c 'curl -d hunter2 x'", "curl"),
        ] {
            assert_eq!(
                classify(Some(command)),
                expected(program, None),
                "{command}"
            );
        }
    }

    #[test]
    fn no_command_or_only_setup_names_no_program() {
        let bare = Operation::default();
        assert_eq!(classify(None), bare);
        assert_eq!(classify(Some("")), bare);
        assert_eq!(classify(Some("cd /tmp && export A=b")), bare);
        assert_eq!(classify(Some("   ")), bare);
    }

    #[test]
    fn quoted_operators_do_not_split() {
        assert_eq!(
            classify(Some("cd x; 'my tool' run")),
            Operation::default(),
            "no word for a program that is not kept"
        );
        assert_eq!(
            classify(Some(r#"git log --format="%H;%s" | head"#)),
            expected("git", Some("log"))
        );
    }

    /// A shell comment is not a command: nothing in it is a program or a
    /// word, and its separators split nothing.
    #[test]
    fn comments_are_not_commands() {
        let bare = Operation::default();
        for (command, expected) in [
            ("# hunter2", bare.clone()),
            ("   # hunter2 run", bare.clone()),
            ("cd /repo # note; hunter2", bare.clone()),
            ("cd /repo # note && hunter2 run", bare.clone()),
            ("cd /repo # note | hunter2 run", bare.clone()),
            ("cd /repo;# note; hunter2", bare.clone()),
            ("cd /repo &# note; hunter2", bare.clone()),
            ("cd /repo >#note; hunter2", bare.clone()),
            ("cargo test # hunter2", expected("cargo", Some("test"))),
            ("cargo # hunter2", expected("cargo", None)),
            ("cd x && make # ci; hunter2", expected("make", None)),
            ("bash -c 'cd x # y; hunter2'", bare.clone()),
            // Quoted or inside a word, `#` is not a comment.
            ("cd x; make '#'; hunter2", expected("make", None)),
            ("cd x; make ci#1", expected("make", None)),
            (
                r#"git log --format="%H #%s" | head"#,
                expected("git", Some("log")),
            ),
            (r"cd x; make \#; hunter2", expected("make", None)),
            ("cd x; make issue#1; hunter2 run", expected("make", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
        assert_eq!(
            segments("cd x; make issue#1; hunter2 run"),
            [
                vec!["cd", "x"],
                vec!["make", "issue#1"],
                vec!["hunter2", "run"]
            ]
        );
        assert_eq!(segments("make '#' # x; y"), [vec!["make", "#"]]);
    }

    /// A shell reading its script from standard input: its operands are the
    /// script's arguments, not a script or a subcommand.
    #[test]
    fn a_shell_reading_stdin_gives_its_name_only() {
        for (command, shell) in [
            ("bash -s -- hunter2 < script.sh", "bash"),
            ("sh -s hunter2", "sh"),
            ("bash -es hunter2", "bash"),
            ("zsh -s -- hunter2", "zsh"),
            ("bash -s", "bash"),
            ("bash", "bash"),
            ("bash < script.sh", "bash"),
        ] {
            assert_eq!(classify(Some(command)), expected(shell, None), "{command}");
        }
        assert_eq!(
            classify(Some("bash ci.sh hunter2")),
            expected("bash", Some("ci.sh"))
        );
        assert_eq!(
            classify(Some("bash -s -c 'make ci'")),
            expected("make", Some("ci"))
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
