use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

/// Serializes the whole read-preserve-write span over the plugin file, for
/// every install path in this module (PRD #381 audit, MEDIUM-2).
///
/// Locking the write alone would not be enough: [`auto_install_to`] reads the
/// existing `BINARY_PATH` back, decides whether to preserve it, and only then
/// writes. Two callers racing that span both read the same "before" state, and
/// the second one republishes a pin the first had just repaired. Same shape and
/// same reasoning as `hooks_manage::SETTINGS_LOCK` and
/// `codex_hooks_manage::INSTALL_LOCK`.
///
/// What it does NOT close is the cross-PROCESS lost update — two deck binaries
/// starting at the same instant, or a deck racing a hand-edit of the plugin.
/// That needs an advisory file lock, which no sibling adapter has either; the
/// atomic publish means the loser of such a race loses a whole update rather
/// than leaving OpenCode a torn JavaScript file to load.
static PLUGIN_LOCK: Mutex<()> = Mutex::new(());

/// Take [`PLUGIN_LOCK`], recovering from a poisoned mutex rather than
/// panicking: a previous caller panicking mid-install says nothing about
/// whether the plugin file is usable now, and the read is re-done from disk
/// under the guard regardless.
fn lock_plugin() -> MutexGuard<'static, ()> {
    PLUGIN_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// The FLAT plugin file dot-agent-deck installs under an OpenCode config root:
/// `<root>/plugin/dot-agent-deck.js`.
///
/// OpenCode discovers local plugins with a **one-level** glob
/// (`{plugin,plugins}/*.{ts,js}` — no `**`), so the plugin must be a file
/// directly under `plugin/`. The pre-existing nested layout
/// (`<root>/plugin/dot-agent-deck/index.js`, see [`stale_plugin_dir`]) sat a
/// directory too deep and was therefore never scanned — it silently never
/// loaded, so the agent's card stayed "No agent". This flat path is what the
/// glob actually matches.
fn plugin_file(root: &Path) -> PathBuf {
    root.join("plugin").join("dot-agent-deck.js")
}

/// The obsolete nested plugin directory (`<root>/plugin/dot-agent-deck/`) from
/// before the flat-file layout. OpenCode never loaded it, but existing users
/// still have it on disk; [`write_plugin`] deletes it when it installs the flat
/// file so every user converges to the discoverable layout with no manual step.
fn stale_plugin_dir(root: &Path) -> PathBuf {
    root.join("plugin").join("dot-agent-deck")
}

/// PRD #163 M1: route the home lookup through the platform seam instead of
/// reading `$HOME` directly, so that on Windows — where `$HOME` is normally
/// unset — the OpenCode plugin roots resolve under `%USERPROFILE%` instead of
/// being missed entirely.
///
/// PRD #163 review: the seam function is
/// [`crate::platform::paths::home_dir_with_tmp_fallback`], *not* `home_dir`,
/// because the raw read this replaced fell back to `/tmp` when `$HOME` was unset.
/// Unix behavior is therefore byte-for-byte what it was, in that case too.
fn home_dir() -> PathBuf {
    crate::platform::paths::home_dir_with_tmp_fallback()
}

fn xdg_config_root(home: &Path) -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home.join(".config"))
}

/// The XDG-default OpenCode config root (`$XDG_CONFIG_HOME/opencode`, defaulting
/// to `$HOME/.config/opencode`). Used as the explicit-install fallback root when
/// no existing layout is found. Deliberately performs **no** existence checks: it
/// is only ever evaluated after the caller has already determined that none of the
/// candidate roots exist, so re-detecting them would be dead work.
fn xdg_default_root() -> PathBuf {
    let home = home_dir();
    xdg_config_root(&home).join("opencode")
}

/// All candidate OpenCode config roots, XDG first then legacy, **without** checking
/// existence — that is the caller's job. This is the single source of truth for which
/// layouts we touch, shared by `existing_plugin_artifacts` (uninstall), `auto_install`,
/// and `install`. Adding a future layout is a one-line change here.
fn candidate_roots() -> Vec<PathBuf> {
    let home = home_dir();
    vec![
        xdg_config_root(&home).join("opencode"),
        home.join(".opencode"),
    ]
}

/// All plugin artifacts that currently exist on disk across candidate roots (XDG
/// and legacy): the flat `dot-agent-deck.js` file AND any obsolete nested
/// `dot-agent-deck/` directory. For uninstall — so it clears both the current
/// layout and the stale one an upgrade may have left behind.
fn existing_plugin_artifacts() -> Vec<PathBuf> {
    let mut artifacts = Vec::new();
    for root in candidate_roots() {
        let file = plugin_file(&root);
        if file.exists() {
            artifacts.push(file);
        }
        let stale = stale_plugin_dir(&root);
        if stale.is_dir() {
            artifacts.push(stale);
        }
    }
    artifacts
}

fn plugin_template(binary_path: &str) -> String {
    let binary_path_json =
        serde_json::to_string(binary_path).unwrap_or_else(|_| "\"dot-agent-deck\"".to_string());
    format!(
        r#"import {{ execFileSync }} from "child_process";

// Duplicate-load guard. The installer fans out to EVERY candidate config root
// that exists (`$XDG_CONFIG_HOME/opencode` and `~/.opencode`) because it cannot
// know which one OpenCode reads. When both exist OpenCode loads both copies into
// one process, and every hook fired twice — observed in production as doubled
// `Received event` lines in the daemon log, i.e. two events per real action.
//
// Fixed here rather than by narrowing the install, because the fan-out is what
// guarantees we land in the root OpenCode actually uses; suppressing the second
// copy keeps that guarantee and costs one flag. First copy loaded wins; any
// later copy exports inert no-op hooks.
const DAD_GUARD = "__dotAgentDeckPluginLoaded";
const DAD_ALREADY_LOADED = globalThis[DAD_GUARD] === true;
globalThis[DAD_GUARD] = true;

const BINARY_PATH = {binary_path_json};
const knownSessions = new Map();
const messageRoles = new Map();
const directorySessions = new Map();
const sessionAliases = new Map();
let shuttingDown = false;

const sendEvent = (payload) => {{
  try {{
    execFileSync(BINARY_PATH, ["hook", "--agent", "opencode"], {{
      input: JSON.stringify(payload),
      timeout: 5000,
      stdio: ["pipe", "ignore", "ignore"],
    }});
  }} catch (_) {{}}
}};

const defaultSessionId = (value) => (value ? value : "unknown");

const normalizeSessionId = (sessionId, directory) => {{
  const dirKey = directory ?? process.cwd();
  if (sessionId && sessionAliases.has(sessionId)) {{
    return sessionAliases.get(sessionId);
  }}
  if (sessionId && sessionId !== "unknown") {{
    const existing = directorySessions.get(dirKey);
    if (existing && existing !== sessionId) {{
      sessionAliases.set(sessionId, existing);
      return existing;
    }}
    directorySessions.set(dirKey, sessionId);
    return sessionId;
  }}
  const fallback = directorySessions.get(dirKey);
  if (fallback) {{
    return fallback;
  }}
  return sessionId ?? "unknown";
}};

const updateSessionInfo = (sessionId, directory, status) => {{
  if (!sessionId || sessionId === "unknown") {{
    return null;
  }}
  const existing = knownSessions.get(sessionId) ?? {{}};
  const cwd = directory ?? existing.cwd ?? process.cwd();
  const info = {{
    cwd,
    status: status ?? existing.status,
  }};
  knownSessions.set(sessionId, info);
  return info;
}};

const cleanupSessionMessages = (sessionId) => {{
  for (const [messageId, info] of messageRoles.entries()) {{
    if (info?.sessionId === sessionId) {{
      messageRoles.delete(messageId);
    }}
  }}
}};

const sessionPayload = (event, directory) => {{
  const props = event?.properties ?? {{}};
  const info = props.info ?? {{}};
  const status = props.status ?? {{}};
  const cwd = info.directory ?? props.directory ?? directory ?? process.cwd();
  return {{
    session_id: normalizeSessionId(
      defaultSessionId(props.sessionID ?? info.id),
      cwd
    ),
    event: event?.type ?? "session.unknown",
    status: status.type,
    cwd,
  }};
}};

const permissionPayload = (event, directory) => {{
  const props = event?.properties ?? {{}};
  const prompt =
    props.prompt ??
    props.title ??
    props.message ??
    props.text ??
    props.question ??
    "";
  const cwd = directory ?? process.cwd();
  return {{
    session_id: normalizeSessionId(
      defaultSessionId(props.sessionID ?? props.sessionId),
      cwd
    ),
    event: event?.type ?? "permission.unknown",
    prompt,
    cwd,
  }};
}};

const ensureSessionRegistered = (sessionId, directory, status, shouldEmitEvent = true) => {{
  if (!sessionId || sessionId === "unknown") {{
    return;
  }}
  const alreadyKnown = knownSessions.has(sessionId);
  const info = updateSessionInfo(sessionId, directory, status);
  if (!alreadyKnown && shouldEmitEvent) {{
    sendEvent({{
      session_id: sessionId,
      event: "session.created",
      status,
      cwd: info?.cwd ?? process.cwd(),
    }});
  }}
}};

const closeSession = (sessionId, directory, emitEvent = true, removeAlias = true) => {{
  if (!sessionId || sessionId === "unknown") {{
    return;
  }}
  const info = knownSessions.get(sessionId);
  knownSessions.delete(sessionId);
  cleanupSessionMessages(sessionId);
  if (removeAlias) {{
    for (const [alias, target] of sessionAliases.entries()) {{
      if (alias === sessionId || target === sessionId) {{
        sessionAliases.delete(alias);
      }}
    }}
    for (const [dirKey, id] of directorySessions.entries()) {{
      if (id === sessionId) {{
        directorySessions.delete(dirKey);
      }}
    }}
  }}
  if (emitEvent) {{
    sendEvent({{
      session_id: sessionId,
      event: "session.deleted",
      cwd: directory ?? info?.cwd ?? process.cwd(),
    }});
  }}
}};

const flushSessions = () => {{
  for (const [sessionId, info] of knownSessions.entries()) {{
    closeSession(sessionId, info?.cwd, true, true);
  }}
}};

const handleShutdown = () => {{
  if (shuttingDown) {{
    return;
  }}
  shuttingDown = true;
  flushSessions();
}};

process.once("exit", handleShutdown);
for (const signal of ["SIGINT", "SIGTERM"]) {{
  process.once(signal, handleShutdown);
}}

const recordUserMessage = (event, directory) => {{
  const info = event?.properties?.info;
  const messageId = info?.id;
  if (!messageId) {{
    return;
  }}
  const role = (info?.role ?? "").toLowerCase();
  if (role !== "user") {{
    messageRoles.delete(messageId);
    return;
  }}
  const dir = info?.directory ?? directory ?? process.cwd();
  messageRoles.set(messageId, {{
    role,
    sessionId: normalizeSessionId(info.sessionID ?? null, dir),
  }});
}};

const emitUserPrompt = (sessionId, prompt, directory) => {{
  const text = (prompt ?? "").trim();
  if (!text) {{
    return;
  }}
  ensureSessionRegistered(sessionId, directory);
  const sessionInfo = knownSessions.get(sessionId);
  sendEvent({{
    session_id: sessionId,
    event: "session.prompt",
    prompt: text,
    cwd: directory ?? sessionInfo?.cwd ?? process.cwd(),
  }});
}};

const handleMessagePartUpdated = (event, directory) => {{
  const part = event?.properties?.part;
  if (!part?.messageID || part.type !== "text" || !part.text) {{
    return;
  }}
  const info = messageRoles.get(part.messageID);
  if (!info || info.role !== "user") {{
    return;
  }}
  const sessionId = normalizeSessionId(
    info.sessionId ?? defaultSessionId(event?.properties?.sessionID),
    directory
  );
  emitUserPrompt(sessionId, part.text, directory);
  messageRoles.delete(part.messageID);
}};

export const DotAgentDeckPlugin = async (ctx) => {{
  // A second copy of this plugin in the same OpenCode process returns inert
  // hooks, so one real action produces one event. See DAD_GUARD above.
  if (DAD_ALREADY_LOADED) {{
    return {{ event: async () => {{}} }};
  }}

  const directory = ctx?.directory ?? process.cwd();

  return {{
    event: async (input) => {{
      const event = input?.event;
      const eventType = event?.type ?? "";
      if (
        eventType === "message.created" ||
        eventType === "message.updated"
      ) {{
        recordUserMessage(event, directory);
        return;
      }}
      if (eventType === "message.part.updated") {{
        handleMessagePartUpdated(event, directory);
        return;
      }}
      if (eventType === "permission.asked" || eventType === "permission.replied") {{
        const payload = permissionPayload(event, directory);
        ensureSessionRegistered(payload.session_id, payload.cwd);
        sendEvent(payload);
        return;
      }}
      if (eventType === "server.instance.disposed") {{
        flushSessions();
        return;
      }}
      if (!event?.type?.startsWith("session.")) {{
        return;
      }}
      const payload = sessionPayload(event, directory);
      if (event?.type === "session.deleted") {{
        closeSession(payload.session_id, payload.cwd, false, false);
        return;
      }}
      ensureSessionRegistered(
        payload.session_id,
        payload.cwd,
        payload.status,
        event?.type !== "session.created"
      );
      sendEvent(payload);
    }},
    "tool.execute.before": async (input, output) => {{
      const sessionId = normalizeSessionId(
        defaultSessionId(input?.sessionID),
        directory
      );
      ensureSessionRegistered(sessionId, directory);
      sendEvent({{
        session_id: sessionId,
        event: "tool.execute.before",
        tool_name: input?.tool,
        tool_input: output?.args,
        cwd: directory,
      }});
    }},
    "tool.execute.after": async (input) => {{
      const sessionId = normalizeSessionId(
        defaultSessionId(input?.sessionID),
        directory
      );
      ensureSessionRegistered(sessionId, directory);
      sendEvent({{
        session_id: sessionId,
        event: "tool.execute.after",
        tool_name: input?.tool,
        tool_input: input?.args,
        cwd: directory,
      }});
    }},
  }};
}};

export default DotAgentDeckPlugin;
"#
    )
}

/// Ensure `<root>/plugin/` exists and (over)write the flat `dot-agent-deck.js`
/// pinned to `binary_path`, returning the file path written. Also removes any
/// obsolete nested `dot-agent-deck/` directory (see [`stale_plugin_dir`]) so an
/// upgrade migrates the layout in place. Shared by every install path
/// (auto + explicit + test seam).
fn write_plugin(root: &Path, binary_path: &str) -> std::io::Result<PathBuf> {
    let plugin_dir = root.join("plugin");
    std::fs::create_dir_all(&plugin_dir)?;

    // Migrate away from the pre-flat nested layout OpenCode never scanned.
    // Best-effort: a failure to remove the dead dir must not abort the install
    // of the working flat file.
    let stale = stale_plugin_dir(root);
    if stale.is_dir() {
        let _ = std::fs::remove_dir_all(&stale);
    }

    let path = plugin_file(root);
    let content = plugin_template(binary_path);
    // PRD #381 audit, MEDIUM-2. This was `std::fs::write`, the only one of the
    // four config writers not publishing atomically — and the file it writes is
    // JavaScript OpenCode *executes*. `fs::write` follows a pre-created symlink
    // at the destination and truncates in place, so a crash or a concurrent
    // startup could leave a partial plugin for OpenCode to load. Temp file plus
    // `rename` removes both. (The shared helper's own temp-name predictability
    // is issue #731, which fixes it for all four writers at once; routing this
    // one in is what puts the OpenCode plugin behind that fix too.)
    crate::agent_hook_config::write_atomic(&plugin_dir, &path, content.as_bytes())?;

    Ok(path)
}

/// The `BINARY_PATH` an already-installed plugin under `root` pins, or `None`
/// when there is no plugin file, it cannot be read, or it carries no readable
/// literal (a hand-edited or truncated file).
///
/// The plugin is generated **JavaScript**, not JSON, and PRD #381 blames
/// exactly that shape difference for OpenCode being the last of the three
/// integrations to be noticed — a JSON-shaped fix silently misses it. So this
/// reads the one line [`plugin_template`] emits and decodes the literal with
/// `serde_json`, which is the same escaping the template wrote it with.
///
/// **Line-oriented, and both halves of that are fixes.** It used to
/// `split_once("const BINARY_PATH = ")` and then `split_once(";\n")`:
///
/// - The terminator was the literal `";\n"`, so a plugin a user had opened in a
///   CRLF editor (`;\r\n`) parsed as "no pin found" and the still-valid pin was
///   clobbered on the next auto-install — the "repair only when missing" rule
///   silently bypassed for that corner (reviewer N1). [`str::lines`] splits on
///   `\n` and strips a trailing `\r`, so both line endings decode the same.
/// - The marker matched **anywhere** in the file, including inside a comment in
///   a hand-edited plugin, and whatever followed became the preserved — and
///   republished — value (PRD #381 audit, MEDIUM-1). Anchoring it to the start
///   of a line is what [`plugin_template`] actually emits (`const BINARY_PATH =
///   …;` at column 0), so a commented-out or incidental occurrence cannot
///   become the pin.
///
/// Stripping the trailing `;` rather than splitting on one is deliberate: a `;`
/// inside the path itself survives, because `serde_json` escaping keeps the
/// literal on one line and the `;` the template writes is always the last byte
/// of it. The first matching line wins, as before.
fn existing_binary_path(root: &Path) -> Option<String> {
    let js = std::fs::read_to_string(plugin_file(root)).ok()?;
    js.lines()
        .filter_map(|line| line.strip_prefix("const BINARY_PATH = "))
        .filter_map(|rest| rest.strip_suffix(';'))
        .find_map(|literal| serde_json::from_str::<String>(literal).ok())
}

/// Remove one plugin artifact — a flat file or an obsolete nested dir — and print
/// a line naming what was removed. A missing path is reported, not an error.
fn uninstall_impl(path: &PathBuf) -> std::io::Result<()> {
    if !path.exists() {
        println!("No OpenCode plugin found to remove.");
        return Ok(());
    }

    if path.is_dir() {
        std::fs::remove_dir_all(path)?;
    } else {
        std::fs::remove_file(path)?;
    }

    println!("Removed OpenCode plugin: {}", path.display());
    Ok(())
}

/// Fan-out core for auto-install: for every candidate root that exists, refresh the
/// flat `plugin/dot-agent-deck.js` (migrating away from any stale nested layout).
/// Roots that don't exist are skipped — no speculative directory creation. Per-target
/// failures are logged via `tracing::warn!` and never abort the remaining targets.
/// Silent on stdout (dashboard startup path).
fn auto_install_to(roots: &[PathBuf], binary_path: &str) {
    // Held across the read-preserve-write span, not just the write. See
    // [`PLUGIN_LOCK`].
    let _guard = lock_plugin();
    for root in roots {
        if !root.exists() {
            continue;
        }
        // PRD #381 M5, and its Open Question 3: repair only what is UNUSABLE.
        // This path used to rewrite `BINARY_PATH` unconditionally on every
        // dashboard startup, so a perfectly valid pin was clobbered by whatever
        // binary happened to be launching — the "not merely different" rule
        // broken on the one integration where nobody was looking. The file is
        // still regenerated either way, so a template change still lands; only
        // the pinned path is carried over.
        //
        // `pin_is_repairable`, not a bare existence probe: a legacy plugin
        // pinning the BARE `"dot-agent-deck"` would otherwise be preserved
        // whenever the process cwd happened to hold a file of that name, and
        // Node's `execFileSync` then resolves that persisted bare name through
        // the AGENT's `$PATH` (PRD #381 audit, MEDIUM-1 — issue #536's own
        // vector).
        let (pinned, repairing) = match existing_binary_path(root) {
            Some(existing) if !crate::platform::paths::pin_is_repairable(&existing) => {
                (existing, false)
            }
            Some(_) => (binary_path.to_string(), true),
            None => (binary_path.to_string(), false),
        };
        match write_plugin(root, &pinned) {
            // Repair logs what it changed: silently mutating global config is
            // the same class of thing that caused this bug.
            Ok(path) if repairing => tracing::info!(
                "repaired the OpenCode plugin at {}: its BINARY_PATH was not a usable \
                 durable path, now pinned to {pinned}",
                path.display()
            ),
            Ok(path) => tracing::info!("auto-installed OpenCode plugin: {}", path.display()),
            Err(e) => tracing::warn!(
                "auto-install: failed to write OpenCode plugin under {}: {e}",
                root.display()
            ),
        }
    }
}

/// Fan-out core for explicit install: write the plugin into every candidate root that
/// exists; if none exist, fall back to `fallback_root()` (the XDG-default config root),
/// creating it — the first-time-install behavior. The fallback closure is evaluated
/// lazily, only when no layout exists, so the common path avoids the extra filesystem
/// probe. Each successful write emits one `Installed OpenCode plugin: <path>` line to
/// `out`. Every target is attempted even if an earlier one fails; the first error (if
/// any) is returned so the caller surfaces it.
fn install_to_roots(
    roots: &[PathBuf],
    fallback_root: impl FnOnce() -> PathBuf,
    binary_path: &str,
    out: &mut impl std::io::Write,
) -> std::io::Result<()> {
    // The explicit path writes rather than preserves, but it publishes to the
    // same file the auto path reads back — so it takes the same lock. See
    // [`PLUGIN_LOCK`].
    let _guard = lock_plugin();
    let mut targets: Vec<PathBuf> = Vec::new();
    for root in roots {
        if root.exists() {
            targets.push(root.clone());
        }
    }
    if targets.is_empty() {
        targets.push(fallback_root());
    }

    let mut first_err: Option<std::io::Error> = None;
    for root in &targets {
        match write_plugin(root, binary_path) {
            Ok(path) => {
                let _ = writeln!(out, "Installed OpenCode plugin: {}", path.display());
            }
            Err(e) => {
                tracing::warn!(
                    "install: failed to write OpenCode plugin under {}: {e}",
                    root.display()
                );
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Silently install OpenCode plugin into every existing layout.
/// Intended for dashboard startup — never prints to stdout, refusal included.
pub fn auto_install() {
    auto_install_resolved(
        &candidate_roots(),
        crate::platform::paths::durable_binary_path(),
    );
}

/// [`auto_install`] with the binary-path resolution injected, so the PRD #381 M6
/// refusal branch is reachable from a test.
///
/// On a refusal nothing is written and no directory is created — the plugin file
/// is never opened, so there is no truncated or abandoned JavaScript left for
/// OpenCode to load — and the complaint goes to `tracing::warn!` and nowhere
/// else, because this is the dashboard-startup path.
fn auto_install_resolved(roots: &[PathBuf], binary_path: Result<String, String>) {
    match binary_path {
        Ok(binary_path) => auto_install_to(roots, &binary_path),
        Err(e) => tracing::warn!("auto-install: {e}"),
    }
}

/// `dot-agent-deck hooks install --agent opencode`. Unlike [`auto_install`],
/// this ALWAYS writes the freshly resolved path: the user asked for this
/// install by name, so an existing pin is not preserved (PRD #381 M5).
pub fn install() -> std::io::Result<()> {
    let binary_path =
        crate::platform::paths::durable_binary_path().map_err(std::io::Error::other)?;

    install_to_roots(
        &candidate_roots(),
        xdg_default_root,
        &binary_path,
        &mut std::io::stdout(),
    )
}

pub fn uninstall() -> std::io::Result<()> {
    let artifacts = existing_plugin_artifacts();
    if artifacts.is_empty() {
        println!("No OpenCode plugin found to remove.");
        return Ok(());
    }
    for artifact in &artifacts {
        uninstall_impl(artifact)?;
    }
    Ok(())
}

// --- Testable versions that accept a custom path ---

pub fn uninstall_from(path: &PathBuf) -> std::io::Result<()> {
    uninstall_impl(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_template_uses_exec_file_sync() {
        let content = plugin_template("/usr/local/bin/dot-agent-deck");
        assert!(content.contains("import { execFileSync } from \"child_process\";"));
        assert!(!content.contains("execSync("));
        assert!(content.contains(r#"BINARY_PATH = "/usr/local/bin/dot-agent-deck""#));
        assert!(content.contains("const knownSessions = new Map();"));
        assert!(content.contains("process.once(\"exit\", handleShutdown);"));
        assert!(content.contains(r#"["hook", "--agent", "opencode"]"#));
        assert!(content.contains("event?.type?.startsWith(\"session.\")"));
        assert!(content.contains("\"tool.execute.before\""));
        assert!(content.contains("eventType === \"message.created\""));
        assert!(content.contains("eventType === \"message.updated\""));
        assert!(content.contains("const permissionPayload"));
        assert!(content.contains("\"permission.asked\""));
    }

    /// The installer intentionally fans out to every existing OpenCode config
    /// root, so when both `$XDG_CONFIG_HOME/opencode` and `~/.opencode` exist
    /// OpenCode loads two copies of the plugin into one process and every hook
    /// fires twice (observed as doubled `Received event` lines in the daemon
    /// log). The template must therefore carry a process-wide duplicate-load
    /// guard whose second copy returns inert hooks.
    #[test]
    fn plugin_template_suppresses_a_duplicate_load() {
        let content = plugin_template("/usr/local/bin/dot-agent-deck");
        // The flag is read BEFORE it is set, so the first copy loaded wins.
        let read_at = content
            .find("const DAD_ALREADY_LOADED = globalThis[DAD_GUARD] === true;")
            .expect("guard must snapshot the flag");
        let set_at = content
            .find("globalThis[DAD_GUARD] = true;")
            .expect("guard must claim the flag");
        assert!(
            read_at < set_at,
            "the flag must be READ before being SET, or the first copy would \
             see its own claim and disable itself"
        );
        // A duplicate returns hooks that do nothing.
        assert!(
            content.contains("if (DAD_ALREADY_LOADED) {"),
            "the plugin factory must short-circuit on a duplicate load"
        );
        // The guard must sit on globalThis: two copies are separate ES modules,
        // so module-local state would not be shared between them.
        assert!(
            content.contains(r#"const DAD_GUARD = "__dotAgentDeckPluginLoaded";"#),
            "the guard must be keyed on globalThis, not module scope"
        );
    }

    /// The plugin must be a FLAT `.js` file directly under `plugin/` — that is the
    /// only layout OpenCode's one-level `{plugin,plugins}/*.{ts,js}` glob discovers.
    /// The obsolete nested dir sits a level deeper and is never scanned.
    #[test]
    fn plugin_file_is_flat_under_plugin_dir() {
        let root = PathBuf::from("/some/opencode");
        assert_eq!(
            plugin_file(&root),
            PathBuf::from("/some/opencode/plugin/dot-agent-deck.js")
        );
        assert_eq!(
            stale_plugin_dir(&root),
            PathBuf::from("/some/opencode/plugin/dot-agent-deck")
        );
    }

    /// Read the installed plugin under `root` and assert its `BINARY_PATH` matches.
    fn assert_plugin_binary(root: &Path, binary_path: &str) {
        let file = plugin_file(root);
        let content = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("expected plugin at {}: {e}", file.display()));
        assert!(
            content.contains(&format!(r#"BINARY_PATH = "{binary_path}""#)),
            "plugin at {} should pin BINARY_PATH = {binary_path:?}",
            file.display()
        );
    }

    #[test]
    fn auto_install_writes_to_both_existing_roots() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-both");

        assert_plugin_binary(&xdg_root, "/bin/deck-both");
        assert_plugin_binary(&legacy_root, "/bin/deck-both");
    }

    #[test]
    fn auto_install_only_legacy_present_skips_xdg() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode"); // NOT created
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&legacy_root).unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-legacy");

        assert_plugin_binary(&legacy_root, "/bin/deck-legacy");
        assert!(!xdg_root.exists(), "absent XDG root must not be created");
        assert!(!plugin_file(&xdg_root).exists());
    }

    #[test]
    fn auto_install_only_xdg_present_skips_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode"); // NOT created
        std::fs::create_dir_all(&xdg_root).unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-xdg");

        assert_plugin_binary(&xdg_root, "/bin/deck-xdg");
        assert!(
            !legacy_root.exists(),
            "absent legacy root must not be created"
        );
        assert!(!plugin_file(&legacy_root).exists());
    }

    #[test]
    fn auto_install_neither_present_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        // Neither root created.

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-none");

        assert!(!xdg_root.exists());
        assert!(!legacy_root.exists());
        assert!(!plugin_file(&xdg_root).exists());
        assert!(!plugin_file(&legacy_root).exists());
    }

    /// Note what makes this pass since PRD #381: `/bin/deck-old` does not
    /// exist, so the second pass REPAIRS a dead pin rather than overwriting a
    /// live one. The auto path deliberately no longer clobbers a `BINARY_PATH`
    /// that still resolves — see
    /// `auto_install_repairs_a_dead_binary_path_and_preserves_a_valid_one`,
    /// which pins both halves.
    #[test]
    fn auto_install_idempotent_overwrites_every_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        let roots = [xdg_root.clone(), legacy_root.clone()];

        auto_install_to(&roots, "/bin/deck-old");
        auto_install_to(&roots, "/bin/deck-new");

        for root in [&xdg_root, &legacy_root] {
            let content = std::fs::read_to_string(plugin_file(root)).unwrap();
            assert!(content.contains(r#"BINARY_PATH = "/bin/deck-new""#));
            assert!(!content.contains(r#"BINARY_PATH = "/bin/deck-old""#));
        }
    }

    #[test]
    fn auto_install_one_layout_failure_still_writes_other() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        // Block the XDG write: a regular file where `plugin/` must be a dir makes
        // `create_dir_all` fail for the XDG target only.
        std::fs::write(xdg_root.join("plugin"), b"not a dir").unwrap();

        auto_install_to(&[xdg_root.clone(), legacy_root.clone()], "/bin/deck-resil");

        assert_plugin_binary(&legacy_root, "/bin/deck-resil");
        assert!(!plugin_file(&xdg_root).exists());
    }

    #[test]
    fn install_fan_out_writes_every_existing_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        let fallback_root = tmp.path().join("fallback").join("opencode");

        let mut out = Vec::new();
        install_to_roots(
            &[xdg_root.clone(), legacy_root.clone()],
            || fallback_root.clone(),
            "/bin/deck-install",
            &mut out,
        )
        .unwrap();

        assert_plugin_binary(&xdg_root, "/bin/deck-install");
        assert_plugin_binary(&legacy_root, "/bin/deck-install");
        // Fallback NOT used because at least one layout existed.
        assert!(!plugin_file(&fallback_root).exists());

        // Stdout names every written path, one line per layout.
        let stdout = String::from_utf8(out).unwrap();
        let lines = stdout
            .lines()
            .filter(|l| l.starts_with("Installed OpenCode plugin:"))
            .count();
        assert_eq!(lines, 2, "one line per written layout, got: {stdout:?}");
        let xdg_index = plugin_file(&xdg_root).display().to_string();
        let legacy_index = plugin_file(&legacy_root).display().to_string();
        assert!(stdout.contains(xdg_index.as_str()));
        assert!(stdout.contains(legacy_index.as_str()));
    }

    #[test]
    fn install_falls_back_to_xdg_default_when_no_layout_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode"); // absent
        let legacy_root = tmp.path().join(".opencode"); // absent
        let fallback_root = tmp.path().join(".config").join("opencode");

        let mut out = Vec::new();
        install_to_roots(
            &[xdg_root.clone(), legacy_root.clone()],
            || fallback_root.clone(),
            "/bin/deck-fallback",
            &mut out,
        )
        .unwrap();

        let content = std::fs::read_to_string(plugin_file(&fallback_root)).unwrap();
        assert!(content.contains(r#"BINARY_PATH = "/bin/deck-fallback""#));

        let stdout = String::from_utf8(out).unwrap();
        let lines = stdout
            .lines()
            .filter(|l| l.starts_with("Installed OpenCode plugin:"))
            .count();
        assert_eq!(lines, 1);
        let fallback_index = plugin_file(&fallback_root).display().to_string();
        assert!(stdout.contains(fallback_index.as_str()));
    }

    #[test]
    fn install_one_layout_failure_still_writes_other_and_surfaces_error() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg_root = tmp.path().join(".config").join("opencode");
        let legacy_root = tmp.path().join(".opencode");
        std::fs::create_dir_all(&xdg_root).unwrap();
        std::fs::create_dir_all(&legacy_root).unwrap();
        std::fs::write(xdg_root.join("plugin"), b"not a dir").unwrap(); // block XDG

        let fallback_root = tmp.path().join("fallback");
        let mut out = Vec::new();
        let result = install_to_roots(
            &[xdg_root.clone(), legacy_root.clone()],
            || fallback_root.clone(),
            "/bin/deck-resil2",
            &mut out,
        );

        assert!(
            result.is_err(),
            "a failed layout must surface as an io::Result error"
        );
        // The other layout is still written despite the failure.
        assert_plugin_binary(&legacy_root, "/bin/deck-resil2");
        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.contains("Installed OpenCode plugin:"));
    }

    /// Regression (the "OpenCode shows No agent" bug): a user upgrading from the
    /// old nested layout has `plugin/dot-agent-deck/index.js` on disk, which
    /// OpenCode's one-level glob never scanned. Installing must (a) write the flat
    /// `plugin/dot-agent-deck.js` OpenCode DOES discover, and (b) delete the dead
    /// nested dir so no stale copy lingers — converging every user with no manual
    /// step.
    #[test]
    fn install_migrates_stale_nested_layout_to_flat_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".config").join("opencode");
        // Seed the obsolete nested layout as an upgrade would leave it.
        let stale_dir = stale_plugin_dir(&root);
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("index.js"), b"// old nested plugin").unwrap();

        auto_install_to(std::slice::from_ref(&root), "/bin/deck-migrated");

        // Flat, discoverable file is present and current.
        assert_plugin_binary(&root, "/bin/deck-migrated");
        assert!(
            plugin_file(&root).is_file(),
            "flat plugin/dot-agent-deck.js must exist after install"
        );
        // The dead nested dir is gone.
        assert!(
            !stale_dir.exists(),
            "stale nested plugin/dot-agent-deck/ dir must be removed on install"
        );
    }

    /// Uninstall must clear BOTH the current flat file and any leftover nested dir,
    /// so a machine that was never re-installed after the migration still ends up
    /// clean.
    #[test]
    fn uninstall_impl_removes_flat_file_and_nested_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join(".config").join("opencode");

        // A flat file...
        let file = plugin_file(&root);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"// flat plugin").unwrap();
        uninstall_impl(&file).unwrap();
        assert!(!file.exists(), "flat plugin file must be removed");

        // ...and, independently, a stale nested dir.
        let stale_dir = stale_plugin_dir(&root);
        std::fs::create_dir_all(&stale_dir).unwrap();
        std::fs::write(stale_dir.join("index.js"), b"// old").unwrap();
        uninstall_impl(&stale_dir).unwrap();
        assert!(
            !stale_dir.exists(),
            "stale nested plugin dir must be removed"
        );
    }

    // -----------------------------------------------------------------------
    // PRD #381 — the OpenCode plugin's `BINARY_PATH`.
    //
    // Its own tests, and its own parse, because the plugin is generated
    // JAVASCRIPT rather than JSON: the PRD blames exactly that shape difference
    // for OpenCode being the last of the three integrations to be noticed, so a
    // fix that only speaks JSON silently misses it.
    // -----------------------------------------------------------------------

    /// A real executable at `path`, parents created — the resolver's "exists and
    /// is executable" gate is a real `stat`, so this cannot be faked.
    fn write_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("path has a parent")).expect("create parent");
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").expect("write executable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    /// The `…/target/release/dot-agent-deck` the field defect wrote, plus the
    /// durable `<home>/.local/bin/dot-agent-deck` the resolver must prefer.
    fn artifact_and_durable(root: &Path) -> (PathBuf, PathBuf) {
        let artifact = root
            .join("checkout")
            .join("target")
            .join("release")
            .join("dot-agent-deck");
        write_executable(&artifact);
        let durable = root
            .join("home")
            .join(".local")
            .join("bin")
            .join("dot-agent-deck");
        write_executable(&durable);
        (artifact, durable)
    }

    /// Driving the plugin writer with the REAL resolver and a build-artifact
    /// `current_exe()`: `BINARY_PATH` must be the durable path, and no
    /// `target/` path may appear anywhere in the generated JavaScript.
    #[test]
    fn plugin_binary_path_is_never_the_build_artifact_it_was_installed_from() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let (artifact, durable) = artifact_and_durable(tmp.path());
        let root = tmp.path().join("opencode-root");
        std::fs::create_dir_all(&root).expect("create root");

        let resolved = crate::platform::paths::durable_binary_path_with(
            Ok(artifact.clone()),
            &tmp.path().join("home"),
            None,
        )
        .expect("a seeded ~/.local/bin candidate must resolve");
        auto_install_to(std::slice::from_ref(&root), &resolved);

        let js = std::fs::read_to_string(plugin_file(&root)).expect("read plugin");
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            durable.to_str(),
            "BINARY_PATH must be the durable path, not the artifact"
        );
        for marker in ["target/release", "target/debug"] {
            assert!(
                !js.contains(marker),
                "the generated plugin names `{marker}`:\n{js}"
            );
        }
    }

    /// PRD #381 M5: on the AUTO path a `BINARY_PATH` pointing at a deleted file
    /// is repaired, and a still-valid one is left exactly as it was.
    ///
    /// This path used to rewrite `BINARY_PATH` unconditionally on every
    /// dashboard startup, which is the "repair what merely differs" behaviour
    /// Open Question 3 rules out — and on the one integration nobody was
    /// watching.
    #[test]
    fn auto_install_repairs_a_dead_binary_path_and_preserves_a_valid_one() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let (artifact, durable) = artifact_and_durable(tmp.path());
        let home = tmp.path().join("home");
        let resolved =
            crate::platform::paths::durable_binary_path_with(Ok(artifact.clone()), &home, None)
                .expect("resolve durable");

        // A pin that still exists: preserved, byte for byte in the value.
        let valid_root = tmp.path().join("valid-root");
        std::fs::create_dir_all(&valid_root).expect("create root");
        let users_own = tmp.path().join("users-own").join("dot-agent-deck");
        write_executable(&users_own);
        write_plugin(&valid_root, users_own.to_str().expect("UTF-8")).expect("seed valid plugin");

        auto_install_to(std::slice::from_ref(&valid_root), &resolved);
        assert_eq!(
            existing_binary_path(&valid_root).as_deref(),
            users_own.to_str(),
            "a BINARY_PATH that still exists must not be repointed just because it differs"
        );

        // A pin that is positively gone: repaired to the resolved path.
        let dead_root = tmp.path().join("dead-root");
        std::fs::create_dir_all(&dead_root).expect("create root");
        let gone = tmp.path().join("pruned-worktree").join("dot-agent-deck");
        assert!(!gone.exists(), "the dead pin must genuinely not exist");
        write_plugin(&dead_root, gone.to_str().expect("UTF-8")).expect("seed dead plugin");

        auto_install_to(std::slice::from_ref(&dead_root), &resolved);
        assert_eq!(
            existing_binary_path(&dead_root).as_deref(),
            durable.to_str(),
            "a BINARY_PATH whose target is gone must be repaired"
        );

        // Idempotent: a second pass over either root changes nothing.
        let before = std::fs::read(plugin_file(&dead_root)).expect("read repaired plugin");
        auto_install_to(std::slice::from_ref(&dead_root), &resolved);
        assert_eq!(
            before,
            std::fs::read(plugin_file(&dead_root)).expect("read again"),
            "a second auto-install pass changed the repaired plugin"
        );
    }

    /// PRD #381 M6: a refusal writes nothing at all — not a truncated plugin,
    /// not even the `plugin/` directory. An abandoned half-written file is
    /// worse than none: OpenCode would load it.
    #[test]
    fn auto_install_refusal_leaves_no_plugin_behind() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("opencode-root");
        std::fs::create_dir_all(&root).expect("create root");

        auto_install_resolved(
            std::slice::from_ref(&root),
            Err("no durable dot-agent-deck".to_string()),
        );

        assert!(
            !plugin_file(&root).exists(),
            "a refused auto-install wrote {}",
            plugin_file(&root).display()
        );
        assert!(
            !root.join("plugin").exists(),
            "a refused auto-install created the plugin directory"
        );
    }

    /// The JS parse is the one that has to survive a hand-edited or truncated
    /// file: anything it cannot read confidently is `None`, which
    /// [`auto_install_to`] treats as "no pin to preserve" and overwrites.
    #[test]
    fn existing_binary_path_reads_the_generated_literal_and_nothing_else() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        write_plugin(&root, "/with space/dot-agent-deck").expect("write plugin");
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some("/with space/dot-agent-deck"),
            "a quoted path must round-trip through the JS literal"
        );

        let truncated = tmp.path().join("truncated");
        let file = plugin_file(&truncated);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("create dir");
        std::fs::write(&file, b"const BINARY_PATH = \"unterminated").expect("write truncated");
        assert_eq!(
            existing_binary_path(&truncated),
            None,
            "a truncated plugin must not be read as a valid pin"
        );

        let absent = tmp.path().join("absent");
        assert_eq!(existing_binary_path(&absent), None);
    }

    /// Reviewer N1. The parse used to split on the literal `";\n"`, so a plugin
    /// a user had opened in a CRLF editor terminated its line with `;\r\n`, read
    /// back as `None`, and [`auto_install_to`] then treated a perfectly valid
    /// pin as "nothing to preserve" and clobbered it with the launching build's
    /// path — the "repair only when unusable" rule silently bypassed for that
    /// corner.
    #[test]
    fn a_crlf_converted_plugin_still_yields_its_pin_and_keeps_it() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        let users_own = tmp.path().join("users-own").join("dot-agent-deck");
        write_executable(&users_own);
        let pin = users_own.to_str().expect("UTF-8");

        write_plugin(&root, pin).expect("seed plugin");
        let file = plugin_file(&root);
        let crlf = std::fs::read_to_string(&file)
            .expect("read plugin")
            .replace('\n', "\r\n");
        std::fs::write(&file, &crlf).expect("rewrite as CRLF");

        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some(pin),
            "a CRLF line ending must not hide the pin"
        );

        auto_install_to(std::slice::from_ref(&root), "/bin/deck-launching");
        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some(pin),
            "a valid pin was clobbered because its line ended CRLF"
        );
    }

    /// PRD #381 audit, MEDIUM-1 (the OpenCode half). `split_once` accepted the
    /// first `const BINARY_PATH = ` marker ANYWHERE in the file, so a mention
    /// inside a comment in a hand-edited plugin became the preserved — and
    /// republished — value. The marker is now anchored to the start of a line,
    /// which is where [`plugin_template`] emits it.
    #[test]
    fn a_commented_out_marker_cannot_become_the_preserved_pin() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        let users_own = tmp.path().join("users-own").join("dot-agent-deck");
        write_executable(&users_own);
        let pin = users_own.to_str().expect("UTF-8");

        write_plugin(&root, pin).expect("seed plugin");
        let file = plugin_file(&root);
        let body = std::fs::read_to_string(&file).expect("read plugin");
        let hand_edited =
            format!("// was: const BINARY_PATH = \"/tmp/attacker/dot-agent-deck\";\n{body}");
        std::fs::write(&file, &hand_edited).expect("rewrite with a comment above");

        assert_eq!(
            existing_binary_path(&root).as_deref(),
            Some(pin),
            "a commented-out marker must not be read as the pin"
        );

        auto_install_to(std::slice::from_ref(&root), "/bin/deck-launching");
        let after = std::fs::read_to_string(&file).expect("read regenerated plugin");
        assert!(
            !after.contains("/tmp/attacker/dot-agent-deck"),
            "a commented-out path was promoted into the generated constant:\n{after}"
        );
    }

    /// PRD #381 audit, MEDIUM-2: the plugin is JavaScript OpenCode *executes*,
    /// and it is now published temp-file-plus-`rename` like the other three
    /// config writers. `std::fs::write` followed a pre-created symlink at the
    /// destination and truncated it in place; `rename` replaces the name.
    #[cfg(unix)]
    #[test]
    fn write_plugin_publishes_atomically_and_does_not_write_through_a_symlink() {
        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        let file = plugin_file(&root);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("create plugin dir");

        let elsewhere = tmp.path().join("elsewhere.js");
        std::fs::write(&elsewhere, b"// the victim's own file\n").expect("seed the link target");
        std::os::unix::fs::symlink(&elsewhere, &file).expect("pre-create the destination symlink");

        write_plugin(&root, "/bin/deck").expect("publish plugin");

        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("read the link target"),
            "// the victim's own file\n",
            "the publish wrote through a pre-created destination symlink"
        );
        assert!(
            std::fs::symlink_metadata(&file)
                .expect("stat the published plugin")
                .file_type()
                .is_file(),
            "the destination should now be a regular file, not the symlink"
        );
        assert_eq!(existing_binary_path(&root).as_deref(), Some("/bin/deck"));

        // No temp file is left beside the destination.
        let strays: Vec<_> = std::fs::read_dir(file.parent().expect("parent"))
            .expect("list plugin dir")
            .map(|e| e.expect("dir entry").file_name())
            .filter(|n| n != "dot-agent-deck.js")
            .collect();
        assert!(strays.is_empty(), "temp file left behind: {strays:?}");
    }

    /// PRD #381 audit, MEDIUM-1 (OpenCode, the pin side): a legacy plugin
    /// pinning the BARE `"dot-agent-deck"` is repaired even when the process cwd
    /// holds a file of that name. `Path::try_exists` is cwd-relative; Node's
    /// `execFileSync` resolves the persisted string through the AGENT's `$PATH`.
    #[test]
    #[serial_test::serial]
    fn a_bare_pin_is_repaired_even_when_the_cwd_holds_a_file_of_that_name() {
        struct CwdGuard(PathBuf);
        impl Drop for CwdGuard {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }

        let tmp = crate::test_temp::tempdir().expect("plugin tempdir");
        let root = tmp.path().join("root");
        write_plugin(&root, crate::platform::paths::DEFAULT_BINARY_NAME).expect("seed bare pin");

        let cwd = tmp.path().join("some-checkout");
        write_executable(&cwd.join(crate::platform::paths::DEFAULT_BINARY_NAME));
        let _restore = CwdGuard(std::env::current_dir().expect("read cwd"));
        std::env::set_current_dir(&cwd).expect("move into the decoy directory");
        assert_eq!(
            Path::new(crate::platform::paths::DEFAULT_BINARY_NAME)
                .try_exists()
                .ok(),
            Some(true),
            "the fixture must reproduce the cwd collision, or it proves nothing"
        );

        let durable = tmp.path().join("opt").join("dot-agent-deck");
        write_executable(&durable);
        auto_install_to(
            std::slice::from_ref(&root),
            durable.to_str().expect("UTF-8"),
        );

        assert_eq!(
            existing_binary_path(&root).as_deref(),
            durable.to_str(),
            "issue #536: a bare BINARY_PATH survived because the cwd held a file \
             of that name"
        );
    }
}
