//! What kind of shell command an agent ran, read from the command text its
//! hook reported — for `agent.operation` spans.
//!
//! **The command line is not sent.** [`classify`] reads the text once and
//! returns two values at most: the program's basename and its first
//! argument, when no option comes before it. Each word is kept only when it has the
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
//! **Only a plain command names anything.** Shell syntax is not parsed: a
//! word read that an allow-list does not match declines the command, neither
//! program nor word. Only the first line counts, as the deck's cards show it,
//! split on `&&`, `||`, `;` and `|`. The first segment that runs something
//! other than a shell builtin that only sets up the next one
//! ([`SETUP_PROGRAMS`]) is the one classified, after its leading `NAME=value`
//! assignments and an `env`, `sudo`, `command`, `time` or `nice` in front,
//! with the options each knows and the options' values. A program that is
//! shell grammar — a reserved word, a declaration builtin, a precommand
//! modifier, `eval` or `exec` ([`UNSUPPORTED_PROGRAMS`]) — declines the
//! command. Every word read on
//! the way, up to and including the program's first non-option argument, must
//! be a simple word (`is_simple_word`) or, where an assignment can stand, a
//! simple assignment (`is_simple_assignment`). A quote, an escape, a `$`, a
//! backtick, a comment, a group, a redirection or a glob in one of them
//! declines the command, and no later segment is read. Words after the first
//! argument are not read. The one quoted word read is a shell's `-c` script:
//! a single `'…'` or `"…"` with no quote, `$`, backtick or `\` inside
//! (`unquote`), whose inside is read by this same rule.
//!
//! **[`describe`] reads on past the first command**, by the same rule applied
//! to each segment in turn: it keeps every command of the line, at most
//! [`MAX_COMMANDS`], and the command one of [`WRAPPERS`] runs. A segment that
//! declines ends the list, and so does one with a word in it that could hide
//! where the next segment starts: every word of a segment before the next one
//! is read, and must be a simple word, a simple word with a `=` in it, one of
//! [`PLAIN_REDIRECTIONS`] or a quoted string `unquote` reads (`is_plain_word`).
//! Those words are read and not kept: only each command's program and word
//! are, as [`classify`] keeps them.

/// What [`classify`] keeps of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operation {
    /// The program's basename, when it passes [`is_command_word`].
    pub program: Option<String>,
    /// Its first argument, when no option comes before it, it passes
    /// [`is_command_word`] and the program is not one of
    /// [`WORDLESS_PROGRAMS`].
    pub word: Option<String>,
}

/// What [`describe`] keeps of a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Description {
    /// The first command, as [`classify`] gives it.
    pub operation: Operation,
    /// Every command of the line in order, the first one included, each as
    /// [`classify`] would give it on its own, up to the first that declines
    /// and at most [`MAX_COMMANDS`]. Each has a program.
    pub commands: Vec<Operation>,
    /// The command the first command runs when that one is a wrapper on
    /// [`WRAPPERS`], read as a command of its own.
    pub wrapped: Operation,
}

impl Description {
    fn of(operation: Operation) -> Self {
        Description {
            commands: operation
                .program
                .is_some()
                .then(|| operation.clone())
                .into_iter()
                .collect(),
            operation,
            wrapped: Operation::default(),
        }
    }

    /// The commands as one value: each its program, or its program and word
    /// with a space between, joined with `,` — `cargo fmt,cargo xtask`.
    /// `None` when there is none. Neither a space nor a `,` is in a command
    /// word, so the value reads back unambiguously.
    pub fn commands_value(&self) -> Option<String> {
        let commands: Vec<String> = self
            .commands
            .iter()
            .filter_map(|command| {
                let program = command.program.as_deref()?;
                Some(match &command.word {
                    Some(word) => format!("{program} {word}"),
                    None => program.to_string(),
                })
            })
            .collect();
        (!commands.is_empty()).then(|| commands.join(","))
    }
}

/// The most commands [`describe`] keeps of one line.
pub const MAX_COMMANDS: usize = 8;

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
    describe_tool(tool_name, command).operation
}

/// Describe the command a shell tool call reported, read as its tool reports
/// it: a joined argv ([`JOINED_ARGV_TOOL`]) gives at most the one command
/// [`classify_joined_argv`] names, and no wrapped command; any other is read by
/// [`describe`].
pub fn describe_tool(tool_name: Option<&str>, command: Option<&str>) -> Description {
    if tool_name == Some(JOINED_ARGV_TOOL) {
        Description::of(classify_joined_argv(command))
    } else {
        describe(command)
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

/// Shell grammar the classifier does not interpret — the reserved words,
/// declaration builtins and precommand modifiers of [`SHELLS`], `eval` and
/// `exec`: a command whose program is one of them names nothing, since its
/// operand is a name, data or a command of its own. `time`, a reserved word
/// of bash and zsh, is read as the wrapper it is, and `export`, one of zsh's,
/// is skipped as a [`SETUP_PROGRAMS`] word. The punctuation words already
/// decline, as words that are not simple or not command words, and are
/// listed to keep the list whole.
pub const UNSUPPORTED_PROGRAMS: &[&str] = &[
    "eval",
    "exec",
    // POSIX sh's reserved words (Shell Command Language, 2.4), which are
    // dash's too.
    "!",
    "{",
    "}",
    "case",
    "do",
    "done",
    "elif",
    "else",
    "esac",
    "fi",
    "for",
    "if",
    "in",
    "then",
    "until",
    "while",
    // bash's: `compgen -k` and the bash manual's "Reserved Words".
    "[[",
    "]]",
    "coproc",
    "function",
    "select",
    // zsh's: the "Reserved Words" of its Shell Grammar documentation.
    "end",
    "foreach",
    "repeat",
    "nocorrect",
    "declare",
    "float",
    "integer",
    "local",
    "readonly",
    "typeset",
    // zsh's precommand modifiers (Shell Grammar, "Precommand Modifiers")
    // that are not read as prefixes; `builtin` is bash's too.
    "-",
    "noglob",
    "builtin",
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
    /// Long options that take a value: `--chdir dir`. An attached
    /// `--chdir=dir` is not a simple word, so it declines.
    long_values: &'static [&'static str],
    /// Long options that take none.
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

/// A program with no options.
const NO_OPTIONS: Options = Options {
    short_values: "",
    short_flags: "",
    long_values: &[],
    long_flags: &[],
    numbers: false,
};

/// GNU `timeout`.
const TIMEOUT: Options = Options {
    short_values: "ks",
    short_flags: "fpv",
    long_values: &["kill-after", "signal"],
    long_flags: &["foreground", "preserve-status", "verbose"],
    numbers: false,
};

/// `docker exec`.
const DOCKER_EXEC: Options = Options {
    short_values: "euw",
    short_flags: "dit",
    long_values: &["detach-keys", "env", "env-file", "user", "workdir"],
    long_flags: &["detach", "interactive", "privileged", "tty"],
    numbers: false,
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

/// The punctuation a simple word may hold besides ASCII letters and digits.
const SIMPLE_WORD_PUNCTUATION: &[u8] = b"/._+:,@%-";

/// `^[A-Za-z0-9/._+:,@%-]+$`: a word the shell reads as it is written. No
/// `$`, backtick, quote, `#`, `(`, `)`, `{`, `}`, `[`, `]`, `\`, `<`, `>`, `*`,
/// `?`, `!`, `~`, `=`, `&`, `;`, `|` or whitespace.
fn is_simple_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || SIMPLE_WORD_PUNCTUATION.contains(&b))
}

/// `NAME=value`, `NAME` matching `^[A-Za-z_][A-Za-z0-9_]*$` and `value` empty
/// or a simple word.
fn is_simple_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, value)| {
        name.bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            && (value.is_empty() || is_simple_word(value))
    })
}

/// Redirections written as one word that name no file but `/dev/null`.
pub const PLAIN_REDIRECTIONS: &[&str] = &[
    "2>&1",
    "1>&2",
    ">&2",
    ">/dev/null",
    "2>/dev/null",
    "&>/dev/null",
];

/// A word the shell ends where [`segments`] ends it, so that the segment after
/// it starts where `segments` says: a simple word, one with a `=` in it, one of
/// [`PLAIN_REDIRECTIONS`], or one quoted string `unquote` reads.
fn is_plain_word(word: &str) -> bool {
    let simple_or_equals = !word.is_empty()
        && word.bytes().all(|b| {
            b.is_ascii_alphanumeric() || SIMPLE_WORD_PUNCTUATION.contains(&b) || b == b'='
        });
    simple_or_equals || PLAIN_REDIRECTIONS.contains(&word) || unquote(word).is_some()
}

/// The inside of `word` when it is one quoted string, `'…'` or `"…"`, with no
/// quote, `$`, backtick or `\` inside: the shell reads it literally either way.
fn unquote(word: &str) -> Option<&str> {
    let quote = word.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    word[1..]
        .strip_suffix(quote)
        .filter(|inside| !inside.contains(['\'', '"', '$', '`', '\\']))
}

/// Classify the command a shell tool call reported. `None` for no text gives
/// an operation with neither program nor word.
pub fn classify(command: Option<&str>) -> Operation {
    describe(command).operation
}

/// [`classify`], and every command of the first line after the first, and the
/// command a wrapper runs: see [`Description`].
pub fn describe(command: Option<&str>) -> Description {
    let line = command.and_then(|c| c.lines().next()).unwrap_or("");
    describe_line(line)
}

/// [`classify`] for a command reported as an argv joined with single spaces,
/// as Codex's `shell` tool's is. Which of its spaces were inside an argument
/// is lost, so no word is read from it at all: only the program, and only
/// where it is certain. That is the first word of the line, or, for a shell
/// run as `<shell> -c <script>` (Codex runs its commands as
/// `["/bin/sh", "-lc", "<script>"]`), the first word of the script, each only
/// when it is a simple word. A program that only sets up, prefixes or wraps
/// the next one gives nothing, since that one's words may belong to a later
/// argument; so does a line with an empty argument in it, a shell with any
/// other options, or one of [`UNSUPPORTED_PROGRAMS`].
pub fn classify_joined_argv(command: Option<&str>) -> Operation {
    let line = command.and_then(|c| c.lines().next()).unwrap_or("");
    let words: Vec<&str> = line.split(' ').collect();
    // An empty argument, or one starting or ending with a space, leaves an
    // empty word: nothing can be placed.
    if words.iter().any(|w| w.is_empty()) {
        return Operation::default();
    }
    let program = match words.as_slice() {
        [shell, flags, script @ ..]
            if is_simple_word(shell) && SHELLS.contains(&program_name(shell)) =>
        {
            let dash_c = flags.strip_prefix('-').is_some_and(|cluster| {
                cluster.contains('c')
                    && cluster.chars().all(|c| c == 'c' || SHELL_FLAGS.contains(c))
            });
            match script {
                [program, ..] if dash_c && is_simple_word(program) => program_name(program),
                _ => return Operation::default(),
            }
        }
        [program, ..] if is_simple_word(program) => program_name(program),
        _ => return Operation::default(),
    };
    let unsure = SETUP_PROGRAMS.contains(&program)
        || SHELLS.contains(&program)
        || UNSUPPORTED_PROGRAMS.contains(&program)
        || ["env", "sudo", "command", "time", "nice"].contains(&program);
    if unsure {
        return Operation::default();
    }
    named(program, &[])
}

fn describe_line(line: &str) -> Description {
    let mut out = Description::default();
    for segment in segments(line) {
        let Some(read) = read_segment(&segment) else {
            continue;
        };
        if out.commands.is_empty() {
            out.operation = read.operation;
            out.wrapped = read.wrapped;
        }
        if read.commands.is_empty() {
            break;
        }
        let room = MAX_COMMANDS - out.commands.len();
        out.commands.extend(read.commands.into_iter().take(room));
        // The next segment starts where this one's last word ends only when
        // the shell ends each of its words where `segments` does.
        if out.commands.len() == MAX_COMMANDS || !segment.iter().all(|w| is_plain_word(w)) {
            break;
        }
    }
    out
}

/// Read one segment's words. `None` when the segment only sets up the next
/// one; a description with no command when it declines.
fn read_segment(segment: &[&str]) -> Option<Description> {
    let Some(words) = command_words(segment) else {
        return Some(Description::default());
    };
    let program = program_name(words.first()?);
    let rest = &words[1..];
    if UNSUPPORTED_PROGRAMS.contains(&program) {
        return Some(Description::default());
    }
    if SETUP_PROGRAMS.contains(&program) {
        // All of its words are read: a quote or a substitution in one could
        // hide where the next segment starts.
        let plain = rest
            .iter()
            .all(|w| is_simple_word(w) || is_simple_assignment(w));
        return (!plain).then(Description::default);
    }
    if SHELLS.contains(&program) {
        return Some(match shell_operand(rest) {
            None => Description::default(),
            Some(ShellInput::File(at)) => Description::of(named(program, &rest[at..])),
            Some(ShellInput::Stdin) => Description::of(named(program, &[])),
            // A quoted script holds no quote, so a script inside it is one
            // simple word: the nesting ends there.
            Some(ShellInput::Command(at)) => match rest.get(at).and_then(|&script| {
                Some(script)
                    .filter(|s| is_simple_word(s))
                    .or_else(|| unquote(script))
            }) {
                Some(script) => describe_line(script),
                None => Description::default(),
            },
        });
    }
    let mut description = Description::of(named(program, rest));
    if !description.commands.is_empty() {
        description.wrapped = wrapped_start(program, rest)
            .and_then(|at| read_segment(&rest[at..]))
            .map(|wrapped| wrapped.operation)
            .unwrap_or_default();
    }
    Some(description)
}

/// Where a wrapper on [`WRAPPERS`] starts the command it runs.
enum WrappedAt {
    /// After its options, as these read them, and this many operands:
    /// `timeout 600 cargo test`, `docker exec box cargo test`.
    Operands(&'static Options, usize),
    /// After the first of these words, `--` in `kubectl exec pod -- make`.
    /// Every word before it must be an option when `operands` is false.
    After {
        markers: &'static [&'static str],
        operands: bool,
    },
}

/// A program that runs a command given as its arguments, and where that
/// command starts, so the span can say what it ran.
struct Wrapper {
    program: &'static str,
    /// The first argument, for a wrapper that is one of a program's
    /// subcommands.
    subcommand: Option<&'static str>,
    at: WrappedAt,
}

/// The wrappers whose command [`describe`] reads. Each is listed only where
/// the command's place in the arguments is certain; a wrapper not listed is
/// described as itself only.
const WRAPPERS: &[Wrapper] = &[
    Wrapper {
        program: "timeout",
        subcommand: None,
        at: WrappedAt::Operands(&TIMEOUT, 1),
    },
    Wrapper {
        program: "nohup",
        subcommand: None,
        at: WrappedAt::Operands(&NO_OPTIONS, 0),
    },
    Wrapper {
        program: "docker",
        subcommand: Some("exec"),
        at: WrappedAt::Operands(&DOCKER_EXEC, 1),
    },
    Wrapper {
        program: "kubectl",
        subcommand: Some("exec"),
        at: WrappedAt::After {
            markers: &["--"],
            operands: true,
        },
    },
    Wrapper {
        program: "nix",
        subcommand: Some("develop"),
        at: WrappedAt::After {
            markers: &["-c", "--command"],
            operands: true,
        },
    },
    Wrapper {
        program: "nix",
        subcommand: Some("shell"),
        at: WrappedAt::After {
            markers: &["-c", "--command"],
            operands: true,
        },
    },
    // `devbox run <script> -- <args>` passes `<args>` to a script, so only a
    // `--` with nothing but options before it starts a command.
    Wrapper {
        program: "devbox",
        subcommand: Some("run"),
        at: WrappedAt::After {
            markers: &["--"],
            operands: false,
        },
    },
];

/// Where in `args` the command `program` wraps starts. `None` when `program`
/// is not on [`WRAPPERS`], a word before that command is not simple or not
/// one the wrapper is known to take, or there is no command.
fn wrapped_start(program: &str, args: &[&str]) -> Option<usize> {
    let wrapper = WRAPPERS.iter().find(|w| {
        w.program == program
            && w.subcommand
                .is_none_or(|subcommand| args.first() == Some(&subcommand))
    })?;
    let skip = usize::from(wrapper.subcommand.is_some());
    let args_after = &args[skip..];
    let at = match &wrapper.at {
        WrappedAt::Operands(options, count) => {
            let len = options_len(args_after, options)?;
            let operands = args_after.get(len..len + count)?;
            if !operands
                .iter()
                .all(|w| is_simple_word(w) && !w.starts_with('-'))
            {
                return None;
            }
            len + count
        }
        WrappedAt::After { markers, operands } => {
            let at = args_after.iter().position(|w| markers.contains(w))?;
            if !args_after[..at]
                .iter()
                .all(|w| is_simple_word(w) && (*operands || w.starts_with('-')))
            {
                return None;
            }
            at + 1
        }
    };
    let start = skip + at;
    (start < args.len()).then_some(start)
}

/// An operation running `program` with `args`, its words filtered. No
/// word is kept for a program that is not kept: what it is cannot be told.
/// The arguments are read up to the first that is not an option, each a
/// simple word, or the command declines — for one of [`WORDLESS_PROGRAMS`]
/// too, though its word is then dropped. The word is kept only when it is
/// the first argument: an option before it may take it as its value
/// (`sshpass -p <password>`), and which options do is not known.
fn named(program: &str, args: &[&str]) -> Operation {
    if !is_command_word(program) {
        return Operation::default();
    }
    let mut word = None;
    for (at, arg) in args.iter().enumerate() {
        if !is_simple_word(arg) {
            return Operation::default();
        }
        if !arg.starts_with('-') && !arg.starts_with('+') {
            word = Some(arg.to_string())
                .filter(|w| at == 0 && is_command_word(w) && !WORDLESS_PROGRAMS.contains(&program));
            break;
        }
    }
    Operation {
        program: Some(program.to_string()),
        word,
    }
}

/// The basename of the program a simple word runs.
fn program_name(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// A segment's words from its program on: leading assignments and the prefix
/// programs in front of it, with their options, the options' values and
/// assignments, dropped. `None` when one of those words is not simple, or a
/// prefix's options cannot be told apart from the command it runs.
fn command_words<'a>(segment: &'a [&'a str]) -> Option<&'a [&'a str]> {
    let mut words = segment;
    loop {
        let assignments = words.iter().take_while(|w| is_simple_assignment(w)).count();
        words = &words[assignments..];
        let Some(first) = words.first() else {
            return Some(words);
        };
        if !is_simple_word(first) {
            return None;
        }
        let prefix = program_name(first);
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
/// they run (past a `--`). `None` when an option is not a simple word or not
/// one `options` knows, or its value is missing or not a simple word.
fn options_len(words: &[&str], options: &Options) -> Option<usize> {
    let mut i = 0;
    while let Some(&word) = words.get(i) {
        // The command, or an assignment in front of it, is read by the
        // caller.
        if !word.starts_with('-') || word == "-" {
            break;
        }
        if !is_simple_word(word) {
            return None;
        }
        if word == "--" {
            return Some(i + 1);
        }
        let takes_value = if let Some(long) = word.strip_prefix("--") {
            if options.long_values.contains(&long) {
                true
            } else if options.long_flags.contains(&long) {
                false
            } else {
                return None;
            }
        } else {
            let cluster = &word[1..];
            if options.numbers && cluster.bytes().all(|b| b.is_ascii_digit()) {
                false
            } else {
                let mut takes_next = false;
                for (at, c) in cluster.char_indices() {
                    if options.short_values.contains(c) {
                        // A value attached to the option is the rest of this
                        // word.
                        takes_next = at + c.len_utf8() == cluster.len();
                        break;
                    }
                    if !options.short_flags.contains(c) {
                        return None;
                    }
                }
                takes_next
            }
        };
        if takes_value && !words.get(i + 1).is_some_and(|v| is_simple_word(v)) {
            return None;
        }
        i += if takes_value { 2 } else { 1 };
    }
    Some(i)
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
/// commands it runs. `None` when an option is not a simple word or not one a
/// shell has, or its value is missing or not a simple word.
fn shell_operand(args: &[&str]) -> Option<ShellInput> {
    let mut command = false;
    let mut stdin = false;
    let mut i = 0;
    while let Some(&word) = args.get(i) {
        // The operand is read by the caller: a `-c` script may be quoted.
        if !word.starts_with(['-', '+']) {
            break;
        }
        if !is_simple_word(word) {
            return None;
        }
        if word == "--" || word == "-" {
            i += 1;
            break;
        }
        let values = if let Some(long) = word.strip_prefix("--") {
            match long {
                "rcfile" | "init-file" => 1,
                "login" | "norc" | "noprofile" | "noediting" | "posix" | "restricted"
                | "verbose" | "debugger" | "dump-strings" | "dump-po-strings" | "help"
                | "version" | "pretty-print" => 0,
                _ => return None,
            }
        } else {
            let (minus, cluster) = match word.strip_prefix('-') {
                Some(cluster) => (true, cluster),
                None => (false, &word[1..]),
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
            values
        };
        if !(1..=values).all(|at| args.get(i + at).is_some_and(|v| is_simple_word(v))) {
            return None;
        }
        i += 1 + values;
    }
    Some(if command {
        ShellInput::Command(i)
    } else if stdin || i == args.len() {
        ShellInput::Stdin
    } else {
        ShellInput::File(i)
    })
}

/// `line` split into segments on `&&`, `||`, `;` and `|`, each a list of its
/// words as written, split on spaces and tabs. Nothing else in the shell's syntax is read, quotes
/// included, but for one case: a word starting with a quote runs on to the
/// matching quote, so a quoted `-c` script stays one word for `unquote`. Every
/// word read before the one classified is a simple word, with no quote in it,
/// so the split is the shell's up to there.
fn segments(line: &str) -> Vec<Vec<&str>> {
    let mut out = vec![Vec::new()];
    let mut rest = line;
    loop {
        rest = rest.trim_start_matches(BLANKS);
        let Some(first) = rest.chars().next() else {
            break;
        };
        let separator = if rest.starts_with("&&") || rest.starts_with("||") {
            2
        } else if matches!(first, ';' | '|') {
            1
        } else {
            0
        };
        if separator > 0 {
            out.push(Vec::new());
            rest = &rest[separator..];
            continue;
        }
        let quoted = match first {
            '\'' | '"' => rest[1..].find(first).map_or(0, |at| at + 2),
            _ => 0,
        };
        let end = quoted + word_len(&rest[quoted..]);
        out.last_mut().expect("never empty").push(&rest[..end]);
        rest = &rest[end..];
    }
    out.retain(|segment| !segment.is_empty());
    out
}

/// What ends a word for the shell on one line. Any other whitespace is part
/// of the word, and makes it not simple.
const BLANKS: [char; 2] = [' ', '\t'];

/// How long the word `text` starts with is, up to a blank or a separator.
fn word_len(text: &str) -> usize {
    text.char_indices()
        .find(|&(at, c)| {
            BLANKS.contains(&c) || matches!(c, ';' | '|') || text[at..].starts_with("&&")
        })
        .map_or(text.len(), |(at, _)| at)
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
            ("git -C repo status", expected("git", None)),
            ("cargo +nightly fmt", expected("cargo", None)),
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

    /// An option before the first argument can take that argument as its
    /// value — a password, a user name, a directory — and which options do
    /// is not known for most programs: a word is sent only when it comes
    /// straight after the program.
    #[test]
    fn no_word_after_an_option() {
        for (command, expected) in [
            ("sshpass -p x ssh host", expected("sshpass", None)),
            ("mysql -u admin db", expected("mysql", None)),
            ("psql -U admin", expected("psql", None)),
            ("git -C dir status", expected("git", None)),
            ("kubectl -n ns get pods", expected("kubectl", None)),
            ("cargo +nightly fmt", expected("cargo", None)),
            ("bash -c 'sshpass -p x ssh host'", expected("sshpass", None)),
            ("sudo -u root mysql -u admin db", expected("mysql", None)),
            ("cargo test --workspace", expected("cargo", Some("test"))),
            ("npm run build", expected("npm", Some("run"))),
            ("gh pr checks 1814", expected("gh", Some("pr"))),
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
            ("command -v rg", expected("command", None)),
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

    /// A quoted argument before the word declines the command, a shell's
    /// later arguments are not its script, and a quoted script with a quote in
    /// it is not read.
    #[test]
    fn word_boundaries_survive_unwrapping() {
        let bare = Operation::default();
        for (command, expected) in [
            ("cargo 'private payload'", bare.clone()),
            ("env cargo 'abc;node secret'", bare.clone()),
            ("sudo -u root cargo \"a && b\"", bare.clone()),
            (r#"env bash -c 'node "private payload"'"#, bare.clone()),
            (
                "FOO=1 env -u X sudo cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "bash -c 'cargo test' private payload",
                expected("cargo", Some("test")),
            ),
            ("bash -c cargo private", expected("cargo", None)),
            ("sh -c 'node \"private payload\"' x", bare.clone()),
            (r#"sh -c 'sh -c "make ci"'"#, bare.clone()),
            ("sh -c 'sh -c make ci'", expected("make", None)),
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
            // An attached `=value` is not a simple word.
            ("time --format=%e cargo test", unresolved.clone()),
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
                "env --unset x -- cargo test",
                expected("cargo", Some("test")),
            ),
            ("env --unset=x -- cargo test", unresolved.clone()),
            ("env -S 'cargo test'", unresolved.clone()),
            ("env --split-string='cargo test'", unresolved.clone()),
            ("env --bogus x cargo test", unresolved.clone()),
            (
                "nice --adjustment 10 cargo test",
                expected("cargo", Some("test")),
            ),
            ("nice --adjustment=10 cargo test", unresolved.clone()),
            ("nice -n 10 cargo test", expected("cargo", Some("test"))),
            ("nice -n10 cargo test", expected("cargo", Some("test"))),
            ("nice -10 cargo test", expected("cargo", Some("test"))),
            (
                "sudo -u root -g wheel -C 5 systemctl restart x",
                expected("systemctl", Some("restart")),
            ),
            (
                "sudo --user root -H make install",
                expected("make", Some("install")),
            ),
            ("sudo --user=root -H make install", unresolved.clone()),
            (
                "sudo -u '/x/sentinelvalue' make install",
                unresolved.clone(),
            ),
            ("bash -o 'x y' -c 'make ci'", unresolved.clone()),
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

    /// An assignment's value can hold a `/`: the part after it is the value's,
    /// not a program's, so a word with a `=` in it names no program however it
    /// is read. A program's own path still gives its basename.
    #[test]
    fn an_assignment_value_is_never_the_program() {
        let bare = Operation::default();
        for (command, expected) in [
            ("WORKDIR=/private/sentinelvalue cargo test", bare.clone()),
            (
                "/bin/sh -lc WORKDIR=/private/sentinelvalue cargo test",
                bare.clone(),
            ),
            ("/bin/sh -lc WORKDIR=/private/sh -c cargo", bare.clone()),
            ("1WORKDIR=/private/sentinelvalue cargo test", bare.clone()),
            ("X=/private/sh -lc cargo test", bare.clone()),
            ("/usr/bin/cargo test", expected("cargo", None)),
            ("/bin/sh -lc /usr/bin/cargo test", expected("cargo", None)),
        ] {
            assert_eq!(classify_joined_argv(Some(command)), expected, "{command}");
            assert_eq!(
                classify_tool(Some("shell"), Some(command)),
                expected,
                "{command}"
            );
        }
        for (command, expected) in [
            (
                "WORKDIR=/private/sentinelvalue cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "/bin/sh -lc 'WORKDIR=/private/sentinelvalue cargo test'",
                expected("cargo", Some("test")),
            ),
            ("1WORKDIR=/private/sentinelvalue cargo test", bare.clone()),
            (
                "/bin/sh -lc '1WORKDIR=/private/sentinelvalue cargo test'",
                bare.clone(),
            ),
            ("env 1X=/private/sentinelvalue cargo test", bare.clone()),
            ("1X=/private/env cargo test", bare.clone()),
            ("/usr/bin/cargo test", expected("cargo", Some("test"))),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// Shell syntax other than plain words before the word — a comment, a
    /// substitution, a quote, a group, an escape, a redirection — names
    /// nothing, however much of the command a parser could have recovered.
    #[test]
    fn shell_syntax_before_the_word_names_nothing() {
        let bare = Operation::default();
        for command in [
            "#/private/sentinelvalue",
            "#/private/sentinelvalue cargo test",
            "cd x && #/private/sentinelvalue",
            "X=$(printf /x/sentinelvalue ) cargo test",
            "X=$(cat /x/sentinelvalue ) cargo test",
            "X=`cat /x/sentinelvalue` cargo test",
            "`cat /x/sentinelvalue` cargo test",
            "$(cat /x/sentinelvalue) test",
            "$VAR/x/sentinelvalue cargo",
            "${VAR}/x/sentinelvalue cargo",
            "X=\"/x/sentinelvalue y\" cargo",
            "X='/x/sentinelvalue y' cargo",
            "{ cargo; }",
            "(cd x; cargo test)",
            "<<EOF /x/sentinelvalue",
            "python3 - <<EOF",
            r"\cargo test",
            "<(cat /x/sentinelvalue) cargo",
            "diff <(cat /x/sentinelvalue) b",
            "cd \"$DIR\" && cargo test",
            "cd x; make '#'; hunter2",
            "! cargo test",
            "~/bin/sentinelvalue run",
            "/x/sentinel*value run",
            "bash -c 'X=$(cat /x/sentinelvalue ) cargo test'",
            "bash -c \"cd $DIR && cargo test\"",
            "bash -c 'cargo test'x",
            // Only a space or a tab ends a word for the shell.
            "X=a\u{a0}/x/sentinelvalue cargo test",
            "X=a\u{2003}/x/sentinelvalue cargo test",
            "X=a\u{b}/x/sentinelvalue cargo test",
        ] {
            assert_eq!(classify(Some(command)), bare, "{command}");
        }
        for command in [
            "/bin/sh -lc #/private/sentinelvalue",
            "/bin/sh -lc #note/private/sentinelvalue",
            "/bin/sh -lc #/private/sentinelvalue cargo test",
            "#/private/sentinelvalue",
            "/bin/sh -lc $(cat /x/sentinelvalue)",
            "/bin/sh -lc `/x/sentinelvalue`",
            "/bin/sh -lc $VAR/x/sentinelvalue",
            "/bin/sh -lc \\/x/sentinelvalue",
            "$VAR/x/sentinelvalue cargo",
            "'/bin/sh' -lc cargo",
        ] {
            assert_eq!(
                classify_tool(Some("shell"), Some(command)),
                bare,
                "{command}"
            );
        }
        // A program whose word is never sent is read up to its first
        // argument all the same.
        for command in [
            "cat <<EOF > /x/sentinelvalue",
            "cat <(cat /x/sentinelvalue) | cargo test",
            "grep --include=*.rs sentinelvalue",
            "echo \"$(cat /x/sentinelvalue)\"",
        ] {
            assert_eq!(classify(Some(command)), bare, "{command}");
        }
        // After the word nothing is read.
        for (command, expected) in [
            (
                "cargo test $(cat /x/sentinelvalue)",
                expected("cargo", Some("test")),
            ),
            ("cat notes.txt > /x/sentinelvalue", expected("cat", None)),
            ("grep -r sentinelvalue \"$X\"", expected("grep", None)),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// A shell reserved word, `eval` or `exec` is shell grammar rather than a
    /// program: its operand is data or a command of its own, so the command
    /// names nothing, however it is reached.
    #[test]
    fn shell_keywords_name_nothing() {
        let bare = Operation::default();
        for keyword in UNSUPPORTED_PROGRAMS {
            for command in [
                format!("{keyword} sentinelvalue"),
                format!("sudo {keyword} sentinelvalue"),
                format!("env {keyword} sentinelvalue"),
                format!("FOO=1 time {keyword} sentinelvalue"),
                format!("cd x && {keyword} sentinelvalue"),
                format!("bash -c '{keyword} sentinelvalue'"),
                format!("sh -lc \"{keyword} sentinelvalue\""),
                format!("bash -c {keyword}"),
            ] {
                assert_eq!(classify(Some(&command)), bare, "{command}");
            }
            for command in [
                format!("{keyword} sentinelvalue"),
                format!("/bin/sh -lc {keyword} sentinelvalue"),
            ] {
                assert_eq!(
                    classify_tool(Some("shell"), Some(&command)),
                    bare,
                    "{command}"
                );
            }
        }
        for command in [
            "case sentinelvalue in a) true;; esac",
            "eval sentinelvalue",
            "exec sentinelvalue",
            "sudo eval sentinelvalue",
            "env case sentinelvalue in a) true;; esac",
            "bash -c 'case sentinelvalue in a) true;; esac'",
            "if test sentinelvalue; then cargo test; fi",
            "for x in sentinelvalue; do cargo test; done",
            "while sentinelvalue; do cargo test; done",
            "function sentinelvalue",
        ] {
            assert_eq!(classify(Some(command)), bare, "{command}");
        }
        // `time` is a reserved word too, and the wrapper the classifier reads.
        assert_eq!(
            classify(Some("time cargo build")),
            expected("cargo", Some("build"))
        );
        // Looked up, not run: the program is `command`, with no word after
        // its option.
        assert_eq!(classify(Some("command -v case")), expected("command", None));
    }

    /// Every reserved word and declaration or precommand word of the shells
    /// the classifier unwraps names nothing. The words are listed here, not
    /// read from [`UNSUPPORTED_PROGRAMS`], so a word missing from that list
    /// fails this test.
    #[test]
    fn shell_reserved_words_name_nothing() {
        // POSIX sh and dash.
        let posix = [
            "case", "do", "done", "elif", "else", "esac", "fi", "for", "if", "in", "then", "until",
            "while",
        ];
        // bash's `compgen -k` and the bash manual's reserved words.
        let bash = ["coproc", "function", "select"];
        // zsh's Shell Grammar "Reserved Words", its declaration builtins and
        // precommand modifiers.
        let zsh = [
            "end",
            "foreach",
            "repeat",
            "nocorrect",
            "noglob",
            "builtin",
            "declare",
            "typeset",
            "local",
            "readonly",
            "integer",
            "float",
        ];
        let bare = Operation::default();
        for word in posix.iter().chain(&bash).chain(&zsh) {
            for command in [
                format!("{word} sentinelvalue"),
                format!("time {word} sentinelvalue"),
                format!("sudo {word} sentinelvalue"),
                format!("cd repo && {word} sentinelvalue"),
                format!("zsh -c '{word} sentinelvalue'"),
                format!("bash -c '{word} sentinelvalue'"),
            ] {
                assert_eq!(classify(Some(&command)), bare, "{command}");
            }
            for command in [
                format!("{word} sentinelvalue"),
                format!("/bin/zsh -lc {word} sentinelvalue"),
            ] {
                assert_eq!(
                    classify_tool(Some("shell"), Some(&command)),
                    bare,
                    "{command}"
                );
            }
        }
        for command in [
            "zsh -c 'foreach sentinelvalue (a b); true; end'",
            "zsh -c 'repeat sentinelvalue do true; done'",
            "time foreach sentinelvalue (a b); true; end",
            "cd repo && foreach sentinelvalue (a b); true; end",
            "integer sentinelvalue",
            "float sentinelvalue",
        ] {
            assert_eq!(classify(Some(command)), bare, "{command}");
        }
        assert_eq!(
            classify_tool(
                Some("shell"),
                Some("/bin/zsh -lc foreach sentinelvalue (a b); true; end")
            ),
            bare
        );
        // `time` is the wrapper the classifier reads, and `command -v` looks
        // a word up rather than running it.
        assert_eq!(
            classify(Some("time cargo build")),
            expected("cargo", Some("build"))
        );
        assert_eq!(classify(Some("command -v case")), expected("command", None));
    }

    #[test]
    fn plain_commands_still_classify() {
        for (command, expected) in [
            ("cargo test --workspace", expected("cargo", Some("test"))),
            (
                "cargo test 2>&1 | tail -50",
                expected("cargo", Some("test")),
            ),
            ("npm run build", expected("npm", Some("run"))),
            ("gh pr checks 1814", expected("gh", Some("pr"))),
            ("FOO=1 cargo test", expected("cargo", Some("test"))),
            ("RUST_LOG=debug cargo run", expected("cargo", Some("run"))),
            ("FOO= cargo test", expected("cargo", Some("test"))),
            ("cd repo && cargo test", expected("cargo", Some("test"))),
            ("cd repo; cargo test", expected("cargo", Some("test"))),
            ("sudo -u root apt install", expected("apt", Some("install"))),
            ("time cargo build", expected("cargo", Some("build"))),
            ("/usr/bin/cargo test", expected("cargo", Some("test"))),
            ("./scripts/check.sh", expected("check.sh", None)),
            (
                "rwx sandbox exec -- cargo test",
                expected("rwx", Some("sandbox")),
            ),
            ("bash -c 'cargo test'", expected("cargo", Some("test"))),
            ("bash -c \"cargo test\"", expected("cargo", Some("test"))),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
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
            ("node -e 'console.log(1)'", Operation::default()),
            ("tool TOKEN=abc", Operation::default()),
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
            ("curl -u hunter2 https://example.invalid", "curl"),
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

    /// A quote declines the command when it comes before the word, and is
    /// not read when it comes after it.
    #[test]
    fn a_quote_before_the_word_names_nothing() {
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

    /// A shell comment is not a command: a `#` up to the first argument
    /// declines the command, so nothing after it — a separator included — gives
    /// a program or a word.
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
            ("cargo # hunter2", bare.clone()),
            ("cd x && make # ci; hunter2", bare.clone()),
            ("bash -c 'cd x # y; hunter2'", bare.clone()),
            // A `#` anywhere in a word up to the first argument declines,
            // quoted, escaped or not.
            ("cd x; make ci#1", bare.clone()),
            (r"cd x; make \#; hunter2", bare.clone()),
            ("cd x; make issue#1; hunter2 run", bare.clone()),
            // After the first argument, nothing is read.
            (
                r#"git log --format="%H #%s" | head"#,
                expected("git", Some("log")),
            ),
        ] {
            assert_eq!(classify(Some(command)), expected, "{command}");
        }
    }

    /// Segments split on the separators alone, except inside a word that
    /// starts with a quote.
    #[test]
    fn segments_split_on_separators_outside_a_leading_quote() {
        assert_eq!(
            segments("cd x;make issue#1&&hunter2 run 2>&1||y|z"),
            [
                vec!["cd", "x"],
                vec!["make", "issue#1"],
                vec!["hunter2", "run", "2>&1"],
                vec!["y"],
                vec!["z"]
            ]
        );
        assert_eq!(
            segments("bash -c 'a; b' && c \"d | e\"f; g"),
            [
                vec!["bash", "-c", "'a; b'"],
                vec!["c", "\"d | e\"f"],
                vec!["g"]
            ]
        );
        assert_eq!(segments("a 'b c"), [vec!["a", "'b", "c"]]);
        assert_eq!(segments(" ; "), Vec::<Vec<&str>>::new());
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
        ] {
            assert_eq!(classify(Some(command)), expected(shell, None), "{command}");
        }
        // A redirection before the word is not a plain command.
        assert_eq!(classify(Some("bash < script.sh")), Operation::default());
        assert_eq!(
            classify(Some("bash ci.sh hunter2")),
            expected("bash", Some("ci.sh"))
        );
        assert_eq!(
            classify(Some("bash -s -c 'make ci'")),
            expected("make", Some("ci"))
        );
    }

    fn commands(command: &str) -> Option<String> {
        describe(Some(command)).commands_value()
    }

    fn wrapped(command: &str) -> Operation {
        describe(Some(command)).wrapped
    }

    /// Every command of a chained line is described, in order, each by the
    /// rule the first one is; the first stays `program` and `word`.
    #[test]
    fn a_chain_describes_each_command() {
        for (command, expected) in [
            (
                "cargo fmt && cargo xtask affected-checks --run",
                "cargo fmt,cargo xtask",
            ),
            ("cd repo && cargo fmt && cargo test", "cargo fmt,cargo test"),
            ("cargo fmt && cd x && cargo test", "cargo fmt,cargo test"),
            ("cargo test 2>&1 | tail -n 5", "cargo test,tail"),
            ("cargo test >/dev/null && make ci", "cargo test,make ci"),
            (
                "cargo build; npm run lint || make ci",
                "cargo build,npm run,make ci",
            ),
            ("grep -r hunter2 src | wc -l", "grep,wc"),
            ("cargo test --features=e2e && make ci", "cargo test,make ci"),
            // A quoted string after the word is read, not kept, and the
            // separator inside it is not one.
            (
                "git commit -m 'Fix it; hunter2 run' && git push",
                "git commit,git push",
            ),
            (
                "cargo fmt && echo done \"a && hunter2 run\" && make ci",
                "cargo fmt,echo,make ci",
            ),
            (
                "bash -c 'cargo fmt && cargo test' && make ci",
                "cargo fmt,cargo test,make ci",
            ),
            ("cargo test", "cargo test"),
            ("ls", "ls"),
        ] {
            assert_eq!(commands(command).as_deref(), Some(expected), "{command}");
        }
        let chain = describe(Some("cargo fmt && cargo xtask affected-checks --run"));
        assert_eq!(chain.operation, expected("cargo", Some("fmt")));
        assert_eq!(
            chain.commands,
            [
                expected("cargo", Some("fmt")),
                expected("cargo", Some("xtask"))
            ]
        );
    }

    /// A segment that declines ends the list, and so does a word that could
    /// hide where the next segment starts: nothing after either is read.
    #[test]
    fn a_declining_segment_ends_the_chain() {
        for (command, expected) in [
            ("cargo fmt && 'my tool' run && make ci", Some("cargo fmt")),
            (
                "cargo fmt && X=$(cat /x/sentinelvalue) cargo test && make ci",
                Some("cargo fmt"),
            ),
            (
                "cargo fmt && eval sentinelvalue && make ci",
                Some("cargo fmt"),
            ),
            ("cargo fmt && Rscript x && make ci", Some("cargo fmt")),
            ("cargo fmt && cd \"$DIR\" && hunter2 run", Some("cargo fmt")),
            (
                "cargo fmt && cargo test $X && hunter2 run",
                Some("cargo fmt,cargo test"),
            ),
            // The quote opens in a word after the first argument: the shell
            // reads `; hunter2 run; y` as part of one argument.
            ("make ci x'; hunter2 run; y'", Some("make ci")),
            ("cargo fmt # note && hunter2 run", Some("cargo fmt")),
            ("cargo build & hunter2 run", Some("cargo build")),
            ("cargo fmt >| out; hunter2 run", Some("cargo fmt")),
            ("cargo fmt > out.txt && hunter2 run", Some("cargo fmt")),
            ("cargo fmt |& hunter2 run", Some("cargo fmt")),
            ("cargo fmt 2>err.log && hunter2 run", Some("cargo fmt")),
            ("echo done \"it's\" && hunter2 run", Some("echo")),
            // A quoted first argument declines, as it does for `classify`.
            ("echo \"a\" && hunter2 run", None),
            (
                "cargo fmt && bash -c 'cd x' && hunter2 run",
                Some("cargo fmt"),
            ),
            ("cargo fmt && \\", Some("cargo fmt")),
            ("cd 'my dir' && cargo test", None),
            ("'my tool' run && cargo test", None),
        ] {
            assert_eq!(commands(command).as_deref(), expected, "{command}");
            assert!(
                !commands(command).unwrap_or_default().contains("hunter2"),
                "{command}"
            );
        }
    }

    #[test]
    fn a_chain_keeps_at_most_max_commands() {
        let line = (0..MAX_COMMANDS + 3)
            .map(|i| format!("make t{i}"))
            .collect::<Vec<_>>()
            .join(" && ");
        let described = describe(Some(&line));
        assert_eq!(described.commands.len(), MAX_COMMANDS);
        assert_eq!(
            described.commands[MAX_COMMANDS - 1],
            expected("make", Some("t7"))
        );
        let script = format!("bash -c '{line}' && make last");
        assert_eq!(describe(Some(&script)).commands.len(), MAX_COMMANDS);
    }

    /// A wrapper on the list gives the command it runs, read as a command of
    /// its own; the program and word stay the wrapper's.
    #[test]
    fn a_wrapper_gives_the_command_it_runs() {
        for (command, expected) in [
            ("timeout 600 cargo test", expected("cargo", Some("test"))),
            (
                "timeout -s KILL -k 5 10m cargo build",
                expected("cargo", Some("build")),
            ),
            (
                "timeout --preserve-status 10 make ci",
                expected("make", Some("ci")),
            ),
            (
                "nohup cargo build --release",
                expected("cargo", Some("build")),
            ),
            (
                "docker exec -it box cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "docker exec -u root -w /app box make ci",
                expected("make", Some("ci")),
            ),
            (
                "kubectl exec -it pod -c app -- sh -c 'make ci'",
                expected("make", Some("ci")),
            ),
            (
                "kubectl exec pod -- cat /x/sentinelvalue",
                expected("cat", None),
            ),
            ("nix develop -c cargo test", expected("cargo", Some("test"))),
            (
                "nix develop --command make ci",
                expected("make", Some("ci")),
            ),
            ("nix shell -c make ci", expected("make", Some("ci"))),
            ("devbox run -- cargo test", expected("cargo", Some("test"))),
            ("devbox run --pure -- make ci", expected("make", Some("ci"))),
            (
                "timeout 5 env FOO=1 cargo test",
                expected("cargo", Some("test")),
            ),
            (
                "timeout 5 bash -c 'cargo test'",
                expected("cargo", Some("test")),
            ),
            ("timeout 5 grep hunter2 x", expected("grep", None)),
            (
                "cd x && sudo timeout 5 cargo test",
                expected("cargo", Some("test")),
            ),
        ] {
            assert_eq!(wrapped(command), expected, "{command}");
        }
        let described = describe(Some("timeout 600 cargo test"));
        assert_eq!(described.operation, expected("timeout", Some("600")));
        assert_eq!(described.commands, [expected("timeout", Some("600"))]);
        assert_eq!(
            describe(Some("docker exec -it box cargo test")).operation,
            expected("docker", Some("exec"))
        );
    }

    /// A wrapper not on the list, an option or operand it is not known to
    /// take, or a wrapped command that declines gives no wrapped command.
    #[test]
    fn an_unknown_or_unclear_wrapper_gives_no_wrapped_command() {
        for command in [
            "rwx sandbox exec -- cargo test",
            "cargo run -- sentinelvalue",
            "npm run test -- sentinelvalue",
            "timeout --signal=KILL 10 cargo test",
            "timeout --bogus 10 cargo test",
            "timeout 5",
            "timeout -k",
            "docker exec -e FOO=1 box cargo test",
            "docker exec --bogus box cargo test",
            "docker run box cargo test",
            "kubectl exec pod",
            "kubectl exec 'my pod' -- make ci",
            "kubectl exec pod --",
            "kubectl -n ns exec pod -- make ci",
            "nix develop .#ci -c cargo test",
            "nix build -c cargo test",
            "devbox run test -- sentinelvalue",
            "devbox run -- ",
            "timeout 5 'my tool' run",
            "timeout 5 eval sentinelvalue",
            "timeout 5 cd x",
            "timeout 5 X=$(cat /x/sentinelvalue) cargo",
            // Only the first command's wrapper is read.
            "cargo fmt && timeout 5 cargo test",
        ] {
            assert_eq!(wrapped(command), Operation::default(), "{command}");
        }
    }

    /// A joined argv names one program at most, so it has no chain and no
    /// wrapped command.
    #[test]
    fn a_joined_argv_describes_its_program_only() {
        for (command, program) in [
            ("/bin/sh -lc cargo fmt && cargo test", Some("cargo")),
            ("timeout 5 cargo test", Some("timeout")),
            ("/bin/sh -lc cd x && cargo test", None),
        ] {
            let described = describe_tool(Some(JOINED_ARGV_TOOL), Some(command));
            assert_eq!(described.operation, op(program, None), "{command}");
            assert_eq!(described.commands_value().as_deref(), program, "{command}");
            assert_eq!(described.wrapped, Operation::default(), "{command}");
        }
        assert_eq!(
            describe_tool(Some("Bash"), Some("cargo fmt && cargo test")).commands_value(),
            Some("cargo fmt,cargo test".to_string())
        );
    }

    #[test]
    fn commands_value_joins_programs_and_words() {
        assert_eq!(describe(None).commands_value(), None);
        assert_eq!(describe(Some("cd x")).commands_value(), None);
        assert_eq!(
            Description {
                commands: vec![expected("cargo", Some("fmt")), expected("tail", None)],
                ..Description::default()
            }
            .commands_value()
            .as_deref(),
            Some("cargo fmt,tail")
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
