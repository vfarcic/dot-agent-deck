# Telemetry internals

How the deck's OpenTelemetry export (PRD #1741) decides two things the user page, [`docs/telemetry.md`](../telemetry.md), describes only by their result: what an `agent.operation` span says about a shell command, and which `codex` or `devin` the `PATH` shim adds to.

## From a command line to `deck.operation.program` and `deck.operation.word`

The classifier is `src/telemetry/operation.rs`. It reads the first line of the command, up to a shell comment (`# …`), which it never reads. When the line chains several commands, the first one that does more than set up the next is the one described: `cd`, `export`, `source`, `.`, `set` and `true` are skipped (`SETUP_PROGRAMS`), and so are `VAR=value` assignments and an `env`, `sudo`, `command`, `time` or `nice` in front of the program, with their options and the options' values (`time -f '%e' cargo test` is `cargo test`). Each of those wrappers has a table of the options it knows (`Options`); when the classifier meets an option it does not know, as with `env -S`, it sends no program and no word for that command, only the span and its duration. A program word that still has a `=` in it, as `1X=/a/b` (not a valid assignment), sends no program and no word: the classifier never takes the basename of such a word (`program_name`), because what follows its last `/` may be part of a value.

A command run through `sh -c` or `bash -c` is described by the script it runs, not by the arguments after the script, and a quoted argument stays one argument, so `cargo 'two words'` sends no word. A shell reading its script from standard input, as `bash -s`, sends only its own name, and when the program itself is not sent, its word is not sent either. The shape filter and the wordless programs that come after this are what the user page lists, because they decide what can leave the machine.

Codex reports a shell call as its arguments joined with spaces (`JOINED_ARGV_TOOL`), which loses where each argument ends. So for Codex the classifier keeps the program only, never a word, and no program either when it cannot tell which word is the program: a command that starts with `cd`, `env` or another shell, or whose program word has a `=` in it, as a `VAR=value` does. For `WORKDIR=/private/x cargo test`, taking the basename first would have sent `x`.

## Which `codex` and `devin` the shim adds to

The shim (`src/telemetry/shim.rs`) decides what to add from the environment it runs in, so a program between the shim and the agent that changes that environment would make it decide from the wrong configuration. It therefore adds only to a program it recognises as the agent itself, judged on the file with symlinks resolved (`launcher_is_certain`):

- an ELF or Mach-O executable;
- for Codex only, `bin/codex.js` inside a `node_modules/@openai/codex` package, which is the `bin` entry of the package npm installs. This is a recognition heuristic, not authentication: any readable file at that canonical path is accepted, a hard-linked or modified copy included;
- a script under `/nix/store/` whose text, read in full up to 64 KiB, contains neither `HOME` (and so neither `CODEX_HOME` nor `XDG_CONFIG_HOME`) nor `OTEL_`.

Everything else declines: a script in any language outside those paths, a file it cannot read, a Nix store script larger than the limit. A recognised launcher is trusted not to change `CODEX_HOME`, `XDG_CONFIG_HOME`, `HOME` or an `OTEL_` variable after the shim has decided; a substituted or modified launcher that does gets the additions decided from the environment the shim saw. The Nix check reads the script's text for those literal names (`names_no_decision_variable`); it does not prove what the script does, so it misses a name the script computes or splits (`CODEX_HO""ME`, or a name assembled from parts) and a chained wrapper, a store script that runs another script which changes those variables. The user page says which installs are supported and what to do with a customised launcher. An earlier version classified scripts by their `#!` interpreter and declined only shells; a shell under another name and a Python wrapper both got through, which is why the rule is now an allow-list.

The e2e tests stand in for the agents in these forms: `telemetry/otlp/016` links a Python recorder from a fake `node_modules/@openai/codex/bin/codex.js`, and `telemetry/otlp/017` builds a small native `devin` with `cc` that runs the recorder.
