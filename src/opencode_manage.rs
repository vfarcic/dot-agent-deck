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
        r#"const {{ execFileSync }} = require("child_process");
const BINARY_PATH = {binary_path_json};

module.exports = {{
  name: "dot-agent-deck",
  subscribe: [
    "session.created",
    "session.deleted",
    "session.idle",
    "session.error",
    "session.status.updated",
    "tool.execute.before",
    "tool.execute.after",
    "permission.asked",
  ],
  onEvent(event) {{
    try {{
      execFileSync(BINARY_PATH, ["hook", "--agent", "opencode"], {{
        input: JSON.stringify(event),
        timeout: 5000,
        stdio: ["pipe", "ignore", "ignore"],
      }});
    }} catch (_) {{}}
  }},
}};
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
        assert!(content.contains("execFileSync"));
        assert!(!content.contains("execSync("));
        assert!(content.contains(r#"BINARY_PATH = "/usr/local/bin/dot-agent-deck""#));
        assert!(content.contains(r#"["hook", "--agent", "opencode"]"#));
        assert!(content.contains("session.created"));
        assert!(content.contains("onEvent"));
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
}
