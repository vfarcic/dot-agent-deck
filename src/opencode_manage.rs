use std::path::{Path, PathBuf};

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

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
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
    std::fs::write(&path, content)?;

    Ok(path)
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
    for root in roots {
        if !root.exists() {
            continue;
        }
        match write_plugin(root, binary_path) {
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
/// Intended for dashboard startup — never prints to stdout.
pub fn auto_install() {
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    auto_install_to(&candidate_roots(), &binary_path);
}

pub fn install() -> std::io::Result<()> {
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

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
}
