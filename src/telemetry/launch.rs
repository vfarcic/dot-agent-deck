//! What an agent's launch needs, beyond its environment, for its own
//! telemetry to reach the user's backend tagged with the deck's run — for the
//! agents that do not switch it on from `OTEL_*` variables alone.
//!
//! - **Codex** needs its exporters named in its configuration. The deck adds
//!   `-c otel.trace_exporter=…` and `-c otel.metrics_exporter=…` to the
//!   command it runs, for each signal that has an endpoint. Never
//!   `otel.exporter`: Codex's log events carry tool arguments and output. No
//!   header goes on the command line — Codex reads the `OTEL_EXPORTER_OTLP_*`
//!   header variables from its environment once an exporter is on. Nothing is
//!   added when the user's Codex configuration names an exporter itself, or
//!   the command already passes an `otel.` override; or when an endpoint
//!   carries credentials (a `user:password@` or a query string), which the
//!   command line would show.
//! - **Devin** needs an endpoint in its user configuration and does not read
//!   `OTEL_RESOURCE_ATTRIBUTES`. The deck writes a copy of the user's Devin
//!   configuration with an `otel` block added — the endpoint of each signal
//!   the user's `OTEL_LOGS_EXPORTER` / `OTEL_METRICS_EXPORTER` leave on, the
//!   run's resource attributes, and prompt and tool logging off — to a file
//!   only its owner can read, in a directory only its owner can enter, under
//!   the daemon's state directory; passes it with `--config`; and removes it
//!   when the run ends. The deck adds no credentials to it, but it is a copy
//!   of the user's configuration and holds whatever that holds. A user's own
//!   `otel` block is kept as it is, gaining only the resource attributes it
//!   lacks; one that switches export off, or a command that already passes
//!   `--config`, gets nothing.
//! - **Pi** exports through the deck's own Pi extension, which stays inert
//!   unless the deck sets [`PI_TELEMETRY_ENV`] — which it does not when the
//!   user set that variable themselves, so `0` keeps Pi's export off.
//!
//! Codex's and Devin's are switched on only for a launch the deck can read
//! with certainty: the agent's program at the start of a command, after
//! `VAR=value` assignments, after `env` with its options, or after the `--` of
//! the deck's own `dot-agent-deck wrap`; and an environment for it that the
//! command does not change in a way the deck cannot read ([`launch_env`]).
//! Any other launch — another launcher, a sourced file, an `export`, an
//! `env -i`, a home or `OTEL_*` variable that is not a plain literal
//! assignment in front of the agent — gets nothing. Where it is switched on,
//! the user's configuration and exporter selectors are read from that
//! environment.
//!
//! With telemetry off none of this runs: [`super::Telemetry::prepare_run`]
//! returns before calling [`prepare`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::event::AgentType;

use super::config::{EnvMap, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_PROTOCOL, non_empty};
use super::operation::{ENV, options_len};

/// Set in a Pi agent's environment when the deck's Pi extension should export.
pub const PI_TELEMETRY_ENV: &str = "DOT_AGENT_DECK_PI_TELEMETRY";

/// The variables a launch reads beyond `OTEL_*`: where Codex's and Devin's
/// user configurations live, and the user's own say on Pi's export.
pub const LAUNCH_ENV_KEYS: [&str; 4] = ["HOME", "CODEX_HOME", "XDG_CONFIG_HOME", PI_TELEMETRY_ENV];

/// The prefix of the per-run Devin configuration files.
pub const DEVIN_CONFIG_PREFIX: &str = "devin-";

/// What [`prepare`] decided for one run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Launch {
    /// Variables to add to the agent's environment.
    pub env: Vec<(String, String)>,
    /// Words to add after the agent's program in its command, shell-quoted.
    pub args: Vec<String>,
    /// A per-run configuration file the deck wrote, to remove with the run.
    pub config_file: Option<PathBuf>,
}

impl Launch {
    /// `command` with [`Self::args`] after the agent's program. `None` when
    /// there is nothing to add, or no such program to add it after.
    pub fn command(&self, agent: &AgentType, command: Option<&str>) -> Option<String> {
        if self.args.is_empty() {
            return None;
        }
        let command = command?;
        let at = locate(command, agent_basename(agent)?)?.end;
        Some(format!(
            "{} {}{}",
            &command[..at],
            self.args.join(" "),
            &command[at..]
        ))
    }
}

/// What one run's launch needs.
pub struct LaunchInputs<'a> {
    pub agent: &'a AgentType,
    /// The command the spawn runs, as configured.
    pub command: Option<&'a str>,
    /// The agent's environment as the deck sees it: the daemon's `OTEL_*`
    /// variables and [`LAUNCH_ENV_KEYS`], with the spawn's own overlay applied.
    pub env: &'a EnvMap,
    /// The resource attributes the agent receives, parsed.
    pub resource: &'a [(String, String)],
    pub run_id: &'a str,
    /// Where per-run configuration files go.
    pub config_dir: Option<&'a Path>,
}

/// Decide `inputs`' launch, writing any per-run configuration file it needs.
pub fn prepare(inputs: &LaunchInputs<'_>) -> Launch {
    match inputs.agent {
        AgentType::Codex if cfg!(unix) => Launch {
            args: codex_args(inputs.command, inputs.env),
            ..Launch::default()
        },
        AgentType::Devin if cfg!(unix) => devin_launch(inputs).unwrap_or_default(),
        AgentType::Pi if pi_exports(inputs.env) => Launch {
            env: vec![(PI_TELEMETRY_ENV.to_string(), "1".to_string())],
            ..Launch::default()
        },
        _ => Launch::default(),
    }
}

/// The program name an agent's command runs.
fn agent_basename(agent: &AgentType) -> Option<&'static str> {
    crate::agent_registry::spec(agent)
        .detect_basenames
        .first()
        .copied()
}

/// A signal an agent can export, with the variables that configure it.
struct Signal {
    path: &'static str,
    endpoint: &'static str,
    protocol: &'static str,
    exporter: &'static str,
}

const TRACES: Signal = Signal {
    path: "v1/traces",
    endpoint: "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
    protocol: "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
    exporter: "OTEL_TRACES_EXPORTER",
};

const METRICS: Signal = Signal {
    path: "v1/metrics",
    endpoint: "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
    protocol: "OTEL_EXPORTER_OTLP_METRICS_PROTOCOL",
    exporter: "OTEL_METRICS_EXPORTER",
};

const LOGS: Signal = Signal {
    path: "v1/logs",
    endpoint: "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    protocol: "OTEL_EXPORTER_OTLP_LOGS_PROTOCOL",
    exporter: "OTEL_LOGS_EXPORTER",
};

impl Signal {
    /// Whether the user switched this signal off, or to something other than
    /// OTLP, with its `OTEL_*_EXPORTER` selector.
    fn declined(&self, env: &EnvMap) -> bool {
        non_empty(env, self.exporter).is_some_and(|v| !v.to_ascii_lowercase().contains("otlp"))
    }

    /// Where this signal goes: its own endpoint as written, else the general
    /// one with the signal's path. `None` when neither is set or the user
    /// declined the signal.
    fn endpoint(&self, env: &EnvMap) -> Option<String> {
        if self.declined(env) {
            return None;
        }
        if let Some(own) = non_empty(env, self.endpoint) {
            return Some(own.to_string());
        }
        let general = non_empty(env, OTEL_EXPORTER_OTLP_ENDPOINT)?;
        Some(format!("{}/{}", general.trim_end_matches('/'), self.path))
    }

    /// The signal's OTLP/HTTP protocol: its own, else the general one, else
    /// `http/protobuf`. `None` for anything that is not OTLP over HTTP.
    fn http_protocol(&self, env: &EnvMap) -> Option<HttpProtocol> {
        let value = non_empty(env, self.protocol)
            .or_else(|| non_empty(env, OTEL_EXPORTER_OTLP_PROTOCOL))
            .unwrap_or("http/protobuf");
        match value.to_ascii_lowercase().as_str() {
            "http/protobuf" => Some(HttpProtocol::Protobuf),
            "http/json" => Some(HttpProtocol::Json),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HttpProtocol {
    Protobuf,
    Json,
}

// ---- the launch environment ----

/// Whether variable `name` decides which configuration an agent reads or
/// what its telemetry does: its home directories and every `OTEL_*` variable.
fn decides_telemetry(name: &str) -> bool {
    matches!(name, "HOME" | "XDG_CONFIG_HOME" | "CODEX_HOME") || name.starts_with("OTEL_")
}

/// The environment the agent `located` in `command` starts with: `env` with
/// the `VAR=value` assignments in front of it applied. `None` when the deck
/// cannot read it with certainty: something in front of it changes the
/// environment in a way not read here ([`Located::environment_unknown`]); a
/// variable that [`decides_telemetry`] is named in front of it other than by
/// one of those assignments (an `export`, an earlier command, another
/// program's option, a `$HOME`), or is assigned a value with `$`, `` ` `` or
/// `~` in it; or that environment switches the OTel SDK off.
fn launch_env(command: &str, located: &Located, env: &EnvMap) -> Option<EnvMap> {
    if located.environment_unknown {
        return None;
    }
    let mut named: HashMap<&str, usize> = HashMap::new();
    for name in identifiers(&command[..located.end]).filter(|n| decides_telemetry(n)) {
        *named.entry(name).or_default() += 1;
    }
    let mut assigned: HashMap<&str, usize> = HashMap::new();
    for (name, value) in &located.assignments {
        if decides_telemetry(name) {
            if value.contains(['$', '`', '~']) {
                return None;
            }
            *assigned.entry(name).or_default() += 1;
        }
    }
    if named != assigned {
        return None;
    }
    let mut launch = env.clone();
    launch.extend(located.assignments.iter().cloned());
    if non_empty(&launch, "OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
        return None;
    }
    Some(launch)
}

/// The shell-variable-shaped runs of `text`: letters, digits and `_`.
fn identifiers(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
}

// ---- Codex ----

/// The `-c` overrides that switch on Codex's traces and metrics exporters,
/// shell-quoted, or none — see the module docs.
fn codex_args(command: Option<&str>, env: &EnvMap) -> Vec<String> {
    let Some(command) = command else {
        return Vec::new();
    };
    let Some(codex) = locate(command, "codex") else {
        return Vec::new();
    };
    if command.contains("otel.") {
        return Vec::new();
    }
    let Some(env) = launch_env(command, &codex, env) else {
        return Vec::new();
    };
    let env = &env;
    let Some(home) = codex_home(env) else {
        return Vec::new();
    };
    if codex_config_names_an_exporter(&home.join("config.toml")) != Some(false) {
        return Vec::new();
    }
    let mut args = Vec::new();
    for (key, signal) in [
        ("otel.trace_exporter", &TRACES),
        ("otel.metrics_exporter", &METRICS),
    ] {
        let (Some(endpoint), Some(protocol)) = (signal.endpoint(env), signal.http_protocol(env))
        else {
            continue;
        };
        if may_carry_credentials(&endpoint) {
            return Vec::new();
        }
        let protocol = match protocol {
            HttpProtocol::Protobuf => "binary",
            HttpProtocol::Json => "json",
        };
        let value = format!(
            "{key}={{otlp-http={{endpoint={},protocol=\"{protocol}\"}}}}",
            toml_string(&endpoint)
        );
        args.push("-c".to_string());
        args.push(shell_quote(&value));
    }
    args
}

/// The Codex home Codex reads in its launch environment `env`: its
/// `CODEX_HOME`, else `.codex` in its home directory. `None` when that is not
/// an absolute path.
fn codex_home(env: &EnvMap) -> Option<PathBuf> {
    let absolute = |value: &str| Some(PathBuf::from(value)).filter(|p| p.is_absolute());
    match non_empty(env, "CODEX_HOME") {
        Some(value) => absolute(value),
        None => Some(absolute(non_empty(env, "HOME")?)?.join(".codex")),
    }
}

/// Whether an endpoint URL may carry a credential the command line would
/// show: a `user:password@` before its host, or a query string.
fn may_carry_credentials(endpoint: &str) -> bool {
    let after_scheme = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    authority.contains('@') || endpoint.contains('?')
}

/// Whether the Codex configuration at `path` sets one of Codex's OTel
/// exporters. `Some(false)` when it does not or does not exist; `None` when it
/// cannot be read or parsed, which the caller treats as "leave it alone".
fn codex_config_names_an_exporter(path: &Path) -> Option<bool> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
        Err(_) => return None,
    };
    let table: toml::Table = text.parse().ok()?;
    Some(
        table
            .get("otel")
            .and_then(toml::Value::as_table)
            .is_some_and(|otel| {
                ["exporter", "trace_exporter", "metrics_exporter"]
                    .iter()
                    .any(|key| otel.contains_key(*key))
            }),
    )
}

/// `value` as a TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---- Devin ----

/// Devin's launch: its per-run configuration written and named with
/// `--config`. `None` when the run gets nothing — see the module docs.
fn devin_launch(inputs: &LaunchInputs<'_>) -> Option<Launch> {
    let command = inputs.command?;
    let devin = locate(command, "devin")?;
    if command.contains("--config") {
        return None;
    }
    let env = &launch_env(command, &devin, inputs.env)?;
    // Devin exports logs and metrics over OTLP/HTTP protobuf only, whatever
    // the protocol variable says; a gRPC endpoint would not answer it.
    if non_empty(env, OTEL_EXPORTER_OTLP_PROTOCOL)
        .is_some_and(|p| !p.to_ascii_lowercase().starts_with("http/"))
    {
        return None;
    }
    let user_path = devin_user_config(env)?;
    let user = match std::fs::read_to_string(&user_path) {
        Ok(text) => serde_json::from_str(&strip_jsonc(&text)).ok()?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(_) => return None,
    };
    let config = devin_config(user, env, inputs.resource)?;
    let path = inputs
        .config_dir?
        .join(format!("{DEVIN_CONFIG_PREFIX}{}.json", inputs.run_id));
    if let Err(e) = write_private(&path, &serde_json::to_vec_pretty(&config).ok()?) {
        tracing::warn!("telemetry: Devin's per-run configuration could not be written ({e})");
        return None;
    }
    Some(Launch {
        args: vec!["--config".to_string(), shell_quote(&path.to_string_lossy())],
        config_file: Some(path),
        ..Launch::default()
    })
}

/// Devin's user configuration file, resolved as Devin resolves it.
fn devin_user_config(env: &EnvMap) -> Option<PathBuf> {
    if let Some(xdg) = non_empty(env, "XDG_CONFIG_HOME").map(Path::new)
        && xdg.is_absolute()
    {
        return Some(xdg.join("devin").join("config.json"));
    }
    let home = Path::new(non_empty(env, "HOME")?);
    if !home.is_absolute() {
        return None;
    }
    Some(home.join(".config").join("devin").join("config.json"))
}

/// The user's Devin configuration `user` with the deck's `otel` block merged
/// in. `None` when the run gets no configuration.
fn devin_config(
    mut user: serde_json::Value,
    env: &EnvMap,
    resource: &[(String, String)],
) -> Option<serde_json::Value> {
    use serde_json::{Map, Value};
    let root = user.as_object_mut()?;
    let attributes: Map<String, Value> = resource
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    match root.get_mut("otel") {
        Some(Value::Object(otel)) => {
            if otel.get("enabled") == Some(&Value::Bool(false)) {
                return None;
            }
            let own = otel
                .entry("resource_attributes")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()?;
            for (key, value) in attributes {
                own.entry(key).or_insert(value);
            }
        }
        Some(_) => return None,
        None => {
            // Devin reads no `OTEL_*_EXPORTER` selector itself, so a signal the
            // user switched off there gets no endpoint here.
            let mut otel = Map::new();
            let general = non_empty(env, OTEL_EXPORTER_OTLP_ENDPOINT);
            let shared = [&LOGS, &METRICS]
                .iter()
                .all(|signal| !signal.declined(env) && non_empty(env, signal.endpoint).is_none());
            match general {
                Some(general) if shared => {
                    otel.insert("endpoint".into(), Value::String(general.to_string()));
                }
                _ => {
                    for (key, signal) in [("logs_endpoint", &LOGS), ("metrics_endpoint", &METRICS)]
                    {
                        if let Some(endpoint) = signal.endpoint(env) {
                            otel.insert(key.into(), Value::String(endpoint));
                        }
                    }
                }
            }
            if otel.is_empty() {
                return None;
            }
            otel.insert("resource_attributes".into(), Value::Object(attributes));
            otel.insert("disable_user_prompt_logging".into(), Value::Bool(true));
            otel.insert("disable_tool_logging".into(), Value::Bool(true));
            root.insert("otel".into(), Value::Object(otel));
        }
    }
    Some(user)
}

/// `text`, JSON with `//` and `/* */` comments and trailing commas, as plain
/// JSON. Strings are copied as they are.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push(c);
                while let Some(s) = chars.next() {
                    out.push(s);
                    match s {
                        '\\' => {
                            if let Some(escaped) = chars.next() {
                                out.push(escaped);
                            }
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '/' if chars.peek() == Some(&'/') => {
                for s in chars.by_ref() {
                    if s == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for s in chars.by_ref() {
                    if last == '*' && s == '/' {
                        break;
                    }
                    last = s;
                }
                out.push(' ');
            }
            ',' => {
                // A trailing comma: the next significant character closes.
                let rest: String = chars.clone().collect();
                let next = strip_jsonc_lookahead(&rest);
                if !matches!(next, Some('}' | ']')) {
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The first character of `rest` that is not whitespace or inside a comment.
fn strip_jsonc_lookahead(rest: &str) -> Option<char> {
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {}
            '/' if chars.peek() == Some(&'/') => {
                for s in chars.by_ref() {
                    if s == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for s in chars.by_ref() {
                    if last == '*' && s == '/' {
                        break;
                    }
                    last = s;
                }
            }
            c => return Some(c),
        }
    }
    None
}

/// Write `bytes` to a new file at `path` readable by its owner only, in a
/// directory only its owner can enter: created so, or an existing one made
/// so — refused when it is a symlink or another user owns it.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        crate::platform::fsperm::ensure_owner_only_dir(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Remove a per-run configuration file the deck wrote.
pub fn remove_config_file(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("telemetry: a per-run agent configuration could not be removed ({e})");
    }
}

/// Remove every per-run configuration file in `dir`, the deck's own
/// directory for them: called before any run exists, so each one there was
/// left by a daemon that ended without removing it. Anything else in `dir` is
/// left alone, and a missing `dir` has nothing to remove.
pub fn remove_leftover_config_files(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(DEVIN_CONFIG_PREFIX)
            && name.ends_with(".json")
            && entry.file_type().is_ok_and(|t| t.is_file())
        {
            remove_config_file(&entry.path());
        }
    }
}

// ---- Pi ----

/// Whether the deck's Pi extension has somewhere to send logs: a logs
/// endpoint, over OTLP/HTTP, not declined by the user. The extension sends
/// OTLP/JSON.
fn pi_exports(env: &EnvMap) -> bool {
    non_empty(env, PI_TELEMETRY_ENV).is_none()
        && LOGS.endpoint(env).is_some()
        && LOGS.http_protocol(env).is_some()
}

// ---- the command line ----

/// A command-line word, by byte span, with the shell's quoting removed.
#[derive(Debug)]
enum Token {
    Word { end: usize, text: String },
    Operator,
}

/// `command`'s words and control operators. Quotes are honoured so a quoted
/// space or operator does not split a word.
fn tokens(command: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut text = String::new();
    let mut in_word = false;
    let mut chars = command.char_indices().peekable();
    let mut end = 0;
    while let Some((i, c)) = chars.next() {
        match c {
            '\'' | '"' => {
                in_word = true;
                end = command.len();
                while let Some((j, q)) = chars.next() {
                    if q == c {
                        end = j + 1;
                        break;
                    }
                    if q == '\\' && c == '"' {
                        if let Some((_, escaped)) = chars.next() {
                            text.push(escaped);
                        }
                        continue;
                    }
                    text.push(q);
                }
            }
            '\\' => {
                in_word = true;
                end = i + 1;
                if let Some((j, escaped)) = chars.next() {
                    text.push(escaped);
                    end = j + escaped.len_utf8();
                }
            }
            ';' | '|' | '&' | '\n' | '(' | ')' => {
                if in_word {
                    out.push(Token::Word {
                        end,
                        text: std::mem::take(&mut text),
                    });
                    in_word = false;
                }
                out.push(Token::Operator);
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(Token::Word {
                        end,
                        text: std::mem::take(&mut text),
                    });
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                text.push(c);
                end = i + c.len_utf8();
            }
        }
    }
    if in_word {
        out.push(Token::Word { end, text });
    }
    out
}

/// Shell builtins that can change the environment of the commands after them.
const ENVIRONMENT_BUILTINS: &[&str] = &[
    "source", ".", "eval", "export", "set", "unset", "declare", "typeset",
];

/// Where `command` runs a program — see [`locate`].
#[derive(Debug)]
struct Located {
    /// The byte offset just past the program's word.
    end: usize,
    /// The `VAR=value` assignments in front of it in its command, `env`'s
    /// included, in order, with the shell's quoting removed.
    assignments: Vec<(String, String)>,
    /// Whether something in front of it changes its environment in a way not
    /// read here: an `env` that clears or unsets variables, or an earlier
    /// command with one of [`ENVIRONMENT_BUILTINS`] anywhere in it.
    environment_unknown: bool,
}

/// The first place `command` runs `program` — a word whose basename is
/// `program`, at the start of a command (after a control operator), past
/// `VAR=value` assignments, an `env` with its options, or the deck's own
/// `dot-agent-deck wrap` up to its `--`. `None` when there is no such word, or
/// something else runs it: another launcher may set its environment.
fn locate(command: &str, program: &str) -> Option<Located> {
    let mut sourced = false;
    let mut segment: Vec<(usize, String)> = Vec::new();
    let mut tokens = tokens(command).into_iter();
    loop {
        let token = tokens.next();
        if let Some(Token::Word { end, text }) = token {
            segment.push((end, text));
            continue;
        }
        if let Some(mut found) = locate_in(&segment, program) {
            found.environment_unknown |= sourced;
            return Some(found);
        }
        sourced |= segment
            .iter()
            .any(|(_, w)| ENVIRONMENT_BUILTINS.contains(&w.as_str()));
        segment.clear();
        token.as_ref()?;
    }
}

/// [`locate`] within one command's words.
fn locate_in(words: &[(usize, String)], program: &str) -> Option<Located> {
    let mut found = Located {
        end: 0,
        assignments: Vec::new(),
        environment_unknown: false,
    };
    let mut i = 0;
    while let Some((end, word)) = words.get(i) {
        if crate::event::is_env_assignment(word) {
            if let Some((name, value)) = word.split_once('=') {
                found
                    .assignments
                    .push((name.to_string(), value.to_string()));
            }
            i += 1;
            continue;
        }
        let base = basename(word);
        if base == program {
            found.end = *end;
            return Some(found);
        }
        let rest: Vec<String> = words[i + 1..].iter().map(|(_, w)| w.clone()).collect();
        if base == "env" {
            let len = options_len(&rest, &ENV)?;
            found.environment_unknown |= rest[..len].iter().any(|w| {
                w.starts_with("--unset") || w == "--ignore-environment" || {
                    !w.starts_with("--") && w.starts_with('-') && w.contains(['i', 'u'])
                }
            });
            i += 1 + len;
        } else if base == "dot-agent-deck" && rest.first().map(String::as_str) == Some("wrap") {
            // The deck's wrapper passes its environment on to the command
            // after its `--`.
            i += 2 + rest.iter().position(|w| w == "--")?;
        } else {
            return None;
        }
    }
    None
}

/// The last `/`-separated part of `word`.
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// `value` as one POSIX shell word.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn resource() -> Vec<(String, String)> {
        vec![
            ("team".into(), "platform".into()),
            ("deck.run.id".into(), "run-1".into()),
        ]
    }

    fn inputs<'a>(
        agent: &'a AgentType,
        command: &'a str,
        env: &'a EnvMap,
        resource: &'a [(String, String)],
        dir: &'a Path,
    ) -> LaunchInputs<'a> {
        LaunchInputs {
            agent,
            command: Some(command),
            env,
            resource,
            run_id: "run-1",
            config_dir: Some(dir),
        }
    }

    /// Run `args` (already shell-quoted) through `sh` and read back the argv
    /// it builds.
    fn argv(command: &str) -> Vec<String> {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "for a in {command}; do printf '%s\\n' \"$a\"; done"
            ))
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn codex_gets_trace_and_metric_exporters_and_nothing_else() {
        let home = tempfile::tempdir().unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://api.example.com/"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=SECRET-VALUE"),
        ]);
        let res = resource();
        let launch = prepare(&inputs(
            &AgentType::Codex,
            "codex --model m",
            &env,
            &res,
            home.path(),
        ));
        let command = launch
            .command(&AgentType::Codex, Some("codex --model m"))
            .unwrap();
        assert_eq!(
            argv(&command),
            [
                "codex",
                "-c",
                r#"otel.trace_exporter={otlp-http={endpoint="https://api.example.com/v1/traces",protocol="binary"}}"#,
                "-c",
                r#"otel.metrics_exporter={otlp-http={endpoint="https://api.example.com/v1/metrics",protocol="binary"}}"#,
                "--model",
                "m",
            ]
        );
        assert!(!command.contains("SECRET-VALUE"), "no header on argv");
        assert!(!command.contains("otel.exporter="), "no log exporter");
        assert!(launch.env.is_empty() && launch.config_file.is_none());
    }

    #[test]
    fn codex_signals_follow_their_own_endpoint_protocol_and_selector() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_str().unwrap();
        let args = |pairs: &[(&str, &str)]| {
            let mut all = vec![("HOME", h)];
            all.extend_from_slice(pairs);
            codex_args(Some("codex"), &env(&all)).join(" ")
        };
        let traces_only = args(&[(
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
            "http://t:4318/v1/traces",
        )]);
        assert!(traces_only.contains("otel.trace_exporter"));
        assert!(!traces_only.contains("metrics"), "{traces_only}");
        let json = args(&[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_METRICS_EXPORTER", "none"),
        ]);
        assert!(json.contains(r#"protocol="json""#), "{json}");
        assert!(
            !json.contains("metrics"),
            "the user declined metrics: {json}"
        );
        assert_eq!(
            args(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ]),
            ""
        );
    }

    #[test]
    fn codex_gets_nothing_when_the_user_configured_its_exporters() {
        let home = tempfile::tempdir().unwrap();
        let codex_home = home.path().join("alt");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::fs::write(
            codex_home.join("config.toml"),
            "model = \"x\"\n[otel]\nexporter = \"none\"\n",
        )
        .unwrap();
        let base = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        let mut with_home = base.clone();
        with_home.insert("CODEX_HOME".into(), codex_home.to_str().unwrap().into());
        assert!(codex_args(Some("codex"), &with_home).is_empty());
        let assigned = format!("CODEX_HOME={} codex", codex_home.display());
        assert!(codex_args(Some(&assigned), &base).is_empty());
        assert!(
            !codex_args(Some("codex"), &base).is_empty(),
            "~/.codex has none"
        );
        assert!(codex_args(Some("codex -c otel.exporter=x"), &base).is_empty());
        assert!(codex_args(Some("devbox run codex-big"), &base).is_empty());
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex/config.toml"), "not = [toml").unwrap();
        assert!(
            codex_args(Some("codex"), &base).is_empty(),
            "an unreadable config is left alone"
        );
    }

    /// The overrides reach Codex through `env` and the deck's own wrapper, as
    /// `sh` builds its argv.
    #[test]
    fn codex_through_a_forwarding_prefix_gets_its_overrides_as_its_own_args() {
        let home = tempfile::tempdir().unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        for (command, program_at) in [
            ("env codex --model m", 1),
            ("env -C /w FOO=1 codex --model m", 4),
            ("dot-agent-deck wrap --agent codex -- codex --model m", 5),
        ] {
            let launch = prepare(&inputs(
                &AgentType::Codex,
                command,
                &env,
                &resource(),
                home.path(),
            ));
            let argv = argv(&launch.command(&AgentType::Codex, Some(command)).unwrap());
            assert_eq!(argv[program_at], "codex", "{command}: {argv:?}");
            assert_eq!(argv[program_at + 1], "-c", "{command}: {argv:?}");
            assert!(argv[program_at + 2].starts_with("otel.trace_exporter="));
            assert_eq!(argv[argv.len() - 2..], ["--model", "m"], "{command}");
        }
    }

    /// The configuration checked is the one the launched Codex reads; when
    /// that cannot be told without running the command, nothing is added.
    #[test]
    fn codex_home_is_the_launched_codexs_or_nothing_is_added() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_str().unwrap();
        let alt = home.path().join("alt");
        std::fs::create_dir_all(&alt).unwrap();
        std::fs::write(
            alt.join("config.toml"),
            "[otel]\ntrace_exporter = \"none\"\n",
        )
        .unwrap();
        let empty = home.path().join("empty");
        let alt = alt.to_str().unwrap();
        let empty = empty.to_str().unwrap();
        let base = env(&[
            ("HOME", h),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        let added = |command: &str| !codex_args(Some(command), &base).is_empty();
        for command in [
            "CODEX_HOME=\"$HOME/alt\" codex".to_string(),
            "CODEX_HOME=$HOME/alt codex".to_string(),
            "CODEX_HOME=~/alt codex".to_string(),
            format!("CODEX_HOME=$(echo {alt}) codex"),
            format!("CODEX_HOME=`echo {alt}` codex"),
            "CODEX_HOME=relative codex".to_string(),
            format!("cd /w && CODEX_HOME={alt} codex"),
            format!("CODEX_HOME={alt} env codex"),
            format!("env CODEX_HOME={alt} codex"),
            format!("CODEX_HOME={alt} /bin/dot-agent-deck wrap --agent codex -- codex"),
            format!("/bin/dot-agent-deck wrap --agent codex -- CODEX_HOME={alt} codex"),
            format!("export CODEX_HOME={empty} && codex"),
            format!("export HOME={empty}; codex"),
            "HOME=~ codex".to_string(),
            format!("env -i HOME={empty} codex"),
            "env -u CODEX_HOME codex".to_string(),
            "env --unset=CODEX_HOME codex".to_string(),
            "source ~/.envrc && codex".to_string(),
            ". ./env.sh; codex".to_string(),
            format!("CODEX_HOME={empty}; codex"),
            format!("FOO=1 . {h}/env.sh && codex"),
            format!("cd /w && FOO=1 source {h}/env.sh && codex"),
            "true && env FOO=1 eval x; codex".to_string(),
            "set -a; codex".to_string(),
            format!("declare -x CODEX_HOME={empty}; codex"),
            format!("CODEX_HOME={empty} HOME=$PWD codex"),
            format!("CODEX_HOME={empty} FOO=$HOME codex"),
        ] {
            assert!(!added(&command), "{command}");
        }
        for command in [
            format!("CODEX_HOME={empty} codex"),
            format!("cd /w && CODEX_HOME={empty} codex"),
            format!("env CODEX_HOME={empty} codex"),
            format!("HOME={empty} codex"),
            format!("CODEX_HOME={alt} CODEX_HOME={empty} codex"),
        ] {
            assert!(added(&command), "{command}");
        }
    }

    /// An endpoint that may carry a credential is not put on a command line.
    #[test]
    fn codex_gets_nothing_for_an_endpoint_with_credentials() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_str().unwrap();
        let args = |pairs: &[(&str, &str)]| {
            let mut all = vec![("HOME", h)];
            all.extend_from_slice(pairs);
            codex_args(Some("codex"), &env(&all))
        };
        for endpoint in [
            "https://user:synthetic-password-1741@collector.example",
            "https://synthetic-token-1741@collector.example:4318/",
            "https://collector.example/v1?api_key=synthetic-token-1741",
        ] {
            assert!(
                args(&[("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)]).is_empty(),
                "{endpoint}"
            );
        }
        assert!(
            args(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.example"),
                (
                    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                    "https://u:p@m.example/v1/metrics"
                ),
            ])
            .is_empty(),
            "one credential-bearing signal declines both"
        );
        assert!(
            !args(&[(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "https://collector.example/a@b"
            )])
            .is_empty()
        );
        assert!(!args(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.example")]).is_empty());
    }

    /// A sourced file sets `CODEX_HOME` to a home whose configuration turns
    /// the exporters off: the shell's own reading of the command proves it,
    /// and the deck, which cannot read the file, adds nothing.
    #[test]
    fn a_sourced_file_in_front_of_codex_adds_nothing() {
        let home = tempfile::tempdir().unwrap();
        let alt = home.path().join("alt");
        std::fs::create_dir(&alt).unwrap();
        std::fs::write(
            alt.join("config.toml"),
            "[otel]\ntrace_exporter = \"none\"\n",
        )
        .unwrap();
        let script = home.path().join("env.sh");
        std::fs::write(&script, format!("export CODEX_HOME={}\n", alt.display())).unwrap();
        let shell = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "FOO=1 . {}; printf '%s' \"$CODEX_HOME\"",
                script.display()
            ))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(shell.stdout).unwrap(),
            alt.to_str().unwrap()
        );
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        let command = format!("FOO=1 . {} && codex", script.display());
        assert!(codex_args(Some(&command), &env).is_empty(), "{command}");
        assert!(!codex_args(Some("codex"), &env).is_empty(), "plain codex");
    }

    /// The exporter selectors and endpoints Codex gets are those of its own
    /// environment, assignments in front of it included; one it cannot be
    /// sure of adds nothing.
    #[test]
    fn codex_signals_follow_its_launch_environment() {
        let home = tempfile::tempdir().unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        let args = |command: &str| codex_args(Some(command), &env).join(" ");
        let plain = args("codex");
        assert!(plain.contains("otel.trace_exporter"), "{plain}");
        assert!(plain.contains("otel.metrics_exporter"), "{plain}");
        let no_traces = args("env OTEL_TRACES_EXPORTER=none codex");
        assert!(!no_traces.contains("trace_exporter"), "{no_traces}");
        assert!(no_traces.contains("otel.metrics_exporter"), "{no_traces}");
        let no_metrics = args("OTEL_METRICS_EXPORTER=none codex");
        assert!(no_metrics.contains("otel.trace_exporter"), "{no_metrics}");
        assert!(!no_metrics.contains("metrics_exporter"), "{no_metrics}");
        let own = args("OTEL_EXPORTER_OTLP_ENDPOINT=http://own:4318 codex");
        assert!(own.contains("http://own:4318/v1/traces"), "{own}");
        assert!(!own.contains("http://c:4318"), "{own}");
        for command in [
            "OTEL_SDK_DISABLED=true codex",
            "env OTEL_TRACES_EXPORTER=$NONE codex",
            "export OTEL_TRACES_EXPORTER=none; codex",
            "OTEL_TRACES_EXPORTER=none; codex",
            "env -u OTEL_EXPORTER_OTLP_ENDPOINT codex",
            "devbox run codex",
            "devbox run -e OTEL_TRACES_EXPORTER=none codex",
            "op run --env-file .env -- codex",
        ] {
            assert_eq!(args(command), "", "{command}");
        }
    }

    #[test]
    fn the_args_go_after_the_program_wherever_it_starts() {
        let launch = Launch {
            args: vec!["-c".into(), "'a b'".into()],
            ..Launch::default()
        };
        let codex = AgentType::Codex;
        for (command, expected) in [
            ("codex", "codex -c 'a b'"),
            (
                "/usr/local/bin/codex resume",
                "/usr/local/bin/codex -c 'a b' resume",
            ),
            (
                "CODEX_HOME='/x y' codex",
                "CODEX_HOME='/x y' codex -c 'a b'",
            ),
            ("cd /w && codex --yolo", "cd /w && codex -c 'a b' --yolo"),
            (
                "/bin/dot-agent-deck wrap --agent codex -- codex",
                "/bin/dot-agent-deck wrap --agent codex -- codex -c 'a b'",
            ),
            ("env codex --yolo", "env codex -c 'a b' --yolo"),
            (
                "env -u X -C /w FOO=1 /bin/codex",
                "env -u X -C /w FOO=1 /bin/codex -c 'a b'",
            ),
            (
                "/bin/dot-agent-deck wrap --agent codex -- env codex",
                "/bin/dot-agent-deck wrap --agent codex -- env codex -c 'a b'",
            ),
        ] {
            assert_eq!(
                launch.command(&codex, Some(command)).as_deref(),
                Some(expected),
                "{command}"
            );
        }
        for command in [
            "echo codex",
            "devbox run codex-big",
            "mycodex",
            "env -S 'codex'",
            "devbox run codex",
            "devbox run -- codex",
            "/bin/deck wrap --agent codex -- codex",
            "op run -- codex",
            "sudo codex",
        ] {
            assert_eq!(launch.command(&codex, Some(command)), None, "{command}");
        }
        assert_eq!(Launch::default().command(&codex, Some("codex")), None);
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn devin_gets_a_private_per_run_config_merged_with_the_users() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state").join("telemetry");
        let user_dir = home.path().join(".config").join("devin");
        std::fs::create_dir_all(&user_dir).unwrap();
        std::fs::write(
            user_dir.join("config.json"),
            "{\n  // my model\n  \"agent\": { \"model\": \"swe\", },\n  \"hooks\": {\"Stop\": []}, /* x */\n}\n",
        )
        .unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "authorization=SECRET-VALUE"),
        ]);
        let res = resource();
        let launch = prepare(&inputs(&AgentType::Devin, "devin", &env, &res, &state));
        let path = launch.config_file.clone().expect("a config file");
        assert!(path.starts_with(&state));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&state), 0o700);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("SECRET-VALUE"));
        let config: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(config["agent"]["model"], "swe");
        assert!(config["hooks"]["Stop"].is_array(), "the user's hooks stay");
        assert_eq!(
            config["otel"],
            serde_json::json!({
                "endpoint": "http://c:4318",
                "resource_attributes": {"team": "platform", "deck.run.id": "run-1"},
                "disable_user_prompt_logging": true,
                "disable_tool_logging": true,
            })
        );
        assert_eq!(
            argv(&launch.command(&AgentType::Devin, Some("devin")).unwrap()),
            ["devin", "--config", path.to_str().unwrap()]
        );
        remove_config_file(&path);
        assert!(!path.exists());
    }

    #[test]
    fn a_users_own_devin_otel_block_only_gains_resource_attributes() {
        let env = env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")]);
        let user = serde_json::json!({"otel": {
            "logs_endpoint": "https://mine/v1/logs",
            "disable_tool_logging": false,
            "resource_attributes": {"team": "mine"},
        }});
        let merged = devin_config(user, &env, &resource()).unwrap();
        assert_eq!(
            merged["otel"],
            serde_json::json!({
                "logs_endpoint": "https://mine/v1/logs",
                "disable_tool_logging": false,
                "resource_attributes": {"team": "mine", "deck.run.id": "run-1"},
            })
        );
        assert!(
            devin_config(
                serde_json::json!({"otel": {"enabled": false}}),
                &env,
                &resource()
            )
            .is_none(),
            "the user switched it off"
        );
        let signal_only = devin_config(
            serde_json::json!({}),
            &self::env(&[("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "http://m/v1/metrics")]),
            &resource(),
        )
        .unwrap();
        assert_eq!(
            signal_only["otel"]["metrics_endpoint"],
            "http://m/v1/metrics"
        );
        assert!(signal_only["otel"].get("endpoint").is_none());
        assert!(
            devin_config(
                serde_json::json!({}),
                &self::env(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://t")]),
                &resource()
            )
            .is_none(),
            "Devin exports no traces"
        );
    }

    /// Devin reads no exporter selector, so the deck's block configures only
    /// the signals the user left on.
    #[test]
    fn devin_signals_follow_the_users_exporter_selectors() {
        let config = |pairs: &[(&str, &str)]| {
            let mut all = vec![("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318")];
            all.extend_from_slice(pairs);
            devin_config(serde_json::json!({}), &env(&all), &resource()).map(|c| c["otel"].clone())
        };
        let endpoints = |otel: serde_json::Value| {
            ["endpoint", "logs_endpoint", "metrics_endpoint"]
                .iter()
                .filter_map(|k| otel.get(*k).map(|v| format!("{k}={}", v.as_str().unwrap())))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            config(&[
                ("OTEL_LOGS_EXPORTER", "none"),
                ("OTEL_METRICS_EXPORTER", "none")
            ]),
            None
        );
        assert_eq!(
            endpoints(config(&[("OTEL_LOGS_EXPORTER", "none")]).unwrap()),
            ["metrics_endpoint=http://c:4318/v1/metrics"]
        );
        assert_eq!(
            endpoints(config(&[("OTEL_METRICS_EXPORTER", "console")]).unwrap()),
            ["logs_endpoint=http://c:4318/v1/logs"]
        );
        assert_eq!(
            endpoints(config(&[("OTEL_METRICS_EXPORTER", "otlp")]).unwrap()),
            ["endpoint=http://c:4318"]
        );
        assert_eq!(
            endpoints(config(&[("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://l/v1/logs")]).unwrap()),
            [
                "logs_endpoint=http://l/v1/logs",
                "metrics_endpoint=http://c:4318/v1/metrics"
            ]
        );
        let own = devin_config(
            serde_json::json!({"otel": {"endpoint": "https://mine"}}),
            &env(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                ("OTEL_LOGS_EXPORTER", "none"),
                ("OTEL_METRICS_EXPORTER", "none"),
            ]),
            &resource(),
        )
        .unwrap();
        assert_eq!(
            own["otel"]["endpoint"], "https://mine",
            "the user's own block stands"
        );
        assert_eq!(own["otel"]["resource_attributes"]["deck.run.id"], "run-1");
    }

    /// The per-run file goes only in a directory the deck can vouch for.
    #[test]
    fn devin_config_directory_is_owner_only_or_refused() {
        let home = tempfile::tempdir().unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        let res = resource();
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let linked = home.path().join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        assert_eq!(
            prepare(&inputs(&AgentType::Devin, "devin", &env, &res, &linked)),
            Launch::default()
        );
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

        let open = home.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let launch = prepare(&inputs(&AgentType::Devin, "env devin", &env, &res, &open));
        assert!(launch.config_file.is_some());
        assert_eq!(
            mode(&open),
            0o700,
            "an existing directory is made owner-only"
        );
        assert_eq!(
            launch
                .command(&AgentType::Devin, Some("env devin"))
                .unwrap(),
            format!("env devin {}", launch.args.join(" "))
        );
    }

    #[test]
    fn devin_gets_nothing_when_it_should_not() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        let h = home.path().to_str().unwrap();
        let res = resource();
        let none = |command: &str, pairs: &[(&str, &str)]| {
            let mut all = vec![
                ("HOME", h),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            ];
            all.extend_from_slice(pairs);
            let env = env(&all);
            prepare(&inputs(&AgentType::Devin, command, &env, &res, &state)) == Launch::default()
        };
        assert!(none("devin --config /mine.json", &[]));
        assert!(none("devin", &[("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")]));
        std::fs::create_dir_all(home.path().join(".config/devin")).unwrap();
        std::fs::write(home.path().join(".config/devin/config.json"), "{ broken").unwrap();
        assert!(none("devin", &[]), "a config it cannot read is left alone");
        assert!(!state.exists(), "nothing was written");
    }

    /// Devin's configuration and signals are read from its own launch
    /// environment, assignments in front of it included.
    #[test]
    fn devin_follows_its_launch_environment() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        let off = home.path().join("off");
        std::fs::create_dir_all(off.join("devin")).unwrap();
        std::fs::write(
            off.join("devin/config.json"),
            r#"{"otel":{"enabled":false}}"#,
        )
        .unwrap();
        let mine = home.path().join("mine");
        std::fs::create_dir_all(mine.join("devin")).unwrap();
        std::fs::write(mine.join("devin/config.json"), r#"{"theme": "mine"}"#).unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
        ]);
        let res = resource();
        let launch =
            |command: &str| prepare(&inputs(&AgentType::Devin, command, &env, &res, &state));
        for command in [
            format!("env XDG_CONFIG_HOME={} devin", off.display()),
            format!("XDG_CONFIG_HOME={} devin", off.display()),
            "env OTEL_LOGS_EXPORTER=none OTEL_METRICS_EXPORTER=none devin".to_string(),
            "OTEL_SDK_DISABLED=true devin".to_string(),
            "XDG_CONFIG_HOME=~/off devin".to_string(),
            "export XDG_CONFIG_HOME=/x; devin".to_string(),
            "env -i devin".to_string(),
            "devbox run devin".to_string(),
        ] {
            assert_eq!(launch(&command), Launch::default(), "{command}");
        }
        let config = |command: &str| {
            let path = launch(command).config_file.expect(command);
            let config: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            remove_config_file(&path);
            config
        };
        let logs_only = config("OTEL_METRICS_EXPORTER=none devin");
        assert_eq!(logs_only["otel"]["logs_endpoint"], "http://c:4318/v1/logs");
        assert!(logs_only["otel"].get("metrics_endpoint").is_none());
        let own = config(&format!("env XDG_CONFIG_HOME={} devin", mine.display()));
        assert_eq!(own["theme"], "mine", "the launched Devin's own config");
    }

    #[test]
    fn devin_reads_its_config_where_devin_does() {
        let e = env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "/x")]);
        assert_eq!(
            devin_user_config(&e),
            Some(PathBuf::from("/x/devin/config.json"))
        );
        let e = env(&[("HOME", "/h"), ("XDG_CONFIG_HOME", "rel")]);
        assert_eq!(
            devin_user_config(&e),
            Some(PathBuf::from("/h/.config/devin/config.json"))
        );
    }

    #[test]
    fn jsonc_becomes_json() {
        let text = r#"{
  // a comment with "quotes", and a comma,
  "a": "x // not a comment, /* nor this */",
  /* block
     comment */ "b": [1, 2,],
  "c": {"d": "\"",},
}"#;
        let value: serde_json::Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"a": "x // not a comment, /* nor this */", "b": [1, 2], "c": {"d": "\""}})
        );
    }

    #[test]
    fn pi_is_told_to_export_only_with_a_logs_endpoint_over_http() {
        let launch = |pairs: &[(&str, &str)]| {
            let env = env(pairs);
            let dir = PathBuf::from("/nonexistent");
            let res = resource();
            prepare(&inputs(&AgentType::Pi, "pi", &env, &res, &dir)).env
        };
        let on = vec![(PI_TELEMETRY_ENV.to_string(), "1".to_string())];
        assert_eq!(launch(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c")]), on);
        assert_eq!(
            launch(&[("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "http://c/v1/logs")]),
            on
        );
        assert!(launch(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://t")]).is_empty());
        assert!(
            launch(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"),
                ("OTEL_LOGS_EXPORTER", "none")
            ])
            .is_empty()
        );
        assert!(
            launch(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")
            ])
            .is_empty()
        );
        assert!(
            launch(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"),
                (PI_TELEMETRY_ENV, "0")
            ])
            .is_empty(),
            "the user's own value stands"
        );
    }

    #[test]
    fn other_agents_get_nothing() {
        let env = env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c")]);
        let dir = PathBuf::from("/nonexistent");
        let res = resource();
        for agent in [AgentType::ClaudeCode, AgentType::OpenCode, AgentType::None] {
            assert_eq!(
                prepare(&inputs(&agent, "claude", &env, &res, &dir)),
                Launch::default()
            );
        }
    }
}
