use std::path::PathBuf;

fn plugin_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".opencode")
        .join("plugin")
        .join("dot-agent-deck")
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

fn install_impl(dir: &PathBuf, binary_path: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;

    let path = dir.join("index.js");
    let content = plugin_template(binary_path);
    std::fs::write(&path, content)?;

    println!("Installed OpenCode plugin: {}", path.display());
    Ok(())
}

fn uninstall_impl(dir: &PathBuf) -> std::io::Result<()> {
    if !dir.exists() {
        println!("No OpenCode plugin found to remove.");
        return Ok(());
    }

    std::fs::remove_dir_all(dir)?;

    println!("Removed OpenCode plugin: {}", dir.display());
    Ok(())
}

/// Silently install OpenCode plugin if OpenCode is detected.
/// Intended for dashboard startup — never prints to stdout.
pub fn auto_install() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let opencode_dir = PathBuf::from(&home).join(".opencode");
    if !opencode_dir.exists() {
        return;
    }

    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    let dir = plugin_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("auto-install: failed to create OpenCode plugin dir: {e}");
        return;
    }

    let path = dir.join("index.js");
    let content = plugin_template(&binary_path);
    if let Err(e) = std::fs::write(&path, content) {
        tracing::warn!("auto-install: failed to write OpenCode plugin: {e}");
        return;
    }

    tracing::info!("auto-installed OpenCode plugin: {}", path.display());
}

/// Auto-install to a custom dir, checking a custom opencode_dir for detection (for testing).
pub fn auto_install_to(opencode_dir: &std::path::Path, target_dir: &std::path::Path) {
    if !opencode_dir.exists() {
        return;
    }

    let binary_path = "dot-agent-deck".to_string();

    std::fs::create_dir_all(target_dir).expect("failed to create plugin dir");

    let path = target_dir.join("index.js");
    let content = plugin_template(&binary_path);
    std::fs::write(&path, content).expect("failed to write plugin");
}

pub fn install() -> std::io::Result<()> {
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dot-agent-deck".into());

    install_impl(&plugin_dir(), &binary_path)
}

pub fn uninstall() -> std::io::Result<()> {
    uninstall_impl(&plugin_dir())
}

// --- Testable versions that accept a custom path ---

pub fn install_to(dir: &PathBuf, binary_path: &str) -> std::io::Result<()> {
    install_impl(dir, binary_path)
}

pub fn uninstall_from(dir: &PathBuf) -> std::io::Result<()> {
    uninstall_impl(dir)
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

    #[test]
    fn install_creates_plugin_file() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("dot-agent-deck");

        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck").unwrap();

        let index = plugin_dir.join("index.js");
        assert!(index.exists());
        let content = std::fs::read_to_string(&index).unwrap();
        assert!(content.contains("execFileSync"));
        assert!(content.contains(r#"BINARY_PATH = "/usr/local/bin/dot-agent-deck""#));
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("dot-agent-deck");

        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck").unwrap();
        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck").unwrap();

        let index = plugin_dir.join("index.js");
        assert!(index.exists());
        let content = std::fs::read_to_string(&index).unwrap();
        assert!(content.contains("dot-agent-deck"));
    }

    #[test]
    fn uninstall_removes_directory() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("dot-agent-deck");

        install_to(&plugin_dir, "/usr/local/bin/dot-agent-deck").unwrap();
        assert!(plugin_dir.exists());

        uninstall_from(&plugin_dir).unwrap();
        assert!(!plugin_dir.exists());
    }

    #[test]
    fn uninstall_noop_when_no_dir() {
        let dir = tempfile::tempdir().unwrap();
        let plugin_dir = dir.path().join("nonexistent");

        uninstall_from(&plugin_dir).unwrap(); // Should not panic
    }

    #[test]
    fn auto_install_skips_when_no_opencode_dir() {
        let dir = tempfile::tempdir().unwrap();
        let opencode_dir = dir.path().join("nonexistent_opencode");
        let target_dir = dir.path().join("plugin");

        auto_install_to(&opencode_dir, &target_dir);
        assert!(
            !target_dir.exists(),
            "Should not create plugin when opencode dir missing"
        );
    }

    #[test]
    fn auto_install_installs_when_opencode_dir_exists() {
        let dir = tempfile::tempdir().unwrap();
        let opencode_dir = dir.path().join(".opencode");
        std::fs::create_dir(&opencode_dir).unwrap();
        let target_dir = dir.path().join("plugin");

        auto_install_to(&opencode_dir, &target_dir);

        let index = target_dir.join("index.js");
        assert!(
            index.exists(),
            "Should create index.js when opencode exists"
        );
        let content = std::fs::read_to_string(&index).unwrap();
        assert!(content.contains("dot-agent-deck"));
    }

    #[test]
    fn auto_install_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let opencode_dir = dir.path().join(".opencode");
        std::fs::create_dir(&opencode_dir).unwrap();
        let target_dir = dir.path().join("plugin");

        auto_install_to(&opencode_dir, &target_dir);
        auto_install_to(&opencode_dir, &target_dir);

        let index = target_dir.join("index.js");
        assert!(index.exists());
    }
}
