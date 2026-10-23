//! What an agent's launch needs, beyond its environment, for its own
//! telemetry to reach the user's backend tagged with the deck's run — for the
//! agents that do not switch it on from `OTEL_*` variables alone.
//!
//! - **Codex** needs its exporters named in its configuration. The deck's
//!   shim ([`super::shim`]) adds `-c otel.trace_exporter=…` and
//!   `-c otel.metrics_exporter=…` in front of Codex's own arguments, for each
//!   signal that has an endpoint ([`codex_args`]). Never `otel.exporter`:
//!   Codex's log events carry tool arguments and output. No header goes on the
//!   command line — Codex reads the `OTEL_EXPORTER_OTLP_*` header variables
//!   from its environment once an exporter is on. Nothing is added when the
//!   user's Codex configuration names an exporter itself, or the arguments
//!   already pass an `otel.` override; or when an endpoint carries credentials
//!   (a `user:password@` or a query string), which the command line would
//!   show.
//! - **Devin** needs an endpoint in its user configuration and does not read
//!   `OTEL_RESOURCE_ATTRIBUTES`. The shim writes a copy of the user's Devin
//!   configuration with an `otel` block added — the endpoint of each signal
//!   the user's `OTEL_LOGS_EXPORTER` / `OTEL_METRICS_EXPORTER` leave on, the
//!   run's resource attributes, and prompt and tool logging off — to a file
//!   only its owner can read, in a directory only its owner can enter, under
//!   the daemon's state directory, named for the deck's run
//!   ([`devin_args`]); passes it with `--config`; and the daemon removes it
//!   when the run ends. The deck does not copy the user's OTLP header
//!   variables into it, but it is a copy of the user's configuration and holds
//!   whatever that holds. A user's own `otel` block is kept as it is, gaining
//!   only the resource attributes it lacks; one that switches export off,
//!   or arguments that already pass `--config`, get nothing, and no new
//!   `otel` block is created for an endpoint that carries credentials (as
//!   for Codex).
//! - **Pi** exports through the deck's own Pi extension, which stays inert
//!   unless the deck sets [`PI_TELEMETRY_ENV`] — which it does not when the
//!   user set that variable themselves, so `0` keeps Pi's export off, nor when
//!   the agent's environment sets `OTEL_SDK_DISABLED=true` or an
//!   `OTEL_LOGS_EXPORTER` with no `otlp` in it. The extension declines on
//!   those two as well.
//!
//! Codex's and Devin's decisions read the environment the agent really starts
//! with — the shim's own, after whatever script or launcher ran it — so the
//! user's configuration homes, opt-outs and exporter selectors are the ones
//! the agent will see. Any doubt adds nothing.
//!
//! With telemetry off none of this runs: [`super::Telemetry::prepare_run`]
//! returns before calling [`extra_env`], and no shim is on any agent's `PATH`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::event::AgentType;

use super::config::{EnvMap, OTEL_EXPORTER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_PROTOCOL, non_empty};

/// Set in a Pi agent's environment when the deck's Pi extension should export.
pub const PI_TELEMETRY_ENV: &str = "DOT_AGENT_DECK_PI_TELEMETRY";

/// The daemon's variables a launch reads beyond `OTEL_*`: the user's own say
/// on Pi's export, and the `PATH` the shim directory goes in front of.
pub const LAUNCH_ENV_KEYS: [&str; 2] = [PI_TELEMETRY_ENV, "PATH"];

/// The variables naming where Codex's and Devin's configurations live, read
/// as paths: exactly as set (`path_value`).
pub const CONFIG_HOME_ENV_KEYS: [&str; 3] = ["CODEX_HOME", "HOME", "XDG_CONFIG_HOME"];

/// The value of `key`, one of [`CONFIG_HOME_ENV_KEYS`], in `env`, exactly as
/// set — a directory's name may begin or end with a space — or `None` when it
/// is absent or empty.
fn path_value<'a>(env: &'a EnvMap, key: &str) -> Option<&'a str> {
    env.get(key).map(String::as_str).filter(|v| !v.is_empty())
}

/// The prefix of the per-run Devin configuration files.
pub const DEVIN_CONFIG_PREFIX: &str = "devin-";

/// The variables to add to the environment of an agent of kind `agent` whose
/// environment as the deck sees it is `env`: the daemon's `OTEL_*` variables
/// and [`LAUNCH_ENV_KEYS`], with the spawn's own overlay applied.
pub fn extra_env(agent: &AgentType, env: &EnvMap) -> Vec<(String, String)> {
    match agent {
        AgentType::Pi if pi_exports(env) => vec![(PI_TELEMETRY_ENV.to_string(), "1".to_string())],
        _ => Vec::new(),
    }
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
    /// OTLP, with its `OTEL_*_EXPORTER` selector: set, with no `otlp` among
    /// its comma-separated entries.
    fn declined(&self, env: &EnvMap) -> bool {
        non_empty(env, self.exporter)
            .is_some_and(|v| !v.split(',').any(|e| e.trim().eq_ignore_ascii_case("otlp")))
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

/// Whether `env` switches the OTel SDK off.
fn sdk_disabled(env: &EnvMap) -> bool {
    non_empty(env, "OTEL_SDK_DISABLED").is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// Whether one of `args` starts with or contains `needle`, read lossily.
fn any_arg(args: &[OsString], matches: impl Fn(&str) -> bool) -> bool {
    args.iter().any(|a| matches(&a.to_string_lossy()))
}

// ---- Codex ----

/// The arguments to put in front of Codex's own `args` that switch on its
/// traces and metrics exporters in `env`, the environment it starts with, or
/// none — see the module docs.
pub fn codex_args(args: &[OsString], env: &EnvMap) -> Vec<String> {
    if sdk_disabled(env) || any_arg(args, |a| a.contains("otel.")) {
        return Vec::new();
    }
    let Some(home) = codex_home(env) else {
        return Vec::new();
    };
    if codex_config_names_an_exporter(&home.join("config.toml")) != Some(false) {
        return Vec::new();
    }
    let mut out = Vec::new();
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
        out.push("-c".to_string());
        out.push(format!(
            "{key}={{otlp-http={{endpoint={},protocol=\"{protocol}\"}}}}",
            toml_string(&endpoint)
        ));
    }
    out
}

/// The Codex home Codex reads in its environment `env`: its `CODEX_HOME`,
/// else `.codex` in its home directory. `None` when that is not an absolute
/// path.
fn codex_home(env: &EnvMap) -> Option<PathBuf> {
    let absolute = |value: &str| Some(PathBuf::from(value)).filter(|p| p.is_absolute());
    match path_value(env, "CODEX_HOME") {
        Some(value) => absolute(value),
        None => Some(absolute(path_value(env, "HOME")?)?.join(".codex")),
    }
}

/// Whether an endpoint URL may carry a credential that a command line or a
/// file the deck writes would show: a `user:password@` before its host, or a
/// query string.
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

/// What a Devin launch needs: its run, and where its file goes.
pub struct DevinRun<'a> {
    /// The deck's run the launch belongs to, as the deck put it in the
    /// environment; only a run id as [`super::span::new_run_id`] makes them
    /// is used in a file name.
    pub run_id: &'a str,
    /// The deck's directory for per-run configuration files.
    pub config_dir: &'a Path,
    /// Tells this launch's file apart from another Devin's in the same run:
    /// the process id the launched Devin will have.
    pub pid: u32,
}

/// The arguments to put in front of Devin's own `args` — `--config` and the
/// per-run configuration it writes for `run` from `env`, the environment
/// Devin starts with — or `None` when the launch gets nothing; see the module
/// docs.
pub fn devin_args(args: &[OsString], env: &EnvMap, run: &DevinRun<'_>) -> Option<Vec<String>> {
    if !is_run_id(run.run_id) || sdk_disabled(env) || any_arg(args, |a| a.starts_with("--config")) {
        return None;
    }
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
    let resource = non_empty(env, super::config::OTEL_RESOURCE_ATTRIBUTES)
        .map(super::resource::parse_resource_attributes)
        .unwrap_or_default();
    let config = devin_config(user, env, &resource)?;
    let path = run.config_dir.join(config_file_name(run.run_id, run.pid));
    let arg = path.to_str()?.to_string();
    if let Err(e) = write_private(&path, &serde_json::to_vec_pretty(&config).ok()?) {
        tracing::warn!("telemetry: Devin's per-run configuration could not be written ({e})");
        return None;
    }
    Some(vec!["--config".to_string(), arg])
}

/// Devin's user configuration file, resolved as Devin resolves it.
fn devin_user_config(env: &EnvMap) -> Option<PathBuf> {
    if let Some(xdg) = path_value(env, "XDG_CONFIG_HOME").map(Path::new)
        && xdg.is_absolute()
    {
        return Some(xdg.join("devin").join("config.json"));
    }
    let home = Path::new(path_value(env, "HOME")?);
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
            if otel.is_empty()
                || otel
                    .values()
                    .any(|endpoint| endpoint.as_str().is_some_and(may_carry_credentials))
            {
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

/// Whether `id` is a run id as [`super::span::new_run_id`] makes them: 32
/// lowercase hex digits.
fn is_run_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The name of run `run_id`'s Devin configuration for the Devin that will
/// have process id `pid`.
fn config_file_name(run_id: &str, pid: u32) -> String {
    format!("{DEVIN_CONFIG_PREFIX}{run_id}-{pid}.json")
}

/// The run whose configuration file `name` is, when it is named as the deck
/// names them ([`config_file_name`]).
fn config_file_run(name: &[u8]) -> Option<&[u8]> {
    let rest = name
        .strip_prefix(DEVIN_CONFIG_PREFIX.as_bytes())?
        .strip_suffix(b".json")?;
    let Some([b'-', pid @ ..]) = rest.get(32..) else {
        return None;
    };
    let id = &rest[..32];
    let pid_ok = (1..=10).contains(&pid.len()) && pid.iter().all(u8::is_ascii_digit);
    (pid_ok && std::str::from_utf8(id).is_ok_and(is_run_id)).then_some(id)
}

/// Remove every per-run configuration file in `dir`, the deck's own
/// directory for them: called before any run exists, so each one there was
/// left by a daemon that ended without removing it — and at a daemon's
/// shutdown. See `remove_config_files_where`.
pub fn remove_leftover_config_files(dir: &Path) {
    remove_config_files_where(dir, |_| true);
}

/// Remove run `run_id`'s per-run configuration files in `dir`: its run ended.
pub fn remove_run_config_files(dir: &Path, run_id: &str) {
    remove_config_files_where(dir, |run| run == run_id.as_bytes());
}

/// Remove the per-run configuration files in `dir` whose run `which` selects.
/// Only regular files named as the deck names them ([`config_file_run`]) are
/// removed, and only when `dir` is a directory of the current user's, not a
/// symlink; anything else — the shim directory beside them included — is
/// left alone, and a missing `dir` has nothing to remove.
///
/// On Unix the directory is opened once, without following a symlink, and
/// read and unlinked from through that descriptor, so a name swapped in after
/// the check cannot redirect it.
#[cfg(unix)]
fn remove_config_files_where(dir: &Path, which: impl Fn(&[u8]) -> bool) {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let handle = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)
    {
        Ok(handle) => handle,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(
                "telemetry: agent configurations not removed: their directory could not be opened as a directory of its own ({e})"
            );
            return;
        }
    };
    match handle.metadata() {
        Ok(meta) if meta.is_dir() && meta.uid() == crate::platform::paths::current_uid() => {}
        _ => {
            tracing::warn!(
                "telemetry: agent configurations not removed: their directory is not the current user's"
            );
            return;
        }
    }
    for name in dir_entry_names(&handle) {
        if config_file_run(name.to_bytes()).is_some_and(&which)
            && is_regular_file_at(&handle, &name)
        {
            unlink_at(&handle, &name);
        }
    }
}

/// [`remove_config_files_where`] where there is no descriptor-relative
/// unlink: the root is checked to be a real directory and then read through
/// its path, which a directory swapped in between the check and the read
/// would redirect.
#[cfg(not(unix))]
fn remove_config_files_where(dir: &Path, which: impl Fn(&[u8]) -> bool) {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.is_dir() => {}
        _ => return,
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if config_file_run(entry.file_name().as_encoded_bytes()).is_some_and(&which)
            && entry.file_type().is_ok_and(|t| t.is_file())
        {
            remove_config_file(&entry.path());
        }
    }
}

/// The names of the entries of the directory open at `dir`, `.` and `..`
/// left out. Empty when it cannot be read.
#[cfg(unix)]
fn dir_entry_names(dir: &std::fs::File) -> Vec<std::ffi::CString> {
    use std::os::fd::AsRawFd;
    // `fdopendir` takes ownership of the descriptor it is given, so it gets
    // its own copy.
    // SAFETY: `dup` of a descriptor `dir` owns; the result is checked.
    let fd = unsafe { libc::dup(dir.as_raw_fd()) };
    if fd < 0 {
        return Vec::new();
    }
    // SAFETY: `fd` is a fresh descriptor of a directory, owned by nothing else.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        // SAFETY: `fdopendir` failed, so `fd` is still ours to close.
        unsafe { libc::close(fd) };
        return Vec::new();
    }
    let mut names = Vec::new();
    loop {
        // SAFETY: `stream` is an open directory stream, read by this thread
        // only.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: `readdir` returned an entry whose `d_name` is a
        // NUL-terminated name, valid until the next `readdir`.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() != b"." && name.to_bytes() != b".." {
            names.push(name.to_owned());
        }
    }
    // SAFETY: closes the stream and the descriptor `fdopendir` took.
    unsafe { libc::closedir(stream) };
    names
}

/// Whether the entry `name` of the directory open at `dir` is a regular file,
/// a symlink not followed.
#[cfg(unix)]
fn is_regular_file_at(dir: &std::fs::File, name: &std::ffi::CStr) -> bool {
    use std::os::fd::AsRawFd;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `name` is NUL-terminated and `st` is written by a successful
    // call, read only then.
    let rc = unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    // SAFETY: `fstatat` returned 0, so it filled `st`.
    rc == 0 && unsafe { st.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFREG
}

/// Unlink the entry `name` of the directory open at `dir`. `unlinkat` removes
/// a name, never what a symlink points at.
#[cfg(unix)]
fn unlink_at(dir: &std::fs::File, name: &std::ffi::CStr) {
    use std::os::fd::AsRawFd;
    // SAFETY: `name` is NUL-terminated; a failure is read from `errno`.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("telemetry: a per-run agent configuration could not be removed ({e})");
        }
    }
}

// ---- Pi ----

/// Whether the deck's Pi extension has somewhere to send logs: a logs
/// endpoint, over OTLP/HTTP, not declined by the user — with the extension's
/// own variable, `OTEL_SDK_DISABLED` or `OTEL_LOGS_EXPORTER`. The extension
/// sends OTLP/JSON, and reads those two opt-outs again itself.
fn pi_exports(env: &EnvMap) -> bool {
    non_empty(env, PI_TELEMETRY_ENV).is_none()
        && !sdk_disabled(env)
        && LOGS.endpoint(env).is_some()
        && LOGS.http_protocol(env).is_some()
}

/// `value` as one POSIX shell word.
pub(super) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const RUN: &str = "0123456789abcdef0123456789abcdef";

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn codex(pairs: &[(&str, &str)]) -> String {
        codex_args(&[], &env(pairs)).join(" ")
    }

    #[test]
    fn codex_gets_trace_and_metric_exporters_and_nothing_else() {
        let home = tempfile::tempdir().unwrap();
        let env = env(&[
            ("HOME", home.path().to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://api.example.com/"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=SECRET-VALUE"),
        ]);
        let args = codex_args(&os(&["--model", "m"]), &env);
        assert_eq!(
            args,
            [
                "-c",
                r#"otel.trace_exporter={otlp-http={endpoint="https://api.example.com/v1/traces",protocol="binary"}}"#,
                "-c",
                r#"otel.metrics_exporter={otlp-http={endpoint="https://api.example.com/v1/metrics",protocol="binary"}}"#,
            ]
        );
        let joined = args.join(" ");
        assert!(!joined.contains("SECRET-VALUE"), "no header on argv");
        assert!(!joined.contains("otel.exporter="), "no log exporter");
    }

    #[test]
    fn codex_signals_follow_their_own_endpoint_protocol_and_selector() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_str().unwrap();
        let traces_only = codex(&[
            ("HOME", h),
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "http://t:4318/v1/traces",
            ),
        ]);
        assert!(traces_only.contains("otel.trace_exporter"));
        assert!(!traces_only.contains("metrics"), "{traces_only}");
        let json = codex(&[
            ("HOME", h),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "http/json"),
            ("OTEL_METRICS_EXPORTER", "none"),
        ]);
        assert!(json.contains(r#"protocol="json""#), "{json}");
        assert!(
            !json.contains("metrics"),
            "the user declined metrics: {json}"
        );
        let no_traces = codex(&[
            ("HOME", h),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            ("OTEL_TRACES_EXPORTER", "none"),
        ]);
        assert!(!no_traces.contains("trace_exporter"), "{no_traces}");
        assert!(no_traces.contains("otel.metrics_exporter"), "{no_traces}");
        assert_eq!(
            codex(&[
                ("HOME", h),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4317"),
                ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
            ]),
            ""
        );
        assert_eq!(
            codex(&[
                ("HOME", h),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
                ("OTEL_SDK_DISABLED", "TRUE"),
            ]),
            "",
            "the SDK is off"
        );
        assert_eq!(codex(&[("HOME", h)]), "", "no endpoint");
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
        let h = home.path().to_str().unwrap();
        let endpoint = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318");
        assert_eq!(
            codex(&[
                ("HOME", h),
                ("CODEX_HOME", codex_home.to_str().unwrap()),
                endpoint
            ]),
            "",
            "CODEX_HOME's config names one"
        );
        assert_ne!(codex(&[("HOME", h), endpoint]), "", "~/.codex has none");
        assert!(
            codex_args(
                &os(&["-c", "otel.exporter=x"]),
                &env(&[("HOME", h), endpoint])
            )
            .is_empty(),
            "the arguments set their own"
        );
        assert_eq!(
            codex(&[("HOME", "relative"), endpoint]),
            "",
            "a home that is not absolute"
        );
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(home.path().join(".codex/config.toml"), "not = [toml").unwrap();
        assert_eq!(
            codex(&[("HOME", h), endpoint]),
            "",
            "an unreadable config is left alone"
        );
    }

    /// An endpoint that may carry a credential is not put on a command line.
    #[test]
    fn codex_gets_nothing_for_an_endpoint_with_credentials() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path().to_str().unwrap();
        for endpoint in [
            "https://user:synthetic-password-1741@collector.example",
            "https://synthetic-token-1741@collector.example:4318/",
            "https://collector.example/v1?api_key=synthetic-token-1741",
        ] {
            assert_eq!(
                codex(&[("HOME", h), ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)]),
                "",
                "{endpoint}"
            );
        }
        assert_eq!(
            codex(&[
                ("HOME", h),
                ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://collector.example"),
                (
                    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                    "https://u:p@m.example/v1/metrics"
                ),
            ]),
            "",
            "one credential-bearing signal declines both"
        );
        assert_ne!(
            codex(&[
                ("HOME", h),
                (
                    "OTEL_EXPORTER_OTLP_ENDPOINT",
                    "https://collector.example/a@b"
                )
            ]),
            ""
        );
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn run(dir: &Path) -> DevinRun<'_> {
        DevinRun {
            run_id: RUN,
            config_dir: dir,
            pid: 42,
        }
    }

    /// The environment a Devin under the deck starts with: its resource
    /// attributes carry the run.
    fn devin_env(home: &Path, pairs: &[(&str, &str)]) -> EnvMap {
        let mut all = vec![
            ("HOME", home.to_str().unwrap()),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "team=platform,deck.run.id=run-1",
            ),
        ];
        all.extend_from_slice(pairs);
        env(&all)
    }

    fn read_config(args: &[String]) -> (PathBuf, serde_json::Value) {
        assert_eq!(args[0], "--config", "{args:?}");
        let path = PathBuf::from(&args[1]);
        let config = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        (path, config)
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
        let env = devin_env(
            home.path(),
            &[("OTEL_EXPORTER_OTLP_HEADERS", "authorization=SECRET-VALUE")],
        );
        let args = devin_args(&os(&["--yolo"]), &env, &run(&state)).expect("a config");
        assert_eq!(args.len(), 2, "{args:?}");
        let (path, config) = read_config(&args);
        assert_eq!(path, state.join(format!("devin-{RUN}-42.json")));
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&state), 0o700);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("SECRET-VALUE")
        );
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
        remove_config_file(&path);
        assert!(!path.exists());
    }

    fn resource() -> Vec<(String, String)> {
        vec![
            ("team".into(), "platform".into()),
            ("deck.run.id".into(), "run-1".into()),
        ]
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
        let env = devin_env(home.path(), &[]);
        let elsewhere = home.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let linked = home.path().join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        assert_eq!(devin_args(&[], &env, &run(&linked)), None);
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

        let open = home.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        assert!(devin_args(&[], &env, &run(&open)).is_some());
        assert_eq!(
            mode(&open),
            0o700,
            "an existing directory is made owner-only"
        );
        assert_eq!(
            devin_args(&[], &env, &run(&open)),
            None,
            "an existing file is never overwritten"
        );
    }

    /// The sweep reads only a directory of the user's own, not one a symlink
    /// leads to, and removes only names the deck gives its files.
    #[test]
    fn the_leftover_sweep_stays_in_its_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let ours = config_file_name(&super::super::span::new_run_id(), 7);
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join(&ours), "{}").unwrap();
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        remove_leftover_config_files(&linked);
        assert!(
            elsewhere.join(&ours).exists(),
            "a file behind a symlinked directory survives"
        );

        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join(&ours), "{}").unwrap();
        std::fs::write(real.join("devin-not-owned.json"), "{}").unwrap();
        std::fs::create_dir(real.join("bin")).unwrap();
        std::fs::write(real.join("bin").join(&ours), "{}").unwrap();
        let target = real.join("target.json");
        std::fs::write(&target, "{}").unwrap();
        let link = config_file_name(&super::super::span::new_run_id(), 8);
        std::os::unix::fs::symlink(&target, real.join(&link)).unwrap();
        remove_leftover_config_files(&real);
        assert!(
            !real.join(&ours).exists(),
            "the deck's own leftover is removed"
        );
        assert!(real.join("devin-not-owned.json").exists());
        assert!(
            real.join("bin").join(&ours).exists(),
            "the shim directory is not swept"
        );
        assert!(target.exists(), "a symlink's target is never removed");
        assert!(
            std::fs::symlink_metadata(real.join(&link)).is_ok(),
            "only regular files are removed"
        );

        remove_leftover_config_files(&root.path().join("absent"));
        let file = root.path().join("file");
        std::fs::write(&file, "").unwrap();
        remove_leftover_config_files(&file);
        assert!(file.exists());
    }

    /// A run's end removes its own files, every Devin of it, and no other run's.
    #[test]
    fn a_runs_end_removes_only_its_own_files() {
        let dir = tempfile::tempdir().unwrap();
        let other = "fedcba9876543210fedcba9876543210";
        let names = [
            config_file_name(RUN, 1),
            config_file_name(RUN, 2),
            config_file_name(other, 1),
        ];
        for name in &names {
            std::fs::write(dir.path().join(name), "{}").unwrap();
        }
        remove_run_config_files(dir.path(), RUN);
        assert!(!dir.path().join(&names[0]).exists());
        assert!(!dir.path().join(&names[1]).exists());
        assert!(dir.path().join(&names[2]).exists(), "another run's stays");
    }

    fn is_config_file_name(name: &[u8]) -> bool {
        config_file_run(name).is_some()
    }

    #[test]
    fn config_file_names_are_the_ones_the_deck_gives() {
        let id = super::super::span::new_run_id();
        assert!(is_run_id(&id));
        assert!(is_config_file_name(config_file_name(&id, 1).as_bytes()));
        assert!(is_config_file_name(
            config_file_name(&id, u32::MAX).as_bytes()
        ));
        for name in [
            "devin-.json",
            "devin-0123456789abcdef0123456789abcdef.json",
            "devin-run-1.json",
            "devin-0123456789abcdef0123456789abcdeg.json",
            "devin-0123456789ABCDEF0123456789abcdef.json",
            "devin-0123456789abcdef0123456789abcdef0.json",
            "devin-0123456789abcdef0123456789abcdef-.json",
            "devin-0123456789abcdef0123456789abcdef-1x.json",
            "devin-0123456789abcdef0123456789abcdef-12345678901.json",
            "devin-0123456789abcdef0123456789abcdef.json.bak",
            "xdevin-0123456789abcdef0123456789abcdef.json",
        ] {
            assert!(!is_config_file_name(name.as_bytes()), "{name}");
        }
    }

    /// An endpoint that may carry a credential is not written into Devin's
    /// file either.
    #[test]
    fn devin_gets_nothing_for_an_endpoint_with_credentials() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        for pairs in [
            &[(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "https://user:synthetic-password-1741@collector.example",
            )][..],
            &[(
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "https://collector.example/?api_key=synthetic-token-1741",
            )][..],
            &[(
                "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
                "https://synthetic-token-1741@l.example/v1/logs",
            )][..],
        ] {
            let env = devin_env(home.path(), pairs);
            assert_eq!(devin_args(&[], &env, &run(&state)), None, "{pairs:?}");
        }
        assert!(!state.exists(), "nothing was written");
        let plain = devin_env(home.path(), &[]);
        assert!(
            devin_args(&[], &plain, &run(&state)).is_some(),
            "the control gets its file"
        );
    }

    #[test]
    fn devin_gets_nothing_when_it_should_not() {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("state");
        let none = |args: &[&str], pairs: &[(&str, &str)], run_id: &str| {
            let env = devin_env(home.path(), pairs);
            let run = DevinRun {
                run_id,
                ..run(&state)
            };
            devin_args(&os(args), &env, &run).is_none()
        };
        assert!(none(&["--config", "/mine.json"], &[], RUN));
        assert!(none(&["--config=/mine.json"], &[], RUN));
        assert!(none(&[], &[("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")], RUN));
        assert!(none(&[], &[("OTEL_SDK_DISABLED", "true")], RUN));
        assert!(none(&[], &[], "../escape"), "only a run id names a file");
        assert!(none(&[], &[], ""));
        std::fs::create_dir_all(home.path().join(".config/devin")).unwrap();
        std::fs::write(home.path().join(".config/devin/config.json"), "{ broken").unwrap();
        assert!(none(&[], &[], RUN), "a config it cannot read is left alone");
        assert!(!state.exists(), "nothing was written");
    }

    /// Devin's configuration and signals are read from the environment it
    /// starts with.
    #[test]
    fn devin_follows_its_environment() {
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
        let mut pid = 0;
        let mut launch = |pairs: &[(&str, &str)]| {
            pid += 1;
            let env = devin_env(home.path(), pairs);
            devin_args(&[], &env, &DevinRun { pid, ..run(&state) })
        };
        assert_eq!(launch(&[("XDG_CONFIG_HOME", off.to_str().unwrap())]), None);
        assert_eq!(
            launch(&[
                ("OTEL_LOGS_EXPORTER", "none"),
                ("OTEL_METRICS_EXPORTER", "none")
            ]),
            None
        );
        let (_, logs_only) = read_config(&launch(&[("OTEL_METRICS_EXPORTER", "none")]).unwrap());
        assert_eq!(logs_only["otel"]["logs_endpoint"], "http://c:4318/v1/logs");
        assert!(logs_only["otel"].get("metrics_endpoint").is_none());
        let (_, own) =
            read_config(&launch(&[("XDG_CONFIG_HOME", mine.to_str().unwrap())]).unwrap());
        assert_eq!(own["theme"], "mine", "the launched Devin's own config");
        let (_, retagged) = read_config(
            &launch(&[("OTEL_RESOURCE_ATTRIBUTES", "deck.run.id=from-the-env")]).unwrap(),
        );
        assert_eq!(
            retagged["otel"]["resource_attributes"],
            serde_json::json!({"deck.run.id": "from-the-env"}),
            "the resource attributes Devin starts with"
        );
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

    /// A configuration home is read exactly as set: a directory whose name
    /// ends in a space is that directory, not the one without the space, so
    /// an opt-out there is honoured. Empty is unset.
    #[test]
    fn configuration_homes_are_read_exactly_as_set() {
        let root = tempfile::tempdir().unwrap();
        let spaced = format!("{} ", root.path().join("conf").display());
        std::fs::create_dir_all(Path::new(&spaced).join("devin")).unwrap();
        std::fs::write(
            Path::new(&spaced).join("config.toml"),
            "[otel]\ntrace_exporter = \"none\"\nmetrics_exporter = \"none\"\n",
        )
        .unwrap();
        std::fs::write(
            Path::new(&spaced).join("devin/config.json"),
            r#"{"otel":{"enabled":false}}"#,
        )
        .unwrap();
        let endpoint = ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c:4318");
        assert_eq!(codex(&[("CODEX_HOME", &spaced), endpoint]), "", "Codex");
        let home = root.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let state = root.path().join("state");
        let devin =
            |pairs: &[(&str, &str)]| devin_args(&[], &devin_env(&home, pairs), &run(&state));
        assert_eq!(devin(&[("XDG_CONFIG_HOME", &spaced)]), None, "Devin");
        let e = env(&[("HOME", "/h "), ("XDG_CONFIG_HOME", "")]);
        assert_eq!(
            devin_user_config(&e),
            Some(PathBuf::from("/h /.config/devin/config.json"))
        );
        assert_eq!(
            codex_home(&env(&[("HOME", "/h"), ("CODEX_HOME", "")])),
            Some(PathBuf::from("/h/.codex"))
        );
        assert_eq!(
            codex_home(&env(&[("CODEX_HOME", " /c")])),
            None,
            "not absolute"
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
        let launch = |pairs: &[(&str, &str)]| extra_env(&AgentType::Pi, &env(pairs));
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
        for opt_out in [
            ("OTEL_SDK_DISABLED", "true"),
            ("OTEL_SDK_DISABLED", " TRUE "),
            ("OTEL_LOGS_EXPORTER", "console"),
            ("OTEL_LOGS_EXPORTER", "otlpx, console"),
        ] {
            assert!(
                launch(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"), opt_out]).is_empty(),
                "{opt_out:?}"
            );
        }
        for opt_in in [
            ("OTEL_SDK_DISABLED", "false"),
            ("OTEL_LOGS_EXPORTER", "console, OTLP"),
        ] {
            assert_eq!(
                launch(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c"), opt_in]),
                on,
                "{opt_in:?}"
            );
        }
    }

    #[test]
    fn other_agents_get_no_extra_env() {
        let env = env(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://c")]);
        for agent in [
            AgentType::ClaudeCode,
            AgentType::OpenCode,
            AgentType::Codex,
            AgentType::Devin,
            AgentType::None,
        ] {
            assert!(extra_env(&agent, &env).is_empty(), "{agent:?}");
        }
    }
}
