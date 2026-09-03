//! PRD #20 W1 — install Codex's native hooks into the active `CODEX_HOME`.
//!
//! Codex 0.144.4 ships a Claude-Code-compatible hooks engine, so its command
//! hooks POST the same stdin JSON shape Claude does and are ingested by the
//! existing [`crate::hook::handle_hook`] `"codex"` arm. This module writes the
//! hook DEFINITIONS — a `hooks.json` whose every command shells
//! `dot-agent-deck hook --agent codex` — into the Codex home the spawned `codex`
//! child reads, so a live interactive session's prompt / tool / turn events ride
//! the deck's existing raw-`AgentEvent` hook socket (no new wire, no
//! `PROTOCOL_VERSION` bump — rule 12).
//!
//! It is the Codex analog of [`crate::hooks_manage::auto_install`] (Claude):
//! **guarded, silent, and SAFE for the user's real `~/.codex`**. Deliberate
//! choices vs. Claude:
//!
//! - We write a SEPARATE `hooks.json` — Codex's highest-precedence,
//!   auto-discovered hook source (`$CODEX_HOME/hooks.json`) — rather than
//!   editing `config.toml`, so the user's real `~/.codex/config.toml` (auth
//!   references, model, project trust, skills, history) is never touched.
//! - We MERGE, never clobber: pre-existing user hooks are preserved; only prior
//!   deck-authored entries (identified by the EXACT command signature
//!   [`HOOK_COMMAND_SUFFIX`], not a loose `dot-agent-deck` substring) are
//!   refreshed, so re-installs never accumulate duplicates and a user hook that
//!   merely mentions `dot-agent-deck` is never deleted (finding #14).
//! - The write is ATOMIC (temp file in the same dir + `rename(2)`) and guarded
//!   by an in-process mutex, so a crash mid-write can't truncate the file and two
//!   panes launching Codex concurrently can't clobber each other (finding #1/M-2).
//! - We treat ONLY `NotFound` as an empty config. Unreadable, malformed, or
//!   structurally-incompatible existing content is NEVER silently discarded:
//!   malformed JSON is backed up to `hooks.json.bak` and the install errors;
//!   a structurally-incompatible shape errors WITHOUT touching the file
//!   (findings #1, L-2).
//!
//! Trust (PRD #20 §4.1, Greptile P1): Codex requires non-managed command hooks
//! to be trusted before they run. The deck used to launch `codex` with
//! `--dangerously-bypass-hook-trust`, which is INVOCATION-GLOBAL — it trusts
//! every enabled hook in the active `CODEX_HOME`, including the user's own
//! untrusted third-party hooks — and, being argv, could be forwarded by any
//! launcher to a Codex reading hooks the deck never inspected. **That flag is
//! gone.** Codex 0.144.4 does expose a *scoped, per-hook* trust store, and the
//! deck now writes exactly that instead:
//!
//! - [`list_hooks_in`] asks Codex itself (`codex app-server` → `hooks/list`) for
//!   each hook's `key`, `currentHash`, `sourcePath`, `command`, and `isManaged`.
//! - [`deck_owned_entries`] keeps ONLY entries whose `sourcePath` is the pinned
//!   home's own `hooks.json`, whose command carries the deck signature, and which
//!   are not `isManaged`. **This is the security predicate — the only path to a
//!   trust write.**
//! - [`trust_deck_hooks_in`] records `[hooks.state."<key>"] { enabled,
//!   trusted_hash }` in `<home>/config.toml` for exactly those keys.
//!
//! The result is strictly narrower than the old bypass and launch-method
//! agnostic: trust lives in the home (not argv), so `codex`, `devbox run
//! codex-big`, and `./run_codex.sh` behave identically, a third-party hook in the
//! very same `hooks.json` stays untrusted, and any edit to a trusted definition
//! flips it to `modified` so Codex refuses it (fail-closed — events degrade to
//! the coarse stdout classifier, never silent over-trust). Residual: a launcher
//! that re-exports `CODEX_HOME` escapes the pin, but that re-homed Codex then has
//! neither our `hooks.json` nor our trust records — a functionality loss, not a
//! trust leak (see `docs/develop/agent-adapters.md`).
//!
//! The `config.toml` edit is FORMAT-PRESERVING (`toml_edit`): only the
//! `hooks.state."<key>"` tables are inserted/replaced, so the user's comments,
//! `model = …`, auth references, and their own trust records survive byte-intact.

use std::io::{self, BufRead as _, BufReader, ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The fixed command signature that identifies a deck-authored Codex hook. Every
/// deck hook command is `<binary_path> hook --agent codex`, so a command ending
/// in this exact suffix is deck-owned. Matching the full verb (rather than the
/// `dot-agent-deck` substring) means a user hook that merely mentions
/// `dot-agent-deck` in an argument is never mistaken for a deck entry
/// (finding #14).
const HOOK_COMMAND_SUFFIX: &str = "hook --agent codex";

/// The interpreter every Codex hook command is quoted for. Unlike Devin's,
/// this one genuinely follows the host: Codex's hooks engine runs the command
/// through `%COMSPEC%`/`cmd.exe /C` on Windows and `$SHELL`/`/bin/sh -lc`
/// elsewhere, and `codex_home` honours `$CODEX_HOME` on every platform, so the
/// Windows arm is reachable. See `agent_hook_config::HookShell`.
const HOOK_SHELL: crate::agent_hook_config::HookShell = crate::agent_hook_config::HookShell::Native;

/// Serializes the read-modify-write of `hooks.json` across concurrent in-process
/// Codex spawns (two panes launching `codex` at once). Combined with the atomic
/// temp-file+rename publish, this closes the concurrent-clobber / partial-write
/// window on the user's real `~/.codex/hooks.json` (finding #1/M-2).
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

/// Codex hook events we install a command handler for. Every name maps to an
/// [`crate::event::EventType`] via [`crate::hook`]'s `map_event_type`, and Codex
/// fires these at the engine level (shared by the interactive TUI and
/// `codex exec`). Covers the lifecycle (`SessionStart`/`Stop`), prompt
/// (`UserPromptSubmit`), tool (`Pre`/`PostToolUse`), permission, compaction, and
/// subagent boundaries — the same class Claude delivers.
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PermissionRequest",
    "Stop",
    "PreCompact",
    "PostCompact",
    "SubagentStart",
    "SubagentStop",
];

/// Resolve the active Codex home the way Codex itself does: `$CODEX_HOME` when
/// set (and non-empty), else `$HOME/.codex`. Returns `None` when neither is
/// available so a guarded caller never falls back to a throwaway/`/tmp` home —
/// in production this is the user's REAL `~/.codex`, preserving auth/skills/
/// history (per the PRD design).
///
/// **Windows: deliberately a no-op, not an oversight (PRD #163 M1, reconfirmed in
/// review).** `$HOME` is normally unset on Windows, so this returns `None` and
/// every caller degrades to a documented skip. That is the correct outcome: the
/// path we would have to guess belongs to a *third-party* tool, and Codex — not
/// this project — decides where its home lives on Windows. Writing hooks into a
/// location Codex does not read would be worse than not installing them: it looks
/// like success and silently delivers nothing. Set `$CODEX_HOME` (which is
/// honoured on every platform, above) to install Codex hooks on Windows.
fn codex_home() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".codex"))
}

/// Whether a command string is a deck-authored hook, by EXACT signature: it ends
/// with [`HOOK_COMMAND_SUFFIX`] (`… hook --agent codex`). A user command that
/// merely contains `dot-agent-deck` (e.g. `audit-wrapper --watch dot-agent-deck`)
/// is NOT deck-owned and is preserved (finding #14).
fn command_is_deck_owned(command: &str) -> bool {
    command.trim_end().ends_with(HOOK_COMMAND_SUFFIX)
}

/// Whether a hooks.json rule was authored by the deck — i.e. one of its command
/// handlers is a deck hook by [`command_is_deck_owned`]. Used to strip stale deck
/// entries before re-adding fresh ones, so re-installs are idempotent and never
/// touch a user's own hooks.
fn rule_is_dot_agent_deck(rule: &Value) -> bool {
    rule.get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(command_is_deck_owned)
            })
        })
}

/// Merge the deck's command hooks for `command` into an existing `hooks.json`
/// value (or `{}`), preserving any user-authored hooks and refreshing (not
/// duplicating) prior deck entries.
fn install_impl(root: &mut Value, command: &str) {
    if !root.is_object() {
        *root = json!({});
    }
    let obj = root.as_object_mut().expect("root is an object");
    if !obj.get("hooks").is_some_and(Value::is_object) {
        obj.insert("hooks".into(), json!({}));
    }
    let hooks = obj
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .expect("hooks is an object");

    // Strip any stale deck-authored entries from every event so a re-install
    // normalizes down to exactly one fresh deck rule per event.
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        if let Some(arr) = hooks.get_mut(&key).and_then(Value::as_array_mut) {
            arr.retain(|rule| !rule_is_dot_agent_deck(rule));
        }
    }

    let entry = json!({
        "hooks": [ { "type": "command", "command": command } ]
    });
    for &event in CODEX_HOOK_EVENTS {
        let arr = hooks.entry(event.to_string()).or_insert_with(|| json!([]));
        if !arr.is_array() {
            *arr = json!([]);
        }
        arr.as_array_mut()
            .expect("hook event value is an array")
            .push(entry.clone());
    }
}

/// Reject a structurally-incompatible existing `hooks.json` shape without
/// mutating it. Accepts a missing `hooks` key (created on install) and an empty
/// object, but rejects a non-object root, a non-object `hooks`, or any event
/// value that is not an array — so a merge never silently replaces user content
/// it doesn't understand (finding #1).
fn validate_structure(root: &Value) -> io::Result<()> {
    let incompatible = |what: &str| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("existing hooks.json is structurally incompatible: {what}"),
        )
    };
    if !root.is_object() {
        return Err(incompatible("root is not a JSON object"));
    }
    let Some(hooks) = root.get("hooks") else {
        return Ok(()); // missing `hooks` is fine — install creates it
    };
    let Some(hooks) = hooks.as_object() else {
        return Err(incompatible("`hooks` is not a JSON object"));
    };
    for (event, value) in hooks {
        if !value.is_array() {
            return Err(incompatible(&format!(
                "hook event `{event}` is not an array"
            )));
        }
    }
    Ok(())
}

/// Testable core: merge the deck's hooks into `<codex_home>/hooks.json`, writing
/// the file atomically (creating the home dir if needed). `binary_path` is the
/// absolute `dot-agent-deck` path the hook command should invoke.
///
/// Safety contract (findings #1, #14, L-2, M-2): the read-modify-write is
/// serialized by [`INSTALL_LOCK`] and published atomically. Only a missing file
/// is treated as empty. Malformed JSON is backed up to `hooks.json.bak` and the
/// call errors (never discarded); a structurally-incompatible shape errors
/// WITHOUT touching the file; unreadable content propagates its error unwritten.
pub fn install_to(codex_home: &Path, binary_path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(codex_home)?;
    let path = codex_home.join("hooks.json");

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let mut root = match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(value) => value,
            Err(parse_err) => {
                // Preserve the user's bytes (best-effort backup) and refuse to
                // overwrite — never discard content we couldn't parse.
                let backup = codex_home.join("hooks.json.bak");
                let _ = std::fs::write(&backup, &bytes);
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "existing hooks.json is not valid JSON (preserved at {}): {parse_err}",
                        backup.display()
                    ),
                ));
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => json!({}),
        // Unreadable (permissions, etc.): propagate rather than overwrite.
        Err(e) => return Err(e),
    };

    validate_structure(&root)?;

    let command =
        crate::agent_hook_config::build_command(binary_path, HOOK_COMMAND_SUFFIX, HOOK_SHELL);
    install_impl(&mut root, &command);
    let contents = serde_json::to_string_pretty(&root)?;
    crate::agent_hook_config::write_atomic(codex_home, &path, contents.as_bytes())
}

/// Whether the active `CODEX_HOME`'s `hooks.json` declares any command hook NOT
/// authored by the deck. Returns:
/// - `Ok(false)` when the file is absent or contains only deck-owned command
///   hooks;
/// - `Ok(true)` when a non-deck command hook is present, or when no `CODEX_HOME`
///   resolves (conservative default);
/// - `Err` when the file exists but is unreadable/malformed.
///
/// **DIAGNOSTIC ONLY (PRD #20 §4.1.4).** This used to be a *precondition* for
/// injecting the invocation-global `--dangerously-bypass-hook-trust`, since that
/// flag would have trusted the user's third-party hooks along with the deck's.
/// That flag is deleted: trust is now per-hook, hash-pinned, and scoped by
/// [`deck_owned_entries`], so a foreign hook in the very same `hooks.json` simply
/// stays untrusted while the deck's own entries are trusted. Do NOT reinstate the
/// coupling — gating scoped trust on this would only degrade users who happen to
/// have hooks of their own.
///
/// It inspects `CODEX_HOME/hooks.json` only. Project-local (`<repo>/.codex`),
/// plugin, and `config.toml`-defined hooks are NOT inspected here; the residual
/// is documented in `docs/develop/agent-adapters.md`.
pub fn foreign_command_hooks_present() -> std::io::Result<bool> {
    let Some(home) = codex_home() else {
        return Ok(true);
    };
    foreign_command_hooks_present_in(&home)
}

/// The resolved active Codex home the way Codex itself would (`$CODEX_HOME`, else
/// `$HOME/.codex`), or `None` when neither resolves. PRD #20 Greptile finding #2:
/// the wrapper resolves this ONCE and PINS it on the spawned Codex child's
/// environment so the home the deck vetted and installed into is exactly the home
/// Codex loads — vet and launch use the same, deck-controlled home instead of a
/// value a launcher could drift between the two.
pub fn active_codex_home() -> Option<PathBuf> {
    codex_home()
}

/// Like [`foreign_command_hooks_present`] but against an EXPLICIT `home`, so a
/// caller can resolve the home once and vet the SAME path it will pin on the
/// child (finding #2). Returns `Ok(false)` when the file is absent.
pub fn foreign_command_hooks_present_in(home: &Path) -> std::io::Result<bool> {
    let path = home.join("hooks.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let root: Value =
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
    Ok(any_foreign_command_hook(&root))
}

/// Walk `root.hooks.<event>[].hooks[]` and report whether any `type == "command"`
/// handler's command is NOT deck-owned (per [`command_is_deck_owned`]).
fn any_foreign_command_hook(root: &Value) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    for rules in hooks.values() {
        let Some(rules) = rules.as_array() else {
            continue;
        };
        for rule in rules {
            let Some(handlers) = rule.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for handler in handlers {
                if handler.get("type").and_then(Value::as_str) != Some("command") {
                    continue;
                }
                let foreign = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| !command_is_deck_owned(command));
                if foreign {
                    return true;
                }
            }
        }
    }
    false
}

/// Silently install the Codex hooks into the active `CODEX_HOME`. Guarded
/// (`CODEX_HOME`/`HOME` must resolve), idempotent-overwrite, never a
/// throwaway/`/tmp` write. Invoked from the wrapper ([`crate::wrap::run_wrap`])
/// just before a `codex` child is spawned, so the hooks are on disk before Codex
/// boots and discovers them. Failures are swallowed (best-effort, like Claude's
/// `auto_install`): a missing home or unwritable dir degrades to the coarse
/// stdout fallback rather than blocking the spawn.
pub fn auto_install() {
    let Some(home) = codex_home() else {
        return;
    };
    // PRD #381: never `current_exe()` directly — a `target/debug` path written
    // here is gone the moment its worktree is pruned, and this write is silent
    // and automatic. A refusal writes nothing and warns; `hooks.json` is left
    // exactly as it was.
    let binary_path = match crate::platform::paths::durable_binary_path() {
        Ok(binary_path) => binary_path,
        Err(e) => {
            tracing::warn!("auto-install: {e}");
            return;
        }
    };
    if let Err(e) = install_to(&home, &binary_path) {
        tracing::warn!("auto-install: failed to write Codex hooks.json: {e}");
    }
}

/// Remove the deck's own hook rules from `<codex_home>/hooks.json`, leaving every
/// user-authored rule (and any file the deck can't parse) alone. The counterpart
/// of [`install_to`], backing `dot-agent-deck hooks uninstall --agent codex`
/// (PRD #20 §4.2.1 — the documented CLI). A missing file is a no-op.
pub fn uninstall_from(codex_home: &Path) -> std::io::Result<()> {
    let path = codex_home.join("hooks.json");

    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut root: Value = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(ErrorKind::InvalidData, format!("hooks.json: {e}")))?;
    validate_structure(&root)?;
    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for value in hooks.values_mut() {
            if let Some(arr) = value.as_array_mut() {
                arr.retain(|rule| !rule_is_dot_agent_deck(rule));
            }
        }
        hooks.retain(|_, value| !value.as_array().is_some_and(|arr| arr.is_empty()));
    }
    let contents = serde_json::to_string_pretty(&root)?;
    crate::agent_hook_config::write_atomic(codex_home, &path, contents.as_bytes())
}

// ---------------------------------------------------------------------------
// PRD #20 §4.1.1/§4.1.2 — scoped, hash-pinned per-hook trust
// ---------------------------------------------------------------------------

/// The name of Codex's per-hook trust store, relative to the Codex home. Trust
/// records live ONLY here: writing a `state` key inside `hooks.json` is rejected
/// by Codex outright (`unknown field 'state'`), which stops the whole file from
/// parsing.
const CONFIG_TOML: &str = "config.toml";

/// How long [`list_hooks_in`] waits for `codex app-server` to answer before giving
/// up (measured round trip on a real Codex 0.144.4: ~0.17 s). The listing is
/// best-effort — a timeout returns `Err` and the caller degrades.
const HOOKS_LIST_TIMEOUT: Duration = Duration::from_secs(5);

/// One hook as Codex itself reports it from `hooks/list`. The deck never computes
/// these values: `current_hash` in particular is Codex's own canonicalization of
/// the definition, so recording it (rather than a hand-rolled sha256) is what
/// makes trust content-pinned and fail-closed — if Codex ever changes its hashing,
/// our stale record simply stops matching and the hooks don't run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexHookEntry {
    /// The full trust key: `<sourcePath>:<event_snake>:<group_idx>:<handler_idx>`.
    /// It MUST be used verbatim — a short `pre_tool_use:0:0` suffix does not match.
    pub key: String,
    /// The handler's command line, as Codex parsed it.
    pub command: String,
    /// The file the definition came from (`<CODEX_HOME>/hooks.json` for ours).
    pub source_path: PathBuf,
    /// Codex's hash of this exact definition (`sha256:…`), recorded as
    /// `trusted_hash`.
    pub current_hash: String,
    /// `untrusted` | `trusted` | `modified` — Codex's verdict for this entry.
    pub trust_status: String,
    /// `true` for a managed (root/MDM-provisioned) hook. Never deck-owned.
    pub is_managed: bool,
}

/// Ask Codex itself for every hook it would load for `cwd` under `home`.
///
/// Mechanism (Codex 0.144.4, verified): `codex app-server` speaks line-delimited
/// JSON-RPC on stdio (an `[experimental]` surface). Two requests suffice, with no
/// credentials and no network:
///
/// ```text
/// → {"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{…}}}
/// ← {"id":1,"result":{"userAgent":…,"codexHome":…}}
/// → {"jsonrpc":"2.0","id":2,"method":"hooks/list","params":{"cwd":"<cwd>"}}
/// ← {"id":2,"result":{"data":[{"cwd":"…","hooks":[ <entry>, … ],
///                             "warnings":[],"errors":[]}]}}
/// ```
///
/// where each `<entry>` carries `key`, `eventName`, `command`, `sourcePath`,
/// `source`, `pluginId`, `isManaged`, `enabled`, `currentHash`, and `trustStatus`
/// (camelCase). This function is the ONE place that shape is decoded.
///
/// `CODEX_HOME` is set to `home` on the child so the listing describes exactly the
/// home the deck installed into and pins on the Codex child. stderr is discarded,
/// the wait is bounded by [`HOOKS_LIST_TIMEOUT`], and the child is killed before
/// returning. EVERY failure — codex absent, non-zero exit, protocol drift, timeout
/// — is an `Err` so the caller degrades quietly (no spawn is ever blocked).
pub fn list_hooks_in(home: &Path, cwd: &Path) -> std::io::Result<Vec<CodexHookEntry>> {
    let mut child = Command::new("codex")
        .arg("app-server")
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let requests = format!(
        "{}\n{}\n",
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "dot-agent-deck",
                    "title": "dot-agent-deck",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "hooks/list",
            "params": { "cwd": cwd.display().to_string() }
        }),
    );

    // Keep the stdin handle ALIVE until the response is read: a real app-server
    // may treat EOF as "shut down" and exit before answering.
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "codex app-server: no stdin pipe"))?;
    let write_result = stdin
        .write_all(requests.as_bytes())
        .and_then(|()| stdin.flush());

    // Read on a helper thread so the wait is genuinely bounded (a blocked
    // `read_line` cannot be cancelled). The thread is detached; it ends when the
    // killed child's stdout closes.
    let response = write_result.and_then(|()| {
        let stdout = child.stdout.take().ok_or_else(|| {
            io::Error::new(ErrorKind::BrokenPipe, "codex app-server: no stdout pipe")
        })?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let deadline = Instant::now() + HOOKS_LIST_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    ErrorKind::TimedOut,
                    "codex app-server: hooks/list did not answer in time",
                ));
            }
            match rx.recv_timeout(remaining) {
                // Only the `id: 2` reply carries the hook listing; the
                // `initialize` reply (and any notification) is skipped.
                Ok(line) => match serde_json::from_str::<Value>(&line) {
                    Ok(value) if value.get("id").and_then(Value::as_i64) == Some(2) => {
                        return parse_hooks_list(&value);
                    }
                    _ => continue,
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(io::Error::new(
                        ErrorKind::TimedOut,
                        "codex app-server: hooks/list did not answer in time",
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "codex app-server: exited without answering hooks/list",
                    ));
                }
            }
        }
    });

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    response
}

/// Decode the `id: 2` `hooks/list` reply into entries, tolerating both the
/// per-`cwd` grouped shape (`result.data[].hooks[]`, what 0.144.4 returns) and a
/// flat `result.hooks[]`. An entry missing `key` or `currentHash` is DROPPED
/// rather than guessed at — a trust record without Codex's own hash is worthless.
/// A JSON-RPC `error` reply, or a reply with no recognizable hook array, is `Err`.
fn parse_hooks_list(response: &Value) -> std::io::Result<Vec<CodexHookEntry>> {
    if let Some(error) = response.get("error") {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!("codex app-server: hooks/list failed: {error}"),
        ));
    }
    let result = response.get("result").ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidData,
            "codex app-server: hooks/list reply carried no result",
        )
    })?;
    let groups: Vec<&Value> = match result.get("data").and_then(Value::as_array) {
        Some(data) => data.iter().collect(),
        None => vec![result],
    };
    let mut found_array = false;
    let mut entries = Vec::new();
    for group in groups {
        let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
            continue;
        };
        found_array = true;
        for hook in hooks {
            let string = |field: &str| {
                hook.get(field)
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
            };
            let (Some(key), Some(current_hash)) = (string("key"), string("currentHash")) else {
                continue;
            };
            entries.push(CodexHookEntry {
                key,
                command: string("command").unwrap_or_default(),
                source_path: PathBuf::from(string("sourcePath").unwrap_or_default()),
                current_hash,
                trust_status: string("trustStatus").unwrap_or_else(|| "unknown".into()),
                is_managed: hook
                    .get("isManaged")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    if !found_array {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "codex app-server: hooks/list reply carried no hooks array",
        ));
    }
    Ok(entries)
}

/// **The security predicate — the only path to a trust write.** Keep an entry only
/// when ALL of the following hold:
///
/// 1. its `source_path` is the PINNED home's own `hooks.json` — the file the deck
///    authored and pins on the Codex child, not a project-local, plugin, or
///    other-home definition;
/// 2. its command carries the exact deck signature [`HOOK_COMMAND_SUFFIX`], so a
///    foreign command in the very same file (or one that merely *mentions*
///    `dot-agent-deck`) is never selected;
/// 3. it is not `isManaged` — a managed hook is provisioned by root/MDM and is
///    never the deck's to trust.
///
/// So the deck records trust ONLY for definitions it wrote itself, one exact hash
/// at a time. Paths are compared verbatim first and, only if that fails, by
/// canonicalized form (so a symlinked home still matches the SAME real file).
pub fn deck_owned_entries<'a>(
    entries: &'a [CodexHookEntry],
    home: &Path,
) -> Vec<&'a CodexHookEntry> {
    let ours = home.join("hooks.json");
    let ours_real = ours.canonicalize().ok();
    entries
        .iter()
        .filter(|entry| {
            let same_file = entry.source_path == ours
                || (ours_real.is_some() && entry.source_path.canonicalize().ok() == ours_real);
            same_file && command_is_deck_owned(&entry.command) && !entry.is_managed
        })
        .collect()
}

/// Record scoped, hash-pinned trust for the deck's OWN hooks in `home`, returning
/// how many entries were trusted (PRD #20 §4.1.2).
///
/// Asks Codex for the listing ([`list_hooks_in`]), narrows it with
/// [`deck_owned_entries`], and writes `[hooks.state."<key>"] { enabled = true,
/// trusted_hash = "<current_hash>" }` for exactly those keys into
/// `<home>/config.toml`. The edit is format-preserving and the publish is atomic
/// under [`INSTALL_LOCK`], so a concurrent deck writer can't interleave and the
/// user's comments/settings survive byte-intact.
///
/// This REPLACES the old invocation-global `--dangerously-bypass-hook-trust`:
/// it is launch-method agnostic (trust lives in the home, not argv), never trusts
/// a hook the deck didn't author, and fails closed (any error ⇒ the hooks stay
/// untrusted and events degrade to the coarse stdout classifier).
pub fn trust_deck_hooks_in(home: &Path, cwd: &Path) -> std::io::Result<usize> {
    let entries = list_hooks_in(home, cwd)?;
    let records: Vec<(String, String)> = deck_owned_entries(&entries, home)
        .into_iter()
        .map(|entry| (entry.key.clone(), entry.current_hash.clone()))
        .collect();
    if records.is_empty() {
        return Ok(0);
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    edit_trust_state(home, |state| {
        for (key, hash) in &records {
            upsert_trust_record(state, key, hash);
        }
    })?;
    Ok(records.len())
}

/// Drop the trust records for the deck's own hooks in `home` — the uninstall
/// counterpart of [`trust_deck_hooks_in`], returning how many were removed.
///
/// Deck ownership is resolved through the SAME predicate as the write
/// ([`deck_owned_entries`]), so a user's own trust record — including one for a
/// foreign hook that happens to live in the deck's `hooks.json` — is never
/// touched. Call it BEFORE removing the definitions, while Codex can still
/// enumerate them.
pub fn untrust_deck_hooks_in(home: &Path) -> std::io::Result<usize> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.to_path_buf());
    let entries = list_hooks_in(home, &cwd)?;
    let keys: Vec<String> = deck_owned_entries(&entries, home)
        .into_iter()
        .map(|entry| entry.key.clone())
        .collect();
    if keys.is_empty() {
        return Ok(0);
    }
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut removed = 0;
    edit_trust_state(home, |state| {
        for key in &keys {
            if state.remove(key.as_str()).is_some() {
                removed += 1;
            }
        }
    })?;
    Ok(removed)
}

/// Read `<home>/config.toml`, hand `edit` the `[hooks.state]` table to mutate, and
/// publish the result atomically — WITHOUT reformatting anything else.
///
/// `toml_edit` (not a `toml`/serde round trip) is what makes this safe on the
/// user's real `~/.codex/config.toml`: comments, key order, spacing, and every
/// unrelated table come back byte-identical, and a new table is appended at the
/// end. A missing file starts from an empty document; an unparseable one is an
/// error and is left untouched (we never discard a config we don't understand).
fn edit_trust_state(home: &Path, edit: impl FnOnce(&mut toml_edit::Table)) -> std::io::Result<()> {
    use toml_edit::{DocumentMut, Item, Table};

    std::fs::create_dir_all(home)?;
    let path = home.join(CONFIG_TOML);
    let existing = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let mut doc = existing.parse::<DocumentMut>().map_err(|e| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("Codex {CONFIG_TOML} is not valid TOML (left unchanged): {e}"),
        )
    })?;

    // `[hooks]` / `[hooks.state]` are created IMPLICIT when absent, so the file
    // gains only the `[hooks.state."<key>"]` header(s) it needs — no bare
    // `[hooks]` / `[hooks.state]` headers appear in the user's config.
    let hooks = doc
        .as_table_mut()
        .entry("hooks")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("Codex {CONFIG_TOML}: `hooks` is not a table (left unchanged)"),
            )
        })?;
    let state = hooks
        .entry("state")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("Codex {CONFIG_TOML}: `hooks.state` is not a table (left unchanged)"),
            )
        })?;

    edit(state);

    crate::agent_hook_config::write_atomic(home, &path, doc.to_string().as_bytes())
}

/// Insert or refresh one `[hooks.state."<key>"] { enabled, trusted_hash }` record.
///
/// An existing record for `key` is updated IN PLACE — as a table or as an inline
/// table, whichever the user (or a previous run) already wrote — so repeated
/// trust writes are idempotent and never duplicate the table.
fn upsert_trust_record(state: &mut toml_edit::Table, key: &str, hash: &str) {
    use toml_edit::{Item, Table, Value as TomlValue, value};

    match state.get_mut(key) {
        Some(Item::Table(existing)) => {
            existing.insert("enabled", value(true));
            existing.insert("trusted_hash", value(hash));
        }
        Some(Item::Value(TomlValue::InlineTable(existing))) => {
            existing.insert("enabled", TomlValue::from(true));
            existing.insert("trusted_hash", TomlValue::from(hash));
        }
        _ => {
            let mut record = Table::new();
            record.insert("enabled", value(true));
            record.insert("trusted_hash", value(hash));
            state.insert(key, Item::Table(record));
        }
    }
}

// ---------------------------------------------------------------------------
// PRD #20 §4.2.1 — command-agnostic install + trust at daemon/TUI startup
// ---------------------------------------------------------------------------
//
// The deck must not care HOW codex is launched. Keying the install off the spawn
// command's basename misses every launcher form (`devbox run codex-big`,
// `run_codex.sh`, an alias), which is exactly why the dogfood `tester` role got
// ZERO integration. Following PRD #201's Pi precedent, the install+trust runs
// ONCE at startup, guarded on codex being present, so hook events reach the pane
// through the inherited `DOT_AGENT_DECK_PANE_ID` regardless of launch method.

/// Whether `codex` is discoverable as an executable regular file on the process
/// `PATH` — the self-guard for [`auto_install_and_trust_at_startup`], mirroring
/// `orchestrator_ext::pi_present_for_env`. A non-executable file named `codex`
/// does not count (it could never run), so a machine without Codex is a cheap
/// no-op.
fn codex_present_on_path() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join("codex");
        let Ok(meta) = std::fs::metadata(&candidate) else {
            return false;
        };
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// Startup entry (PRD #20 §4.2.1): install the deck's Codex hooks into the active
/// `CODEX_HOME` and record scoped trust for them, ONCE, command-agnostically.
///
/// Wired as `CODEX.startup_auto_install` (so the TUI runs it at launch, like
/// Claude's hooks and OpenCode's plugin) and from the `daemon serve` entry (so a
/// headless or lazy-spawned daemon does it too — mirroring how Pi's extension
/// materializes there). Because it never looks at a spawn command, Codex hooks
/// fire however Codex is launched — bare `codex`, an absolute path, or a launcher
/// like `devbox run codex-big` — with events reaching the pane through the
/// inherited `DOT_AGENT_DECK_PANE_ID`.
///
/// Guarded, idempotent, and best-effort: SKIPs unless `codex` is on `PATH` and a
/// real home resolves (never a `/tmp` write), and any failure is logged, never
/// fatal.
pub fn auto_install_and_trust_at_startup() {
    if !codex_present_on_path() {
        tracing::debug!("codex startup install: skipped (codex not on PATH)");
        return;
    }
    let Some(home) = codex_home() else {
        tracing::debug!("codex startup install: skipped (no CODEX_HOME/HOME)");
        return;
    };
    auto_install();
    let cwd = std::env::current_dir().unwrap_or_else(|_| home.clone());
    match trust_deck_hooks_in(&home, &cwd) {
        Ok(count) => {
            tracing::debug!(count, "codex startup install: recorded scoped hook trust")
        }
        Err(e) => tracing::warn!(
            "codex startup install: could not record scoped hook trust ({e}); Codex events \
             degrade to stdout classification"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codex's hook commands are quoted for the HOST's shell — the half of #734
    /// that is a real behaviour change, and the half no test on a POSIX box can
    /// observe through the emitted string.
    ///
    /// Paired with `devin_hooks_manage`'s opposite assertion, this is what keeps
    /// the two writers from being collapsed into one answer: on Linux both
    /// spellings emit identical bytes, so setting this to `Posix` would revert
    /// #734 for the only writer that can reach Windows while every other test in
    /// the workspace stayed green.
    #[test]
    fn hook_commands_are_quoted_for_the_shell_codex_will_use() {
        assert_eq!(
            HOOK_SHELL,
            crate::agent_hook_config::HookShell::Native,
            "Codex runs its hooks through the host's own shell, so the quoting \
             must follow the host"
        );
    }

    #[test]
    fn install_writes_command_hooks_for_every_event() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("install hooks.json");

        let contents =
            std::fs::read_to_string(dir.path().join("hooks.json")).expect("read hooks.json");
        let root: Value = serde_json::from_str(&contents).expect("parse hooks.json");
        let hooks = root
            .get("hooks")
            .and_then(Value::as_object)
            .expect("hooks object");

        for &event in CODEX_HOOK_EVENTS {
            let arr = hooks
                .get(event)
                .and_then(Value::as_array)
                .unwrap_or_else(|| panic!("event {event} present"));
            assert_eq!(arr.len(), 1, "one deck rule per event ({event})");
            let cmd = arr[0]["hooks"][0]["command"].as_str().expect("command str");
            assert_eq!(cmd, "/abs/dot-agent-deck hook --agent codex");
            assert_eq!(arr[0]["hooks"][0]["type"].as_str(), Some("command"));
        }
    }

    #[test]
    fn reinstall_is_idempotent_and_preserves_user_hooks() {
        let dir = tempfile::tempdir().expect("codex home tempdir");
        // A pre-existing user hook the deck must never clobber.
        let user = json!({
            "hooks": {
                "PreToolUse": [
                    { "hooks": [ { "type": "command", "command": "/user/own-hook" } ] }
                ]
            }
        });
        std::fs::write(
            dir.path().join("hooks.json"),
            serde_json::to_string_pretty(&user).unwrap(),
        )
        .unwrap();

        install_to(dir.path(), "/abs/dot-agent-deck").expect("first install");
        install_to(dir.path(), "/abs/dot-agent-deck").expect("second install");

        let contents = std::fs::read_to_string(dir.path().join("hooks.json")).unwrap();
        let root: Value = serde_json::from_str(&contents).unwrap();
        let pre = root["hooks"]["PreToolUse"]
            .as_array()
            .expect("PreToolUse array");
        // The user's hook survives; the deck's is present exactly once (no dupes).
        let user_rules = pre
            .iter()
            .filter(|r| r["hooks"][0]["command"] == json!("/user/own-hook"))
            .count();
        let deck_rules = pre.iter().filter(|r| rule_is_dot_agent_deck(r)).count();
        assert_eq!(user_rules, 1, "user hook preserved");
        assert_eq!(
            deck_rules, 1,
            "deck hook present exactly once after re-install"
        );
    }

    /// The atomic publish must never widen the files it rewrites. `hooks.json`
    /// carries the deck's own hook commands, but `config.toml` is the user's
    /// real Codex config — model choice, auth references, hook-trust records and
    /// anything they hand-wrote — so a `File::create` default of 0644 (under a
    /// typical 022 umask) or 0664 (under 002) would expose it to every local
    /// account the first time the deck installed or trusted its hooks. Mirrors
    /// `devin_hooks_manage::tests::install_never_widens_config_permissions`
    /// (#360), for the copy that kept the bug (#382).
    #[cfg(unix)]
    #[test]
    fn install_and_trust_never_widen_config_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let mode_of = |path: &Path| {
            std::fs::metadata(path)
                .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
                .permissions()
                .mode()
                & 0o777
        };
        let restrict = |path: &Path| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("restrict fixture to 0600");
        };
        let trust = |home: &Path| {
            edit_trust_state(home, |state| {
                upsert_trust_record(state, "deck-hook", "abc123");
            })
            .expect("record trust");
        };

        // Files the deck creates itself are owner-only, not umask-dependent.
        let fresh = crate::test_temp::tempdir().expect("codex home tempdir");
        install_to(fresh.path(), "/abs/dot-agent-deck").expect("install");
        trust(fresh.path());
        assert_eq!(
            mode_of(&fresh.path().join("hooks.json")),
            0o600,
            "a deck-created hooks.json must be owner-only"
        );
        assert_eq!(
            mode_of(&fresh.path().join(CONFIG_TOML)),
            0o600,
            "a deck-created config.toml must be owner-only"
        );

        // An existing owner-only Codex home stays owner-only across install,
        // trust and uninstall — all three go through the same atomic publish.
        let existing = crate::test_temp::tempdir().expect("codex home tempdir");
        let hooks = existing.path().join("hooks.json");
        let config = existing.path().join(CONFIG_TOML);
        std::fs::write(&hooks, b"{}").unwrap();
        std::fs::write(&config, b"model = \"gpt-5\"\n").unwrap();
        restrict(&hooks);
        restrict(&config);

        install_to(existing.path(), "/abs/dot-agent-deck").expect("install");
        trust(existing.path());
        assert_eq!(
            mode_of(&hooks),
            0o600,
            "install must not widen an owner-only hooks.json"
        );
        assert_eq!(
            mode_of(&config),
            0o600,
            "recording trust must not widen an owner-only config.toml"
        );

        uninstall_from(existing.path()).expect("uninstall");
        assert_eq!(
            mode_of(&hooks),
            0o600,
            "uninstall must not widen an owner-only hooks.json"
        );

        // The user's own config bytes survive the trust edit that preserved the
        // mode, so the assertion above is not passing over a clobbered file.
        let contents = std::fs::read_to_string(&config).expect("read config.toml");
        assert!(
            contents.contains("model = \"gpt-5\""),
            "trust edit must preserve unrelated config; got {contents}"
        );
    }
}
