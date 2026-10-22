//! What kind of shell command an agent ran, read from the command text its
//! hook reported — for `agent.operation` spans.
//!
//! **The command line is not sent.** [`classify`] reads the text once and
//! returns three values at most: the runner (an enum), the program's basename
//! and its first non-option argument. Each word is kept only when it has the
//! shape of a command word ([`is_command_word`]: lowercase letters, digits,
//! `.`, `_` and `-`, at most 32 characters), which leaves out a path, a URL, a
//! quoted string with a space in it, and anything with an uppercase letter or
//! a `=` in it. **That shape is not a secret filter**: a short lowercase
//! password, file name or host name has it. So the first argument of a program
//! whose arguments are usually data — text, patterns, files, hosts, request
//! bodies and credentials ([`WORDLESS_PROGRAMS`]) — is never kept, and a
//! wrapper whose options cannot be told apart from the command it runs gives
//! neither a program nor a word. The text itself is not stored, logged or
//! returned.
//!
//! Only the first line counts, as the deck's cards show it. It is split on
//! `&&`, `||`, `;` and `|` outside quotes into words, and those words keep the
//! boundaries the shell gives them: a quoted argument stays one word wherever
//! it is unwrapped from (`rwx sandbox exec … --`), and a shell's `-c` script is
//! read as a script, its later arguments not. The first segment that runs
//! something other than a shell builtin that only sets up the next one
//! ([`SETUP_PROGRAMS`]) is the one classified — after its leading `VAR=value`
//! assignments and an `env`, `sudo`, `command`, `time` or `nice` in front,
//! with their options and the options' values.

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

impl Operation {
    /// An operation of which nothing but its runner is known.
    fn bare(runner: Runner) -> Self {
        Operation {
            runner,
            program: None,
            word: None,
        }
    }
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
pub(super) struct Options {
    /// Short options that take a value: `-u root`, or attached, `-uroot`.
    pub short_values: &'static str,
    /// Short options that take none.
    pub short_flags: &'static str,
    /// Long options that take a value: `--chdir dir` or `--chdir=dir`.
    pub long_values: &'static [&'static str],
    /// Long options that take none, or only an attached `=value`.
    pub long_flags: &'static [&'static str],
    /// Whether a number is an option: `nice -10`.
    pub numbers: bool,
}

/// `env`. `-S` / `--split-string`, which carries the command inside its
/// value, is deliberately unknown.
pub(super) const ENV: Options = Options {
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
/// as Codex's `shell` tool's is: which of its spaces were inside an argument
/// is lost. A shell running a `-c` script is classified by the script, which
/// is the rest of the line — Codex runs its commands as
/// `["/bin/sh", "-lc", "<script>"]`. Anything else gives its program only,
/// and its runner.
pub fn classify_joined_argv(command: Option<&str>) -> Operation {
    let line = command.and_then(|c| c.lines().next()).unwrap_or("");
    let words: Vec<(usize, &str)> = line
        .split(' ')
        .scan(0, |at, word| {
            let start = *at;
            *at += word.len() + 1;
            Some((start, word))
        })
        .filter(|(_, word)| !word.is_empty())
        .collect();
    let Some(&(_, first)) = words.first() else {
        return Operation::bare(Runner::Local);
    };
    let program = basename(first);
    let rest: Vec<String> = words[1..].iter().map(|(_, w)| w.to_string()).collect();
    if SHELLS.contains(&program)
        && let Some((true, at)) = shell_operand(&rest)
        && let Some(&(start, _)) = words.get(at + 1)
    {
        return classify_line(&line[start..], 1);
    }
    match (
        program,
        rest.first().map(String::as_str),
        rest.get(1).map(String::as_str),
    ) {
        ("rwx", Some("sandbox"), Some("exec")) => Operation::bare(Runner::RwxSandbox),
        ("rwx", Some("run"), _) => Operation {
            runner: Runner::RwxRun,
            program: Some("rwx".to_string()),
            word: Some("run".to_string()),
        },
        _ => named(program, &[]),
    }
}

/// How deep `sh -c '…'` and `rwx sandbox exec -- …` nesting is followed.
const MAX_NESTING: usize = 2;

fn classify_line(line: &str, depth: usize) -> Operation {
    segments(line)
        .iter()
        .find_map(|segment| classify_words(segment, depth))
        .unwrap_or(Operation::bare(Runner::Local))
}

/// Classify one segment's words. `None` when the segment only sets up the
/// next one.
fn classify_words(segment: &[String], depth: usize) -> Option<Operation> {
    let Some(words) = command_words(segment) else {
        return Some(Operation::bare(Runner::Local));
    };
    let first = words.first()?;
    let program = basename(first);
    if SETUP_PROGRAMS.contains(&program) {
        return None;
    }
    let rest = &words[1..];
    if SHELLS.contains(&program) {
        return Some(match shell_operand(rest) {
            None => Operation::bare(Runner::Local),
            Some((false, at)) => named(program, &rest[at..]),
            Some((true, _)) if depth >= MAX_NESTING => named(program, &[]),
            Some((true, at)) => match rest.get(at) {
                Some(script) => classify_line(script, depth + 1),
                None => Operation::bare(Runner::Local),
            },
        });
    }
    if program == "rwx" {
        match rest.first().map(String::as_str) {
            Some("sandbox") if rest.get(1).map(String::as_str) == Some("exec") => {
                let inner = rest
                    .iter()
                    .position(|w| w == "--")
                    .map(|i| &rest[i + 1..])
                    .filter(|inner| !inner.is_empty() && depth < MAX_NESTING);
                let mut op = inner
                    .and_then(|inner| classify_words(inner, depth + 1))
                    .unwrap_or(Operation::bare(Runner::Local));
                op.runner = Runner::RwxSandbox;
                return Some(op);
            }
            Some("run") => {
                let mut op = named(program, rest);
                op.runner = Runner::RwxRun;
                return Some(op);
            }
            _ => {}
        }
    }
    Some(named(program, rest))
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
pub(super) fn options_len(words: &[String], options: &Options) -> Option<usize> {
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

/// A shell's words after its name, read as its options: whether `-c` makes
/// its first operand a script, and where that operand is. `None` when an
/// option is not one a shell has, or its value is missing.
fn shell_operand(args: &[String]) -> Option<(bool, usize)> {
    let mut command = false;
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
                'o' | 'O' => values += 1,
                c if SHELL_FLAGS.contains(c) => {}
                _ => return None,
            }
        }
        i += 1 + values;
    }
    (i <= args.len()).then_some((command, i))
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
            ("command -v rg", local("command", Some("rg"))),
            ("command -p make test", local("make", Some("test"))),
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
        // Unquoted, the script is `touch`; `out.txt` is the script's `$0`.
        assert_eq!(
            classify(Some("/bin/sh -lc touch out.txt")),
            local("touch", None)
        );
    }

    /// A quoted argument stays one word however it is unwrapped, and a
    /// shell's later arguments are not its script.
    #[test]
    fn word_boundaries_survive_unwrapping() {
        let sandbox =
            |program: Option<&str>, word: Option<&str>| op(Runner::RwxSandbox, program, word);
        for (command, expected) in [
            (
                "rwx sandbox exec -- cargo 'private payload'",
                sandbox(Some("cargo"), None),
            ),
            (
                "rwx sandbox exec -- cargo 'abc;node secret'",
                sandbox(Some("cargo"), None),
            ),
            (
                "rwx sandbox exec -- cargo \"a && b\"",
                sandbox(Some("cargo"), None),
            ),
            (
                r#"rwx sandbox exec -- bash -c 'node "private payload"'"#,
                sandbox(Some("node"), None),
            ),
            (
                "rwx sandbox exec -- FOO=1 env -u X cargo test",
                sandbox(Some("cargo"), Some("test")),
            ),
            (
                "bash -c 'cargo test' private payload",
                local("cargo", Some("test")),
            ),
            ("bash -c cargo private", local("cargo", None)),
            ("sh -c 'node \"private payload\"' x", local("node", None)),
            (r#"sh -c 'sh -c "make ci"'"#, local("make", Some("ci"))),
            (r#"sh -c 'sh -c "sh -c make"'"#, local("sh", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// A prefix's options and their values are skipped, not read as the
    /// program; a form whose options are not known gives nothing.
    #[test]
    fn prefix_and_shell_options_take_their_values() {
        let unresolved = op(Runner::Local, None, None);
        for (command, expected) in [
            ("time -f hunter2 cargo test", local("cargo", Some("test"))),
            (
                "time -o out.txt -a cargo test",
                local("cargo", Some("test")),
            ),
            ("time --format=%e cargo test", local("cargo", Some("test"))),
            (
                "time --output out -p cargo test",
                local("cargo", Some("test")),
            ),
            (
                "env --chdir /home/private cargo test",
                local("cargo", Some("test")),
            ),
            (
                "env -C /home/private cargo test",
                local("cargo", Some("test")),
            ),
            ("env -u hunter2 cargo test", local("cargo", Some("test"))),
            ("env -iu x Y=1 cargo test", local("cargo", Some("test"))),
            ("env --unset=x -- cargo test", local("cargo", Some("test"))),
            ("env -S 'cargo test'", unresolved.clone()),
            ("env --split-string='cargo test'", unresolved.clone()),
            ("env --bogus x cargo test", unresolved.clone()),
            (
                "nice --adjustment 10 cargo test",
                local("cargo", Some("test")),
            ),
            (
                "nice --adjustment=10 cargo test",
                local("cargo", Some("test")),
            ),
            ("nice -n 10 cargo test", local("cargo", Some("test"))),
            ("nice -n10 cargo test", local("cargo", Some("test"))),
            ("nice -10 cargo test", local("cargo", Some("test"))),
            (
                "sudo -u root -g wheel -C 5 systemctl restart x",
                local("systemctl", Some("restart")),
            ),
            (
                "sudo --user=root -H make install",
                local("make", Some("install")),
            ),
            ("sudo -uroot make install", local("make", Some("install"))),
            ("sudo -e /etc/hosts", unresolved.clone()),
            ("sudo -u", unresolved.clone()),
            (
                "bash --rcfile /home/private -c 'cargo test'",
                local("cargo", Some("test")),
            ),
            (
                "bash --init-file private -lc 'cargo test'",
                local("cargo", Some("test")),
            ),
            ("bash -o pipefail -c 'make ci'", local("make", Some("ci"))),
            ("bash -eo pipefail -c 'make ci'", local("make", Some("ci"))),
            ("bash +O extglob -c 'make ci'", local("make", Some("ci"))),
            ("bash --login -c 'make ci'", local("make", Some("ci"))),
            ("bash --bogus -c 'make ci'", unresolved.clone()),
            ("bash -c", unresolved.clone()),
            ("bash --rcfile", unresolved.clone()),
            ("bash ci.sh -c x", local("bash", Some("ci.sh"))),
            ("bash --rcfile private", local("bash", None)),
            ("sh", local("sh", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// Codex's `shell` tool reports its argv joined with spaces.
    #[test]
    fn a_joined_argv_is_read_by_its_script_or_not_at_all() {
        for (command, expected) in [
            ("/bin/sh -lc touch out.txt", local("touch", Some("out.txt"))),
            (
                "/bin/bash -lc cd x && cargo test",
                local("cargo", Some("test")),
            ),
            (
                "bash --rcfile private -c make ci",
                local("make", Some("ci")),
            ),
            ("cargo private payload", local("cargo", None)),
            ("env hunter2 cargo", local("env", None)),
            (
                "rwx sandbox exec -- cargo private",
                op(Runner::RwxSandbox, None, None),
            ),
            ("rwx run ci", op(Runner::RwxRun, Some("rwx"), Some("run"))),
            ("", op(Runner::Local, None, None)),
        ] {
            assert_eq!(classify_joined_argv(Some(command)), expected, "{command}");
        }
        assert_eq!(
            classify_tool(Some("shell"), Some("/bin/sh -lc make ci")),
            local("make", Some("ci"))
        );
        assert_eq!(
            classify_tool(Some("Bash"), Some("/bin/sh -lc make ci")),
            local("make", None)
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
            assert_eq!(classify(Some(command)), local(program, None), "{command}");
        }
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
